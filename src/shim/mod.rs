// driver shim: exports, real driver discovery, entry-point resolution and the hooked entry points
mod chain;
mod chain_sizes;
mod context;
mod fixed;
mod proxy;
mod sync;

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};

use ash::{ext, khr, vk};
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};

use crate::{generator, log, settings, shaders, vkutil};

// format_args! defers the formatting until log_fmt has checked the level
macro_rules! debug { ($($a:tt)*) => { log::log_fmt(log::Level::Debug, format_args!($($a)*)) } }
macro_rules! info { ($($a:tt)*) => { log::log_fmt(log::Level::Info, format_args!($($a)*)) } }
macro_rules! warn { ($($a:tt)*) => { log::log_fmt(log::Level::Warn, format_args!($($a)*)) } }
macro_rules! error { ($($a:tt)*) => { log::log_fmt(log::Level::Error, format_args!($($a)*)) } }

// with panic = "abort" the hook is the only chance to get the message into the log file
fn panic_to_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| error!("lsfg-metal shim panic: {info}")));
    });
}

// set by build.rs
pub const VERSION: &CStr = unsafe {
    CStr::from_bytes_with_nul_unchecked(concat!(env!("LSFGM_VERSION"), "\0").as_bytes())
};

// a vulkan error keeps its code (the present path maps it back), anything else is text
pub enum Error {
    Vk(vk::Result),
    Msg(String),
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Msg(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Msg(s.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Vk(r) => write!(f, "{r:?}"),
            Error::Msg(s) => f.write_str(s),
        }
    }
}

// swapchain wrapper: fixed here, proxy in the metal module; acquire and get_images share the wrapper read lock
#[allow(clippy::missing_safety_doc)]
pub trait Swapchain: Send + Sync {
    unsafe fn present(
        &mut self,
        queue: vk::Queue,
        info: &vk::PresentInfoKHR,
        real: vk::PFN_vkQueuePresentKHR,
    ) -> Result<vk::Result, Error>;
    // proxy only; the fixed wrapper is never asked, the real functions run instead
    unsafe fn get_images(
        &self,
        _count: *mut u32,
        _images: *mut vk::Image,
    ) -> Result<vk::Result, Error> {
        Ok(vk::Result::SUCCESS)
    }
    unsafe fn acquire(
        &self,
        _timeout: u64,
        _semaphore: vk::Semaphore,
        _fence: vk::Fence,
        _index: *mut u32,
    ) -> Result<vk::Result, Error> {
        Ok(vk::Result::SUCCESS)
    }
    fn as_proxy(&self) -> Option<&crate::metal::ProxySwapchain> {
        None
    }
}

// ---- real driver ----

pub type Created = (vk::SwapchainKHR, Box<dyn Swapchain>);

pub struct Driver {
    _lib: libloading::os::unix::Library,
    pub entry: ash::Entry,
    pub gipa: vk::PFN_vkGetInstanceProcAddr,
    pub gdpa: vk::PFN_vkGetDeviceProcAddr,
    pub path: String,
}

#[repr(C)]
struct DlInfo {
    fname: *const c_char,
    fbase: *mut c_void,
    sname: *const c_char,
    saddr: *mut c_void,
}

extern "C" {
    fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
}

fn shim_dir() -> Option<PathBuf> {
    let mut info = DlInfo {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    unsafe {
        if dladdr(shim_dir as *const c_void, &mut info) == 0 || info.fname.is_null() {
            return None;
        }
        Some(
            PathBuf::from(CStr::from_ptr(info.fname).to_string_lossy().into_owned())
                .parent()?
                .to_path_buf(),
        )
    }
}

fn load_driver() -> Option<Driver> {
    let path = settings::os_env("LSFGM_MOLTENVK")
        .filter(|p| !p.is_empty())
        .or_else(|| {
            shim_dir().map(|d| {
                d.join("libMoltenVK.real.dylib")
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .unwrap_or_default();
    let fail = |e: &str| {
        error!("lsfg-metal shim: cannot load real MoltenVK from '{path}': {e}");
        None
    };
    if path.is_empty() {
        return fail("unknown error");
    }
    use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
    let lib = match unsafe { Library::open(Some(&path), RTLD_NOW | RTLD_LOCAL) } {
        Ok(l) => l,
        Err(e) => return fail(&e.to_string()),
    };
    let sym = |name: &[u8], what: &str| unsafe {
        lib.get::<unsafe extern "system" fn()>(name)
            .ok()
            .map(|s| *s)
            .or_else(|| {
                error!("lsfg-metal shim: real MoltenVK lacks {what}");
                None
            })
    };
    let gipa: vk::PFN_vkGetInstanceProcAddr =
        unsafe { std::mem::transmute(sym(b"vkGetInstanceProcAddr\0", "vkGetInstanceProcAddr")?) };
    let gdpa: vk::PFN_vkGetDeviceProcAddr =
        unsafe { std::mem::transmute(sym(b"vkGetDeviceProcAddr\0", "vkGetDeviceProcAddr")?) };
    if std::ptr::fn_addr_eq(gipa, vkGetInstanceProcAddr as vk::PFN_vkGetInstanceProcAddr) {
        error!("lsfg-metal shim: '{path}' resolved to this shim, not to MoltenVK (leaf name hijacked by DYLD_LIBRARY_PATH?); refusing to recurse");
        return None;
    }
    let entry = unsafe {
        ash::Entry::from_static_fn(ash::StaticFn {
            get_instance_proc_addr: gipa,
        })
    };
    Some(Driver {
        _lib: lib,
        entry,
        gipa,
        gdpa,
        path,
    })
}

static DRIVER: OnceLock<Option<Driver>> = OnceLock::new();
static ACTIVE: OnceLock<bool> = OnceLock::new();

pub fn driver() -> Option<&'static Driver> {
    DRIVER
        .get_or_init(|| {
            panic_to_log();
            load_driver()
        })
        .as_ref()
}

// the layer decides between hooks and passthrough
fn active(drv: &Driver) -> bool {
    *ACTIVE.get_or_init(|| {
        let a = layer().is_some();
        if a {
            info!("lsfg-metal shim active in front of {}", drv.path);
        }
        a
    })
}

// ---- layer ----

pub struct Layer {
    state: Mutex<LayerState>,
}

struct LayerState {
    config: settings::Config,
    profile: usize,
    revision: u32,
    watcher: Option<settings::Watcher>,
}

fn level(l: settings::LogLevel) -> log::Level {
    match l {
        settings::LogLevel::Debug => log::Level::Debug,
        settings::LogLevel::Info => log::Level::Info,
        settings::LogLevel::Warning => log::Level::Warn,
        settings::LogLevel::Error => log::Level::Error,
    }
}

static LAYER: OnceLock<Option<Layer>> = OnceLock::new();

pub fn layer() -> Option<&'static Layer> {
    LAYER
        .get_or_init(|| {
            let l = match Layer::new() {
                Ok(l) => l,
                Err(e) => {
                    error!("lsfg-metal shim: failed to initialize, passing through:");
                    error!("- {e}");
                    None
                }
            }?;
            // metal never reaches back up here, so push its configuration the moment we have one
            let (dll, allow_fp16) = l.config(|c| (c.dll.clone(), c.allow_half_precision));
            crate::metal::init(crate::metal::Setup::new(l.profile(), allow_fp16, dll));
            Some(l)
        })
        .as_ref()
}

impl Layer {
    fn new() -> Result<Option<Layer>, String> {
        let config = settings::load()?;
        let Some((profile, method)) = settings::identify(&config) else {
            return Ok(None);
        };
        log::set_level(level(config.log_level));
        if let Some(f) = &config.log_file {
            log::set_file(f);
        }
        let p = &config.profiles[profile];
        info!(
            "Loaded lsfg-metal layer version {} pid={}",
            VERSION.to_string_lossy(),
            std::process::id()
        );
        info!(
            "Using profile with name '{}' (identified via {})",
            p.name,
            method.name()
        );
        info!("  Pacing: {}", p.pacing_mode.name());
        info!("  Multiplier: {}", p.multiplier);
        info!("  Flow scale: {:.2}", p.flow_scale);
        info!("  Performance mode: {}", p.performance_mode);
        let watcher = settings::os_env("LSFGM_ENV")
            .is_none()
            .then(|| settings::Watcher::new(settings::config_path(&settings::os_env)));
        Ok(Some(Layer {
            state: Mutex::new(LayerState {
                config,
                profile,
                revision: 0,
                watcher,
            }),
        }))
    }

    pub fn profile(&self) -> settings::Profile {
        let s = self.state.lock().unwrap();
        s.config.profiles[s.profile].clone()
    }

    pub fn config<R>(&self, f: impl FnOnce(&settings::Config) -> R) -> R {
        f(&self.state.lock().unwrap().config)
    }

    pub fn revision(&self) -> u32 {
        self.state.lock().unwrap().revision
    }

    pub fn multiplier(&self) -> u32 {
        let s = self.state.lock().unwrap();
        s.config.profiles[s.profile].multiplier
    }

    // reload on file change; true when the active profile was replaced (revision bumped)
    pub fn update(&self) -> Result<bool, String> {
        let mut s = self.state.lock().unwrap();
        let Some(w) = &mut s.watcher else {
            return Ok(false);
        };
        let Some(cfg) = w.check_and_reload(&settings::os_env)? else {
            return Ok(false);
        };
        info!("Config file changed on disk, reloading...");
        let name = &s.config.profiles[s.profile].name;
        let Some(i) = cfg.profiles.iter().position(|p| &p.name == name) else {
            return Ok(false);
        };
        s.profile = i;
        s.revision += 1;
        log::set_level(level(cfg.log_level));
        s.config = cfg;
        Ok(true)
    }
}

// ---- global state ----

pub struct InstanceEntry {
    pub handle: vk::Instance,
    pub gipa: vk::PFN_vkGetInstanceProcAddr,
    destroy: vk::PFN_vkDestroyInstance,
    surface_support: Option<vk::PFN_vkGetPhysicalDeviceSurfaceSupportKHR>,
    hook: Option<InstanceHook>,
}

// dispatch for hooked instances; the KHR loaders serve the properties2/features2 calls on vulkan 1.0 instances
pub struct InstanceHook {
    pub instance: ash::Instance,
    pub props2: khr::get_physical_device_properties2::Instance,
    pub surface: khr::surface::Instance,
}

pub struct DeviceEntry {
    pub handle: vk::Device,
    pub physical: vk::PhysicalDevice,
    pub gdpa: vk::PFN_vkGetDeviceProcAddr,
    destroy: vk::PFN_vkDestroyDevice,
    pub hook: Option<DeviceHook>,
}

pub struct DeviceHook {
    pub inst: Arc<InstanceEntry>,
    pub device: ash::Device,
    pub swapchain: khr::swapchain::Device,
    pub physical: vk::PhysicalDevice,
    pub family: u32,
    pub queue: vk::Queue,
    pub pool: vk::CommandPool,
    pub queue_mutex: sync::Recursive,
    pub fp16: bool,
    pub proxy_supported: AtomicBool,
    generator: Mutex<Option<Arc<generator::Instance>>>,
}

impl DeviceHook {
    // lazy generator instance adopting the game's device
    pub fn generator(&self) -> Result<Arc<generator::Instance>, String> {
        let mut g = self.generator.lock().unwrap();
        if let Some(g) = &*g {
            return Ok(g.clone());
        }
        let (dll, allow) = layer()
            .expect("active layer")
            .config(|c| (c.dll.clone(), c.allow_half_precision));
        let dll = dll
            .map(|d| shaders::fix_dll_path(Path::new(&d)))
            .or_else(shaders::find_dll)
            .unwrap_or_default();
        info!(
            "Initializing lsfg-metal instance with half precision {}",
            if allow { "enabled" } else { "disabled" }
        );
        let inst = generator::Instance::adopt(
            self.inst.gipa,
            self.inst.handle,
            self.physical,
            self.device.handle(),
            self.family,
            allow && self.fp16,
            &dll,
            log::debug,
        )?;
        let inst = Arc::new(inst);
        *g = Some(inst.clone());
        Ok(inst)
    }
}

struct SwapchainEntry {
    device: Arc<DeviceEntry>,
    destroy: vk::PFN_vkDestroySwapchainKHR,
    present: vk::PFN_vkQueuePresentKHR,
    wrapper: Option<RwLock<Box<dyn Swapchain>>>,
    proxy: bool,
}

#[derive(Default)]
struct Maps {
    instances: HashMap<vk::Instance, Arc<InstanceEntry>>,
    physical: HashMap<vk::PhysicalDevice, vk::Instance>,
    devices: HashMap<vk::Device, Arc<DeviceEntry>>,
    queues: HashMap<vk::Queue, vk::Device>,
    swapchains: HashMap<vk::SwapchainKHR, Arc<SwapchainEntry>>,
}

static MAPS: LazyLock<RwLock<Maps>> = LazyLock::new(Default::default);
static API: sync::Recursive = sync::Recursive::new();

fn instance_of(pd: vk::PhysicalDevice) -> Option<Arc<InstanceEntry>> {
    let m = MAPS.read().unwrap();
    m.physical
        .get(&pd)
        .and_then(|i| m.instances.get(i))
        .cloned()
}

fn device_of(device: vk::Device) -> Option<Arc<DeviceEntry>> {
    MAPS.read().unwrap().devices.get(&device).cloned()
}

fn queue_device(queue: vk::Queue) -> Option<Arc<DeviceEntry>> {
    let m = MAPS.read().unwrap();
    m.queues.get(&queue).and_then(|d| m.devices.get(d)).cloned()
}

fn swapchain_of(sc: vk::SwapchainKHR) -> Option<Arc<SwapchainEntry>> {
    MAPS.read().unwrap().swapchains.get(&sc).cloned()
}

// ---- helpers ----

unsafe fn ifetch<T>(
    gipa: vk::PFN_vkGetInstanceProcAddr,
    instance: vk::Instance,
    name: &CStr,
) -> Option<T> {
    gipa(instance, name.as_ptr()).map(|f| std::mem::transmute_copy(&f))
}

unsafe fn dfetch<T>(
    gdpa: vk::PFN_vkGetDeviceProcAddr,
    device: vk::Device,
    name: &CStr,
) -> Option<T> {
    gdpa(device, name.as_ptr()).map(|f| std::mem::transmute_copy(&f))
}

// real device function for hooks that forward: via the registered gdpa, else the driver's
unsafe fn real_device_fn<T>(device: vk::Device, name: &CStr) -> Option<T> {
    let gdpa = device_of(device)
        .map(|d| d.gdpa)
        .or_else(|| driver().map(|d| d.gdpa))?;
    dfetch(gdpa, device, name)
}

fn void(f: *const ()) -> vk::PFN_vkVoidFunction {
    Some(unsafe { std::mem::transmute::<*const (), unsafe extern "system" fn()>(f) })
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        "null".into()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn handle<H: vk::Handle>(h: H) -> String {
    match h.as_raw() {
        0 => "null".into(),
        raw => format!("{raw:#x}"),
    }
}

unsafe fn names(pp: *const *const c_char, n: u32) -> Vec<*const c_char> {
    if pp.is_null() || n == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(pp, n as usize).to_vec()
    }
}

unsafe fn has_name(list: &[*const c_char], name: &CStr) -> bool {
    list.iter()
        .any(|&p| !p.is_null() && CStr::from_ptr(p) == name)
}

unsafe fn add_name(list: &mut Vec<*const c_char>, name: &'static CStr) {
    if !has_name(list, name) {
        list.push(name.as_ptr());
    }
}

// ---- hook table ----

fn hook_table(name: &[u8]) -> vk::PFN_vkVoidFunction {
    let f = match name {
        b"vkCreateInstance" => create_instance as *const (),
        b"vkDestroyInstance" => destroy_instance as *const (),
        b"vkCreateDevice" => create_device as *const (),
        b"vkDestroyDevice" => destroy_device as *const (),
        b"vkGetDeviceProcAddr" => vkGetDeviceProcAddr as *const (),
        b"vkGetPhysicalDeviceSurfaceSupportKHR" => surface_support as *const (),
        b"vkCreateSwapchainKHR" => create_swapchain as *const (),
        b"vkDestroySwapchainKHR" => destroy_swapchain as *const (),
        b"vkQueuePresentKHR" => queue_present as *const (),
        b"vkQueueSubmit" => queue_submit as *const (),
        b"vkQueueSubmit2" => queue_submit2 as *const (),
        b"vkQueueSubmit2KHR" => queue_submit2_khr as *const (),
        b"vkQueueBindSparse" => queue_bind_sparse as *const (),
        b"vkQueueWaitIdle" => queue_wait_idle as *const (),
        b"vkGetSwapchainImagesKHR" => get_swapchain_images as *const (),
        b"vkAcquireNextImageKHR" => acquire_next_image as *const (),
        b"vkAcquireNextImage2KHR" => acquire_next_image2 as *const (),
        b"vkSetHdrMetadataEXT" => set_hdr_metadata as *const (),
        b"vkWaitForPresentKHR" => wait_for_present as *const (),
        b"vkGetSwapchainStatusKHR" => get_swapchain_status as *const (),
        b"vkGetPastPresentationTimingGOOGLE" => get_past_presentation_timing as *const (),
        b"vkGetRefreshCycleDurationGOOGLE" => get_refresh_cycle_duration as *const (),
        b"vkReleaseSwapchainImagesEXT" => release_swapchain_images as *const (),
        b"vkDestroySurfaceKHR" => destroy_surface as *const (),
        _ => return None,
    };
    void(f)
}

// ---- exports ----

/// # Safety
/// vulkan entry point; `name` is a nul-terminated string or null
#[no_mangle]
pub unsafe extern "system" fn vkGetInstanceProcAddr(
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let drv = driver()?;
    if !active(drv) {
        return (drv.gipa)(instance, name);
    }
    let r = resolve_instance(drv, instance, name);
    debug!(
        "gipa({}, {}) -> {}",
        handle(instance),
        cstr(name),
        if r.is_some() { "ok" } else { "null" }
    );
    r
}

// instance level
unsafe fn resolve_instance(
    drv: &Driver,
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    let n = CStr::from_ptr(name).to_bytes();
    match n {
        b"vkGetInstanceProcAddr" => return void(vkGetInstanceProcAddr as *const ()),
        b"vkCreateInstance" => return void(create_instance as *const ()),
        _ => {}
    }
    if instance == vk::Instance::null() {
        return (drv.gipa)(instance, name);
    }
    match n {
        b"vkCreateMetalSurfaceEXT" => {
            return (drv.gipa)(instance, name)
                .and_then(|_| void(vkCreateMetalSurfaceEXT as *const ()))
        }
        b"vkCreateMacOSSurfaceMVK" => {
            return (drv.gipa)(instance, name)
                .and_then(|_| void(vkCreateMacOSSurfaceMVK as *const ()))
        }
        _ => {}
    }
    let entry = MAPS.read().unwrap().instances.get(&instance).cloned()?;
    if let Some(h) = hook_table(n) {
        return (entry.gipa)(instance, name).and(Some(h));
    }
    (entry.gipa)(instance, name)
}

/// # Safety
/// vulkan entry point; `name` is a nul-terminated string or null
#[no_mangle]
pub unsafe extern "system" fn vkGetDeviceProcAddr(
    device: vk::Device,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let drv = driver()?;
    if !active(drv) {
        return (drv.gdpa)(device, name);
    }
    let r = resolve_device(drv, device, name);
    debug!(
        "gdpa({}, {}) -> {}",
        handle(device),
        cstr(name),
        if r.is_some() { "ok" } else { "null" }
    );
    r
}

// device level
unsafe fn resolve_device(
    drv: &Driver,
    device: vk::Device,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if name.is_null() || device == vk::Device::null() {
        return None;
    }
    let Some(entry) = device_of(device) else {
        return (drv.gdpa)(device, name);
    };
    if let Some(h) = hook_table(CStr::from_ptr(name).to_bytes()) {
        return (entry.gdpa)(device, name).and(Some(h));
    }
    (entry.gdpa)(device, name)
}

// surface exports: register the layer, forward, record (surface -> layer)
unsafe fn create_surface(
    instance: vk::Instance,
    name: &CStr,
    layer_ptr: *const c_void,
    surface: *mut vk::SurfaceKHR,
    call: impl FnOnce(unsafe extern "system" fn()) -> vk::Result,
) -> vk::Result {
    let Some(drv) = driver() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    active(drv);
    proxy::register_layer(layer_ptr);
    let Some(real) = (drv.gipa)(instance, name.as_ptr()) else {
        return vk::Result::ERROR_EXTENSION_NOT_PRESENT;
    };
    let r = call(real);
    if r == vk::Result::SUCCESS {
        proxy::register_surface(*surface, layer_ptr);
    }
    r
}

/// # Safety
/// vulkan entry point
#[no_mangle]
pub unsafe extern "system" fn vkCreateMetalSurfaceEXT(
    instance: vk::Instance,
    info: *const vk::MetalSurfaceCreateInfoEXT,
    alloc: *const vk::AllocationCallbacks,
    surface: *mut vk::SurfaceKHR,
) -> vk::Result {
    let layer_ptr = if info.is_null() {
        std::ptr::null()
    } else {
        (*info).p_layer.cast::<c_void>()
    };
    create_surface(
        instance,
        c"vkCreateMetalSurfaceEXT",
        layer_ptr,
        surface,
        |f| {
            std::mem::transmute::<unsafe extern "system" fn(), vk::PFN_vkCreateMetalSurfaceEXT>(f)(
                instance, info, alloc, surface,
            )
        },
    )
}

// the view's layer when the view is an NSView, or the view itself when it already is a CAMetalLayer
unsafe fn view_layer(view: *const c_void) -> *const c_void {
    if view.is_null() {
        return std::ptr::null();
    }
    let obj = &*view.cast::<AnyObject>();
    if let Some(cls) = AnyClass::get(c"CAMetalLayer") {
        let is_layer: bool = msg_send![obj, isKindOfClass: cls];
        if is_layer {
            return view;
        }
    }
    let Some(cls) = AnyClass::get(c"NSView") else {
        return std::ptr::null();
    };
    let is_view: bool = msg_send![obj, isKindOfClass: cls];
    if !is_view {
        return std::ptr::null();
    }
    let layer: *mut AnyObject = msg_send![obj, layer];
    layer.cast()
}

/// # Safety
/// vulkan entry point
#[no_mangle]
pub unsafe extern "system" fn vkCreateMacOSSurfaceMVK(
    instance: vk::Instance,
    info: *const vk::MacOSSurfaceCreateInfoMVK,
    alloc: *const vk::AllocationCallbacks,
    surface: *mut vk::SurfaceKHR,
) -> vk::Result {
    let layer_ptr = if info.is_null() {
        std::ptr::null()
    } else {
        view_layer((*info).p_view)
    };
    create_surface(
        instance,
        c"vkCreateMacOSSurfaceMVK",
        layer_ptr,
        surface,
        |f| {
            std::mem::transmute::<unsafe extern "system" fn(), vk::PFN_vkCreateMacOSSurfaceMVK>(f)(
                instance, info, alloc, surface,
            )
        },
    )
}

// ---- instance ----

unsafe extern "system" fn create_instance(
    info: *const vk::InstanceCreateInfo,
    alloc: *const vk::AllocationCallbacks,
    out: *mut vk::Instance,
) -> vk::Result {
    let _api = API.lock();
    let Some(drv) = driver() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let ci = &*info;
    let exts = names(ci.pp_enabled_extension_names, ci.enabled_extension_count);
    debug!("vkCreateInstance entered ({} extensions)", exts.len());
    let Some(real) =
        ifetch::<vk::PFN_vkCreateInstance>(drv.gipa, vk::Instance::null(), c"vkCreateInstance")
    else {
        error!("Could not resolve vkCreateInstance");
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let fail = |what: &str| {
        error!("An error occurred while initializing the lsfg-metal instance:");
        error!("- {what}");
        vk::Result::ERROR_INITIALIZATION_FAILED
    };
    let mut hooked = has_name(&exts, khr::surface::NAME);
    let mut r = vk::Result::ERROR_INITIALIZATION_FAILED;
    if hooked {
        info!("Intercepting instance creation");
        let mut list = exts.clone();
        add_name(&mut list, khr::get_physical_device_properties2::NAME);
        add_name(&mut list, khr::external_memory_capabilities::NAME);
        add_name(&mut list, khr::external_semaphore_capabilities::NAME);
        let mut mci = *ci;
        mci.pp_enabled_extension_names = list.as_ptr();
        mci.enabled_extension_count = list.len() as u32;
        r = real(&mci, alloc, out);
        if r != vk::Result::SUCCESS {
            // the game's own request must still work; only generation is lost
            warn!("Intercepted vkCreateInstance failed ({r:?}); passing through");
            hooked = false;
        }
    }
    if !hooked {
        r = real(info, alloc, out);
        if r != vk::Result::SUCCESS {
            return r;
        }
    }
    let instance = *out;
    match register_instance(drv, instance, hooked) {
        Ok(()) => r,
        Err(e) => {
            if let Some(destroy) =
                ifetch::<vk::PFN_vkDestroyInstance>(drv.gipa, instance, c"vkDestroyInstance")
            {
                destroy(instance, alloc);
            }
            fail(&e)
        }
    }
}

unsafe fn register_instance(
    drv: &Driver,
    instance: vk::Instance,
    hooked: bool,
) -> Result<(), String> {
    let gipa = drv.gipa;
    let destroy =
        ifetch(gipa, instance, c"vkDestroyInstance").ok_or("Could not resolve vkDestroyInstance")?;
    let enumerate: vk::PFN_vkEnumeratePhysicalDevices =
        ifetch(gipa, instance, c"vkEnumeratePhysicalDevices")
            .ok_or("Physical device enumeration failed")?;
    let mut n = 0u32;
    if enumerate(instance, &mut n, std::ptr::null_mut()) != vk::Result::SUCCESS {
        return Err("Physical device enumeration failed".into());
    }
    let mut pds = vec![vk::PhysicalDevice::null(); n as usize];
    let r = enumerate(instance, &mut n, pds.as_mut_ptr());
    if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
        return Err("Physical device enumeration failed".into());
    }
    pds.truncate(n as usize);
    let hook = hooked.then(|| {
        let inst = ash::Instance::load(drv.entry.static_fn(), instance);
        InstanceHook {
            props2: khr::get_physical_device_properties2::Instance::new(&drv.entry, &inst),
            surface: khr::surface::Instance::new(&drv.entry, &inst),
            instance: inst,
        }
    });
    let entry = InstanceEntry {
        handle: instance,
        gipa,
        destroy,
        surface_support: ifetch(gipa, instance, c"vkGetPhysicalDeviceSurfaceSupportKHR"),
        hook,
    };
    let mut m = MAPS.write().unwrap();
    for pd in pds {
        m.physical.insert(pd, instance);
    }
    m.instances.insert(instance, Arc::new(entry));
    Ok(())
}

unsafe extern "system" fn destroy_instance(
    instance: vk::Instance,
    alloc: *const vk::AllocationCallbacks,
) {
    let _api = API.lock();
    let entry = {
        let mut m = MAPS.write().unwrap();
        let Some(e) = m.instances.remove(&instance) else {
            return;
        };
        m.physical.retain(|_, i| *i != instance);
        e
    };
    (entry.destroy)(instance, alloc);
}

// ---- device ----

unsafe extern "system" fn create_device(
    pd: vk::PhysicalDevice,
    info: *const vk::DeviceCreateInfo,
    alloc: *const vk::AllocationCallbacks,
    out: *mut vk::Device,
) -> vk::Result {
    let _api = API.lock();
    let Some(drv) = driver() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let inst = instance_of(pd);
    let ihandle = inst.as_ref().map_or(vk::Instance::null(), |i| i.handle);
    let Some(real) = ifetch::<vk::PFN_vkCreateDevice>(drv.gipa, ihandle, c"vkCreateDevice") else {
        error!("Could not resolve vkCreateDevice");
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let ci = &*info;
    let exts = names(ci.pp_enabled_extension_names, ci.enabled_extension_count);
    let Some(inst) = inst.filter(|i| i.hook.is_some() && has_name(&exts, khr::swapchain::NAME))
    else {
        return real(pd, info, alloc, out);
    };
    info!("Intercepting device creation");
    let (device, hook) = match hooked_device(&inst, pd, ci, exts, alloc, real) {
        Ok((d, h)) => (d, Some(h)),
        Err(e) => {
            warn!("Frame generation unavailable on this device; using passthrough: {e}");
            let mut d = vk::Device::null();
            let r = real(pd, info, alloc, &mut d);
            if r != vk::Result::SUCCESS {
                return r;
            }
            (d, None)
        }
    };
    *out = device;
    match register_device(drv, &inst, pd, device, ci, hook) {
        Ok(()) => vk::Result::SUCCESS,
        Err(e) => {
            error!("An error occurred while initializing the lsfg-metal device:");
            error!("- {e}");
            if let Some(destroy) =
                dfetch::<vk::PFN_vkDestroyDevice>(drv.gdpa, device, c"vkDestroyDevice")
            {
                destroy(device, alloc);
            }
            vk::Result::ERROR_INITIALIZATION_FAILED
        }
    }
}

unsafe fn hooked_device(
    inst: &Arc<InstanceEntry>,
    pd: vk::PhysicalDevice,
    ci: &vk::DeviceCreateInfo,
    mut exts: Vec<*const c_char>,
    alloc: *const vk::AllocationCallbacks,
    real: vk::PFN_vkCreateDevice,
) -> Result<(vk::Device, DeviceHook), String> {
    let ih = inst.hook.as_ref().expect("hooked instance");
    let offered = vkutil::check(
        ih.instance.enumerate_device_extension_properties(pd),
        "vkEnumerateDeviceExtensionProperties",
    )?;
    let has = |n: &CStr| offered.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
    add_name(&mut exts, khr::timeline_semaphore::NAME);
    add_name(&mut exts, khr::synchronization2::NAME);
    let proxy = has(ext::metal_objects::NAME);
    if proxy {
        add_name(&mut exts, ext::metal_objects::NAME);
    }
    let f16_ext = has(khr::shader_float16_int8::NAME);
    if f16_ext {
        add_name(&mut exts, khr::shader_float16_int8::NAME);
    }
    let fams = ih.instance.get_physical_device_queue_family_properties(pd);
    let qcis = if ci.queue_create_info_count == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(ci.p_queue_create_infos, ci.queue_create_info_count as usize)
    };
    let both = vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE;
    let family = qcis
        .iter()
        .find(|q| {
            q.queue_count > 0
                && q.flags.is_empty()
                && fams
                    .get(q.queue_family_index as usize)
                    .is_some_and(|f| f.queue_flags.contains(both))
        })
        .map(|q| q.queue_family_index)
        .ok_or("No requested graphics/compute queue; frame generation is unavailable")?;
    let mut tl = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
    let mut f16 = vk::PhysicalDeviceShaderFloat16Int8Features::default();
    let mut s2 = vk::PhysicalDeviceSynchronization2Features::default();
    let mut f2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut tl)
        .push_next(&mut f16)
        .push_next(&mut s2);
    ih.props2.get_physical_device_features2(pd, &mut f2);
    if tl.timeline_semaphore != vk::TRUE {
        return Err("Device has no timeline semaphore support".into());
    }
    if s2.synchronization2 != vk::TRUE {
        return Err("Device has no synchronization2 support".into());
    }
    let fp16 = f16_ext && f16.shader_float16 == vk::TRUE;
    let chain = chain::copy(ci.p_next, fp16);
    let mut mci = *ci;
    mci.p_next = chain.head;
    mci.pp_enabled_extension_names = exts.as_ptr();
    mci.enabled_extension_count = exts.len() as u32;
    let mut device = vk::Device::null();
    let r = real(pd, &mci, alloc, &mut device);
    if r != vk::Result::SUCCESS {
        return Err(format!("vkCreateDevice failed: {r:?}"));
    }
    let dev = ash::Device::load(ih.instance.fp_v1_0(), device);
    let queue = dev.get_device_queue(family, 0);
    let pool = match vkutil::create_command_pool(&dev, family, true) {
        Ok(p) => p,
        Err(e) => {
            (dev.fp_v1_0().destroy_device)(device, alloc);
            return Err(e);
        }
    };
    let hook = DeviceHook {
        inst: inst.clone(),
        swapchain: khr::swapchain::Device::new(&ih.instance, &dev),
        device: dev,
        physical: pd,
        family,
        queue,
        pool,
        queue_mutex: sync::Recursive::new(),
        fp16,
        proxy_supported: AtomicBool::new(proxy),
        generator: Mutex::new(None),
    };
    Ok((device, hook))
}

unsafe fn register_device(
    drv: &Driver,
    inst: &Arc<InstanceEntry>,
    pd: vk::PhysicalDevice,
    device: vk::Device,
    ci: &vk::DeviceCreateInfo,
    hook: Option<DeviceHook>,
) -> Result<(), String> {
    let gdpa = ifetch(inst.gipa, inst.handle, c"vkGetDeviceProcAddr").unwrap_or(drv.gdpa);
    let destroy =
        dfetch(gdpa, device, c"vkDestroyDevice").ok_or("Could not resolve vkDestroyDevice")?;
    let get_queue: vk::PFN_vkGetDeviceQueue =
        dfetch(gdpa, device, c"vkGetDeviceQueue").ok_or("Could not resolve vkGetDeviceQueue")?;
    let get_queue2: Option<vk::PFN_vkGetDeviceQueue2> = dfetch(gdpa, device, c"vkGetDeviceQueue2");
    let qcis = if ci.queue_create_info_count == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(ci.p_queue_create_infos, ci.queue_create_info_count as usize)
    };
    let mut queues = Vec::new();
    for q in qcis {
        for j in 0..q.queue_count {
            let mut h = vk::Queue::null();
            match get_queue2 {
                Some(g2) if !q.flags.is_empty() => {
                    let qi = vk::DeviceQueueInfo2::default()
                        .flags(q.flags)
                        .queue_family_index(q.queue_family_index)
                        .queue_index(j);
                    g2(device, &qi, &mut h);
                }
                _ => get_queue(device, q.queue_family_index, j, &mut h),
            }
            queues.push(h);
        }
    }
    let entry = Arc::new(DeviceEntry {
        handle: device,
        physical: pd,
        gdpa,
        destroy,
        hook,
    });
    let mut m = MAPS.write().unwrap();
    m.devices.insert(device, entry);
    for q in queues {
        m.queues.insert(q, device);
    }
    Ok(())
}

unsafe extern "system" fn destroy_device(
    device: vk::Device,
    alloc: *const vk::AllocationCallbacks,
) {
    let _api = API.lock();
    let (entry, swapchains) = {
        let mut m = MAPS.write().unwrap();
        let Some(e) = m.devices.remove(&device) else {
            return;
        };
        m.queues.retain(|_, d| *d != device);
        let mut scs = Vec::new();
        m.swapchains.retain(|&sc, s| {
            let ours = s.device.handle == device;
            if ours {
                scs.push((sc, s.clone()));
            }
            !ours
        });
        (e, scs)
    };
    // leaked swapchains: wrappers drop outside the map lock, their driver swapchains go before the device
    for (sc, s) in swapchains {
        let (destroy, proxy) = (s.destroy, s.proxy);
        drop(s);
        if !proxy {
            destroy(device, sc, std::ptr::null());
        }
    }
    // shader modules and pools live on this device; drop them before the real destroy
    if let Some(h) = &entry.hook {
        h.generator.lock().unwrap().take();
        h.device.destroy_command_pool(h.pool, None);
    }
    let destroy = entry.destroy;
    drop(entry);
    destroy(device, alloc);
}

// ---- surface support ----

unsafe extern "system" fn surface_support(
    pd: vk::PhysicalDevice,
    family: u32,
    surface: vk::SurfaceKHR,
    out: *mut vk::Bool32,
) -> vk::Result {
    let Some(inst) = instance_of(pd) else {
        return vk::Result::ERROR_UNKNOWN;
    };
    let Some(real) = inst.surface_support else {
        return vk::Result::ERROR_UNKNOWN;
    };
    let r = real(pd, family, surface, out);
    if r == vk::Result::SUCCESS && *out == vk::TRUE {
        if let Some(h) = &inst.hook {
            let fams = h.instance.get_physical_device_queue_family_properties(pd);
            *out = fams
                .get(family as usize)
                .is_some_and(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                as vk::Bool32;
        }
    }
    r
}

// ---- swapchain ----

// support filter; the surface capabilities are returned for the wrapper's image-count clamp
unsafe fn supported(
    h: &DeviceHook,
    ci: &vk::SwapchainCreateInfoKHR,
) -> Option<vk::SurfaceCapabilitiesKHR> {
    use vk::Format as F;
    let sdr = ci.image_color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        && matches!(
            ci.image_format,
            F::B8G8R8A8_UNORM | F::R8G8B8A8_UNORM | F::B8G8R8A8_SRGB | F::R8G8B8A8_SRGB
        );
    let hdr = ci.image_format == F::R16G16B16A16_SFLOAT
        && ci.image_color_space == vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT;
    if !((sdr || hdr)
        && ci.image_array_layers == 1
        && !ci.flags.contains(vk::SwapchainCreateFlagsKHR::PROTECTED))
    {
        warn!(
            "Frame generation disabled for this swapchain (format, colour space, array layers or protected flag); preserving native presentation"
        );
        return None;
    }
    let caps = h
        .inst
        .hook
        .as_ref()?
        .surface
        .get_physical_device_surface_capabilities(h.physical, ci.surface)
        .ok()?;
    if !caps
        .supported_usage_flags
        .contains(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
    {
        warn!("Frame generation disabled: surface does not support image transfers");
        return None;
    }
    Some(caps)
}

// the proxy wrapper when it applies, else the fixed one
unsafe fn choose_wrapper(
    dev: &Arc<DeviceEntry>,
    ci: &vk::SwapchainCreateInfoKHR,
    caps: &vk::SurfaceCapabilitiesKHR,
    real: vk::PFN_vkCreateSwapchainKHR,
    alloc: *const vk::AllocationCallbacks,
) -> Result<(Created, bool), Error> {
    let h = dev.hook.as_ref().expect("hooked device");
    let profile = layer().expect("active layer").profile();
    // multiplier 1 generates nothing, so the fixed path's plain forwarding is the right shape for it
    if h.proxy_supported.load(Ordering::Relaxed)
        && profile.multiplier > 1
        && profile.pacing_mode == settings::PacingMode::Adaptive
        && settings::os_env("LSFGM_VULKAN_PROXY").as_deref() != Some("0")
    {
        match proxy::create(dev, ci) {
            Ok(Some(created)) => return Ok((created, true)),
            Ok(None) => {}
            Err(e) => {
                h.proxy_supported.store(false, Ordering::Relaxed);
                warn!("Vulkan proxy swapchain failed, using the fixed present path: {e}");
            }
        }
    }
    let f = fixed::Fixed::create(dev.clone(), ci, caps, real, alloc)?;
    let sc = f.swapchain;
    Ok(((sc, Box::new(f)), false))
}

unsafe extern "system" fn create_swapchain(
    device: vk::Device,
    info: *const vk::SwapchainCreateInfoKHR,
    alloc: *const vk::AllocationCallbacks,
    out: *mut vk::SwapchainKHR,
) -> vk::Result {
    let _api = API.lock();
    let fail = |what: &str| {
        error!("An error occurred while initializing the lsfg-metal swapchain:");
        error!("- {what}");
        vk::Result::ERROR_INITIALIZATION_FAILED
    };
    let Some(dev) = device_of(device) else {
        return fail("Unknown device handle");
    };
    let Some(real) =
        dfetch::<vk::PFN_vkCreateSwapchainKHR>(dev.gdpa, device, c"vkCreateSwapchainKHR")
    else {
        return fail("Could not resolve vkCreateSwapchainKHR");
    };
    // a proxy handle is ours, the driver must never see it on either branch below
    let mut ci = *info;
    if proxy::is_proxy(ci.old_swapchain) {
        ci.old_swapchain = vk::SwapchainKHR::null();
    }
    let ci = &ci;
    let (sc, wrapper, is_proxy) = match dev.hook.as_ref().and_then(|h| supported(h, ci)) {
        Some(caps) => match choose_wrapper(&dev, ci, &caps, real, alloc) {
            Ok(((sc, w), p)) => (sc, Some(w), p),
            Err(e) => {
                let _ = fail(&e.to_string());
                return match e {
                    Error::Vk(r) => r,
                    Error::Msg(_) => vk::Result::ERROR_INITIALIZATION_FAILED,
                };
            }
        },
        None => {
            let mut sc = vk::SwapchainKHR::null();
            let r = real(device, ci, alloc, &mut sc);
            if r != vk::Result::SUCCESS {
                return r;
            }
            (sc, None, false)
        }
    };
    let Some(destroy) =
        dfetch::<vk::PFN_vkDestroySwapchainKHR>(dev.gdpa, device, c"vkDestroySwapchainKHR")
    else {
        return fail("Could not resolve vkDestroySwapchainKHR");
    };
    let Some(present) = dfetch::<vk::PFN_vkQueuePresentKHR>(dev.gdpa, device, c"vkQueuePresentKHR")
    else {
        drop(wrapper);
        if !is_proxy {
            destroy(device, sc, alloc);
        }
        return fail("Could not resolve vkQueuePresentKHR");
    };
    *out = sc;
    MAPS.write().unwrap().swapchains.insert(
        sc,
        Arc::new(SwapchainEntry {
            device: dev,
            destroy,
            present,
            wrapper: wrapper.map(RwLock::new),
            proxy: is_proxy,
        }),
    );
    vk::Result::SUCCESS
}

unsafe extern "system" fn destroy_swapchain(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    alloc: *const vk::AllocationCallbacks,
) {
    let _api = API.lock();
    // the wrapper drops outside the map lock (a proxy waits for its worker there)
    let entry = MAPS.write().unwrap().swapchains.remove(&swapchain);
    match entry {
        Some(e) => {
            let (destroy, proxy) = (e.destroy, e.proxy);
            drop(e);
            if !proxy {
                destroy(device, swapchain, alloc);
            }
        }
        None => {
            if let Some(destroy) =
                real_device_fn::<vk::PFN_vkDestroySwapchainKHR>(device, c"vkDestroySwapchainKHR")
            {
                destroy(device, swapchain, alloc);
            }
        }
    }
}

// ---- present ----

unsafe extern "system" fn queue_present(
    queue: vk::Queue,
    info: *const vk::PresentInfoKHR,
) -> vk::Result {
    let pi = &*info;
    if pi.swapchain_count == 0 || pi.p_swapchains.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let Some(entry) = swapchain_of(*pi.p_swapchains) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let _api = API.lock();
    if pi.swapchain_count != 1 && proxy::invalidate_proxies(queue, pi) {
        return vk::Result::ERROR_OUT_OF_DATE_KHR;
    }
    let Some(w) = &entry.wrapper else {
        return (entry.present)(queue, info);
    };
    let r = w.write().unwrap().present(queue, pi, entry.present);
    match r {
        Ok(r) => r,
        Err(Error::Vk(vk::Result::ERROR_OUT_OF_DATE_KHR)) => vk::Result::ERROR_OUT_OF_DATE_KHR,
        Err(Error::Vk(r)) => {
            warn!("A Vulkan error occurred while calling vkQueuePresentKHR");
            warn!("- {r:?}");
            if r.as_raw() < 0 {
                if !pi.p_results.is_null() {
                    *pi.p_results = r;
                }
                r
            } else {
                vk::Result::ERROR_OUT_OF_DATE_KHR
            }
        }
        Err(Error::Msg(e)) => {
            warn!("An error occurred while calling vkQueuePresentKHR");
            warn!("- {e}");
            if !pi.p_results.is_null() {
                *pi.p_results = vk::Result::ERROR_OUT_OF_DATE_KHR;
            }
            vk::Result::ERROR_OUT_OF_DATE_KHR
        }
    }
}

// ---- queue hooks ----

unsafe fn queue_call<T>(
    queue: vk::Queue,
    name: &CStr,
    call: impl FnOnce(T) -> vk::Result,
) -> vk::Result {
    let Some(dev) = queue_device(queue) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some(real) = dfetch::<T>(dev.gdpa, dev.handle, name) else {
        return vk::Result::ERROR_EXTENSION_NOT_PRESENT;
    };
    match &dev.hook {
        Some(h) if h.queue == queue => {
            let _q = h.queue_mutex.lock();
            call(real)
        }
        _ => call(real),
    }
}

unsafe extern "system" fn queue_submit(
    queue: vk::Queue,
    count: u32,
    submits: *const vk::SubmitInfo,
    fence: vk::Fence,
) -> vk::Result {
    queue_call(queue, c"vkQueueSubmit", |f: vk::PFN_vkQueueSubmit| {
        f(queue, count, submits, fence)
    })
}

// vkQueueSubmit2 and vkQueueSubmit2KHR share one signature; each fetches the real one under its own name
unsafe extern "system" fn queue_submit2(
    queue: vk::Queue,
    count: u32,
    submits: *const vk::SubmitInfo2,
    fence: vk::Fence,
) -> vk::Result {
    queue_call(queue, c"vkQueueSubmit2", |f: vk::PFN_vkQueueSubmit2| {
        f(queue, count, submits, fence)
    })
}

unsafe extern "system" fn queue_submit2_khr(
    queue: vk::Queue,
    count: u32,
    submits: *const vk::SubmitInfo2,
    fence: vk::Fence,
) -> vk::Result {
    queue_call(queue, c"vkQueueSubmit2KHR", |f: vk::PFN_vkQueueSubmit2| {
        f(queue, count, submits, fence)
    })
}

unsafe extern "system" fn queue_bind_sparse(
    queue: vk::Queue,
    count: u32,
    info: *const vk::BindSparseInfo,
    fence: vk::Fence,
) -> vk::Result {
    queue_call(
        queue,
        c"vkQueueBindSparse",
        |f: vk::PFN_vkQueueBindSparse| f(queue, count, info, fence),
    )
}

unsafe extern "system" fn queue_wait_idle(queue: vk::Queue) -> vk::Result {
    queue_call(queue, c"vkQueueWaitIdle", |f: vk::PFN_vkQueueWaitIdle| {
        f(queue)
    })
}

// ---- swapchain image and acquire hooks ----

fn proxy_result(r: Result<vk::Result, Error>) -> vk::Result {
    match r {
        Ok(r) | Err(Error::Vk(r)) => r,
        Err(Error::Msg(_)) => vk::Result::ERROR_INITIALIZATION_FAILED,
    }
}

unsafe extern "system" fn get_swapchain_images(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    count: *mut u32,
    images: *mut vk::Image,
) -> vk::Result {
    if let Some(e) = swapchain_of(swapchain).filter(|e| e.proxy) {
        let r = e
            .wrapper
            .as_ref()
            .expect("proxy")
            .read()
            .unwrap()
            .get_images(count, images);
        return proxy_result(r);
    }
    match real_device_fn::<vk::PFN_vkGetSwapchainImagesKHR>(device, c"vkGetSwapchainImagesKHR") {
        Some(f) => f(device, swapchain, count, images),
        None => vk::Result::ERROR_EXTENSION_NOT_PRESENT,
    }
}

unsafe extern "system" fn acquire_next_image(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    timeout: u64,
    semaphore: vk::Semaphore,
    fence: vk::Fence,
    index: *mut u32,
) -> vk::Result {
    if let Some(e) = swapchain_of(swapchain).filter(|e| e.proxy) {
        let r = e
            .wrapper
            .as_ref()
            .expect("proxy")
            .read()
            .unwrap()
            .acquire(timeout, semaphore, fence, index);
        return proxy_result(r);
    }
    match real_device_fn::<vk::PFN_vkAcquireNextImageKHR>(device, c"vkAcquireNextImageKHR") {
        Some(f) => f(device, swapchain, timeout, semaphore, fence, index),
        None => vk::Result::ERROR_EXTENSION_NOT_PRESENT,
    }
}

unsafe extern "system" fn acquire_next_image2(
    device: vk::Device,
    info: *const vk::AcquireNextImageInfoKHR,
    index: *mut u32,
) -> vk::Result {
    let ai = &*info;
    if let Some(e) = swapchain_of(ai.swapchain).filter(|e| e.proxy) {
        if !ai.p_next.is_null() || ai.device_mask != 1 {
            return vk::Result::ERROR_OUT_OF_DATE_KHR;
        }
        let r = e.wrapper.as_ref().expect("proxy").read().unwrap().acquire(
            ai.timeout,
            ai.semaphore,
            ai.fence,
            index,
        );
        return proxy_result(r);
    }
    match real_device_fn::<vk::PFN_vkAcquireNextImage2KHR>(device, c"vkAcquireNextImage2KHR") {
        Some(f) => f(device, info, index),
        None => vk::Result::ERROR_EXTENSION_NOT_PRESENT,
    }
}

// ---- proxy guards: unhooked swapchain entry points must never hand a proxy handle to the driver ----

unsafe fn device_call<T>(
    device: vk::Device,
    name: &CStr,
    call: impl FnOnce(T) -> vk::Result,
) -> vk::Result {
    match real_device_fn::<T>(device, name) {
        Some(f) => call(f),
        None => vk::Result::ERROR_EXTENSION_NOT_PRESENT,
    }
}

// the array may mix proxies and driver swapchains; only the driver ones are forwarded
unsafe extern "system" fn set_hdr_metadata(
    device: vk::Device,
    count: u32,
    swapchains: *const vk::SwapchainKHR,
    metadata: *const vk::HdrMetadataEXT,
) {
    if count == 0 {
        return;
    }
    let n = count as usize;
    let (scs, md): (Vec<_>, Vec<_>) = std::slice::from_raw_parts(swapchains, n)
        .iter()
        .zip(std::slice::from_raw_parts(metadata, n))
        .filter(|(s, _)| !proxy::is_proxy(**s))
        .map(|(s, m)| (*s, *m))
        .unzip();
    if scs.is_empty() {
        return;
    }
    if let Some(f) = real_device_fn::<vk::PFN_vkSetHdrMetadataEXT>(device, c"vkSetHdrMetadataEXT")
    {
        f(device, scs.len() as u32, scs.as_ptr(), md.as_ptr());
    }
}

// the proxy has no present ids to wait for; the worker owns presentation
unsafe extern "system" fn wait_for_present(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    present_id: u64,
    timeout: u64,
) -> vk::Result {
    if proxy::is_proxy(swapchain) {
        return vk::Result::SUCCESS;
    }
    device_call(device, c"vkWaitForPresentKHR", |f: vk::PFN_vkWaitForPresentKHR| {
        f(device, swapchain, present_id, timeout)
    })
}

unsafe extern "system" fn get_swapchain_status(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
) -> vk::Result {
    if proxy::is_proxy(swapchain) {
        return vk::Result::SUCCESS;
    }
    device_call(
        device,
        c"vkGetSwapchainStatusKHR",
        |f: vk::PFN_vkGetSwapchainStatusKHR| f(device, swapchain),
    )
}

unsafe extern "system" fn get_past_presentation_timing(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    count: *mut u32,
    timings: *mut vk::PastPresentationTimingGOOGLE,
) -> vk::Result {
    if proxy::is_proxy(swapchain) {
        *count = 0;
        return vk::Result::SUCCESS;
    }
    device_call(
        device,
        c"vkGetPastPresentationTimingGOOGLE",
        |f: vk::PFN_vkGetPastPresentationTimingGOOGLE| f(device, swapchain, count, timings),
    )
}

// the proxy reports a nominal 60 hz cycle; a zero would divide games by zero
unsafe extern "system" fn get_refresh_cycle_duration(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    props: *mut vk::RefreshCycleDurationGOOGLE,
) -> vk::Result {
    if proxy::is_proxy(swapchain) {
        (*props).refresh_duration = 16_666_667;
        return vk::Result::SUCCESS;
    }
    device_call(
        device,
        c"vkGetRefreshCycleDurationGOOGLE",
        |f: vk::PFN_vkGetRefreshCycleDurationGOOGLE| f(device, swapchain, props),
    )
}

unsafe extern "system" fn release_swapchain_images(
    device: vk::Device,
    info: *const vk::ReleaseSwapchainImagesInfoEXT,
) -> vk::Result {
    if proxy::is_proxy((*info).swapchain) {
        return vk::Result::SUCCESS;
    }
    device_call(
        device,
        c"vkReleaseSwapchainImagesEXT",
        |f: vk::PFN_vkReleaseSwapchainImagesEXT| f(device, info),
    )
}

// the (surface -> layer) registry shrinks with the surface
unsafe extern "system" fn destroy_surface(
    instance: vk::Instance,
    surface: vk::SurfaceKHR,
    alloc: *const vk::AllocationCallbacks,
) {
    crate::metal::forget_surface(surface);
    let gipa = MAPS
        .read()
        .unwrap()
        .instances
        .get(&instance)
        .map(|e| e.gipa)
        .or_else(|| driver().map(|d| d.gipa));
    if let Some(f) =
        gipa.and_then(|g| ifetch::<vk::PFN_vkDestroySurfaceKHR>(g, instance, c"vkDestroySurfaceKHR"))
    {
        f(instance, surface, alloc);
    }
}

// ---- static constructor ----

extern "C" fn constructor() {
    panic_to_log();
    // the vulkan path initialises lazily inside the exports; the metal front end is armed at load
    let on = |k| settings::os_env(k).is_some_and(|v| !v.is_empty() && v != "0");
    if on("LSFGM_METAL") {
        let _ = layer();
        crate::metal::install();
    } else if on("LSFGM_OPENGL") {
        let _ = layer();
        crate::metal::install_opengl();
    }
}

#[used]
#[link_section = "__DATA,__mod_init_func"]
static CONSTRUCTOR: extern "C" fn() = constructor;
