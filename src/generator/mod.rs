// generator library: instance (own / adopt) and context with the timeline protocol
pub mod native;
pub mod pipeline;
pub mod signature;

use std::ffi::CStr;
use std::path::Path;
use std::sync::Arc;

use ash::{khr, vk};

use crate::shaders;
use crate::vkutil::{self, check};
use pipeline::Pipeline;

pub type LogFn = fn(&str);

pub struct Instance {
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub sync2: khr::synchronization2::Device,
    pub queue: vk::Queue,
    pub family: u32,
    pub fp16: bool,
    pub shaders: shaders::Library,
    pub log: LogFn,
    // vkWaitSemaphores is core 1.2; an adopted 1.1 device only has the khr alias, or neither
    pub timeline_wait: bool,
    pub timeline_khr: Option<khr::timeline_semaphore::Device>,
    owned: bool,
    leak: bool,
    _driver: Option<libloading::Library>,
}

impl Instance {
    // own constructor: private instance and device on the driver dylib at `driver`
    pub fn own(
        driver: &Path,
        device_id: &str,
        dll: &Path,
        allow_fp16: bool,
        leak: bool,
        log: LogFn,
    ) -> Result<Instance, String> {
        let (lib, entry) = vkutil::load_driver(driver)?;
        let instance = vkutil::create_instance(&entry, c"lsfg-metal", vk::API_VERSION_1_2, true)?;
        let (pd, device, family, fp16) =
            match Self::own_device(&entry, &instance, device_id, allow_fp16) {
                Ok(x) => x,
                Err(e) => {
                    unsafe { instance.destroy_instance(None) };
                    return Err(e);
                }
            };
        let shaders = match shaders::Library::load(&device, fp16, dll, log) {
            Ok(s) => s,
            Err(e) => {
                unsafe {
                    device.destroy_device(None);
                    instance.destroy_instance(None);
                }
                return Err(e);
            }
        };
        Ok(Self::assemble(
            instance,
            pd,
            device,
            family,
            fp16,
            shaders,
            log,
            true,
            leak,
            Some(lib),
        ))
    }

    fn own_device(
        entry: &ash::Entry,
        instance: &ash::Instance,
        device_id: &str,
        allow_fp16: bool,
    ) -> Result<(vk::PhysicalDevice, ash::Device, u32, bool), String> {
        let pd = vkutil::select_physical_device(instance, device_id)?;
        let gipa = entry.static_fn().get_instance_proc_addr;
        vkutil::check_driver(gipa, instance.handle(), pd)?;
        let family = vkutil::find_queue_family(instance, pd, vk::QueueFlags::COMPUTE, false)?;
        let fp16 = allow_fp16 && vkutil::half_precision_supported(instance, pd);
        let mut s2 = vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
            .shader_float16(fp16)
            .timeline_semaphore(true);
        let prio = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&prio)];
        let exts = [khr::synchronization2::NAME.as_ptr()];
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&exts)
            .push_next(&mut s2)
            .push_next(&mut f12);
        let device = check(
            unsafe { instance.create_device(pd, &info, None) },
            "vkCreateDevice",
        )?;
        Ok((pd, device, family, fp16))
    }

    // adopt constructor: borrows live handles (they must outlive the instance); destroys nothing but its shader modules
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        gipa: vk::PFN_vkGetInstanceProcAddr,
        instance: vk::Instance,
        physical_device: vk::PhysicalDevice,
        device: vk::Device,
        family: u32,
        fp16: bool,
        dll: &Path,
        log: LogFn,
    ) -> Result<Instance, String> {
        let instance = unsafe {
            ash::Instance::load(
                &ash::StaticFn {
                    get_instance_proc_addr: gipa,
                },
                instance,
            )
        };
        vkutil::check_driver(gipa, instance.handle(), physical_device)?;
        let device = unsafe { ash::Device::load(instance.fp_v1_0(), device) };
        // ash loads only the khr name and installs a panicking stub when it is missing, so fail early instead
        let barrier2 = unsafe {
            instance.get_device_proc_addr(device.handle(), c"vkCmdPipelineBarrier2KHR".as_ptr())
        };
        if barrier2.is_none() {
            return Err("vkCmdPipelineBarrier2KHR is not available on the adopted device (VK_KHR_synchronization2 not enabled)".into());
        }
        let shaders = shaders::Library::load(&device, fp16, dll, log)?;
        Ok(Self::assemble(
            instance,
            physical_device,
            device,
            family,
            fp16,
            shaders,
            log,
            false,
            false,
            None,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        instance: ash::Instance,
        physical_device: vk::PhysicalDevice,
        device: ash::Device,
        family: u32,
        fp16: bool,
        shaders: shaders::Library,
        log: LogFn,
        owned: bool,
        leak: bool,
        driver: Option<libloading::Library>,
    ) -> Instance {
        let sync2 = khr::synchronization2::Device::new(&instance, &device);
        let queue = unsafe { device.get_device_queue(family, 0) };
        // ash installs a panicking stub for an entry point the driver does not have, so probe it
        let probe = |name: &CStr| unsafe {
            instance
                .get_device_proc_addr(device.handle(), name.as_ptr())
                .is_some()
        };
        let timeline_wait = probe(c"vkWaitSemaphores");
        let timeline_khr = (!timeline_wait && probe(c"vkWaitSemaphoresKHR"))
            .then(|| khr::timeline_semaphore::Device::new(&instance, &device));
        Instance {
            instance,
            physical_device,
            device,
            sync2,
            queue,
            family,
            fp16,
            shaders,
            log,
            timeline_wait,
            timeline_khr,
            owned,
            leak,
            _driver: driver,
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.shaders.destroy(&self.device);
        if self.owned && !self.leak {
            unsafe {
                self.device.destroy_device(None);
                self.instance.destroy_instance(None);
            }
        }
    }
}

// one generation context for one size / flow / mode
pub struct Context {
    inst: Arc<Instance>,
    pub sync: vk::Semaphore,
    internal: vk::Semaphore,
    fence: vk::Fence,
    first: bool,
    iteration: u32,
    total: u32,
    index: u32,
    sync_value: u64,
    internal_value: u64,
    fence_pending: bool,
    unsignaled: bool,
    pub pipeline: Pipeline,
}

impl Context {
    pub fn new(
        inst: Arc<Instance>,
        w: u32,
        h: u32,
        flow: f32,
        perf: bool,
        hdr: bool,
    ) -> Result<Context, String> {
        let pipeline = Pipeline::new(inst.clone(), w, h, flow, perf, hdr)?;
        let d = &inst.device;
        let (sync, internal, fence) = (
            vkutil::create_semaphore(d, true)?,
            vkutil::create_semaphore(d, true)?,
            vkutil::create_fence(d)?,
        );
        Ok(Context {
            inst,
            sync,
            internal,
            fence,
            first: true,
            iteration: 0,
            total: 0,
            index: 0,
            sync_value: 0,
            internal_value: 0,
            fence_pending: false,
            unsignaled: false,
            pipeline,
        })
    }

    // (source image with 2 layers, destination image, sync semaphore)
    pub fn handles(&self) -> (vk::Image, vk::Image, vk::Semaphore) {
        (self.pipeline.source, self.pipeline.destination, self.sync)
    }

    fn submit(
        &self,
        cbs: &[vk::CommandBuffer],
        wait: Option<(vk::Semaphore, u64)>,
        signal: Option<(vk::Semaphore, u64)>,
        fence: vk::Fence,
    ) -> Result<(), String> {
        let (ws, wv): (Vec<_>, Vec<_>) = wait.into_iter().unzip();
        let (ss, sv): (Vec<_>, Vec<_>) = signal.into_iter().unzip();
        let stages = vec![vk::PipelineStageFlags::TOP_OF_PIPE; ws.len()];
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
            unsafe { self.inst.device.queue_submit(self.inst.queue, &info, fence) },
            "vkQueueSubmit",
        )
    }

    // wait for everything submitted so far: the completion fence when pending, else the whole queue
    // a partial iteration never submits the fence, and the queue may be the game's own
    fn settle(&mut self, what: &str) -> Result<(), String> {
        let d = &self.inst.device;
        if self.fence_pending {
            unsafe { d.wait_for_fences(&[self.fence], true, u64::MAX) }
                .map_err(|_| format!("Unable to wait for completion of {what} iteration"))?;
            check(unsafe { d.reset_fences(&[self.fence]) }, "vkResetFences")?;
            self.fence_pending = false;
        } else if !self.first {
            // the pre-pass signals internal and a signalled acquire signals sync; unsignaled has neither
            let can_wait = (self.inst.timeline_wait || self.inst.timeline_khr.is_some())
                && !(self.unsignaled && self.index > 0);
            if can_wait {
                let mut sems = vec![self.internal];
                let mut vals = vec![self.internal_value];
                if self.index > 0 && !self.unsignaled {
                    sems.push(self.sync);
                    vals.push(self.sync_value);
                }
                let info = vk::SemaphoreWaitInfo::default()
                    .semaphores(&sems)
                    .values(&vals);
                let r = match &self.inst.timeline_khr {
                    Some(t) => unsafe { t.wait_semaphores(&info, u64::MAX) },
                    None => unsafe { d.wait_semaphores(&info, u64::MAX) },
                };
                check(r, "vkWaitSemaphores")?;
            } else {
                check(
                    unsafe { d.queue_wait_idle(self.inst.queue) },
                    "vkQueueWaitIdle",
                )?;
            }
        }
        Ok(())
    }

    // start an iteration with `total` frames to generate
    pub fn dispatch(&mut self, total: u32, unsignaled: bool) -> Result<(), String> {
        if self.first {
            self.first = false;
            self.iteration = 0;
        } else {
            self.settle("previous")?;
            self.iteration += 1;
            // a partial signaled iteration left the caller's pre-signal for the next acquire unconsumed
            if self.index > 0 && self.index < self.total {
                self.sync_value += 1;
            }
        }
        self.pipeline.set_iteration(self.iteration);
        self.sync_value += 1;
        self.internal_value += 1;
        let wait = (!unsignaled).then_some((self.sync, self.sync_value));
        let fence = if total == 0 {
            self.fence
        } else {
            vk::Fence::null()
        };
        self.submit(
            &[self.pipeline.cmd[0]],
            wait,
            Some((self.internal, self.internal_value)),
            fence,
        )?;
        self.fence_pending = total == 0;
        self.unsignaled = unsignaled;
        self.total = total;
        self.index = 0;
        Ok(())
    }

    // produce one generated frame
    pub fn acquire(&mut self, unsignaled: bool, timestamp: f32) -> Result<(), String> {
        if self.total == 0 || self.index >= self.total {
            return Err("All generated frames already acquired".into());
        }
        let t = self
            .pipeline
            .param_update(self.index, self.total, timestamp)?;
        let wait = if self.index == 0 {
            Some((self.internal, self.internal_value))
        } else if !unsignaled {
            self.sync_value += 1;
            Some((self.sync, self.sync_value))
        } else {
            None
        };
        self.sync_value += 1;
        let last = self.index == self.total - 1;
        let signal = (!unsignaled).then_some((self.sync, self.sync_value));
        let fence = if last { self.fence } else { vk::Fence::null() };
        // the parameter update and the main pass go in one batch rather than two with a semaphore hop;
        // the param buffer already ends with a transfer -> compute barrier, so one batch on the in-order queue is equivalent
        self.submit(&[t, self.pipeline.cmd[1]], wait, signal, fence)?;
        self.fence_pending |= last;
        self.index += 1;
        Ok(())
    }

    // only our own submits: a caller sharing this queue drains its own before dropping us
    pub fn idle(&mut self) -> Result<(), String> {
        self.settle("current")
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let _ = self.idle();
        unsafe {
            self.inst.device.destroy_semaphore(self.sync, None);
            self.inst.device.destroy_semaphore(self.internal, None);
            self.inst.device.destroy_fence(self.fence, None);
        }
    }
}
