// per-layer generator: pool, worker thread, backend device, context wrapper, dumps and stats
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, SendError, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use ash::{ext, khr, vk};
use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2::Message;
use objc2_core_graphics::CGColorSpace;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLEvent, MTLGPUFamily, MTLOrigin, MTLPixelFormat,
    MTLResourceOptions, MTLSharedEvent, MTLSharedEventListener, MTLSize, MTLStorageMode,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use super::drawable::ProxyDrawable;
use super::latency;
use super::metalfx;
use super::{enabled, hooks, set_enabled, setup, vk_format, Setup};
use crate::generator::signature::{Colour, Signature};
use crate::generator::{native, Context, Instance};
use crate::log;
use crate::pacer::{display_refresh, Estimator, Pacer, Sample};
use crate::settings::{PacingMode, ScalerMode};
use crate::shaders;
use crate::vkutil::{self, check};

type Texture = Retained<ProtocolObject<dyn MTLTexture>>;
type Drawable = Retained<ProtocolObject<dyn CAMetalDrawable>>;

// bound on every cpu wait for the gpu; a hung frame falls back to native presents instead of a hang
pub(super) const GPU_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Job {
    pub latency: latency::Frame,
    pub cb: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    pub drawable: Retained<ProxyDrawable>,
    pub duration: f64,
    pub serial: u64,
    pub sample: Sample,
}

enum Msg {
    Job(Job),
    Forget(Vec<Texture>),
}
unsafe impl Send for Msg {}

struct Pool {
    textures: Vec<Texture>,
    outstanding: u32,
    next_id: usize,
}

// (game size, shown size) of an upscaled layer
pub type Upscale = ((u32, u32), (u32, u32));
// game size, shown size, format and colour kind of a scaler
type ScaleKey = ((u32, u32), (u32, u32), MTLPixelFormat, u32);
// a pixel format and colour space with the colour kind they classify as
type ColourCache = Option<(MTLPixelFormat, Option<Retained<CGColorSpace>>, Option<u32>)>;

pub struct Generator {
    latency: latency::Probe,
    pub layer: Retained<CAMetalLayer>,
    pub device: Retained<ProtocolObject<dyn MTLDevice>>,
    present_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    game_event: Retained<ProtocolObject<dyn MTLSharedEvent>>,
    present_event: Retained<ProtocolObject<dyn MTLSharedEvent>>,
    listener: Retained<MTLSharedEventListener>,
    game_serial: AtomicU64,
    blocked: Mutex<f64>,
    estimator: Mutex<Estimator>,
    tx: Sender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    started: Once,
    pool: Mutex<Pool>,
    pool_cv: Condvar,
    upscale: Mutex<Option<Upscale>>,
    // held across a state change and the real resize, never while only reading, so the size getter cannot wait on core animation
    sizing: Mutex<()>,
    // a size the game set itself; none follows the layer's natural size
    explicit: Mutex<Option<(u32, u32)>>,
    // whether a scaler builds for this key
    tried: Mutex<Option<(ScaleKey, bool)>>,
    // the colour kind last worked out, with the format and colour space it was for; classifying costs core graphics calls
    colour: Mutex<ColourCache>,
}
unsafe impl Send for Generator {}
unsafe impl Sync for Generator {}

static GENERATORS: Mutex<Option<HashMap<usize, &'static Generator>>> = Mutex::new(None);

impl Generator {
    // one per layer, created on demand, never freed
    pub fn get(layer: &CAMetalLayer) -> Option<&'static Generator> {
        let mut map = GENERATORS.lock().unwrap();
        let map = map.get_or_insert_with(HashMap::new);
        if let Some(g) = map.get(&(layer as *const _ as usize)) {
            return Some(g);
        }
        let device = layer.device().or_else(|| MTLCreateSystemDefaultDevice())?;
        // a size the game set before this generator existed went past the size hook
        let real = hooks::real_drawable_size(layer);
        let explicit = (real.0 > 0 && real.1 > 0 && real != natural_size(layer)).then_some(real);
        let (tx, rx) = channel();
        let g = Box::leak(Box::new(Generator {
            latency: latency::Probe::default(),
            layer: layer.retain(),
            present_queue: device.newCommandQueue()?,
            game_event: device.newSharedEvent()?,
            present_event: device.newSharedEvent()?,
            listener: MTLSharedEventListener::new(),
            device,
            game_serial: AtomicU64::new(0),
            blocked: Mutex::new(0.0),
            estimator: Mutex::new(Estimator::default()),
            tx,
            rx: Mutex::new(Some(rx)),
            started: Once::new(),
            pool: Mutex::new(Pool {
                textures: Vec::new(),
                outstanding: 0,
                next_id: 1,
            }),
            pool_cv: Condvar::new(),
            upscale: Mutex::new(None),
            sizing: Mutex::new(()),
            explicit: Mutex::new(explicit),
            tried: Mutex::new(None),
            colour: Mutex::new(None),
        }));
        map.insert(layer as *const _ as usize, g);
        Some(g)
    }

    // the size the game draws at: its own while upscaling, else the layer's
    pub fn game_size(&self) -> (f64, f64) {
        match self.upscaling() {
            Some(((w, h), _)) => (w as f64, h as f64),
            None => {
                let s = self.layer.drawableSize();
                (s.width, s.height)
            }
        }
    }

    pub fn upscaling(&self) -> Option<Upscale> {
        *self.upscale.lock().unwrap()
    }

    // upscale from the game's size, or not, with the real drawables resized in the same step
    pub fn set_upscaling(&self, up: Option<Upscale>, game: (u32, u32)) {
        let _sizing = self.sizing.lock().unwrap();
        if up.is_some() {
            hooks::mark_upscaled();
        }
        *self.upscale.lock().unwrap() = up;
        hooks::set_real_drawable_size(&self.layer, up.map_or(game, |u| u.1));
    }

    // forget the state without touching the size, for a layer a vulkan driver takes over
    pub fn clear_upscaling(&self) {
        *self.upscale.lock().unwrap() = None;
    }

    // back to the game's size when the scale that failed is still the current one; it is not tried again
    fn stop_upscaling(&self, key: Upscale, why: &str) {
        let _sizing = self.sizing.lock().unwrap();
        {
            let mut state = self.upscale.lock().unwrap();
            if *state != Some(key) {
                return;
            }
            *state = None;
        }
        if let Some(t) = self.tried.lock().unwrap().as_mut().filter(|t| (t.0 .0, t.0 .1) == key) {
            t.1 = false;
        }
        hooks::set_real_drawable_size(&self.layer, key.0);
        log::warn(&format!("MetalFX upscaling off for this layer ({why}); frames show at the game's size"));
    }

    // the generator of a layer, without creating one
    pub fn existing(layer: &CAMetalLayer) -> Option<&'static Generator> {
        GENERATORS.lock().unwrap().as_ref()?.get(&(layer as *const _ as usize)).copied()
    }

    // the shown size when metalfx should upscale this layer from `game`; the scaler is tried once per key
    pub fn upscale_to(&self, game: (u32, u32), format: MTLPixelFormat, kind: u32) -> Option<(u32, u32)> {
        if !setup().is_some_and(|s| s.profile.scaler == ScalerMode::MetalFx) {
            return None;
        }
        let shown = metalfx::shown_size(&self.layer);
        if shown.0 <= game.0 || shown.1 <= game.1 {
            // a game already at the window's full size has nothing to upscale; say so once, as the opengl path does
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                log::info(&format!(
                    "MetalFX upscaling not used: the game presents at {}x{}, not smaller than the window's {}x{} pixels",
                    game.0, game.1, shown.0, shown.1
                ))
            });
            return None;
        }
        let key = (game, shown, format, kind);
        let mut tried = self.tried.lock().unwrap();
        if tried.is_none_or(|t| t.0 != key) {
            // on the game's thread, which may have no pool of its own
            let ok = autoreleasepool(|_| {
                metalfx::Scaler::new(&self.device, game, shown, unorm(format), kind).is_some()
            });
            if !ok {
                log::info(&format!(
                    "MetalFX upscaling not used: MetalFX cannot scale {}x{} to {}x{} here (it needs macOS 13 and a GPU it supports)",
                    game.0, game.1, shown.0, shown.1
                ));
            }
            *tried = Some((key, ok));
        }
        tried.is_some_and(|t| t.1).then_some(shown)
    }

    // the layer's colour kind, worked out again only when its pixel format or colour space changes
    pub fn colour_kind(&self) -> Option<u32> {
        let format = self.layer.pixelFormat();
        let space = self.layer.colorspace();
        let mut cached = self.colour.lock().unwrap();
        if let Some((f, s, kind)) = &*cached {
            // the cache holds the space it saw, so a new one can never reuse its address
            if *f == format && s.as_deref().map(|s| s as *const _) == space.as_deref().map(|s| s as *const _) {
                return *kind;
            }
        }
        let kind = hooks::colour(format, space.as_deref()).map(|c| c.kind);
        *cached = Some((format, space, kind));
        kind
    }

    // metal front end, every nextDrawable: follow the game's size and the window, start or stop upscaling
    pub fn refresh_upscale(&self, kind: Option<u32>) {
        let natural = natural_size(&self.layer);
        let current = self.upscaling();
        // the size the game set, else the natural size a layer follows until it is sized
        let explicit = *self.explicit.lock().unwrap();
        let game = explicit.unwrap_or(natural);
        if game.0 == 0 || game.1 == 0 {
            return;
        }
        let up = kind.and_then(|k| self.upscale_to(game, self.layer.pixelFormat(), k));
        match (up, current) {
            (Some(shown), Some(c)) if c == (game, shown) => {}
            (Some(shown), _) => {
                self.set_upscaling(Some((game, shown)), game);
                log::info(&format!("MetalFX upscaling {}x{} to {}x{}", game.0, game.1, shown.0, shown.1));
            }
            (None, Some(_)) => self.set_upscaling(None, game),
            // after upscaling stopped the size was set for the game; one that never set its own follows the window again
            (None, None) => {
                if explicit.is_none() && hooks::real_drawable_size(&self.layer) != natural {
                    hooks::set_real_drawable_size(&self.layer, natural);
                }
            }
        }
    }

    // the game sets a size: remembered as its own; on an upscaled layer the real drawables keep the shown size
    pub fn game_sets_size(&self, size: (u32, u32)) -> bool {
        *self.explicit.lock().unwrap() = Some(size);
        let mut up = self.upscale.lock().unwrap();
        let Some((game, shown)) = *up else { return false };
        if size != game {
            *up = Some((size, shown));
        }
        true
    }

    pub fn game_event(&self) -> &ProtocolObject<dyn MTLSharedEvent> {
        &self.game_event
    }

    pub fn next_serial(&self) -> u64 {
        self.game_serial.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn add_blocked(&self, secs: f64) {
        *self.blocked.lock().unwrap() += secs;
    }

    // one interval sample per source frame, consuming the blocked time accumulated since the last one
    pub fn frame_sample(&self) -> Sample {
        let blocked = std::mem::take(&mut *self.blocked.lock().unwrap());
        let s = self.estimator.lock().unwrap().sample(super::now(), blocked);
        static PACE_DEBUG: OnceLock<bool> = OnceLock::new();
        if *PACE_DEBUG.get_or_init(|| std::env::var_os("LSFGM_PACE_DEBUG").is_some()) {
            let t = if s.trusted { "" } else { " (untrusted)" };
            log::info(&format!(
                "pace interval={}ms blocked={}ms{t}",
                s.interval * 1000.0,
                blocked * 1000.0
            ));
        }
        s
    }

    pub fn new_texture(&self, w: usize, h: usize, format: MTLPixelFormat) -> Option<Texture> {
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format, w, h, false,
            )
        };
        desc.setUsage(
            MTLTextureUsage::RenderTarget
                | MTLTextureUsage::ShaderRead
                | MTLTextureUsage::ShaderWrite
                | MTLTextureUsage::PixelFormatView,
        );
        desc.setStorageMode(MTLStorageMode::Private);
        self.device.newTextureWithDescriptor(&desc)
    }

    // pool of drawables handed to the game, at most three outstanding
    pub fn acquire_drawable(&'static self) -> Option<Retained<ProxyDrawable>> {
        let size = self.game_size();
        if size.0 < 1.0 || size.1 < 1.0 {
            return None;
        }
        let (w, h, format) = (size.0 as usize, size.1 as usize, self.layer.pixelFormat());
        let mut pool = self.pool.lock().unwrap();
        while pool.outstanding >= 3 {
            // like the real layer's one-second nextDrawable timeout: the caller gets a real drawable
            let (p, r) = self.pool_cv.wait_timeout(pool, Duration::from_secs(1)).unwrap();
            pool = p;
            if r.timed_out() && pool.outstanding >= 3 {
                static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log::warn("Metal drawable pool exhausted (the game holds three drawables); this frame and any like it present natively");
                }
                return None;
            }
        }
        pool.textures
            .retain(|t| t.width() == w && t.height() == h && t.pixelFormat() == format);
        let texture = match pool.textures.pop() {
            Some(t) => t,
            None => self.new_texture(w, h, format)?,
        };
        pool.outstanding += 1;
        let id = pool.next_id;
        pool.next_id += 1;
        Some(ProxyDrawable::new(texture, &self.layer, id, Some(self)))
    }

    pub fn pool_return(&self, texture: Texture) {
        let mut pool = self.pool.lock().unwrap();
        pool.outstanding -= 1;
        pool.textures.push(texture);
        self.pool_cv.notify_all();
    }

    pub fn forget(&'static self, textures: Vec<Texture>) {
        self.send(Msg::Forget(textures));
    }

    pub fn enqueue(&'static self, mut job: Job) {
        job.latency.enqueue();
        self.send(Msg::Job(job));
    }

    fn send(&'static self, msg: Msg) {
        self.started.call_once(|| {
            let rx = self.rx.lock().unwrap().take().unwrap();
            let spawned = std::thread::Builder::new()
                .name("lsfg-metal".into())
                .spawn(move || {
                    // presents are on the display's deadline: user-interactive qos keeps the worker off the efficiency cores
                    if unsafe { pthread_set_qos_class_self_np(0x21, 0) } != 0 {
                        log::warn("Metal presentation worker could not get user-interactive qos");
                    }
                    autoreleasepool(|_| Worker::new(self)).run(rx)
                });
            if let Err(e) = spawned {
                log::error(&format!("cannot start the Metal presentation worker: {e}"));
                set_enabled(false);
            }
        });
        // the worker is gone: no more generation, show this frame ourselves
        if let Err(SendError(Msg::Job(mut job))) = self.tx.send(msg) {
            job.latency.start();
            log::error("Metal presentation worker is gone, presenting natively from now on");
            set_enabled(false);
            present_natively(self, &job, None);
        }
    }
}

// bounds times scale, the size a layer's drawables follow until it is sized
fn natural_size(layer: &CAMetalLayer) -> (u32, u32) {
    let (b, s) = (layer.bounds().size, layer.contentsScale());
    ((b.width * s).round() as u32, (b.height * s).round() as u32)
}

// metal has no timed wait; poll the status with a deadline, false on timeout
fn wait_completed(cb: &ProtocolObject<dyn MTLCommandBuffer>) -> bool {
    let deadline = Instant::now() + GPU_TIMEOUT;
    loop {
        let s = cb.status();
        if s == MTLCommandBufferStatus::Completed || s == MTLCommandBufferStatus::Error {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

// ---- vulkan helpers shared with the proxy swapchain ----

// timeline-aware submit; binary semaphores carry value 0
pub(crate) fn submit(
    device: &ash::Device,
    queue: vk::Queue,
    cbs: &[vk::CommandBuffer],
    waits: &[(vk::Semaphore, u64)],
    signals: &[(vk::Semaphore, u64)],
    fence: vk::Fence,
    stage: vk::PipelineStageFlags,
) -> Result<(), String> {
    let (ws, wv): (Vec<_>, Vec<_>) = waits.iter().copied().unzip();
    let (ss, sv): (Vec<_>, Vec<_>) = signals.iter().copied().unzip();
    let stages = vec![stage; ws.len()];
    let mut tl = vk::TimelineSemaphoreSubmitInfo::default()
        .wait_semaphore_values(&wv)
        .signal_semaphore_values(&sv);
    let info = [vk::SubmitInfo::default()
        .wait_semaphores(&ws)
        .wait_dst_stage_mask(&stages)
        .command_buffers(cbs)
        .signal_semaphores(&ss)
        .push_next(&mut tl)];
    check(
        unsafe { device.queue_submit(queue, &info, fence) },
        "vkQueueSubmit",
    )
}

// shared event -> timeline semaphore
pub(crate) fn import_event(
    device: &ash::Device,
    event: &ProtocolObject<dyn MTLSharedEvent>,
) -> Result<vk::Semaphore, String> {
    let mut imp = vk::ImportMetalSharedEventInfoEXT::default()
        .mtl_shared_event(event as *const _ as *mut c_void);
    let mut ty = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .initial_value(0);
    let info = vk::SemaphoreCreateInfo::default()
        .push_next(&mut imp)
        .push_next(&mut ty);
    check(
        unsafe { device.create_semaphore(&info, None) },
        "vkCreateSemaphore",
    )
}

// texture -> image; the caller's chain rides on the import structure
// an imported metal texture is already backed by memory: the spec treats the image as bound, so no allocation
pub(crate) fn import_texture(
    device: &ash::Device,
    texture: &ProtocolObject<dyn MTLTexture>,
    mut info: vk::ImageCreateInfo,
) -> Result<vk::Image, String> {
    let mut imp = vk::ImportMetalTextureInfoEXT::default()
        .plane(vk::ImageAspectFlags::PLANE_0)
        .mtl_texture(texture as *const _ as *mut c_void);
    imp.p_next = info.p_next;
    info.p_next = &imp as *const _ as *const c_void;
    check(unsafe { device.create_image(&info, None) }, "vkCreateImage")
}

pub(crate) fn image_info(
    format: vk::Format,
    (w, h): (u32, u32),
    usage: vk::ImageUsageFlags,
) -> vk::ImageCreateInfo<'static> {
    vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
}

// ---- backend ----

pub(super) struct Backend {
    _instance: ash::Instance,
    pub(super) device: ash::Device,
    queue: vk::Queue,
    pub(super) pool: vk::CommandPool,
    inst: Arc<Instance>,
    _lib: libloading::Library,
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
    fn pthread_set_qos_class_self_np(qos: u32, relative: c_int) -> c_int;
}

// LSFGM_GENERATOR_MOLTENVK, else LSFGM_MOLTENVK, else libMoltenVK.real.dylib beside this library
fn driver_path() -> Option<PathBuf> {
    for k in ["LSFGM_GENERATOR_MOLTENVK", "LSFGM_MOLTENVK"] {
        if let Some(p) = std::env::var_os(k).filter(|p| !p.is_empty()) {
            return Some(p.into());
        }
    }
    let mut info = DlInfo {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    unsafe {
        if dladdr(driver_path as *const c_void, &mut info) == 0 || info.fname.is_null() {
            return None;
        }
        Some(
            Path::new(CStr::from_ptr(info.fname).to_str().ok()?)
                .parent()?
                .join("libMoltenVK.real.dylib"),
        )
    }
}

impl Backend {
    pub(super) fn create(setup: &Setup, gpu: Option<String>) -> Result<Backend, String> {
        // each layer's worker builds its own backend; moltenvk instance creation must not race
        static CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = CREATE.lock().unwrap_or_else(|e| e.into_inner());
        let path = driver_path().ok_or("real MoltenVK is not loaded")?;
        let (lib, entry) = vkutil::load_driver(&path)?;
        let instance = vkutil::create_instance(&entry, c"lsfg-metal", vk::API_VERSION_1_2, false)?;
        let built = (|| {
            // the layer's own gpu, not whichever one moltenvk enumerates first on a two-gpu mac
            let pd = match gpu.as_deref().filter(|n| !n.is_empty()) {
                Some(n) => vkutil::select_physical_device(&instance, n).or_else(|_| {
                    log::warn(&format!(
                        "No Vulkan device named '{n}'; using the first one"
                    ));
                    vkutil::select_physical_device(&instance, "")
                })?,
                None => vkutil::select_physical_device(&instance, "")?,
            };
            let family = vkutil::find_queue_family(
                &instance,
                pd,
                vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE,
                false,
            )?;
            let exts = check(
                unsafe { instance.enumerate_device_extension_properties(pd) },
                "vkEnumerateDeviceExtensionProperties",
            )?;
            if !exts
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(ext::metal_objects::NAME))
            {
                return Err("driver lacks VK_EXT_metal_objects".to_string());
            }
            let fp16 = setup.allow_fp16 && vkutil::half_precision_supported(&instance, pd);
            let mut s2 =
                vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
            let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
                .shader_float16(fp16)
                .timeline_semaphore(true);
            let prio = [1.0f32];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&prio)];
            let names = [
                khr::synchronization2::NAME.as_ptr(),
                ext::metal_objects::NAME.as_ptr(),
            ];
            let info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&names)
                .push_next(&mut s2)
                .push_next(&mut f12);
            let device = check(
                unsafe { instance.create_device(pd, &info, None) },
                "vkCreateDevice",
            )?;
            let rest = (|| {
                let queue = unsafe { device.get_device_queue(family, 0) };
                let pool = vkutil::create_command_pool(&device, family, true)?;
                log::info(&format!(
                    "Initializing lsfg-metal instance with half precision {}",
                    if setup.allow_fp16 {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ));
                let gipa = entry.static_fn().get_instance_proc_addr;
                let inst = Instance::adopt(
                    gipa,
                    instance.handle(),
                    pd,
                    device.handle(),
                    family,
                    fp16,
                    &setup.dll,
                    log::debug,
                )
                .inspect_err(|_| unsafe { device.destroy_command_pool(pool, None) })?;
                Ok::<_, String>((queue, pool, Arc::new(inst)))
            })();
            match rest {
                Ok((queue, pool, inst)) => Ok((pd, device, queue, pool, inst)),
                Err(e) => {
                    unsafe { device.destroy_device(None) };
                    Err(e)
                }
            }
        })();
        match built {
            Ok((_, device, queue, pool, inst)) => Ok(Backend {
                _instance: instance,
                device,
                queue,
                pool,
                inst,
                _lib: lib,
            }),
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn submit(
        &self,
        cbs: &[vk::CommandBuffer],
        waits: &[(vk::Semaphore, u64)],
        signals: &[(vk::Semaphore, u64)],
        fence: vk::Fence,
    ) -> Result<(), String> {
        submit(
            &self.device,
            self.queue,
            cbs,
            waits,
            signals,
            fence,
            vk::PipelineStageFlags::TOP_OF_PIPE,
        )
    }
}

// ---- context wrapper on the backend queue ----

pub(super) struct Wrapper {
    ctx: Context,
    source: vk::Image,
    dest: vk::Image,
    sync: vk::Semaphore,
    extent: (u32, u32),
    iteration: u32,
    remaining: u32,
    sync_counter: u64,
    in_flight: bool,
    generated: bool,
}

impl Wrapper {
    pub(super) fn new(
        b: &Backend,
        (w, h): (u32, u32),
        flow: f32,
        perf: bool,
        colour: Colour,
    ) -> Result<Wrapper, String> {
        let ctx = Context::new(b.inst.clone(), w, h, flow, perf, colour)?;
        let (source, dest, sync) = ctx.handles();
        Ok(Wrapper {
            ctx,
            source,
            dest,
            sync,
            extent: (w, h),
            iteration: 0,
            remaining: 0,
            sync_counter: 0,
            in_flight: false,
            generated: false,
        })
    }

    // copy the game's frame into the alternating source layer and start the iteration
    pub(super) fn dispatch(
        &mut self,
        b: &Backend,
        cb: vk::CommandBuffer,
        src: vk::Image,
        wait: (vk::Semaphore, u64),
        inserted: u32,
        signal: (vk::Semaphore, u64),
    ) -> Result<(), String> {
        let d = &b.device;
        // the worker waits for the previous frame's final copy before reusing this context
        let layer = self.iteration % 2;
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        unsafe {
            let pre = [
                vkutil::image_barrier(
                    A::NONE,
                    A::TRANSFER_READ,
                    L::PRESENT_SRC_KHR,
                    L::GENERAL,
                    src,
                    0,
                    1,
                ),
                vkutil::image_barrier(
                    A::NONE,
                    A::TRANSFER_WRITE,
                    L::UNDEFINED,
                    L::GENERAL,
                    self.source,
                    layer,
                    1,
                ),
            ];
            d.cmd_pipeline_barrier(
                cb,
                P::TOP_OF_PIPE,
                P::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &pre,
            );
            vkutil::cmd_blit(d, cb, src, 0, self.source, layer, self.extent);
            let post = [vkutil::image_barrier(
                A::TRANSFER_READ,
                A::NONE,
                L::GENERAL,
                L::PRESENT_SRC_KHR,
                src,
                0,
                1,
            )];
            d.cmd_pipeline_barrier(
                cb,
                P::TRANSFER,
                P::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &post,
            );
            check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
        }
        b.submit(
            &[cb],
            &[wait],
            &[(self.sync, self.sync_counter + 1), signal],
            vk::Fence::null(),
        )?;
        self.in_flight = true;
        self.ctx.dispatch(inserted, false)?;
        self.remaining = inserted;
        self.sync_counter += 1;
        self.iteration += 1;
        Ok(())
    }

    // macOS: submit the main pass early so the gpu works while we wait for a drawable
    fn generate(&mut self, ts: f64) -> Result<(), String> {
        if !self.generated && self.remaining > 0 {
            self.ctx.acquire(false, ts as f32)?;
            self.generated = true;
        }
        Ok(())
    }

    // copy one generated frame out of the context into the target image
    pub(super) fn acquire(
        &mut self,
        b: &Backend,
        cb: vk::CommandBuffer,
        target: vk::Image,
        ts: f64,
        signal: (vk::Semaphore, u64),
        fence: vk::Fence,
    ) -> Result<(), String> {
        let d = &b.device;
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        unsafe {
            let pre = [
                vkutil::image_barrier(
                    A::NONE,
                    A::TRANSFER_READ,
                    L::GENERAL,
                    L::GENERAL,
                    self.dest,
                    0,
                    1,
                ),
                vkutil::image_barrier(
                    A::NONE,
                    A::TRANSFER_WRITE,
                    L::UNDEFINED,
                    L::GENERAL,
                    target,
                    0,
                    1,
                ),
            ];
            d.cmd_pipeline_barrier(
                cb,
                P::TOP_OF_PIPE,
                P::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &pre,
            );
            vkutil::cmd_blit(d, cb, self.dest, 0, target, 0, self.extent);
            let post = [vkutil::image_barrier(
                A::TRANSFER_WRITE,
                A::MEMORY_READ,
                L::GENERAL,
                L::PRESENT_SRC_KHR,
                target,
                0,
                1,
            )];
            d.cmd_pipeline_barrier(
                cb,
                P::TRANSFER,
                P::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &post,
            );
            check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
        }
        let waits = [(self.sync, self.sync_counter + 1)];
        let mut signals: Vec<_> = (self.remaining > 1)
            .then_some((self.sync, self.sync_counter + 2))
            .into_iter()
            .collect();
        signals.push(signal);
        if !self.generated {
            self.ctx.acquire(false, ts as f32)?;
        }
        self.generated = false;
        b.submit(&[cb], &waits, &signals, fence)?;
        self.remaining -= 1;
        self.sync_counter += 1;
        if self.remaining > 0 {
            self.sync_counter += 1;
        }
        Ok(())
    }

    pub(super) fn idle(&mut self, b: &Backend) {
        if self.in_flight {
            self.in_flight = false;
            let _ = unsafe { b.device.queue_wait_idle(b.queue) };
        }
    }

    pub(super) fn destroy(mut self, b: &Backend) {
        self.idle(b);
        let _ = self.ctx.idle();
    }
}

// ---- native generator on the layer's own device ----

// the dll's spir-v, read once for every layer
fn spirv(dll: &Path) -> Result<&'static HashMap<u32, Vec<u32>>, String> {
    static SPIRV: OnceLock<Result<HashMap<u32, Vec<u32>>, String>> = OnceLock::new();
    SPIRV
        .get_or_init(|| shaders::parse(&std::fs::read(dll).map_err(|e| format!("{}: {e}", dll.display()))?))
        .as_ref()
        .map_err(|e| e.clone())
}

// LSFGM_NATIVE=0 keeps generation on moltenvk
pub(super) fn native_off() -> bool {
    std::env::var_os("LSFGM_NATIVE").is_some_and(|v| v == "0")
}

pub(super) struct Native {
    pub(super) pipeline: native::Pipeline,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    // iteration of the frame in flight and the next one, as the vulkan context counts them
    pub(super) iteration: u32,
    next: u32,
    // the block keeps the last main pass's timestamp until the next one, so the pre-pass sees it too
    pub(super) timestamp: f32,
    generated: bool,
    // the last command buffer committed; the queue is in order, so it completing means the frame is done
    pub(super) last: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
}

impl Native {
    pub(super) fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        setup: &Setup,
        extent: (u32, u32),
        colour: Colour,
    ) -> Result<Native, String> {
        // only tested on apple silicon; intel and amd gpus stay on moltenvk
        if !device.supportsFamily(MTLGPUFamily::Apple7) {
            return Err("not an Apple silicon GPU".into());
        }
        let p = &setup.profile;
        let queue = device.newCommandQueue().ok_or("no Metal command queue")?;
        let pipeline = native::Pipeline::new(
            device,
            spirv(&setup.dll)?,
            setup.allow_fp16,
            extent,
            p.flow_for(extent.1),
            p.performance_mode,
            colour,
            log::debug,
        )?;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| log::info("Frame generation runs natively on Metal"));
        Ok(Native {
            pipeline,
            queue,
            iteration: 0,
            next: 0,
            timestamp: 0.0,
            generated: false,
            last: None,
        })
    }

    pub(super) fn cb(&self) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, String> {
        self.queue.commandBuffer().ok_or_else(|| "no Metal command buffer".into())
    }

    pub(super) fn commit(&mut self, cb: Retained<ProtocolObject<dyn MTLCommandBuffer>>) {
        cb.commit();
        self.last = Some(cb);
    }

    // a new iteration: the next source layer and a main pass still to run
    pub(super) fn begin(&mut self) {
        (self.iteration, self.next, self.generated) = (self.next, self.next + 1, false);
    }

    // wait for everything committed so far
    pub(super) fn settle(&mut self) -> Result<(), String> {
        match self.last.take() {
            Some(cb) if !wait_completed(&cb) => Err("Metal frame did not complete".into()),
            Some(cb) if cb.status() == MTLCommandBufferStatus::Error => Err(format!(
                "Metal frame failed: {}",
                cb.error().map_or("unknown error".into(), |e| e.localizedDescription().to_string())
            )),
            _ => Ok(()),
        }
    }

    // generated frame into `target`, running the main pass first if it has not run
    pub(super) fn produce(
        &mut self,
        target: &ProtocolObject<dyn MTLTexture>,
        ts: f64,
        signal: Option<(&ProtocolObject<dyn MTLEvent>, u64)>,
    ) -> Result<(), String> {
        self.main_pass(ts)?;
        let cb = self.cb()?;
        self.pipeline.copy_out(&cb, target)?;
        if let Some((e, v)) = signal {
            cb.encodeSignalEvent_value(e, v);
        }
        self.commit(cb);
        self.generated = false;
        Ok(())
    }

    pub(super) fn main_pass(&mut self, ts: f64) -> Result<(), String> {
        if !self.generated {
            self.timestamp = ts as f32;
            let cb = self.cb()?;
            self.pipeline.encode(&cb, true, self.iteration, self.timestamp)?;
            self.commit(cb);
            self.generated = true;
        }
        Ok(())
    }
}

// srgb drawables go through a unorm view, so copies move the encoded bytes instead of converting them
fn unorm_view(texture: &ProtocolObject<dyn MTLTexture>) -> Result<Texture, &'static str> {
    let format = texture.pixelFormat();
    match unorm(format) {
        f if f != format => texture
            .newTextureViewWithPixelFormat(f)
            .ok_or("could not create a unorm view of an srgb drawable"),
        _ => Ok(texture.retain()),
    }
}

pub(super) fn unorm(format: MTLPixelFormat) -> MTLPixelFormat {
    match format {
        MTLPixelFormat::BGRA8Unorm_sRGB => MTLPixelFormat::BGRA8Unorm,
        MTLPixelFormat::RGBA8Unorm_sRGB => MTLPixelFormat::RGBA8Unorm,
        MTLPixelFormat::BGR10_XR_sRGB => MTLPixelFormat::BGR10_XR,
        MTLPixelFormat::BGRA10_XR_sRGB => MTLPixelFormat::BGRA10_XR,
        other => other,
    }
}

// metalfx from the game's size into an intermediate that is copied into the real drawable
struct Scale {
    key: ScaleKey,
    scaler: metalfx::Scaler,
    // metalfx writes only private textures, and drawables are managed
    out: Texture,
    // generated frames at the game's size, for the scaler to read
    stage: Option<Texture>,
}

impl Scale {
    // unorm views on both sides, in the scale's format
    fn encode(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        src: &ProtocolObject<dyn MTLTexture>,
        dst: &ProtocolObject<dyn MTLTexture>,
    ) -> Result<(), &'static str> {
        self.scaler.encode(cb, src, &self.out);
        let blit = cb.blitCommandEncoder().ok_or("no Metal blit encoder")?;
        unsafe { blit.copyFromTexture_toTexture(&self.out, dst) };
        blit.endEncoding();
        Ok(())
    }
}

// built on first use and again when the sizes or format change; a failure turns upscaling off for the layer
fn scale_for<'a>(
    slot: &'a mut Option<Scale>,
    gen: &Generator,
    format: MTLPixelFormat,
    (game, shown): Upscale,
    kind: u32,
) -> Result<&'a mut Scale, Fail> {
    let key = (game, shown, format, kind);
    if slot.as_ref().is_none_or(|s| s.key != key) {
        *slot = None;
        let built = metalfx::Scaler::new(&gen.device, game, shown, format, kind)
            .zip(gen.new_texture(shown.0 as usize, shown.1 as usize, format));
        let Some((scaler, out)) = built else {
            gen.stop_upscaling((game, shown), &format!("no MetalFX scaler for pixel format {}", format.0));
            return Err(Fail::Skip("no MetalFX scaler".into()));
        };
        *slot = Some(Scale { key, scaler, out, stage: None });
    }
    Ok(slot.as_mut().unwrap())
}

// one pixel of a dumped frame, deeper than 8 bits, as rgb floats
type Decode = fn(&[u8]) -> [f32; 3];

// ---- worker ----

enum Fail {
    // this frame shows as it is, and the context stays
    Skip(String),
    Mismatch(String),
    Error(String),
}

// metal devices and the objects a native generator holds are thread-safe; each moves to one thread
pub(super) struct Unshared<T>(pub(super) T);
unsafe impl<T> Send for Unshared<T> {}

impl<T> Unshared<T> {
    pub(super) fn get(self) -> T {
        self.0
    }
}

// the result of a native build on its own thread
pub(super) type Built = Receiver<Result<Unshared<Native>, String>>;

// a native generator being built for this source size and format
struct Pending {
    key: ((u32, u32), MTLPixelFormat, Colour),
    rx: Built,
}

impl From<String> for Fail {
    fn from(e: String) -> Self {
        Fail::Error(e)
    }
}

impl From<&str> for Fail {
    fn from(e: &str) -> Self {
        Fail::Error(e.into())
    }
}

struct Import {
    _texture: Texture,
    image: vk::Image,
}

#[derive(Default)]
struct Stats {
    source: u64,
    original: u64,
    generated: u64,
    seconds: f64,
    samples: u64,
}

struct Worker {
    gen: &'static Generator,
    setup: &'static Setup,
    backend: Option<Backend>,
    ctx: Option<Wrapper>,
    // the native generator; moltenvk runs instead when LSFGM_NATIVE=0 or the native one cannot be built
    native: Option<Native>,
    native_off: bool,
    pending: Option<Pending>,
    views: HashMap<usize, Texture>,
    pacer: Option<Pacer>,
    extent: (u32, u32),
    format: vk::Format,
    // srgb and unorm share a vulkan format, so a switch between them is only visible here
    pixel_format: MTLPixelFormat,
    colour: Colour,
    imports: HashMap<usize, Import>,
    sems: Option<(vk::Semaphore, vk::Semaphore, vk::Fence)>,
    frame_pending: bool,
    cmd: Vec<vk::CommandBuffer>,
    present_serial: u64,
    refresh: f64,
    stats: Stats,
    stats_on: bool,
    dump_dir: Option<PathBuf>,
    // metalfx on the native queue, and on the present queue for frames shown as they are
    scale: Option<Scale>,
    plain: Option<Scale>,
    // moltenvk writes each shown frame at the game's size here; the present scales it
    ring: Vec<Texture>,
    ring_next: usize,
    staged: Option<Texture>,
    // the layer's upscaling as this job started; a swapchain recreated meanwhile cannot mix sizes within a frame
    up: Option<Upscale>,
}

impl Worker {
    fn new(gen: &'static Generator) -> Worker {
        Worker {
            gen,
            setup: setup().expect("worker without a profile"),
            backend: None,
            ctx: None,
            native: None,
            native_off: native_off(),
            pending: None,
            views: HashMap::new(),
            pacer: None,
            extent: (0, 0),
            format: vk::Format::UNDEFINED,
            pixel_format: MTLPixelFormat::Invalid,
            colour: Colour::SDR,
            imports: HashMap::new(),
            sems: None,
            frame_pending: false,
            cmd: Vec::new(),
            present_serial: 0,
            refresh: display_refresh(),
            stats: Stats::default(),
            stats_on: std::env::var_os("LSFGM_STATS").is_some(),
            dump_dir: std::env::var_os("LSFGM_METAL_DUMP")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            scale: None,
            plain: None,
            ring: Vec::new(),
            ring_next: 0,
            staged: None,
            up: None,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        hooks::mark_worker();
        for msg in rx {
            autoreleasepool(|_| match msg {
                Msg::Job(mut job) => self.process(&mut job),
                Msg::Forget(textures) => {
                    for t in textures {
                        self.forget(Retained::as_ptr(&t) as usize);
                    }
                }
            });
        }
    }

    fn forget(&mut self, key: usize) {
        self.views.remove(&key);
        if let (Some(i), Some(b)) = (self.imports.remove(&key), &self.backend) {
            unsafe {
                b.device.destroy_image(i.image, None);
            }
        }
    }

    fn process(&mut self, job: &mut Job) {
        job.latency.start();
        if job.sample.interval.is_finite() && job.sample.interval > 0.0 {
            self.stats.seconds += job.sample.interval;
            self.stats.samples += 1;
        }
        // multiplier 1 only upscales
        if !enabled() || self.setup.profile.multiplier < 2 {
            return self.present_natively(job);
        }
        match self.generate(job) {
            Ok(()) => {}
            Err(Fail::Skip(what)) => {
                log::log_fmt(log::Level::Debug, format_args!("Metal frame shown as it is: {what}"));
                self.present_natively(job);
            }
            Err(Fail::Mismatch(what)) => {
                log::log_fmt(log::Level::Debug, format_args!("Metal frame skipped: {what}"));
                self.reset();
                self.present_natively(job);
            }
            Err(Fail::Error(what)) => {
                log::error("Metal frame generation failed, presenting natively from now on:");
                log::error(&format!("- {what}"));
                set_enabled(false);
                self.reset();
                self.present_natively(job);
            }
        }
    }

    // idle and drop the context, clear the imports
    fn reset(&mut self) {
        if let (Some(c), Some(b)) = (self.ctx.take(), &self.backend) {
            c.destroy(b);
        }
        if let Some(mut n) = self.native.take() {
            let _ = n.settle();
        }
        let keys: Vec<_> = self.imports.keys().copied().collect();
        for k in keys {
            self.forget(k);
        }
        // each view holds its drawable, so old-size ones would live on
        self.views.clear();
        self.ring.clear();
        self.extent = (0, 0);
    }

    fn backend(&mut self) -> Result<&Backend, Fail> {
        if self.backend.is_none() {
            let gpu = self.gen.layer.device().map(|d| d.name().to_string());
            self.backend = Some(Backend::create(self.setup, gpu)?);
        }
        Ok(self.backend.as_ref().unwrap())
    }

    // (re)build the context when the source texture size or format changes
    fn prepare(&mut self, texture: &ProtocolObject<dyn MTLTexture>) -> Result<(), Fail> {
        let (w, h) = (texture.width() as u32, texture.height() as u32);
        // the xr formats have no vulkan equivalent, so only the native generator takes them
        let format = vk_format(texture.pixelFormat()).unwrap_or(vk::Format::UNDEFINED);
        let colour = hooks::colour(texture.pixelFormat(), self.gen.layer.colorspace().as_deref())
            .ok_or_else(|| Fail::Mismatch(format!("unsupported drawable pixel format {}", texture.pixelFormat().0)))?;
        if (self.ctx.is_some() || self.native.is_some())
            && (w, h) == self.extent
            && texture.pixelFormat() == self.pixel_format
            && colour == self.colour
        {
            return Ok(());
        }
        let key = ((w, h), texture.pixelFormat(), colour);
        if !self.native_off {
            // the native build runs off the worker, so the game never waits on it; its frames show as they are meanwhile
            match self.pending.as_ref().filter(|p| p.key == key).map(|p| p.rx.try_recv()) {
                Some(Err(TryRecvError::Empty)) => return Err(Fail::Skip("native generator still building".into())),
                Some(Ok(Ok(n))) => {
                    self.pending = None;
                    self.reset();
                    self.plain = None;
                    self.announce(texture, colour);
                    self.native = Some(n.0);
                    (self.extent, self.format, self.pixel_format, self.colour) = ((w, h), format, texture.pixelFormat(), colour);
                    return Ok(());
                }
                Some(result) => {
                    self.pending = None;
                    let e = match result {
                        Ok(Err(e)) => e,
                        _ => "the build thread ended".into(),
                    };
                    log::warn(&format!("Native Metal generator unavailable ({e}); using MoltenVK"));
                    self.native_off = true;
                }
                None => {
                    self.reset();
                    let p = &self.setup.profile;
                    if !Signature::new(p.performance_mode).fits(w, h, p.flow_for(h)) {
                        return Err(Fail::Mismatch(format!("{w}x{h} is too small to generate frames for")));
                    }
                    let (tx, rx) = channel();
                    let (device, setup) = (Unshared(self.gen.device.clone()), self.setup);
                    std::thread::Builder::new()
                        .name("lsfg-metal native build".into())
                        .spawn(move || {
                            let device = device.get();
                            let built = autoreleasepool(|_| Native::new(&device, setup, (w, h), colour));
                            let _ = tx.send(built.map(Unshared));
                        })
                        .map_err(|e| format!("no thread for the native build: {e}"))?;
                    self.pending = Some(Pending { key, rx });
                    return Err(Fail::Skip("native generator building".into()));
                }
            }
        }
        self.reset();
        let p = &self.setup.profile;
        if !Signature::new(p.performance_mode).fits(w, h, p.flow_for(h)) {
            return Err(Fail::Mismatch(format!("{w}x{h} is too small to generate frames for")));
        }
        // only this layer goes without generation, so it is not an error that would stop every other layer
        if format == vk::Format::UNDEFINED {
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                log::warn("XR layer without the native generator; its frames show as they are");
            }
            return Err(Fail::Mismatch("XR layer without the native generator".into()));
        }
        self.announce(texture, colour);
        self.backend()?;
        let b = self.backend.as_ref().ok_or("backend missing")?;
        self.ctx = Some(Wrapper::new(b, (w, h), p.flow_for(h), p.performance_mode, colour)?);
        if self.sems.is_none() {
            let game = import_event(&b.device, &self.gen.game_event)?;
            let present = import_event(&b.device, &self.gen.present_event)?;
            self.sems = Some((game, present, vkutil::create_fence(&b.device)?));
        }
        while self.cmd.len() < p.multiplier as usize + 2 {
            self.cmd
                .push(vkutil::allocate_command_buffer(&b.device, b.pool)?);
        }
        (self.extent, self.format, self.pixel_format, self.colour) = ((w, h), format, texture.pixelFormat(), colour);
        Ok(())
    }

    // the display rate and pacer for a new context, and its log line
    fn announce(&mut self, texture: &ProtocolObject<dyn MTLTexture>, colour: Colour) {
        let (w, h) = (texture.width() as u32, texture.height() as u32);
        let p = &self.setup.profile;
        // the window may have moved to another display, or the mode changed, since the last build
        self.refresh = autoreleasepool(|_| display_refresh());
        let m = p.multiplier;
        let adaptive = p.pacing_mode == PacingMode::Adaptive;
        let mode = if adaptive {
            format!("adaptive up to {m}")
        } else {
            format!("multiplier {m}")
        };
        log::info(&format!(
            "Metal presentation {w}x{h} (pixel format {}{}, stored {:?}, colour kind {}), {mode}, display {} Hz, flow {:.2}",
            texture.pixelFormat().0,
            match texture.pixelFormat() {
                MTLPixelFormat::BGRA8Unorm_sRGB
                | MTLPixelFormat::RGBA8Unorm_sRGB
                | MTLPixelFormat::BGR10_XR_sRGB
                | MTLPixelFormat::BGRA10_XR_sRGB => " srgb",
                _ => "",
            },
            colour.store,
            colour.kind,
            (1.0 / self.refresh).round(),
            p.flow_for(h)
        ));
        self.pacer = adaptive.then(|| Pacer::new(self.refresh, m));
    }

    // cached view of a drawable for the native generator
    fn view(&mut self, texture: &ProtocolObject<dyn MTLTexture>) -> Result<Texture, Fail> {
        let key = texture as *const _ as usize;
        if let Some(v) = self.views.get(&key) {
            return Ok(v.clone());
        }
        let size = (texture.width() as u32, texture.height() as u32);
        if (size != self.extent && Some(size) != self.up.map(|u| u.1))
            || (texture.pixelFormat() != self.pixel_format && vk_format(texture.pixelFormat()).ok() != Some(self.format))
        {
            return Err(Fail::Mismatch(
                "drawable does not match the layer it came from".into(),
            ));
        }
        let v = unorm_view(texture)?;
        self.views.insert(key, v.clone());
        Ok(v)
    }

    // cached texture import on the backend
    fn imported(&mut self, texture: &ProtocolObject<dyn MTLTexture>) -> Result<vk::Image, Fail> {
        let key = texture as *const _ as usize;
        if let Some(i) = self.imports.get(&key) {
            return Ok(i.image);
        }
        if (texture.width() as u32, texture.height() as u32) != self.extent
            || vk_format(texture.pixelFormat()).ok() != Some(self.format)
        {
            return Err(Fail::Mismatch(
                "drawable does not match the layer it came from".into(),
            ));
        }
        let b = self.backend.as_ref().ok_or("backend missing")?;
        let info = image_info(
            self.format,
            self.extent,
            vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
        );
        // moltenvk samples the texture as it is, so an srgb one would be linearised by the swizzling blits
        let view = unorm_view(texture)?;
        let image = import_texture(&b.device, &view, info)?;
        self.imports.insert(
            key,
            Import {
                _texture: view,
                image,
            },
        );
        Ok(image)
    }

    fn next_serial(&mut self) -> u64 {
        self.present_serial += 1;
        self.present_serial
    }

    fn present_event(&self) -> &ProtocolObject<dyn MTLEvent> {
        ProtocolObject::from_ref(&*self.gen.present_event)
    }

    fn present_cb(&self) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, String> {
        self.gen
            .present_queue
            .commandBuffer()
            .ok_or_else(|| "no Metal command buffer".into())
    }

    // a present command buffer that waits for the pipeline, never blocking the worker
    fn present_when_ready(
        &mut self,
        target: &ProtocolObject<dyn CAMetalDrawable>,
        duration: f64,
        value: u64,
    ) -> Result<(), Fail> {
        let cb = self.present_cb()?;
        cb.encodeWaitForEvent_value(self.present_event(), value);
        if let (Some(staged), Some(up)) = (self.staged.take(), self.up) {
            let dst = unorm_view(&target.texture())?;
            // only moltenvk stages, so a skip here is past start() too
            let scale = scale_for(&mut self.plain, self.gen, staged.pixelFormat(), up, self.colour.kind).map_err(|e| match e {
                Fail::Skip(what) => Fail::Mismatch(what),
                e => e,
            })?;
            scale.encode(&cb, &staged, &dst)?;
        }
        present(&cb, ProtocolObject::from_ref(target), duration);
        cb.commit();
        Ok(())
    }

    // keep the proxy alive until the pipeline has read it
    fn retire(&self, proxy: Retained<ProxyDrawable>, value: u64) {
        let block = RcBlock::new(
            move |_: NonNull<ProtocolObject<dyn MTLSharedEvent>>, _: u64| {
                let _keep = &proxy;
            },
        );
        unsafe {
            self.gen.present_event.notifyListener_atValue_block(
                &self.gen.listener,
                value,
                RcBlock::as_ptr(&block),
            )
        };
    }

    fn copy(
        &self,
        cb: vk::CommandBuffer,
        src: vk::Image,
        dst: vk::Image,
        signal: (vk::Semaphore, u64),
        fence: vk::Fence,
    ) -> Result<(), String> {
        let b = self.backend.as_ref().unwrap();
        let d = &b.device;
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        unsafe {
            let pre = [
                vkutil::image_barrier(A::NONE, A::TRANSFER_READ, L::GENERAL, L::GENERAL, src, 0, 1),
                vkutil::image_barrier(
                    A::NONE,
                    A::TRANSFER_WRITE,
                    L::UNDEFINED,
                    L::GENERAL,
                    dst,
                    0,
                    1,
                ),
            ];
            d.cmd_pipeline_barrier(
                cb,
                P::TOP_OF_PIPE,
                P::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &pre,
            );
            vkutil::cmd_blit(d, cb, src, 0, dst, 0, self.extent);
            check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
        }
        b.submit(&[cb], &[], &[signal], fence)
    }

    // while upscaling, a real drawable of another size or format belongs to a layer changed since this job began
    fn shown(&self, target: &ProtocolObject<dyn MTLTexture>) -> Result<(), Fail> {
        match self.up {
            Some((_, s)) if (target.width() as u32, target.height() as u32) != s => {
                Err(Fail::Skip("the drawable is from before a resize".into()))
            }
            Some(_) if unorm(target.pixelFormat()) != unorm(self.pixel_format) => {
                Err(Fail::Skip("the drawable's pixel format changed".into()))
            }
            _ => Ok(()),
        }
    }

    // the next ring texture for a frame moltenvk writes at the game's size, scaled by its present
    fn stage(&mut self, target: &ProtocolObject<dyn MTLTexture>) -> Result<vk::Image, Fail> {
        // moltenvk took this frame in start(): skipped now, its context would wait on work never submitted
        if let Err(Fail::Skip(what)) = self.shown(target) {
            return Err(Fail::Mismatch(what));
        }
        let (w, h) = self.extent;
        // three real drawables bound the presents in flight, so the fourth texture back is never still read
        if self.ring.len() < 4 {
            let t = self
                .gen
                .new_texture(w as usize, h as usize, unorm(self.pixel_format))
                .ok_or("no MetalFX stage texture")?;
            self.ring.push(t);
        }
        let t = self.ring[self.ring_next % self.ring.len()].clone();
        self.ring_next += 1;
        let image = self.imported(&t)?;
        self.staged = Some(t);
        Ok(image)
    }

    fn next_real(&self, what: &str) -> Result<Drawable, Fail> {
        hooks::original_next_drawable(&self.gen.layer).ok_or_else(|| Fail::Mismatch(what.into()))
    }

    // wait for the previous frame before its context is reused
    fn settle(&mut self) -> Result<(), Fail> {
        if let Some(n) = &mut self.native {
            n.settle()?;
        }
        if self.frame_pending {
            let (_, _, fence) = self.sems.unwrap();
            let d = &self.backend.as_ref().unwrap().device;
            unsafe { d.wait_for_fences(&[fence], true, GPU_TIMEOUT.as_nanos() as u64) }
                .map_err(|_| "Metal frame did not complete")?;
            check(unsafe { d.reset_fences(&[fence]) }, "vkResetFences")?;
            self.frame_pending = false;
        }
        Ok(())
    }

    // the game's frame into the source and the pre-pass; returns the present value that marks it copied
    fn start(
        &mut self,
        texture: &ProtocolObject<dyn MTLTexture>,
        serial: u64,
        inserted: u32,
    ) -> Result<u64, Fail> {
        if self.native.is_some() {
            let view = self.view(texture)?;
            let copied = self.next_serial();
            let gen = self.gen;
            let n = self.native.as_mut().unwrap();
            n.begin();
            let cb = n.cb()?;
            cb.encodeWaitForEvent_value(ProtocolObject::from_ref(&*gen.game_event), serial);
            n.pipeline.copy_in(&cb, &view, n.iteration % 2)?;
            // signalled once the copy is done, as on moltenvk, so the game's drawable goes back before the pre-pass
            cb.encodeSignalEvent_value(ProtocolObject::from_ref(&*gen.present_event), copied);
            n.pipeline.encode(&cb, false, n.iteration, n.timestamp)?;
            n.commit(cb);
            return Ok(copied);
        }
        let source = self.imported(texture)?;
        let (game_sem, present, _) = self.sems.unwrap();
        let copied = self.next_serial();
        let b = self.backend.as_ref().unwrap();
        self.ctx.as_mut().unwrap().dispatch(
            b,
            self.cmd[0],
            source,
            (game_sem, serial),
            inserted,
            (present, copied),
        )?;
        Ok(copied)
    }

    // the main pass, submitted before the drawable is waited for
    fn early(&mut self, slot: f64) -> Result<(), Fail> {
        match &mut self.native {
            Some(n) => n.main_pass(slot)?,
            None => self.ctx.as_mut().unwrap().generate(slot)?,
        }
        Ok(())
    }

    // generated frame i into the target; returns its present value
    fn produce(
        &mut self,
        target: &ProtocolObject<dyn MTLTexture>,
        i: usize,
        slot: f64,
        last: bool,
    ) -> Result<u64, Fail> {
        if self.native.is_some() {
            self.shown(target)?;
            let view = self.view(target)?;
            let ready = self.next_serial();
            let gen = self.gen;
            let event = ProtocolObject::from_ref(&*gen.present_event);
            let Some(up) = self.up else {
                self.native.as_mut().unwrap().produce(&view, slot, Some((event, ready)))?;
                return Ok(ready);
            };
            // generated at the game's size into the stage, then scaled into the drawable
            let scale = scale_for(&mut self.scale, gen, view.pixelFormat(), up, self.colour.kind)?;
            if scale.stage.is_none() {
                let (w, h) = up.0;
                scale.stage = Some(gen.new_texture(w as usize, h as usize, view.pixelFormat()).ok_or("no MetalFX stage texture")?);
            }
            let stage = scale.stage.clone().unwrap();
            let n = self.native.as_mut().unwrap();
            n.produce(&stage, slot, None)?;
            let cb = n.cb()?;
            scale.encode(&cb, &stage, &view)?;
            cb.encodeSignalEvent_value(event, ready);
            n.commit(cb);
            return Ok(ready);
        }
        let timg = match self.up {
            Some(_) => self.stage(target)?,
            None => self.imported(target)?,
        };
        let ready = self.next_serial();
        let (_, present, fence) = self.sems.unwrap();
        let (b, cb) = (self.backend.as_ref().unwrap(), self.cmd[1 + i]);
        let completion = if last { fence } else { vk::Fence::null() };
        self.ctx
            .as_mut()
            .unwrap()
            .acquire(b, cb, timg, slot, (present, ready), completion)?;
        self.frame_pending |= last;
        Ok(ready)
    }

    // the game's own frame into the target; returns its present value
    fn original(
        &mut self,
        texture: &ProtocolObject<dyn MTLTexture>,
        target: &ProtocolObject<dyn MTLTexture>,
        inserted: usize,
    ) -> Result<u64, Fail> {
        if self.native.is_some() {
            // unorm views on both sides, so a layer that switched srgb-ness still copies the bytes
            self.shown(target)?;
            let (src, dst) = (self.view(texture)?, self.view(target)?);
            let ready = self.next_serial();
            let gen = self.gen;
            let cb = self.native.as_ref().unwrap().cb()?;
            match self.up {
                Some(up) => scale_for(&mut self.scale, gen, src.pixelFormat(), up, self.colour.kind)?.encode(&cb, &src, &dst)?,
                None => {
                    let blit = cb.blitCommandEncoder().ok_or("no Metal blit encoder")?;
                    unsafe { blit.copyFromTexture_toTexture(&src, &dst) };
                    blit.endEncoding();
                }
            }
            cb.encodeSignalEvent_value(ProtocolObject::from_ref(&*gen.present_event), ready);
            let n = self.native.as_mut().unwrap();
            n.commit(cb);
            return Ok(ready);
        }
        let source = self.imported(texture)?;
        let timg = match self.up {
            Some(_) => self.stage(target)?,
            None => self.imported(target)?,
        };
        let ready = self.next_serial();
        let (_, present, fence) = self.sems.unwrap();
        self.copy(self.cmd[1 + inserted], source, timg, (present, ready), fence)?;
        self.frame_pending = true;
        Ok(ready)
    }

    fn generate(&mut self, job: &mut Job) -> Result<(), Fail> {
        self.settle()?;
        self.staged = None;
        self.up = self.gen.upscaling();
        let texture = job.drawable.texture().retain();
        // drawn before a resize: the layer already hands out drawables of the new size, so keep the context for now
        let size = match self.up {
            Some(((w, h), _)) => (w as f64, h as f64),
            None => self.gen.game_size(),
        };
        let near = |v: usize, f: f64| v == f as usize || v == f.round() as usize;
        if !near(texture.width(), size.0) || !near(texture.height(), size.1) {
            return Err(Fail::Skip("the frame is from before a resize".into()));
        }
        self.prepare(&texture)?;
        if job.serial == 0 {
            if let Some(cb) = &job.cb {
                if !wait_completed(cb) {
                    return Err("Metal command buffer did not complete".into());
                }
            }
            // the cpu already waited, so take the held value rather than push the event past the gpu
            job.serial = self.gen.game_event.signaledValue();
        }
        if job.duration > job.sample.interval {
            job.sample = Sample {
                interval: job.duration,
                trusted: true,
            };
        }
        let m = self.setup.profile.multiplier;
        let slots: Vec<f64> = match &mut self.pacer {
            Some(p) => p.slots(job.sample),
            None => (1..=m).map(|i| i as f64 / m as f64).collect(),
        };
        let show_original = *slots.last().unwrap() >= 1.0;
        let inserted = slots.len() - show_original as usize;
        let duration = if job.duration > 0.0 {
            job.duration / slots.len() as f64
        } else {
            0.0
        };
        let copied = self.start(&texture, job.serial, inserted as u32)?;
        if self.stats.source == 89 {
            self.dump(&texture, "previous", copied);
        }
        if self.stats.source == 90 {
            self.dump(&texture, "original", copied);
        }
        for (i, &slot) in slots.iter().take(inserted).enumerate() {
            self.early(slot)?;
            let acquire_start = job.latency.clock();
            let target = self.next_real("no drawable for a generated frame")?;
            let acquire_ms = (job.latency.clock() - acquire_start) * 1000.0;
            let ttex = target.texture();
            let ready = self.produce(&ttex, i, slot, !show_original && i + 1 == inserted)?;
            if self.stats.source == 90 {
                self.dump(&ttex, &format!("generated{i}"), ready);
            }
            if !show_original && i == inserted - 1 {
                forward_presented(job.drawable.clone(), &target);
            }
            self.gen.latency.attach(
                &target,
                job.latency,
                acquire_ms,
                latency::Kind::Generated,
            );
            self.present_when_ready(&target, duration, ready)?;
            self.stats.generated += 1;
        }
        if show_original {
            let acquire_start = job.latency.clock();
            let target = self.next_real("no drawable for the original frame")?;
            let acquire_ms = (job.latency.clock() - acquire_start) * 1000.0;
            let ready = self.original(&texture, &target.texture(), inserted)?;
            forward_presented(job.drawable.clone(), &target);
            self.gen.latency.attach(
                &target,
                job.latency,
                acquire_ms,
                latency::Kind::Original,
            );
            self.present_when_ready(&target, duration, ready)?;
            self.retire(job.drawable.clone(), ready);
            self.stats.original += 1;
        } else {
            self.retire(job.drawable.clone(), copied);
        }
        log::log_fmt(
            log::Level::Debug,
            format_args!(
                "Metal frame {} done: {inserted} inserted, original shown {show_original}, present serial {}",
                self.stats.source, self.present_serial
            ),
        );
        self.count_original();
        Ok(())
    }

    fn present_natively(&mut self, job: &Job) {
        let gen = self.gen;
        let texture = job.drawable.texture();
        let scale = gen.upscaling().and_then(|up| {
            let kind = hooks::colour(texture.pixelFormat(), gen.layer.colorspace().as_deref())?.kind;
            scale_for(&mut self.plain, gen, unorm(texture.pixelFormat()), up, kind).ok()
        });
        if present_natively(gen, job, scale.as_deref()) {
            self.stats.original += 1;
        }
        self.count_original();
    }

    // stats every 60th source frame
    fn count_original(&mut self) {
        self.stats.source += 1;
        if !self.stats_on || !self.stats.source.is_multiple_of(60) {
            return;
        }
        let s = &mut self.stats;
        let mut line = format!(
            "Frame generation stats pid={} source={} original={} generated={} total={}",
            std::process::id(),
            s.source,
            s.original,
            s.generated,
            s.original + s.generated
        );
        if s.samples > 0 && s.seconds > 0.0 {
            line += &format!(" source_fps={}", (s.samples as f64 / s.seconds).round());
            s.seconds = 0.0;
            s.samples = 0;
        }
        if let Some(p) = &mut self.pacer {
            line += &format!(" slots={}", p.histogram());
        }
        line += " (metal presents on this layer)";
        log::info(&line);
    }

    // frame dump: wait for the pipeline on the gpu, read back through a shared buffer
    fn dump(&self, texture: &ProtocolObject<dyn MTLTexture>, name: &str, value: u64) {
        let Some(dir) = &self.dump_dir else { return };
        let (w, h, format) = (texture.width(), texture.height(), texture.pixelFormat());
        let wide = matches!(
            format,
            MTLPixelFormat::RGBA16Float | MTLPixelFormat::BGRA10_XR | MTLPixelFormat::BGRA10_XR_sRGB
        );
        let bpp = if wide { 8 } else { 4 };
        let len = w * h * bpp;
        let (Some(buffer), Ok(cb)) = (
            self.gen
                .device
                .newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared),
            self.present_cb(),
        ) else {
            return;
        };
        cb.encodeWaitForEvent_value(self.present_event(), value);
        let Some(blit) = cb.blitCommandEncoder() else {
            return;
        };
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                texture,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
                MTLSize { width: w, height: h, depth: 1 },
                &buffer,
                0,
                w * bpp,
                len,
            );
        }
        blit.endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        let px =
            unsafe { std::slice::from_raw_parts(buffer.contents().as_ptr() as *const u8, len) };
        let mut out = Vec::with_capacity(len);
        // anything deeper than 8 bits is written as float, so a dump keeps every bit the frame has
        let decode: Option<Decode> = match format {
            MTLPixelFormat::RGBA16Float => Some(|p| {
                let c = |i: usize| half_to_f32(u16::from_le_bytes([p[i], p[i + 1]]));
                [c(0), c(2), c(4)]
            }),
            // 10-bit is one little-endian word, red in the low ten bits (blue for bgr10a2)
            MTLPixelFormat::RGB10A2Unorm => Some(|p| {
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                [0, 10, 20].map(|s| ((v >> s) & 0x3ff) as f32 / 1023.0)
            }),
            MTLPixelFormat::BGR10A2Unorm => Some(|p| {
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                [20, 10, 0].map(|s| ((v >> s) & 0x3ff) as f32 / 1023.0)
            }),
            // xr stores (value * 510 + 384) in ten bits, blue lowest
            MTLPixelFormat::BGR10_XR | MTLPixelFormat::BGR10_XR_sRGB => Some(|p| {
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                [20, 10, 0].map(|s| (((v >> s) & 0x3ff) as f32 - 384.0) / 510.0)
            }),
            // the 64-bit xr keeps each ten bits at the top of a 16-bit word, in b g r a order
            MTLPixelFormat::BGRA10_XR | MTLPixelFormat::BGRA10_XR_sRGB => Some(|p| {
                let c = |i: usize| ((u16::from_le_bytes([p[i], p[i + 1]]) >> 6) as f32 - 384.0) / 510.0;
                [c(4), c(2), c(0)]
            }),
            _ => None,
        };
        if let Some(decode) = decode {
            out.extend_from_slice(format!("PF\n{w} {h}\n-1.0\n").as_bytes());
            for y in (0..h).rev() {
                for p in px[y * w * bpp..(y + 1) * w * bpp].chunks_exact(bpp) {
                    for c in decode(p) {
                        out.extend_from_slice(&c.to_le_bytes());
                    }
                }
            }
        } else {
            let bgra = matches!(format, MTLPixelFormat::BGRA8Unorm | MTLPixelFormat::BGRA8Unorm_sRGB);
            out.extend_from_slice(format!("P6 {w} {h} 255\n").as_bytes());
            for p in px.chunks_exact(4) {
                out.extend_from_slice(&if bgra { [p[2], p[1], p[0]] } else { [p[0], p[1], p[2]] });
            }
        }
        let _ = std::fs::create_dir_all(dir);
        if let Err(e) = std::fs::write(dir.join(name), out) {
            log::warn(&format!("Metal frame dump {name} failed: {e}"));
        }
    }
}

// false when the frame had to be dropped (its presented handlers still fire)
fn present_natively(gen: &'static Generator, job: &Job, scale: Option<&Scale>) -> bool {
    if job.serial == 0 {
        if let Some(cb) = &job.cb {
            if !wait_completed(cb) {
                log::warn("Metal command buffer did not complete, presenting anyway");
            }
        }
    }
    let acquire_start = job.latency.clock();
    let real = hooks::original_next_drawable(&gen.layer);
    let acquire_ms = (job.latency.clock() - acquire_start) * 1000.0;
    let cb = gen.present_queue.commandBuffer();
    let (Some(real), Some(cb)) = (real, cb) else {
        // a game waiting on its presented handler must not block forever
        job.drawable.presented(0.0);
        return false;
    };
    if job.serial != 0 {
        cb.encodeWaitForEvent_value(ProtocolObject::from_ref(&*gen.game_event), job.serial);
    }
    let (src, dst) = (job.drawable.texture(), real.texture());
    let sizes = |t: &ProtocolObject<dyn MTLTexture>| (t.width() as u32, t.height() as u32);
    // mid-resize the sizes differ: still present (one blank frame) so the layer settles; skipping stalls the game
    if let Some(s) = scale.filter(|s| (sizes(src), sizes(&dst)) == (s.key.0, s.key.1) && unorm(dst.pixelFormat()) == s.key.2) {
        if let (Ok(a), Ok(b)) = (unorm_view(src), unorm_view(&dst)) {
            let _ = s.encode(&cb, &a, &b);
        }
    } else if src.width() == dst.width()
        && src.height() == dst.height()
        && src.pixelFormat() == dst.pixelFormat()
    {
        if let Some(blit) = cb.blitCommandEncoder() {
            unsafe { blit.copyFromTexture_toTexture(src, &dst) };
            blit.endEncoding();
        }
    }
    gen.latency
        .attach(&real, job.latency, acquire_ms, latency::Kind::Fallback);
    forward_presented(job.drawable.clone(), &real);
    let keep = job.drawable.clone();
    let block = RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
        let _keep = &keep;
    });
    unsafe { cb.addCompletedHandler(RcBlock::as_ptr(&block)) };
    present(&cb, ProtocolObject::from_ref(&*real), job.duration);
    cb.commit();
    true
}

fn present(
    cb: &ProtocolObject<dyn MTLCommandBuffer>,
    drawable: &ProtocolObject<dyn MTLDrawable>,
    duration: f64,
) {
    if duration > 0.0 {
        cb.presentDrawable_afterMinimumDuration(drawable, duration);
    } else {
        cb.presentDrawable(drawable);
    }
}

// the game's presented handlers fire with the real drawable's time
// dropped with the handler when the shown frame leaves the screen, which recycles the proxy
struct Recycle(Retained<ProxyDrawable>);

impl Drop for Recycle {
    fn drop(&mut self) {
        self.0.recycle();
    }
}

fn forward_presented(proxy: Retained<ProxyDrawable>, shown: &ProtocolObject<dyn CAMetalDrawable>) {
    let proxy = Recycle(proxy);
    let block = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
        proxy.0.presented(unsafe { d.as_ref() }.presentedTime())
    });
    unsafe { shown.addPresentedHandler(RcBlock::as_ptr(&block)) };
}

pub(crate) fn half_to_f32(h: u16) -> f32 {
    let (s, e, m) = (
        (h >> 15) as u32,
        ((h >> 10) & 0x1f) as u32,
        (h & 0x3ff) as u32,
    );
    let bits = match e {
        0 if m == 0 => s << 31,
        0 => return (if s == 1 { -1.0 } else { 1.0 }) * m as f32 / 1024.0 * 2f32.powi(-14),
        31 => (s << 31) | 0x7f80_0000 | (m << 13),
        _ => (s << 31) | ((e + 112) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    #[test]
    fn half_conversion() {
        assert_eq!(super::half_to_f32(0x3c00), 1.0);
        assert_eq!(super::half_to_f32(0x4400), 4.0);
        assert_eq!(super::half_to_f32(0xc000), -2.0);
        assert_eq!(super::half_to_f32(0), 0.0);
    }
}
