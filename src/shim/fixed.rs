// fixed ("vsync") swapchain wrapper and present path
use std::sync::Arc;

use ash::vk;

use super::{context, layer, DeviceEntry, Error, Swapchain};
use crate::log;
use crate::settings::PacingMode;
use crate::vkutil;

// per swapchain image: copy-in command buffer and the present semaphore of the original frame
#[derive(Clone, Copy)]
struct Pass {
    cb: vk::CommandBuffer,
    present: vk::Semaphore,
}

// per inserted frame: copy-out command buffer, acquire and copy semaphores
#[derive(Clone, Copy)]
struct Inner {
    cb: vk::CommandBuffer,
    acquire: vk::Semaphore,
    copy: vk::Semaphore,
}

pub struct Fixed {
    dev: Arc<DeviceEntry>,
    pub swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    extent: (u32, u32),
    hdr: bool,
    revision: u32,
    sync: Vec<Pass>,
    inner: Vec<Inner>,
    ctx: Option<context::Wrapper>,
    failed: bool,
    warned: bool,
    warned_mode: bool,
    stats: Option<(u64, u64)>,
    // created FIFO under override_present_mode: the inner presents stay in that mode too
    forced_fifo: bool,
}

// the mode the game selected for this present through VK_EXT_swapchain_maintenance1, if any
unsafe fn present_mode_of(p_next: *const std::ffi::c_void) -> Option<vk::PresentModeKHR> {
    let mut p = p_next.cast::<vk::BaseInStructure>();
    while !p.is_null() {
        if (*p).s_type == vk::StructureType::SWAPCHAIN_PRESENT_MODE_INFO_EXT {
            let s = &*p.cast::<vk::SwapchainPresentModeInfoEXT>();
            return (s.swapchain_count >= 1 && !s.p_present_modes.is_null())
                .then(|| *s.p_present_modes);
        }
        p = (*p).p_next;
    }
    None
}

fn ok(r: vk::Result) -> bool {
    matches!(r, vk::Result::SUCCESS | vk::Result::SUBOPTIMAL_KHR)
}

impl Fixed {
    // create the real swapchain, then the wrapper on top of it; on failure after the real create the swapchain is destroyed again
    pub unsafe fn create(
        dev: Arc<DeviceEntry>,
        info: &vk::SwapchainCreateInfoKHR,
        caps: &vk::SurfaceCapabilitiesKHR,
        real: vk::PFN_vkCreateSwapchainKHR,
        alloc: *const vk::AllocationCallbacks,
    ) -> Result<Fixed, Error> {
        let hook = dev.hook.as_ref().expect("hooked device");
        let layer = layer().expect("active layer");
        let profile = layer.profile();
        let m = profile.multiplier;
        let mut ci = *info;
        ci.image_usage |= vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC;
        if !profile.preserve_swapchain_image_count {
            let max = if caps.max_image_count == 0 {
                999
            } else {
                caps.max_image_count
            };
            ci.min_image_count = (ci.min_image_count + m).min(8).min(max);
        }
        if profile.override_present_mode {
            ci.present_mode = vk::PresentModeKHR::FIFO;
        }
        let mut swapchain = vk::SwapchainKHR::null();
        let r = real(dev.handle, &ci, alloc, &mut swapchain);
        if r != vk::Result::SUCCESS {
            return Err(Error::Vk(r));
        }
        log::info(&format!(
            "Wrapped swapchain: present mode {:?} (game asked {:?}), {} images (game asked {})",
            ci.present_mode, info.present_mode, ci.min_image_count, info.min_image_count
        ));
        let hdr = ci.image_format == vk::Format::R16G16B16A16_SFLOAT
            && ci.image_color_space == vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT;
        let mut f = Fixed {
            dev: dev.clone(),
            swapchain,
            images: Vec::new(),
            extent: (ci.image_extent.width, ci.image_extent.height),
            hdr,
            revision: layer.revision(),
            sync: Vec::new(),
            inner: Vec::new(),
            ctx: None,
            failed: false,
            warned: false,
            warned_mode: false,
            stats: std::env::var_os("LSFGM_STATS").map(|_| (0, 0)),
            forced_fifo: profile.override_present_mode,
        };
        let mut setup = || -> Result<(), String> {
            f.images = vkutil::check(
                hook.swapchain.get_swapchain_images(swapchain),
                "vkGetSwapchainImagesKHR",
            )?;
            for _ in 0..f.images.len() {
                f.sync.push(Pass {
                    cb: vkutil::allocate_command_buffer(&hook.device, hook.pool)?,
                    present: vkutil::create_semaphore(&hook.device, false)?,
                });
            }
            f.grow(m.saturating_sub(1) as usize)
        };
        if let Err(e) = setup() {
            if let Some(destroy) = super::dfetch::<vk::PFN_vkDestroySwapchainKHR>(
                dev.gdpa,
                dev.handle,
                c"vkDestroySwapchainKHR",
            ) {
                destroy(dev.handle, swapchain, alloc);
            }
            return Err(e.into());
        }
        if profile.pacing_mode == PacingMode::Adaptive {
            log::info(&format!("Adaptive pacing needs the proxy swapchain (VK_EXT_metal_objects); using the fixed multiplier {m}"));
        }
        Ok(f)
    }

    // inner passes grow with the multiplier and never shrink
    fn grow(&mut self, n: usize) -> Result<(), String> {
        let dev = self.dev.clone();
        let hook = dev.hook.as_ref().expect("hooked device");
        while self.inner.len() < n {
            self.inner.push(Inner {
                cb: vkutil::allocate_command_buffer(&hook.device, hook.pool)?,
                acquire: vkutil::create_semaphore(&hook.device, false)?,
                copy: vkutil::create_semaphore(&hook.device, false)?,
            });
        }
        Ok(())
    }

    // a failed run may leave binary semaphores signalled; fresh ones before generation resumes
    fn reset_semaphores(&mut self) -> Result<(), String> {
        let dev = self.dev.clone();
        let d = &dev.hook.as_ref().expect("hooked device").device;
        let fresh = |old: &mut vk::Semaphore| -> Result<(), String> {
            let new = vkutil::create_semaphore(d, false)?;
            unsafe { d.destroy_semaphore(*old, None) };
            *old = new;
            Ok(())
        };
        for p in &mut self.sync {
            fresh(&mut p.present)?;
        }
        for i in &mut self.inner {
            fresh(&mut i.acquire)?;
            fresh(&mut i.copy)?;
        }
        Ok(())
    }

    fn count(&mut self, original: bool) {
        let Some((o, g)) = &mut self.stats else {
            return;
        };
        if original {
            *o += 1;
            if *o % 60 == 0 {
                log::info(&format!(
                    "Frame generation stats pid={} original={o} generated={g} total={} (successful presents on this swapchain)",
                    std::process::id(),
                    *o + *g
                ));
            }
        } else {
            *g += 1;
        }
    }

    // present preamble: config reload, context (re)creation; returns the multiplier
    fn preamble(&mut self) -> u32 {
        let layer = layer().expect("active layer");
        if let Err(e) = layer.update() {
            log::warn(&format!(
                "Keeping the previous frame-generation profile: {e}"
            ));
        }
        let changed = layer.revision() != self.revision;
        if changed {
            self.revision = layer.revision();
            self.ctx = None;
            if std::mem::take(&mut self.failed) {
                if let Err(e) = self.reset_semaphores() {
                    self.failed = true;
                    log::warn(&format!(
                        "Frame generation disabled; using passthrough: {e}"
                    ));
                }
            }
        }
        let m = layer.multiplier();
        if self.ctx.is_none() && !self.failed && m > 1 {
            let profile = layer.profile();
            let dev = self.dev.clone();
            let hook = dev.hook.as_ref().expect("hooked device");
            let (w, h) = self.extent;
            match hook
                .generator()
                .and_then(|g| context::Wrapper::new(g, &profile, w, h, self.hdr))
            {
                Ok(c) => self.ctx = Some(c),
                Err(e) => {
                    self.failed = true;
                    log::warn(&format!(
                        "Frame generation disabled; using passthrough: {e}"
                    ));
                }
            }
        }
        m
    }

    unsafe fn run(
        &mut self,
        ctx: &mut context::Wrapper,
        m: u32,
        queue: vk::Queue,
        info: &vk::PresentInfoKHR,
        real: vk::PFN_vkQueuePresentKHR,
    ) -> Result<vk::Result, Error> {
        let dev = self.dev.clone();
        let hook = dev.hook.as_ref().expect("hooked device");
        let inserted = m - 1;
        let index = *info.p_image_indices as usize;
        let out_of_range = || Error::from("Present image index out of range");
        let image = *self.images.get(index).ok_or_else(out_of_range)?;
        let waits = if info.wait_semaphore_count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(info.p_wait_semaphores, info.wait_semaphore_count as usize)
        };
        let pass = *self.sync.get(index).ok_or_else(out_of_range)?;
        ctx.dispatch(pass.cb, image, waits, inserted)?;
        let swapchain = self.swapchain;
        let present = |wait: &[vk::Semaphore],
                       idx: u32,
                       p_next: *const std::ffi::c_void|
         -> Result<vk::Result, Error> {
            let swapchains = [swapchain];
            let indices = [idx];
            let mut pi = vk::PresentInfoKHR::default()
                .wait_semaphores(wait)
                .swapchains(&swapchains)
                .image_indices(&indices);
            pi.p_next = p_next;
            let r = real(queue, &pi);
            if ok(r) {
                Ok(r)
            } else {
                Err(Error::Vk(r))
            }
        };
        // generated frames follow the mode the game chose for this present, not the created mode
        let game_mode = if self.forced_fifo { None } else { present_mode_of(info.p_next) };
        if let Some(mode) = game_mode.filter(|_| !self.warned_mode) {
            self.warned_mode = true;
            log::info(&format!("Game selects present mode {mode:?} per present; generated frames follow it"));
        }
        let modes = [game_mode.unwrap_or_default()];
        let inner_info =
            game_mode.map(|_| vk::SwapchainPresentModeInfoEXT::default().present_modes(&modes));
        let inner_next: *const std::ffi::c_void =
            inner_info.as_ref().map_or(std::ptr::null(), |n| std::ptr::from_ref(n).cast());
        for i in 0..inserted as usize {
            let inner = *self.inner.get(i).ok_or("Inner pass missing")?;
            let (idx, _) = hook
                .swapchain
                .acquire_next_image(swapchain, 1_000_000_000, inner.acquire, vk::Fence::null())
                .map_err(|r| match r {
                    vk::Result::TIMEOUT | vk::Result::NOT_READY => Error::Vk(r),
                    _ => "Unable to acquire a generated-frame image".into(),
                })?;
            let mut signals = vec![inner.copy];
            if i + 1 == inserted as usize {
                signals.push(pass.present);
            }
            ctx.acquire(
                inner.cb,
                *self.images.get(idx as usize).ok_or_else(out_of_range)?,
                &[inner.acquire],
                &signals,
                (i + 1) as f32 / m as f32,
            )?;
            let _ = present(&[inner.copy], idx, inner_next)?;
            self.count(false);
        }
        let r = present(&[pass.present], index as u32, info.p_next)?;
        self.count(true);
        if !info.p_results.is_null() {
            *info.p_results = r;
        }
        Ok(r)
    }
}

impl Swapchain for Fixed {
    // multi-swapchain or foreign-queue presents fall through to the driver untouched
    unsafe fn present(
        &mut self,
        queue: vk::Queue,
        info: &vk::PresentInfoKHR,
        real: vk::PFN_vkQueuePresentKHR,
    ) -> Result<vk::Result, Error> {
        let dev = self.dev.clone();
        let hook = dev.hook.as_ref().expect("hooked device");
        let _q = hook.queue_mutex.lock();
        if info.swapchain_count != 1 || queue != hook.queue {
            if !self.warned {
                self.warned = true;
                let why = if info.swapchain_count != 1 {
                    "multi-swapchain present"
                } else {
                    "present on a queue other than the adopted one"
                };
                log::warn(&format!(
                    "Native presentation kept for this swapchain: {why}"
                ));
            }
            let r = real(queue, info);
            if ok(r) {
                self.count(true);
            }
            return Ok(r);
        }
        let m = self.preamble();
        if self.ctx.is_none() {
            return Ok(real(queue, info));
        }
        // a no-op once sized; unconditional so a failed grow is retried instead of indexed past
        self.grow(m.saturating_sub(1) as usize)?;
        let mut ctx = self.ctx.take().expect("context");
        match self.run(&mut ctx, m, queue, info, real) {
            Ok(r) => {
                self.ctx = Some(ctx);
                Ok(r)
            }
            Err(e) => {
                let _ = ctx.idle();
                drop(ctx);
                // a starved inner acquire is transient: the next present rebuilds the context
                self.failed = !matches!(e, Error::Vk(vk::Result::TIMEOUT | vk::Result::NOT_READY));
                Err(e)
            }
        }
    }
}

impl Drop for Fixed {
    fn drop(&mut self) {
        let dev = self.dev.clone();
        let hook = dev.hook.as_ref().expect("hooked device");
        let _q = hook.queue_mutex.lock();
        if let Some(c) = &mut self.ctx {
            let _ = c.idle();
        }
        self.ctx = None;
        unsafe {
            let d = &hook.device;
            for p in self.sync.drain(..) {
                d.destroy_semaphore(p.present, None);
                d.free_command_buffers(hook.pool, &[p.cb]);
            }
            for i in self.inner.drain(..) {
                d.destroy_semaphore(i.acquire, None);
                d.destroy_semaphore(i.copy, None);
                d.free_command_buffers(hook.pool, &[i.cb]);
            }
        }
    }
}
