// validate: own instance on a real driver, one context, signaled iterations with synthetic input, timings
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use ash::vk;
use lsfg_metal::generator::{Context, Instance};
use lsfg_metal::vkutil::{self, check};
use lsfg_metal::{log, shaders};

fn relog(m: &str) {
    log::debug(m)
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn run() -> Result<(), String> {
    let (mut w, mut h, mut m, mut flow, mut iters) = (1920u32, 1080u32, 2u32, 1.0f32, 10u32);
    let (mut perf, mut fp16, mut hdr, mut partial) = (false, true, false, false);
    let mut driver = env("LSFGM_MOLTENVK").map(PathBuf::from);
    let mut dll = env("LSFGM_DLL_PATH")
        .map(PathBuf::from)
        .or_else(shaders::find_dll);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--driver" => driver = Some(PathBuf::from(val()?)),
            "--dll" => dll = Some(PathBuf::from(val()?)),
            "-w" => w = val()?.parse().map_err(|_| "bad width")?,
            "-h" => h = val()?.parse().map_err(|_| "bad height")?,
            "-m" => m = val()?.parse().map_err(|_| "bad multiplier")?,
            "-f" => flow = val()?.parse().map_err(|_| "bad flow")?,
            "-n" => iters = val()?.parse().map_err(|_| "bad iteration count")?,
            "-p" => perf = true,
            "--no-fp16" => fp16 = false,
            "--hdr" => hdr = true,
            "--partial" => partial = true,
            _ => return Err("usage: validate [--driver dylib] [--dll lsfg-vk.dll] [-w W] [-h H] [-m M] [-f flow] [-n iterations] [-p] [--no-fp16] [--hdr] [--partial]".into()),
        }
    }
    if !(2..=4).contains(&m) || !(0.25..=1.0).contains(&flow) || w == 0 || h == 0 {
        return Err("multiplier must be 2..4, flow 0.25..1.0, size positive".into());
    }
    let driver = driver.ok_or("no driver: pass --driver or set LSFGM_MOLTENVK")?;
    let dll = dll.ok_or("no dll: pass --dll or set LSFGM_DLL_PATH")?;

    let t0 = Instant::now();
    let inst = Arc::new(Instance::own(&driver, "", &dll, fp16, false, relog)?);
    let mut ctx = Context::new(inst.clone(), w, h, flow, perf, hdr)?;
    let build = t0.elapsed();
    if partial {
        // regression: a partial iteration never submits the completion fence; the next dispatch and the drop must still return
        ctx.dispatch(2, true)?;
        ctx.acquire(true, 0.0)?;
        ctx.dispatch(2, true)?;
        drop(ctx);
        // the same thing on the signalled protocol, where settle waits on the timelines instead
        let mut ctx = Context::new(inst.clone(), w, h, flow, perf, hdr)?;
        let sync = ctx.handles().2;
        let d = &inst.device;
        // the owned device is 1.2, so the branch this test exists for must be the one taken
        assert!(
            inst.timeline_wait,
            "vkWaitSemaphores missing on the owned device"
        );
        let signal = |wait: Option<u64>, value: u64| -> Result<(), String> {
            let (ws, wv): (Vec<_>, Vec<_>) = wait.map(|v| (sync, v)).into_iter().unzip();
            let stages = vec![vk::PipelineStageFlags::TOP_OF_PIPE; ws.len()];
            let mut tl = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&wv)
                .signal_semaphore_values(std::slice::from_ref(&value));
            let info = [vk::SubmitInfo::default()
                .wait_semaphores(&ws)
                .wait_dst_stage_mask(&stages)
                .signal_semaphores(std::slice::from_ref(&sync))
                .push_next(&mut tl)];
            check(
                unsafe { d.queue_submit(inst.queue, &info, vk::Fence::null()) },
                "vkQueueSubmit",
            )
        };
        signal(None, 1)?;
        ctx.dispatch(2, false)?;
        ctx.acquire(false, 0.0)?;
        // the caller signals ahead of the pre-pass that waits on it: a blocked submit stalls the queue
        signal(Some(2), 3)?;
        signal(Some(3), 4)?;
        // one acquire short of the two dispatched, so the next dispatch settles without a fence
        ctx.dispatch(2, false)?;
        ctx.idle()?;
        check(unsafe { d.queue_wait_idle(inst.queue) }, "vkQueueWaitIdle")?;
        drop(ctx);
        println!(
            "partial iteration regression: ok, unsignaled and signalled (build {:.0} ms)",
            build.as_secs_f64() * 1e3
        );
        return Ok(());
    }
    let (src, dst, sync) = ctx.handles();
    let d = &inst.device;
    let pool = vkutil::create_command_pool(d, inst.family, false)?;
    let mut cbs = vec![];
    // caller side of the context timeline protocol: waits/signals on the sync semaphore, one in-order queue
    let submit = |cb: &[vk::CommandBuffer],
                  wait: Option<u64>,
                  signal: Option<u64>,
                  fence: vk::Fence|
     -> Result<(), String> {
        let (ws, wv): (Vec<_>, Vec<_>) = wait.map(|v| (sync, v)).into_iter().unzip();
        let (ss, sv): (Vec<_>, Vec<_>) = signal.map(|v| (sync, v)).into_iter().unzip();
        let stages = vec![vk::PipelineStageFlags::TOP_OF_PIPE; ws.len()];
        let mut tl = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&wv)
            .signal_semaphore_values(&sv);
        let info = [vk::SubmitInfo::default()
            .wait_semaphores(&ws)
            .wait_dst_stage_mask(&stages)
            .command_buffers(cb)
            .signal_semaphores(&ss)
            .push_next(&mut tl)];
        check(
            unsafe { d.queue_submit(inst.queue, &info, fence) },
            "vkQueueSubmit",
        )
    };

    let mut s = 0u64;
    let t1 = Instant::now();
    for it in 0..iters {
        // synthetic input: a solid colour that swings between red and blue, written into layer iteration % 2
        let cb = vkutil::allocate_command_buffer(d, pool)?;
        cbs.push(cb);
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        let x = (it % 2) as f32;
        let color = vk::ClearColorValue {
            float32: [x, 0.25, 1.0 - x, 1.0],
        };
        unsafe {
            d.cmd_clear_color_image(
                cb,
                src,
                vk::ImageLayout::GENERAL,
                &color,
                &[vkutil::color_range(it % 2, 1)],
            );
            check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
        }
        // the previous iteration's last main pass signalled s; do not overwrite its source layer before it is done
        submit(&[cb], (it > 0).then_some(s), Some(s + 1), vk::Fence::null())?;
        ctx.dispatch(m - 1, false)?;
        for k in 0..(m - 1) as u64 {
            ctx.acquire(false, 0.0)?;
            let next = (k + 2 < m as u64).then_some(s + 3 + 2 * k);
            submit(&[], Some(s + 2 + 2 * k), next, vk::Fence::null())?;
        }
        s += 2 * (m as u64 - 1);
    }
    ctx.idle()?;
    check(unsafe { d.queue_wait_idle(inst.queue) }, "vkQueueWaitIdle")?;
    let run = t1.elapsed();
    // the sync semaphore advances by 2(m-1) per iteration
    let value = check(
        unsafe { d.get_semaphore_counter_value(sync) },
        "vkGetSemaphoreCounterValue",
    )?;
    if value != s {
        return Err(format!("sync semaphore reads {value}, expected {s}"));
    }

    // read the centre pixel of the last generated frame back
    let bpp = if hdr { 8 } else { 4 };
    let usage = vk::BufferUsageFlags::TRANSFER_DST;
    let (buf, mem) = vkutil::create_buffer(
        &inst.instance,
        inst.physical_device,
        d,
        usage,
        &vec![0u8; w as usize * h as usize * bpp as usize],
    )?;
    let cb = vkutil::allocate_command_buffer(d, pool)?;
    cbs.push(cb);
    vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
    let region = vk::BufferImageCopy {
        image_subresource: vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        image_extent: vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        },
        ..Default::default()
    };
    let fence = vkutil::create_fence(d)?;
    let pixel = unsafe {
        d.cmd_copy_image_to_buffer(cb, dst, vk::ImageLayout::GENERAL, buf, &[region]);
        check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
        submit(&[cb], None, None, fence)?;
        check(
            d.wait_for_fences(&[fence], true, u64::MAX),
            "vkWaitForFences",
        )?;
        let p = check(
            d.map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()),
            "vkMapMemory",
        )? as *const u8;
        let off = (h as usize / 2 * w as usize + w as usize / 2) * bpp as usize;
        let px = std::slice::from_raw_parts(p.add(off), bpp as usize).to_vec();
        d.unmap_memory(mem);
        d.destroy_fence(fence, None);
        d.destroy_buffer(buf, None);
        d.free_memory(mem, None);
        d.destroy_command_pool(pool, None);
        px
    };
    let frames = iters * (m - 1);
    println!(
        "{w}x{h} m={m} flow={flow:.2} {} fp16={} hdr={hdr}: build {:.0} ms, {iters} iterations / {frames} generated frames in {:.1} ms = {:.2} ms per iteration, {:.2} ms per generated frame, centre pixel {:02x?}",
        if perf { "performance" } else { "quality" },
        inst.fp16,
        build.as_secs_f64() * 1e3,
        run.as_secs_f64() * 1e3,
        run.as_secs_f64() * 1e3 / iters as f64,
        run.as_secs_f64() * 1e3 / frames as f64,
        pixel
    );
    drop(ctx);
    Ok(())
}

fn main() {
    log::set_level(if env("LSFGM_LOG_LEVEL").is_some_and(|l| l.eq_ignore_ascii_case("debug")) {
        log::Level::Debug
    } else {
        log::Level::Info
    });
    if let Err(e) = run() {
        eprintln!("validate: {e}");
        std::process::exit(1);
    }
}
