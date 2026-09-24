// validate: own instance on a real driver, one context, signaled iterations with synthetic input, timings
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use ash::vk;
use lsfg_metal::generator::signature::Colour;
use lsfg_metal::generator::{Context, Instance};
use lsfg_metal::vkutil::{self, check};
use lsfg_metal::{log, shaders};

fn relog(m: &str) {
    log::debug(m)
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

// binary ppm in, rgba8 out; enough of the format for the files the metal dump writes
fn read_ppm(path: &std::path::Path) -> Result<(u32, u32, Vec<u8>), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 && i < bytes.len() {
        match bytes[i] {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1
                }
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                fields.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            }
        }
    }
    let num = |n: usize| -> Result<u32, String> {
        fields[n]
            .parse()
            .map_err(|_| format!("{}: bad header", path.display()))
    };
    if fields.len() != 4 || fields[0] != "P6" {
        return Err(format!("{}: not a binary ppm", path.display()));
    }
    let (w, h) = (num(1)?, num(2)?);
    if num(3)? != 255 {
        return Err(format!("{}: only 8-bit ppm is supported", path.display()));
    }
    let px = bytes.get(i + 1..).unwrap_or_default();
    let want = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| format!("{}: implausible size", path.display()))?;
    if px.len() < want {
        return Err(format!(
            "{}: {} pixel bytes, expected {want}",
            path.display(),
            px.len()
        ));
    }
    let mut rgba = Vec::with_capacity(want / 3 * 4);
    for p in px[..want].chunks_exact(3) {
        rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
    }
    Ok((w, h, rgba))
}

fn write_ppm(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let mut out = format!("P6 {w} {h} 255\n").into_bytes();
    for p in rgba.chunks_exact(4) {
        out.extend_from_slice(&p[..3]);
    }
    std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

struct Run {
    w: u32,
    h: u32,
    m: u32,
    flow: f32,
    iters: u32,
    perf: bool,
    fp16: bool,
    hdr: bool,
}

// the same run on the native metal generator: one command buffer per iteration, one iteration in flight; there is no caller sync to drop, so --bench changes nothing here
fn run_native(
    r: &Run,
    res: &std::collections::HashMap<u32, Vec<u32>>,
    frames: Option<&[Vec<u8>; 2]>,
    out: Option<&std::path::Path>,
) -> Result<(), String> {
    use lsfg_metal::generator::native::Pipeline;
    use objc2_metal::{
        MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
        MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLResourceOptions, MTLSize,
        MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    };
    let (w, h, m) = (r.w, r.h, r.m);
    let device = MTLCreateSystemDefaultDevice().ok_or("no Metal device")?;
    let queue = device.newCommandQueue().ok_or("no Metal queue")?;
    let t0 = Instant::now();
    let mut p = Pipeline::new(&device, res, r.fp16, (w, h), r.flow, r.perf, if r.hdr { Colour::HDR } else { Colour::SDR }, relog)?;
    let build = t0.elapsed();
    let bpp = if r.hdr { 8 } else { 4 };
    let len = w as usize * h as usize * bpp;
    let buffer = |bytes: &[u8]| {
        let b = device
            .newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared)
            .ok_or("no Metal buffer")?;
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), b.contents().as_ptr() as *mut u8, bytes.len()) };
        Ok::<_, String>(b)
    };
    // synthetic input as in the vulkan run: red and blue swinging, green 0.25
    let solid = |x: usize| -> Vec<u8> {
        let px: Vec<u8> = if r.hdr {
            let one = |v: bool| if v { 0x3c00u16 } else { 0 };
            [one(x == 1), 0x3400, one(x == 0), 0x3c00].iter().flat_map(|v| v.to_le_bytes()).collect()
        } else {
            vec![255 * x as u8, 64, 255 * (1 - x) as u8, 255]
        };
        px.repeat(w as usize * h as usize)
    };
    // the frames go in and out through the shim's own copies, on textures standing in for the game's drawables
    let size = MTLSize { width: w as usize, height: h as usize, depth: 1 };
    let origin = MTLOrigin { x: 0, y: 0, z: 0 };
    let format = if r.hdr { MTLPixelFormat::RGBA16Float } else { MTLPixelFormat::RGBA8Unorm };
    let texture = |bytes: &[u8]| {
        let d = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, w as usize, h as usize, false)
        };
        d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
        d.setStorageMode(MTLStorageMode::Shared);
        let t = device.newTextureWithDescriptor(&d).ok_or("no Metal texture")?;
        if !bytes.is_empty() {
            let region = MTLRegion { origin, size };
            let px = std::ptr::NonNull::new(bytes.as_ptr() as *mut _).ok_or("empty frame")?;
            unsafe { t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, px, w as usize * bpp) };
        }
        Ok::<_, String>(t)
    };
    let inputs = match frames {
        Some(f) => [texture(&f[0])?, texture(&f[1])?],
        None => [texture(&solid(0))?, texture(&solid(1))?],
    };
    let targets: Vec<_> = (0..m - 1).map(|_| texture(&[])).collect::<Result<_, _>>()?;
    let reads: Vec<_> = (0..m - 1).map(|_| buffer(&[])).collect::<Result<_, _>>()?;
    let (mut last, mut ts, mut gpu): (Option<objc2::rc::Retained<_>>, f32, f64) = (None, 0.0, 0.0);
    let t1 = Instant::now();
    for it in 0..r.iters {
        let cb = queue.commandBuffer().ok_or("no Metal command buffer")?;
        p.copy_in(&cb, &inputs[it as usize % 2], it % 2)?;
        p.encode(&cb, false, it, ts)?;
        for k in 0..m - 1 {
            ts = (k + 1) as f32 / m as f32;
            p.encode(&cb, true, it, ts)?;
            if out.is_some() && it + 1 == r.iters {
                let target = &targets[k as usize];
                p.copy_out(&cb, target)?;
                let blit = cb.blitCommandEncoder().ok_or("no blit encoder")?;
                unsafe {
                    blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                        target, 0, 0, origin, size, &reads[k as usize], 0, w as usize * bpp, len,
                    )
                };
                blit.endEncoding();
            }
        }
        // the vulkan context settles the previous iteration before it starts the next
        if let Some(prev) = last.take() {
            let prev: &objc2::runtime::ProtocolObject<dyn MTLCommandBuffer> = &prev;
            prev.waitUntilCompleted();
            gpu += prev.GPUEndTime() - prev.GPUStartTime();
        }
        cb.commit();
        last = Some(cb);
    }
    if let Some(prev) = last {
        prev.waitUntilCompleted();
        gpu += prev.GPUEndTime() - prev.GPUStartTime();
    }
    let run = t1.elapsed();
    if let Some(dir) = out {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for (k, b) in reads.iter().enumerate() {
            let px = unsafe { std::slice::from_raw_parts(b.contents().as_ptr() as *const u8, len) };
            write_ppm(&dir.join(format!("generated_{k}.ppm")), w, h, px)?;
        }
    }
    let cb = queue.commandBuffer().ok_or("no Metal command buffer")?;
    let blit = cb.blitCommandEncoder().ok_or("no blit encoder")?;
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            p.destination(), 0, 0, origin, size, &reads[0], 0, w as usize * bpp, len,
        )
    };
    blit.endEncoding();
    cb.commit();
    cb.waitUntilCompleted();
    let off = (h as usize / 2 * w as usize + w as usize / 2) * bpp;
    let pixel = unsafe { std::slice::from_raw_parts((reads[0].contents().as_ptr() as *const u8).add(off), bpp) };
    let frames = r.iters * (m - 1);
    println!(
        "{w}x{h} m={m} flow={:.2} {} fp16={} hdr={} native: build {:.0} ms, {} iterations / {frames} generated frames in {:.1} ms = {:.2} ms per iteration, {:.2} ms per generated frame, gpu {:.2} ms per iteration, centre pixel {:02x?}",
        r.flow,
        if r.perf { "performance" } else { "quality" },
        r.fp16,
        r.hdr,
        build.as_secs_f64() * 1e3,
        r.iters,
        run.as_secs_f64() * 1e3,
        run.as_secs_f64() * 1e3 / r.iters as f64,
        run.as_secs_f64() * 1e3 / frames as f64,
        gpu * 1e3 / r.iters as f64,
        pixel
    );
    Ok(())
}

fn run() -> Result<(), String> {
    let (mut w, mut h, mut m, mut flow, mut iters) = (1920u32, 1080u32, 2u32, 1.0f32, 10u32);
    let (mut perf, mut fp16, mut hdr, mut partial) = (false, true, false, false);
    let (mut bench, mut native, mut input, mut out) = (false, false, None, None);
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
            "--bench" => bench = true,
            "--native" => native = true,
            "--in" => input = Some((PathBuf::from(val()?), PathBuf::from(val()?))),
            "--out" => out = Some(PathBuf::from(val()?)),
            _ => return Err("usage: validate [--driver dylib] [--dll lsfg-vk.dll] [-w W] [-h H] [-m M] [-f flow] [-n iterations] [-p] [--no-fp16] [--hdr] [--partial] [--bench] [--native] [--in a.ppm b.ppm] [--out dir]".into()),
        }
    }
    if !(2..=4).contains(&m) || !(0.25..=1.0).contains(&flow) {
        return Err("multiplier must be 2..4, flow 0.25..1.0".into());
    }
    // two real frames in, generated frames out: same driver, same timeline, file input and output
    let frames = input
        .map(|(a, b)| -> Result<_, String> {
            if hdr {
                return Err("--in reads 8-bit ppm, which is not the hdr format".into());
            }
            let (a, b) = (read_ppm(&a)?, read_ppm(&b)?);
            if (a.0, a.1) != (b.0, b.1) {
                return Err("the two input frames differ in size".into());
            }
            Ok(((a.0, a.1), [a.2, b.2]))
        })
        .transpose()?;
    if let Some(((fw, fh), _)) = &frames {
        (w, h) = (*fw, *fh);
        iters = 2;
    } else if out.is_some() {
        return Err("--out needs --in".into());
    }
    if bench && out.is_some() {
        // in bench mode nothing waits on the generator, so the readback would catch a half-written frame
        return Err("--bench writes no usable frames, so it cannot be combined with --out".into());
    }
    if w == 0 || h == 0 {
        return Err("size must be positive".into());
    }
    if iters == 0 {
        return Err("-n must be at least 1".into());
    }
    // the shim shows such frames without generation, so there is nothing to validate
    if !lsfg_metal::generator::signature::Signature::new(perf).fits(w, h, flow) {
        return Err(format!("{w}x{h} at flow {flow} is below the 64 pixel minimum after flow scaling"));
    }
    if native && partial {
        return Err("--partial tests the Vulkan context; it does not apply to --native".into());
    }
    let dll = dll.ok_or("no dll: pass --dll or set LSFGM_DLL_PATH")?;
    if native {
        let res = shaders::parse(&std::fs::read(&dll).map_err(|e| format!("{}: {e}", dll.display()))?)?;
        let run = Run { w, h, m, flow, iters, perf, fp16, hdr };
        return run_native(&run, &res, frames.as_ref().map(|f| &f.1), out.as_deref());
    }
    let driver = driver.ok_or("no driver: pass --driver or set LSFGM_MOLTENVK")?;

    let t0 = Instant::now();
    let inst = Arc::new(Instance::own(&driver, "", &dll, fp16, false, relog)?);
    let mut ctx = Context::new(inst.clone(), w, h, flow, perf, if hdr { Colour::HDR } else { Colour::SDR })?;
    let build = t0.elapsed();
    if partial {
        // regression: a partial iteration never submits the completion fence; the next dispatch and the drop must still return
        ctx.dispatch(2, true)?;
        ctx.acquire(true, 0.0)?;
        ctx.dispatch(2, true)?;
        drop(ctx);
        // the same thing on the signalled protocol, where settle waits on the timelines instead
        let mut ctx = Context::new(inst.clone(), w, h, flow, perf, if hdr { Colour::HDR } else { Colour::SDR })?;
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

    let region = |layer: u32| vk::BufferImageCopy {
        image_subresource: vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: layer,
            layer_count: 1,
        },
        image_extent: vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        },
        ..Default::default()
    };
    // file input: one staging buffer per frame, uploaded into the source layer its iteration writes
    let staging = match &frames {
        Some((_, px)) => Some([
            vkutil::create_buffer(
                &inst.instance,
                inst.physical_device,
                d,
                vk::BufferUsageFlags::TRANSFER_SRC,
                &px[0],
            )?,
            vkutil::create_buffer(
                &inst.instance,
                inst.physical_device,
                d,
                vk::BufferUsageFlags::TRANSFER_SRC,
                &px[1],
            )?,
        ]),
        None => None,
    };
    // file output: one host buffer and fence reused for every generated frame written
    let readback = match &out {
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let (buf, mem) = vkutil::create_buffer(
                &inst.instance,
                inst.physical_device,
                d,
                vk::BufferUsageFlags::TRANSFER_DST,
                &vec![0u8; w as usize * h as usize * 4],
            )?;
            Some((buf, mem, vkutil::create_fence(d)?))
        }
        None => None,
    };

    let mut s = 0u64;
    let t1 = Instant::now();
    let (mut profile, mut profiled) = (Vec::<f64>::new(), 0u32);
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
            match &staging {
                Some(bufs) => d.cmd_copy_buffer_to_image(
                    cb,
                    bufs[it as usize % 2].0,
                    src,
                    vk::ImageLayout::GENERAL,
                    &[region(it % 2)],
                ),
                None => d.cmd_clear_color_image(
                    cb,
                    src,
                    vk::ImageLayout::GENERAL,
                    &color,
                    &[vkutil::color_range(it % 2, 1)],
                ),
            }
            check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
        }
        // the previous iteration's last main pass signalled s; do not overwrite its source layer before it is done
        // --bench drops the caller side of the protocol: nothing waits, one iteration stays in flight
        submit(
            &[cb],
            (it > 0 && !bench).then_some(s),
            (!bench).then_some(s + 1),
            vk::Fence::null(),
        )?;
        ctx.dispatch(m - 1, bench)?;
        for k in 0..(m - 1) as u64 {
            ctx.acquire(bench, 0.0)?;
            // the last iteration is the one with both input frames in place, so it is the one written out
            let grab = readback.as_ref().filter(|_| it + 1 == iters);
            let (copy, fence) = match grab {
                Some((buf, _, fence)) => {
                    let cb = vkutil::allocate_command_buffer(d, pool)?;
                    cbs.push(cb);
                    vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
                    unsafe {
                        d.cmd_copy_image_to_buffer(
                            cb,
                            dst,
                            vk::ImageLayout::GENERAL,
                            *buf,
                            &[region(0)],
                        );
                        check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
                    }
                    (vec![cb], *fence)
                }
                None => (vec![], vk::Fence::null()),
            };
            let next = (k + 2 < m as u64 && !bench).then_some(s + 3 + 2 * k);
            submit(&copy, (!bench).then_some(s + 2 + 2 * k), next, fence)?;
            if let Some((_, mem, fence)) = grab {
                let px = unsafe {
                    check(
                        d.wait_for_fences(&[*fence], true, u64::MAX),
                        "vkWaitForFences",
                    )?;
                    check(d.reset_fences(&[*fence]), "vkResetFences")?;
                    let p = check(
                        d.map_memory(*mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()),
                        "vkMapMemory",
                    )? as *const u8;
                    let px = std::slice::from_raw_parts(p, w as usize * h as usize * 4).to_vec();
                    d.unmap_memory(*mem);
                    px
                };
                let dir = out.as_ref().expect("readback implies --out");
                write_ppm(&dir.join(format!("generated_{k}.ppm")), w, h, &px)?;
            }
        }
        if !bench {
            s += 2 * (m as u64 - 1);
        }
        // LSFGM_GPU_PROFILE: stage times of this iteration, after a warm-up; bench iterations overlap on the queries
        if let Some(t) = (!bench && it >= 5)
            .then(|| ctx.pipeline.gpu_profile())
            .flatten()
        {
            profile.resize(t.len(), 0.0);
            profile.iter_mut().zip(&t).for_each(|(a, b)| *a += b);
            profiled += 1;
        }
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
    let fence = vkutil::create_fence(d)?;
    let pixel = unsafe {
        d.cmd_copy_image_to_buffer(cb, dst, vk::ImageLayout::GENERAL, buf, &[region(0)]);
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
        for (b, m) in staging.iter().flatten() {
            d.destroy_buffer(*b, None);
            d.free_memory(*m, None);
        }
        if let Some((b, m, f)) = readback {
            d.destroy_buffer(b, None);
            d.free_memory(m, None);
            d.destroy_fence(f, None);
        }
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
    if profiled > 0 {
        let split = ctx.pipeline.sig.split;
        let (mut pre, mut main) = (0.0, 0.0);
        for (st, (t, label)) in profile.iter().zip(&ctx.pipeline.stage_labels).enumerate() {
            let t = t / profiled as f64;
            if st < split {
                pre += t;
            } else {
                main += t;
            }
            println!("stage {st:2} {:5.3} ms  {label}", t);
        }
        println!("pre-pass {pre:.3} ms, main pass {main:.3} ms ({profiled} iterations)");
    }
    drop(ctx);
    Ok(())
}

fn main() {
    log::set_level(
        if env("LSFGM_LOG_LEVEL").is_some_and(|l| l.eq_ignore_ascii_case("debug")) {
            log::Level::Debug
        } else {
            log::Level::Info
        },
    );
    if let Err(e) = run() {
        eprintln!("validate: {e}");
        std::process::exit(1);
    }
}
