// per-layer generator: pool, worker thread, backend device, context wrapper, dumps and stats
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, SendError, Sender};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use ash::{ext, khr, vk};
use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2::Message;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLEvent, MTLOrigin, MTLPixelFormat,
    MTLResourceOptions, MTLSharedEvent, MTLSharedEventListener, MTLSize, MTLStorageMode,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use super::drawable::ProxyDrawable;
use super::latency;
use super::{enabled, hooks, set_enabled, setup, vk_format, Setup};
use crate::generator::{Context, Instance};
use crate::log;
use crate::pacer::{display_refresh, Estimator, Pacer, Sample};
use crate::settings::PacingMode;
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
        }));
        map.insert(layer as *const _ as usize, g);
        Some(g)
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
        let size = self.layer.drawableSize();
        if size.width < 1.0 || size.height < 1.0 {
            return None;
        }
        let (w, h, format) = (
            size.width as usize,
            size.height as usize,
            self.layer.pixelFormat(),
        );
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
                .spawn(move || autoreleasepool(|_| Worker::new(self)).run(rx));
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
            present_natively(self, &job);
        }
    }
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
    pub(super) fn create(setup: &Setup) -> Result<Backend, String> {
        // each layer's worker builds its own backend; moltenvk instance creation must not race
        static CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = CREATE.lock().unwrap_or_else(|e| e.into_inner());
        let path = driver_path().ok_or("real MoltenVK is not loaded")?;
        let (lib, entry) = vkutil::load_driver(&path)?;
        let instance = vkutil::create_instance(&entry, c"lsfg-metal", vk::API_VERSION_1_2, false)?;
        let built = (|| {
            let pd = *check(
                unsafe { instance.enumerate_physical_devices() },
                "vkEnumeratePhysicalDevices",
            )?
            .first()
            .ok_or("no Vulkan device")?;
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
        hdr: bool,
    ) -> Result<Wrapper, String> {
        let ctx = Context::new(b.inst.clone(), w, h, flow, perf, hdr)?;
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

// ---- worker ----

enum Fail {
    Mismatch(String),
    Error(String),
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
    pacer: Option<Pacer>,
    extent: (u32, u32),
    format: vk::Format,
    imports: HashMap<usize, Import>,
    sems: Option<(vk::Semaphore, vk::Semaphore, vk::Fence)>,
    frame_pending: bool,
    cmd: Vec<vk::CommandBuffer>,
    present_serial: u64,
    refresh: f64,
    stats: Stats,
    stats_on: bool,
    dump_dir: Option<PathBuf>,
}

impl Worker {
    fn new(gen: &'static Generator) -> Worker {
        Worker {
            gen,
            setup: setup().expect("worker without a profile"),
            backend: None,
            ctx: None,
            pacer: None,
            extent: (0, 0),
            format: vk::Format::UNDEFINED,
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
        if !enabled() {
            return self.present_natively(job);
        }
        match self.generate(job) {
            Ok(()) => {}
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
        let keys: Vec<_> = self.imports.keys().copied().collect();
        for k in keys {
            self.forget(k);
        }
        self.extent = (0, 0);
    }

    fn backend(&mut self) -> Result<&Backend, Fail> {
        if self.backend.is_none() {
            self.backend = Some(Backend::create(self.setup)?);
        }
        Ok(self.backend.as_ref().unwrap())
    }

    // (re)build the context when the source texture size or format changes
    fn prepare(&mut self, texture: &ProtocolObject<dyn MTLTexture>) -> Result<(), Fail> {
        let (w, h) = (texture.width() as u32, texture.height() as u32);
        let format = vk_format(texture.pixelFormat())?;
        if self.ctx.is_some() && (w, h) == self.extent && format == self.format {
            return Ok(());
        }
        self.reset();
        // the window may have moved to another display, or the mode changed, since the last build
        self.refresh = autoreleasepool(|_| display_refresh());
        let p = &self.setup.profile;
        let m = p.multiplier;
        let adaptive = p.pacing_mode == PacingMode::Adaptive;
        let mode = if adaptive {
            format!("adaptive up to {m}")
        } else {
            format!("multiplier {m}")
        };
        log::info(&format!(
            "Metal presentation {w}x{h} ({format:?}), {mode}, display {} Hz",
            (1.0 / self.refresh).round()
        ));
        let b = self.backend.as_ref().ok_or("backend missing")?;
        self.ctx = Some(Wrapper::new(
            b,
            (w, h),
            p.flow_scale,
            p.performance_mode,
            format == vk::Format::R16G16B16A16_SFLOAT,
        )?);
        self.pacer = adaptive.then(|| Pacer::new(self.refresh, m));
        if self.sems.is_none() {
            let game = import_event(&b.device, &self.gen.game_event)?;
            let present = import_event(&b.device, &self.gen.present_event)?;
            self.sems = Some((game, present, vkutil::create_fence(&b.device)?));
        }
        while self.cmd.len() < m as usize + 2 {
            self.cmd
                .push(vkutil::allocate_command_buffer(&b.device, b.pool)?);
        }
        self.extent = (w, h);
        self.format = format;
        Ok(())
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
        let image = import_texture(&b.device, texture, info)?;
        self.imports.insert(
            key,
            Import {
                _texture: texture.retain(),
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
        &self,
        target: &ProtocolObject<dyn CAMetalDrawable>,
        duration: f64,
        value: u64,
    ) -> Result<(), String> {
        let cb = self.present_cb()?;
        cb.encodeWaitForEvent_value(self.present_event(), value);
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

    fn next_real(&self, what: &str) -> Result<Drawable, Fail> {
        hooks::original_next_drawable(&self.gen.layer).ok_or_else(|| Fail::Mismatch(what.into()))
    }

    fn generate(&mut self, job: &mut Job) -> Result<(), Fail> {
        self.backend()?;
        if self.frame_pending {
            let (_, _, fence) = self.sems.unwrap();
            let d = &self.backend.as_ref().unwrap().device;
            unsafe { d.wait_for_fences(&[fence], true, GPU_TIMEOUT.as_nanos() as u64) }
                .map_err(|_| "Metal frame did not complete")?;
            check(unsafe { d.reset_fences(&[fence]) }, "vkResetFences")?;
            self.frame_pending = false;
        }
        let texture = job.drawable.texture().retain();
        self.prepare(&texture)?;
        if job.serial == 0 {
            if let Some(cb) = &job.cb {
                if !wait_completed(cb) {
                    return Err("Metal command buffer did not complete".into());
                }
            }
            job.serial = self.gen.next_serial();
            // never move the shared event backwards
            if self.gen.game_event.signaledValue() < job.serial {
                self.gen.game_event.setSignaledValue(job.serial);
            }
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
        let source = self.imported(&texture)?;
        let (game_sem, present, fence) = self.sems.unwrap();
        let copied = self.next_serial();
        let b = self.backend.as_ref().unwrap();
        self.ctx.as_mut().unwrap().dispatch(
            b,
            self.cmd[0],
            source,
            (game_sem, job.serial),
            inserted as u32,
            (present, copied),
        )?;
        if self.stats.source == 89 {
            self.dump(&texture, "previous", copied);
        }
        if self.stats.source == 90 {
            self.dump(&texture, "original", copied);
        }
        for (i, &slot) in slots.iter().take(inserted).enumerate() {
            self.ctx.as_mut().unwrap().generate(slot)?;
            let acquire_start = job.latency.clock();
            let target = self.next_real("no drawable for a generated frame")?;
            let acquire_ms = (job.latency.clock() - acquire_start) * 1000.0;
            let ttex = target.texture();
            let timg = self.imported(&ttex)?;
            let ready = self.next_serial();
            let (b, cb) = (self.backend.as_ref().unwrap(), self.cmd[1 + i]);
            let completion = if !show_original && i + 1 == inserted {
                fence
            } else {
                vk::Fence::null()
            };
            self.ctx
                .as_mut()
                .unwrap()
                .acquire(b, cb, timg, slot, (present, ready), completion)?;
            self.frame_pending |= completion != vk::Fence::null();
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
            let timg = self.imported(&target.texture())?;
            let ready = self.next_serial();
            self.copy(self.cmd[1 + inserted], source, timg, (present, ready), fence)?;
            self.frame_pending = true;
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
        if present_natively(self.gen, job) {
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
        let hdr = format == MTLPixelFormat::RGBA16Float;
        let bpp = if hdr { 8 } else { 4 };
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
        if hdr {
            out.extend_from_slice(format!("PF\n{w} {h}\n-1.0\n").as_bytes());
            for y in (0..h).rev() {
                for x in 0..w {
                    for c in 0..3 {
                        let i = (y * w + x) * 8 + c * 2;
                        out.extend_from_slice(
                            &half_to_f32(u16::from_le_bytes([px[i], px[i + 1]])).to_le_bytes(),
                        );
                    }
                }
            }
        } else {
            let bgra = matches!(
                format,
                MTLPixelFormat::BGRA8Unorm | MTLPixelFormat::BGRA8Unorm_sRGB
            );
            let packed = format == MTLPixelFormat::RGB10A2Unorm;
            out.extend_from_slice(format!("P6 {w} {h} 255\n").as_bytes());
            for p in px.chunks_exact(4) {
                // rgb10a2 is one little-endian word, red in the low ten bits
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                out.extend_from_slice(&if packed {
                    [(v >> 2) as u8, (v >> 12) as u8, (v >> 22) as u8]
                } else if bgra {
                    [p[2], p[1], p[0]]
                } else {
                    [p[0], p[1], p[2]]
                });
            }
        }
        let _ = std::fs::create_dir_all(dir);
        if let Err(e) = std::fs::write(dir.join(name), out) {
            log::warn(&format!("Metal frame dump {name} failed: {e}"));
        }
    }
}

// false when the frame had to be dropped (its presented handlers still fire)
fn present_natively(gen: &'static Generator, job: &Job) -> bool {
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
    // mid-resize the sizes differ: still present (one blank frame) so the layer settles; skipping stalls the game
    if src.width() == dst.width()
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
