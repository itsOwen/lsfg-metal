// proxy swapchain: metal textures owned by the shim stand in for the driver's swapchain images
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ash::vk;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;
use objc2_quartz_core::CAMetalLayer;

use super::drawable::ProxyDrawable;
use super::generator::{image_info, import_event, import_texture, submit, Generator, Job};
use super::{hooks, now, set_enabled};
use crate::log;
use crate::pacer::Estimator;

// the game's device as adopted by the shim
#[derive(Clone)]
pub struct GameDevice {
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub family: u32,
}

// runs a closure under the shim's recursive queue mutex
pub type QueueLock = Arc<dyn Fn(&mut dyn FnMut()) + Send + Sync>;

fn locked<R>(lock: &QueueLock, f: impl FnOnce() -> R) -> R {
    let mut f = Some(f);
    let mut out = None;
    lock(&mut || out = Some(f.take().unwrap()()));
    out.unwrap()
}

// the flag is per-device in principle; single-device mode makes it process wide
static PROXY_SUPPORTED: AtomicBool = AtomicBool::new(true);

pub fn proxy_supported() -> bool {
    PROXY_SUPPORTED.load(Ordering::Relaxed)
}

pub fn set_proxy_supported(on: bool) {
    PROXY_SUPPORTED.store(on, Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Free,
    Acquired,
    Queued,
}

struct State {
    status: Vec<Status>,
    invalid: bool,
    blocked: f64,
    estimator: Estimator,
}

pub struct ProxySwapchain {
    game: GameDevice,
    lock: QueueLock,
    gen: &'static Generator,
    layer: Retained<CAMetalLayer>,
    extent: (u32, u32),
    textures: Vec<Retained<ProtocolObject<dyn MTLTexture>>>,
    images: Vec<vk::Image>,
    ready: vk::Semaphore,
    state: Arc<(Mutex<State>, Condvar)>,
}
unsafe impl Send for ProxySwapchain {}
unsafe impl Sync for ProxySwapchain {}

impl ProxySwapchain {
    // Ok(None) when the proxy does not apply and the fixed wrapper must be used
    pub fn create(
        game: GameDevice,
        lock: QueueLock,
        surface: vk::SurfaceKHR,
        info: &vk::SwapchainCreateInfoKHR,
    ) -> Result<Option<ProxySwapchain>, String> {
        let Some(layer) = hooks::layer_of_surface(surface) else {
            return Ok(None);
        };
        let mut p = info.p_next as *const vk::BaseInStructure;
        while !p.is_null() {
            unsafe {
                if (*p).s_type != vk::StructureType::IMAGE_FORMAT_LIST_CREATE_INFO {
                    return Ok(None);
                }
                p = (*p).p_next;
            }
        }
        if info
            .flags
            .contains(!vk::SwapchainCreateFlagsKHR::MUTABLE_FORMAT)
        {
            return Ok(None);
        }
        // the layer must be able to present the swapchain format
        let Some(format) = super::mtl_format(info.image_format) else {
            return Ok(None);
        };
        let setup = super::setup().ok_or("no active frame-generation profile")?;
        hooks::install();
        hooks::install_cb_hooks(&layer);
        set_enabled(true);
        let vsync = matches!(
            info.present_mode,
            vk::PresentModeKHR::FIFO | vk::PresentModeKHR::FIFO_RELAXED
        );
        layer.setDisplaySyncEnabled(setup.profile.override_present_mode || vsync);
        let gen = Generator::get(&layer).ok_or("no Metal device for the layer")?;
        let count = info.min_image_count.max(3) as usize;
        let extent = (info.image_extent.width, info.image_extent.height);
        // no driver swapchain configures the layer for us: device, format, blit target, size
        if layer.device().is_none() {
            layer.setDevice(Some(&gen.device));
        }
        layer.setPixelFormat(format);
        layer.setFramebufferOnly(false);
        layer.setDrawableSize(objc2_core_foundation::CGSize::new(extent.0 as f64, extent.1 as f64));
        let mut textures = Vec::with_capacity(count);
        let mut images = Vec::with_capacity(count);
        let flags = if info
            .flags
            .contains(vk::SwapchainCreateFlagsKHR::MUTABLE_FORMAT)
        {
            vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE
        } else {
            vk::ImageCreateFlags::empty()
        };
        let families =
            if info.queue_family_index_count > 0 && !info.p_queue_family_indices.is_null() {
                unsafe {
                    std::slice::from_raw_parts(
                        info.p_queue_family_indices,
                        info.queue_family_index_count as usize,
                    )
                }
            } else {
                &[]
            };
        let built = (|| {
            for _ in 0..count {
                let t = gen
                    .new_texture(extent.0 as usize, extent.1 as usize, layer.pixelFormat())
                    .ok_or("unable to allocate a proxy image")?;
                let mut ci = image_info(
                    info.image_format,
                    extent,
                    info.image_usage | vk::ImageUsageFlags::TRANSFER_SRC,
                )
                .flags(flags)
                .sharing_mode(info.image_sharing_mode)
                .queue_family_indices(families);
                ci.p_next = info.p_next;
                images.push(import_texture(&game.device, &t, ci)?);
                textures.push(t);
            }
            import_event(&game.device, gen.game_event())
        })();
        let ready = match built {
            Ok(r) => r,
            Err(e) => {
                for i in images {
                    unsafe {
                        game.device.destroy_image(i, None);
                    }
                }
                return Err(e);
            }
        };
        log::info(&format!(
            "Vulkan proxy swapchain: {count} images, independent Metal presentation worker"
        ));
        let state = State {
            status: vec![Status::Free; count],
            invalid: false,
            blocked: 0.0,
            estimator: Estimator::default(),
        };
        Ok(Some(ProxySwapchain {
            game,
            lock,
            gen,
            layer,
            extent,
            textures,
            images,
            ready,
            state: Arc::new((Mutex::new(state), Condvar::new())),
        }))
    }

    // safety: standard vkGetSwapchainImagesKHR pointer contract
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn get_swapchain_images(
        &self,
        p_count: *mut u32,
        p_images: *mut vk::Image,
    ) -> vk::Result {
        let n = self.images.len() as u32;
        unsafe {
            if p_images.is_null() {
                *p_count = n;
                return vk::Result::SUCCESS;
            }
            let want = (*p_count).min(n);
            for (i, &img) in self.images.iter().take(want as usize).enumerate() {
                *p_images.add(i) = img;
            }
            *p_count = want;
        }
        if n > *p_count {
            vk::Result::INCOMPLETE
        } else {
            vk::Result::SUCCESS
        }
    }

    // wait for a free image, then signal the caller's semaphore and fence with an empty submit
    pub fn acquire_next_image(
        &self,
        timeout: u64,
        semaphore: vk::Semaphore,
        fence: vk::Fence,
        index: &mut u32,
    ) -> vk::Result {
        let t0 = now();
        let (m, cv) = &*self.state;
        let mut s = m.lock().unwrap();
        // the layer's natural size (bounds times scale) is what the driver compares against too
        let (b, scale) = (self.layer.bounds().size, self.layer.contentsScale());
        let natural = ((b.width * scale).round() as u32, (b.height * scale).round() as u32);
        if s.invalid || (natural != (0, 0) && natural != self.extent) {
            log::debug(&format!("Vulkan proxy acquire out of date: invalid {} layer {}x{} extent {}x{}", s.invalid, natural.0, natural.1, self.extent.0, self.extent.1));
            return vk::Result::ERROR_OUT_OF_DATE_KHR;
        }
        let deadline = Instant::now() + Duration::from_nanos(timeout.min(i64::MAX as u64));
        let i = loop {
            if let Some(i) = s.status.iter().position(|&st| st == Status::Free) {
                break i;
            }
            if timeout == 0 {
                return vk::Result::NOT_READY;
            }
            if timeout == u64::MAX {
                s = cv.wait(s).unwrap();
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return vk::Result::TIMEOUT;
            }
            s = cv.wait_timeout(s, left).unwrap().0;
        };
        s.status[i] = Status::Acquired;
        s.blocked += now() - t0;
        drop(s);
        let signals: Vec<_> = (semaphore != vk::Semaphore::null())
            .then_some((semaphore, 0))
            .into_iter()
            .collect();
        let r = locked(&self.lock, || {
            submit(
                &self.game.device,
                self.game.queue,
                &[],
                &[],
                &signals,
                fence,
                vk::PipelineStageFlags::ALL_COMMANDS,
            )
        });
        if let Err(e) = r {
            log::warn(&format!("Vulkan proxy acquire signal failed: {e}"));
            m.lock().unwrap().status[i] = Status::Free;
            cv.notify_all();
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        *index = i as u32;
        vk::Result::SUCCESS
    }

    // hand the presented image to the metal worker; a present we cannot honour invalidates the proxy instead
    pub fn queue_present(&self, queue: vk::Queue, info: &vk::PresentInfoKHR) -> vk::Result {
        locked(&self.lock, || {
            let mut p = info.p_next as *const vk::BaseInStructure;
            let mut ok = info.swapchain_count == 1 && queue == self.game.queue;
            while ok && !p.is_null() {
                unsafe {
                    ok = (*p).s_type == vk::StructureType::PRESENT_REGIONS_KHR;
                    p = (*p).p_next;
                }
            }
            if !ok {
                log::warn("Vulkan proxy present needs unsupported metadata or queues; recreating with fixed pacing");
                self.invalidate_locked(queue, Some(info));
                return vk::Result::ERROR_OUT_OF_DATE_KHR;
            }
            let index = unsafe { *info.p_image_indices } as usize;
            if index >= self.images.len() {
                log::debug(&format!("Vulkan proxy present out of date: image index {index} of {}", self.images.len()));
                self.invalidate_locked(queue, Some(info));
                return vk::Result::ERROR_OUT_OF_DATE_KHR;
            }
            let serial = self.gen.next_serial();
            let waits: Vec<_> = wait_semaphores(info).iter().map(|&s| (s, 0)).collect();
            if let Err(e) = submit(
                &self.game.device,
                queue,
                &[],
                &waits,
                &[(self.ready, serial)],
                vk::Fence::null(),
                vk::PipelineStageFlags::ALL_COMMANDS,
            ) {
                log::warn(&format!("Vulkan proxy present submit failed: {e}"));
                self.invalidate_locked(queue, Some(info));
                return vk::Result::ERROR_OUT_OF_DATE_KHR;
            }
            let drawable =
                ProxyDrawable::new(self.textures[index].clone(), &self.layer, index + 1, None);
            let sample = {
                let mut s = self.state.0.lock().unwrap();
                s.status[index] = Status::Queued;
                let blocked = std::mem::take(&mut s.blocked);
                s.estimator.sample(now(), blocked)
            };
            let state = self.state.clone();
            drawable.set_release(move || {
                state.0.lock().unwrap().status[index] = Status::Free;
                state.1.notify_all();
            });
            self.gen.enqueue(Job {
                latency: Default::default(),
                cb: None,
                drawable,
                duration: 0.0,
                serial,
                sample,
            });
            if !info.p_results.is_null() {
                unsafe { *info.p_results = vk::Result::SUCCESS };
            }
            vk::Result::SUCCESS
        })
    }

    // mark invalid and consume the present's wait semaphores
    pub fn invalidate(&self, queue: vk::Queue, info: Option<&vk::PresentInfoKHR>) {
        locked(&self.lock, || self.invalidate_locked(queue, info));
    }

    fn invalidate_locked(&self, queue: vk::Queue, info: Option<&vk::PresentInfoKHR>) {
        self.state.0.lock().unwrap().invalid = true;
        set_proxy_supported(false);
        let waits: Vec<_> = info
            .map(wait_semaphores)
            .unwrap_or(&[])
            .iter()
            .map(|&s| (s, 0))
            .collect();
        if !waits.is_empty() {
            if let Err(e) = submit(
                &self.game.device,
                queue,
                &[],
                &waits,
                &[],
                vk::Fence::null(),
                vk::PipelineStageFlags::ALL_COMMANDS,
            ) {
                log::warn(&format!("Vulkan proxy wait consumption failed: {e}"));
            }
        }
    }
}

fn wait_semaphores<'a>(info: &'a vk::PresentInfoKHR<'_>) -> &'a [vk::Semaphore] {
    if info.wait_semaphore_count == 0 || info.p_wait_semaphores.is_null() {
        return &[];
    }
    unsafe {
        std::slice::from_raw_parts(info.p_wait_semaphores, info.wait_semaphore_count as usize)
    }
}

// `proxies` are the proxy wrappers among the present's swapchains
pub fn invalidate_proxies(
    queue: vk::Queue,
    info: &vk::PresentInfoKHR,
    proxies: &[&ProxySwapchain],
) -> bool {
    if proxies.is_empty() {
        return false;
    }
    for (i, p) in proxies.iter().enumerate() {
        p.invalidate(queue, (i == 0).then_some(info));
    }
    if !info.p_results.is_null() {
        for i in 0..info.swapchain_count as usize {
            unsafe { *info.p_results.add(i) = vk::Result::ERROR_OUT_OF_DATE_KHR };
        }
    }
    true
}

// tear-down waits for every queued frame to leave the worker
impl Drop for ProxySwapchain {
    fn drop(&mut self) {
        let (m, cv) = &*self.state;
        let mut s = m.lock().unwrap();
        while s.status.contains(&Status::Queued) {
            let (g, r) = cv.wait_timeout(s, Duration::from_secs(2)).unwrap();
            s = g;
            if r.timed_out() {
                log::warn("Vulkan proxy swapchain destroyed with frames still queued");
                break;
            }
        }
        drop(s);
        self.gen.forget(std::mem::take(&mut self.textures));
        unsafe {
            for i in self.images.drain(..) {
                self.game.device.destroy_image(i, None);
            }
            self.game.device.destroy_semaphore(self.ready, None);
        }
    }
}
