// context wrapper: copies in and out of the generator context on the game's own queue
use std::sync::Arc;

use ash::vk;

use crate::generator;
use crate::settings::Profile;
use crate::vkutil::{self, check};

pub struct Wrapper {
    ctx: generator::Context,
    dev: ash::Device,
    queue: vk::Queue,
    source: vk::Image,
    dest: vk::Image,
    sync: vk::Semaphore,
    fence: vk::Fence,
    extent: (u32, u32),
    iteration: u32,
    remaining: u32,
    sync_counter: u64,
    in_flight: bool,
    generated: bool,
}

impl Wrapper {
    // builds the generator context and caches its image and semaphore handles
    pub fn new(
        inst: Arc<generator::Instance>,
        profile: &Profile,
        w: u32,
        h: u32,
        hdr: bool,
    ) -> Result<Wrapper, String> {
        let ctx = generator::Context::new(
            inst.clone(),
            w,
            h,
            profile.flow_scale,
            profile.performance_mode,
            hdr,
        )?;
        let (source, dest, sync) = ctx.handles();
        let fence = vkutil::create_fence(&inst.device)?;
        Ok(Wrapper {
            ctx,
            dev: inst.device.clone(),
            queue: inst.queue,
            source,
            dest,
            sync,
            fence,
            extent: (w, h),
            iteration: 0,
            remaining: 0,
            sync_counter: 0,
            in_flight: false,
            generated: false,
        })
    }

    fn submit(
        &self,
        cb: vk::CommandBuffer,
        waits: &[vk::Semaphore],
        wait_values: &[u64],
        signals: &[vk::Semaphore],
        signal_values: &[u64],
        fence: vk::Fence,
    ) -> Result<(), String> {
        let stages = vec![vk::PipelineStageFlags::TOP_OF_PIPE; waits.len()];
        let mut tl = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(wait_values)
            .signal_semaphore_values(signal_values);
        let cbs = [cb];
        let info = vk::SubmitInfo::default()
            .wait_semaphores(waits)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cbs)
            .signal_semaphores(signals)
            .push_next(&mut tl);
        check(
            unsafe { self.dev.queue_submit(self.queue, &[info], fence) },
            "vkQueueSubmit",
        )
    }

    fn barrier(
        &self,
        cb: vk::CommandBuffer,
        src: vk::PipelineStageFlags,
        dst: vk::PipelineStageFlags,
        barriers: &[vk::ImageMemoryBarrier],
    ) {
        unsafe {
            self.dev.cmd_pipeline_barrier(
                cb,
                src,
                dst,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                barriers,
            )
        }
    }

    // copy the game's frame into the source layer and start the pre-pass
    pub fn dispatch(
        &mut self,
        cb: vk::CommandBuffer,
        image: vk::Image,
        waits: &[vk::Semaphore],
        inserted: u32,
    ) -> Result<(), String> {
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
        let d = &self.dev;
        if self.iteration != 0 {
            unsafe { d.wait_for_fences(&[self.fence], true, u64::MAX) }
                .map_err(|_| "Frame-generation copy did not complete")?;
            check(unsafe { d.reset_fences(&[self.fence]) }, "vkResetFences")?;
        }
        let layer = self.iteration % 2;
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        self.barrier(
            cb,
            P::TOP_OF_PIPE,
            P::TRANSFER,
            &[
                vkutil::image_barrier(
                    A::NONE,
                    A::TRANSFER_READ,
                    L::PRESENT_SRC_KHR,
                    L::GENERAL,
                    image,
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
            ],
        );
        vkutil::cmd_blit(d, cb, image, 0, self.source, layer, self.extent);
        self.barrier(
            cb,
            P::TRANSFER,
            P::BOTTOM_OF_PIPE,
            &[vkutil::image_barrier(
                A::TRANSFER_READ,
                A::NONE,
                L::GENERAL,
                L::PRESENT_SRC_KHR,
                image,
                0,
                1,
            )],
        );
        check(unsafe { d.end_command_buffer(cb) }, "vkEndCommandBuffer")?;
        let values = vec![0u64; waits.len()];
        let fence = if inserted == 0 {
            self.fence
        } else {
            vk::Fence::null()
        };
        self.submit(
            cb,
            waits,
            &values,
            &[self.sync],
            &[self.sync_counter + 1],
            fence,
        )?;
        self.in_flight = true;
        self.ctx.dispatch(inserted, false)?;
        self.remaining = inserted;
        self.sync_counter += 1;
        self.iteration += 1;
        Ok(())
    }

    // main pass first (single in-order queue), then the copy out that waits on it
    pub fn acquire(
        &mut self,
        cb: vk::CommandBuffer,
        target: vk::Image,
        waits: &[vk::Semaphore],
        signals: &[vk::Semaphore],
        timestamp: f32,
    ) -> Result<(), String> {
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
        let d = &self.dev;
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        self.barrier(
            cb,
            P::TOP_OF_PIPE,
            P::TRANSFER,
            &[
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
            ],
        );
        vkutil::cmd_blit(d, cb, self.dest, 0, target, 0, self.extent);
        self.barrier(
            cb,
            P::TRANSFER,
            P::BOTTOM_OF_PIPE,
            &[vkutil::image_barrier(
                A::TRANSFER_WRITE,
                A::MEMORY_READ,
                L::GENERAL,
                L::PRESENT_SRC_KHR,
                target,
                0,
                1,
            )],
        );
        check(unsafe { d.end_command_buffer(cb) }, "vkEndCommandBuffer")?;
        let mut ws = vec![self.sync];
        ws.extend_from_slice(waits);
        let mut wv = vec![self.sync_counter + 1];
        wv.resize(ws.len(), 0);
        let (mut ss, mut sv) = (Vec::new(), Vec::new());
        if self.remaining > 1 {
            ss.push(self.sync);
            sv.push(self.sync_counter + 2);
        }
        ss.extend_from_slice(signals);
        sv.resize(ss.len(), 0);
        if !self.generated {
            self.ctx.acquire(false, timestamp)?;
        }
        self.generated = false;
        let fence = if self.remaining == 1 {
            self.fence
        } else {
            vk::Fence::null()
        };
        self.submit(cb, &ws, &wv, &ss, &sv, fence)?;
        self.remaining -= 1;
        self.sync_counter += 1;
        if self.remaining > 0 {
            self.sync_counter += 1;
        }
        Ok(())
    }

    // macos idle: drain the adopted queue, partial iterations included
    pub fn idle(&mut self) -> Result<(), String> {
        if self.in_flight {
            self.in_flight = false;
            check(
                unsafe { self.dev.queue_wait_idle(self.queue) },
                "vkQueueWaitIdle",
            )?;
        }
        Ok(())
    }
}

impl Drop for Wrapper {
    fn drop(&mut self) {
        let _ = self.idle();
        unsafe { self.dev.destroy_fence(self.fence, None) };
    }
}
