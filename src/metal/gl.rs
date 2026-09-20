// opengl front end: frames from the game's buffer swap go through shared iosurfaces
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use ash::vk;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, ProtocolObject, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CGRect};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceHeight, kIOSurfacePixelFormat, kIOSurfaceWidth,
    IOSurfaceRef,
};
use objc2_metal::{
    MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};

use super::generator::{image_info, import_texture, Backend, Wrapper, GPU_TIMEOUT};
use super::{enabled, now, set_enabled, setup};
use crate::log;
use crate::pacer::{display_refresh, Estimator, Pacer, Sample};
use crate::settings::PacingMode;
use crate::vkutil::{self, check};

type FlushFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);

#[link(name = "OpenGL", kind = "framework")]
extern "C" {
    fn CGLGetCurrentContext() -> *mut c_void;
    fn CGLSetParameter(ctx: *mut c_void, pname: i32, params: *const i32) -> i32;
    fn CGLTexImageIOSurface2D(
        ctx: *mut c_void,
        target: u32,
        internal_format: u32,
        width: i32,
        height: i32,
        format: u32,
        ty: u32,
        surface: *const IOSurfaceRef,
        plane: u32,
    ) -> i32;
    fn glGetIntegerv(pname: u32, data: *mut i32);
    fn glIsEnabled(cap: u32) -> u8;
    fn glEnable(cap: u32);
    fn glDisable(cap: u32);
    fn glGenTextures(n: i32, out: *mut u32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glGenFramebuffers(n: i32, out: *mut u32);
    fn glDeleteFramebuffers(n: i32, fbos: *const u32);
    fn glBindFramebuffer(target: u32, fbo: u32);
    fn glFramebufferTexture2D(
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: u32,
        level: i32,
    );
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glReadBuffer(mode: u32);
    fn glBlitFramebuffer(
        sx0: i32,
        sy0: i32,
        sx1: i32,
        sy1: i32,
        dx0: i32,
        dy0: i32,
        dx1: i32,
        dy1: i32,
        mask: u32,
        filter: u32,
    );
    fn glFinish();
}

// appkit owns nsopenglcontext; linking it makes the class resolvable when the constructor runs
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
const GL_DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_TEXTURE_RECTANGLE: u32 = 0x84F5;
const GL_TEXTURE_BINDING_RECTANGLE: u32 = 0x84F6;
const GL_RGBA: u32 = 0x1908;
const GL_BGRA: u32 = 0x80E1;
const GL_UNSIGNED_INT_8_8_8_8_REV: u32 = 0x8367;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_NEAREST: u32 = 0x2600;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_FRAMEBUFFER_SRGB: u32 = 0x8DB9;
const GL_BACK: u32 = 0x0405;
const GL_READ_BUFFER: u32 = 0x0C02;
const CGL_SWAP_INTERVAL: i32 = 222;
const BGRA: i32 = i32::from_be_bytes(*b"BGRA");

static FLUSH: OnceLock<FlushFn> = OnceLock::new();

// swizzle -[NSOpenGLContext flushBuffer]; the original is published before the swap
pub fn install() {
    if FLUSH.get().is_some() || setup().is_none_or(|s| s.profile.multiplier < 2) {
        return;
    }
    let Some(m) =
        AnyClass::get(c"NSOpenGLContext").and_then(|c| c.instance_method(sel!(flushBuffer)))
    else {
        return;
    };
    let _ = FLUSH.set(unsafe { std::mem::transmute::<Imp, FlushFn>(m.implementation()) });
    unsafe {
        m.set_implementation(std::mem::transmute::<*const (), Imp>(
            flush_hook as *const (),
        ))
    };
    set_enabled(true);
}

// one shared surface: an iosurface seen by opengl, metal and vulkan
struct Surface {
    _surface: CFRetained<IOSurfaceRef>,
    _texture: Retained<ProtocolObject<dyn MTLTexture>>,
    gl_texture: u32,
    fbo: u32,
    image: vk::Image,
}

// per opengl context: index 0 holds the source frame, the rest the generated frames
struct Context {
    extent: (u32, u32),
    surfaces: Vec<Surface>,
    wrapper: Option<Wrapper>,
    cmd: Vec<vk::CommandBuffer>,
    gate: vk::Semaphore,
    mark: vk::Semaphore,
    mark_value: u64,
    fence: vk::Fence,
    pacer: Option<Pacer>,
    estimator: Estimator,
    // seconds the game spent in generated swaps during the previous frame
    held: f64,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    source: u64,
    original: u64,
    generated: u64,
}

#[derive(Default)]
struct Front {
    backend: Option<Backend>,
    contexts: HashMap<usize, Context>,
    // contexts that fell back to native swaps, including ones whose setup failed
    failed: HashSet<usize>,
}

// metal textures and iosurfaces are thread-safe objects; every access goes through FRONT's lock
unsafe impl Send for Front {}

// one lock for every gl context; per-context locks if two contexts ever swap at once
static FRONT: Mutex<Option<Front>> = Mutex::new(None);

unsafe extern "C-unwind" fn flush_hook(this: *mut AnyObject, sel: Sel) {
    let orig = *FLUSH.get().unwrap();
    let cgl = CGLGetCurrentContext();
    if !enabled() || cgl.is_null() {
        return orig(this, sel);
    }
    let Some(extent) = drawable_size(this) else {
        return orig(this, sel);
    };
    let entered = now();
    let mut guard = FRONT.lock().unwrap();
    let front = guard.get_or_insert_with(Front::default);
    if let Err(e) = generate(front, cgl, extent, entered, || orig(this, sel)) {
        log::error("OpenGL frame generation failed, presenting natively from now on:");
        log::error(&format!("- {e}"));
        if let (Some(c), Some(b)) = (front.contexts.remove(&(cgl as usize)), &front.backend) {
            c.destroy(b);
        }
        front.failed.insert(cgl as usize);
        orig(this, sel);
    }
}

// the default framebuffer's pixel size, which follows the view's backing store when it asks for one
unsafe fn drawable_size(ctx: *mut AnyObject) -> Option<(u32, u32)> {
    let view: *mut AnyObject = msg_send![ctx, view];
    if view.is_null() {
        return None;
    }
    let mut r: CGRect = msg_send![view, bounds];
    let best: Bool = msg_send![view, wantsBestResolutionOpenGLSurface];
    if best.as_bool() {
        r = msg_send![view, convertRectToBacking: r];
    }
    let (w, h) = (r.size.width.round() as u32, r.size.height.round() as u32);
    (w > 0 && h > 0).then_some((w, h))
}

// framebuffer bindings, the default read buffer, scissor and srgb, restored on drop
struct Saved {
    read: i32,
    draw: i32,
    default_read_buffer: i32,
    scissor: bool,
    srgb: bool,
}

impl Saved {
    unsafe fn take() -> Saved {
        let mut v = [0i32; 3];
        glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &mut v[0]);
        glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut v[1]);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
        glGetIntegerv(GL_READ_BUFFER, &mut v[2]);
        let s = Saved {
            read: v[0],
            draw: v[1],
            default_read_buffer: v[2],
            scissor: glIsEnabled(GL_SCISSOR_TEST) != 0,
            srgb: glIsEnabled(GL_FRAMEBUFFER_SRGB) != 0,
        };
        glDisable(GL_SCISSOR_TEST);
        glDisable(GL_FRAMEBUFFER_SRGB);
        s
    }
}

impl Drop for Saved {
    fn drop(&mut self) {
        unsafe {
            glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
            glReadBuffer(self.default_read_buffer as u32);
            glBindFramebuffer(GL_READ_FRAMEBUFFER, self.read as u32);
            glBindFramebuffer(GL_DRAW_FRAMEBUFFER, self.draw as u32);
            if self.scissor {
                glEnable(GL_SCISSOR_TEST);
            }
            if self.srgb {
                glEnable(GL_FRAMEBUFFER_SRGB);
            }
        }
    }
}

unsafe fn blit(read: u32, draw: u32, (w, h): (u32, u32)) {
    glBindFramebuffer(GL_READ_FRAMEBUFFER, read);
    if read == 0 {
        glReadBuffer(GL_BACK);
    }
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, draw);
    let (w, h) = (w as i32, h as i32);
    glBlitFramebuffer(0, 0, w, h, 0, 0, w, h, GL_COLOR_BUFFER_BIT, GL_NEAREST);
}

unsafe fn generate(
    front: &mut Front,
    cgl: *mut c_void,
    extent: (u32, u32),
    entered: f64,
    present: impl Fn(),
) -> Result<(), String> {
    let s = setup().ok_or("no active frame-generation profile")?;
    let p = &s.profile;
    let key = cgl as usize;
    if front.failed.contains(&key) {
        present();
        return Ok(());
    }
    if front.backend.is_none() {
        // no metal layer here to take a gpu from, so the first device stands
        front.backend = Some(Backend::create(s, None)?);
    }
    let b = front.backend.as_ref().unwrap();
    if front.contexts.get(&key).is_none_or(|c| c.extent != extent) {
        if let Some(old) = front.contexts.remove(&key) {
            old.destroy(b);
        }
        let adaptive = p.pacing_mode == PacingMode::Adaptive;
        let c = Context::new(
            b,
            cgl,
            extent,
            p.multiplier,
            p.flow_scale,
            p.performance_mode,
            adaptive,
        )?;
        let mode = if adaptive {
            "adaptive up to"
        } else {
            "multiplier"
        };
        log::info(&format!(
            "OpenGL presentation {}x{}, {mode} {}",
            extent.0, extent.1, p.multiplier
        ));
        front.contexts.insert(key, c);
    }
    // every frame: the game or wine can reset the swap interval after the context was set up
    if p.override_present_mode {
        CGLSetParameter(cgl, CGL_SWAP_INTERVAL, &1);
    }
    let c = front.contexts.get_mut(&key).unwrap();
    // time held in generated swaps makes the sample untrusted; the pacer then probes with the original alone
    let blocked = std::mem::take(&mut c.held);
    let sample = c.estimator.sample(entered, blocked);
    pace_debug(sample, blocked);
    let m = p.multiplier;
    let slots: Vec<f64> = match &mut c.pacer {
        Some(pacer) => pacer.slots(sample),
        None => (1..=m).map(|i| i as f64 / m as f64).collect(),
    };
    let show_original = *slots.last().unwrap() >= 1.0;
    let inserted = slots.len() - show_original as usize;
    let saved = Saved::take();
    // the game's back buffer into the shared source surface; vulkan reads it only after gl is done
    blit(0, c.surfaces[0].fbo, extent);
    // glfinish waits on the game's frame; a gl sync object would let vulkan start earlier
    glFinish();
    let w = c.wrapper.as_mut().unwrap();
    c.mark_value += 1;
    w.dispatch(
        b,
        c.cmd[0],
        c.surfaces[0].image,
        (c.gate, 0),
        inserted as u32,
        (c.mark, c.mark_value),
    )?;
    if inserted == 0 {
        // nothing waits on this iteration's copy, and the next swap writes the same surface
        w.idle(b);
    }
    let d = &b.device;
    let held_from = now();
    for (i, &ts) in slots.iter().take(inserted).enumerate() {
        c.mark_value += 1;
        w.acquire(
            b,
            c.cmd[1 + i],
            c.surfaces[1 + i].image,
            ts,
            (c.mark, c.mark_value),
            c.fence,
        )?;
        d.wait_for_fences(&[c.fence], true, GPU_TIMEOUT.as_nanos() as u64)
            .map_err(|_| "OpenGL generated frame did not complete")?;
        check(d.reset_fences(&[c.fence]), "vkResetFences")?;
        blit(c.surfaces[1 + i].fbo, 0, extent);
        if i + 1 == inserted && !show_original {
            drop(saved);
            present();
            c.stats.generated += 1;
            c.held = now() - held_from;
            c.count_source();
            return Ok(());
        }
        present();
        c.stats.generated += 1;
    }
    c.held = now() - held_from;
    // the back buffer is undefined after a swap, so the original frame comes back from its copy
    blit(c.surfaces[0].fbo, 0, extent);
    drop(saved);
    present();
    c.stats.original += 1;
    c.count_source();
    Ok(())
}

fn pace_debug(s: Sample, blocked: f64) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("LSFGM_PACE_DEBUG").is_some()) {
        let t = if s.trusted { "" } else { " (untrusted)" };
        log::info(&format!(
            "pace interval={}ms blocked={}ms{t}",
            s.interval * 1000.0,
            blocked * 1000.0
        ));
    }
}

impl Context {
    unsafe fn new(
        b: &Backend,
        cgl: *mut c_void,
        extent: (u32, u32),
        m: u32,
        flow: f32,
        perf: bool,
        adaptive: bool,
    ) -> Result<Context, String> {
        let d = &b.device;
        let mut c = Context {
            extent,
            surfaces: Vec::new(),
            wrapper: None,
            cmd: Vec::new(),
            gate: vkutil::create_semaphore(d, true)?,
            mark: vk::Semaphore::null(),
            mark_value: 0,
            fence: vk::Fence::null(),
            pacer: adaptive
                .then(|| Pacer::new(objc2::rc::autoreleasepool(|_| display_refresh()), m)),
            estimator: Estimator::default(),
            held: 0.0,
            stats: Stats::default(),
        };
        let built = (|| {
            c.mark = vkutil::create_semaphore(d, true)?;
            c.fence = vkutil::create_fence(d)?;
            let mut prior = 0i32;
            glGetIntegerv(GL_TEXTURE_BINDING_RECTANGLE, &mut prior);
            let made = (0..=m).try_for_each(|_| {
                c.surfaces.push(Surface::new(b, cgl, extent)?);
                Ok::<_, String>(())
            });
            glBindTexture(GL_TEXTURE_RECTANGLE, prior as u32);
            made?;
            for _ in 0..=m {
                c.cmd.push(vkutil::allocate_command_buffer(d, b.pool)?);
            }
            c.wrapper = Some(Wrapper::new(b, extent, flow, perf, false)?);
            Ok::<_, String>(())
        })();
        match built {
            Ok(()) => Ok(c),
            Err(e) => {
                c.destroy(b);
                Err(e)
            }
        }
    }

    fn count_source(&mut self) {
        self.stats.source += 1;
        let s = &self.stats;
        if std::env::var_os("LSFGM_STATS").is_none() || !s.source.is_multiple_of(60) {
            return;
        }
        let slots = self
            .pacer
            .as_mut()
            .map(|p| format!(" slots={}", p.histogram()))
            .unwrap_or_default();
        log::info(&format!(
            "Frame generation stats pid={} source={} original={} generated={} total={}{slots} (opengl swaps on this context)",
            std::process::id(),
            s.source,
            s.original,
            s.generated,
            s.original + s.generated
        ));
    }

    fn destroy(mut self, b: &Backend) {
        let d = &b.device;
        if let Some(w) = self.wrapper.take() {
            w.destroy(b);
        }
        unsafe {
            for s in self.surfaces.drain(..) {
                d.destroy_image(s.image, None);
                glDeleteFramebuffers(1, &s.fbo);
                glDeleteTextures(1, &s.gl_texture);
            }
            if !self.cmd.is_empty() {
                d.free_command_buffers(b.pool, &self.cmd);
            }
            d.destroy_semaphore(self.gate, None);
            d.destroy_semaphore(self.mark, None);
            d.destroy_fence(self.fence, None);
        }
    }
}

impl Surface {
    unsafe fn new(b: &Backend, cgl: *mut c_void, (w, h): (u32, u32)) -> Result<Surface, String> {
        let keys = [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfacePixelFormat,
        ];
        let values = [
            &*CFNumber::new_i32(w as i32),
            &*CFNumber::new_i32(h as i32),
            &*CFNumber::new_i32(4),
            &*CFNumber::new_i32(BGRA),
        ];
        let props = CFDictionary::<CFString, CFNumber>::from_slices(&keys, &values);
        let surface = IOSurfaceRef::new(props.as_opaque()).ok_or("IOSurfaceCreate failed")?;
        let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            w as usize,
            h as usize,
            false,
        );
        desc.setUsage(
            MTLTextureUsage::ShaderRead
                | MTLTextureUsage::ShaderWrite
                | MTLTextureUsage::RenderTarget
                | MTLTextureUsage::PixelFormatView,
        );
        desc.setStorageMode(MTLStorageMode::Shared);
        // the backend's vulkan device is the first gpu, which is the system default on apple silicon
        let device =
            MTLCreateSystemDefaultDevice().ok_or("no Metal device for the OpenGL context")?;
        let texture = device
            .newTextureWithDescriptor_iosurface_plane(&desc, &surface, 0)
            .ok_or("unable to wrap an IOSurface in a Metal texture")?;
        let mut gl_texture = 0;
        glGenTextures(1, &mut gl_texture);
        glBindTexture(GL_TEXTURE_RECTANGLE, gl_texture);
        let err = CGLTexImageIOSurface2D(
            cgl,
            GL_TEXTURE_RECTANGLE,
            GL_RGBA,
            w as i32,
            h as i32,
            GL_BGRA,
            GL_UNSIGNED_INT_8_8_8_8_REV,
            &*surface,
            0,
        );
        let mut fbo = 0;
        glGenFramebuffers(1, &mut fbo);
        let mut prior = 0i32;
        glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut prior);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fbo);
        glFramebufferTexture2D(
            GL_DRAW_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_RECTANGLE,
            gl_texture,
            0,
        );
        let status = glCheckFramebufferStatus(GL_DRAW_FRAMEBUFFER);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, prior as u32);
        let image = if err != 0 || status != GL_FRAMEBUFFER_COMPLETE {
            Err(format!(
                "IOSurface framebuffer unusable (CGL {err}, status {status:#x})"
            ))
        } else {
            import_texture(
                &b.device,
                &texture,
                image_info(
                    vk::Format::B8G8R8A8_UNORM,
                    (w, h),
                    vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
                ),
            )
        };
        match image {
            Ok(image) => Ok(Surface {
                _surface: surface,
                _texture: texture,
                gl_texture,
                fbo,
                image,
            }),
            Err(e) => {
                glDeleteFramebuffers(1, &fbo);
                glDeleteTextures(1, &gl_texture);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn CGLChoosePixelFormat(
            attrs: *const u32,
            format: *mut *mut c_void,
            count: *mut i32,
        ) -> i32;
        fn CGLCreateContext(format: *mut c_void, share: *mut c_void, ctx: *mut *mut c_void) -> i32;
        fn CGLSetCurrentContext(ctx: *mut c_void) -> i32;
        fn CGLReleaseContext(ctx: *mut c_void);
        fn CGLReleasePixelFormat(format: *mut c_void);
        fn glGetError() -> u32;
    }

    struct TestContext(*mut c_void, *mut c_void);

    impl Drop for TestContext {
        fn drop(&mut self) {
            unsafe {
                CGLSetCurrentContext(self.1);
                CGLReleaseContext(self.0);
            }
        }
    }

    #[test]
    fn restores_default_read_buffer_with_a_game_fbo_bound() {
        unsafe {
            let attrs = [99, 0x3200, 73, 5, 0];
            let mut format = std::ptr::null_mut();
            let mut count = 0;
            assert_eq!(
                CGLChoosePixelFormat(attrs.as_ptr(), &mut format, &mut count),
                0
            );
            assert!(!format.is_null());
            let mut ctx = std::ptr::null_mut();
            let result = CGLCreateContext(format, std::ptr::null_mut(), &mut ctx);
            CGLReleasePixelFormat(format);
            assert_eq!(result, 0);
            let _context = TestContext(ctx, CGLGetCurrentContext());
            assert_eq!(CGLSetCurrentContext(ctx), 0);
            let mut fbo = 0;
            glGenFramebuffers(1, &mut fbo);
            for read in [0, fbo] {
                for enabled in [false, true] {
                    glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
                    glReadBuffer(0x0404);
                    glBindFramebuffer(GL_READ_FRAMEBUFFER, read);
                    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fbo);
                    for cap in [GL_SCISSOR_TEST, GL_FRAMEBUFFER_SRGB] {
                        if enabled {
                            glEnable(cap)
                        } else {
                            glDisable(cap)
                        }
                    }
                    assert_eq!(glGetError(), 0);
                    let saved = Saved::take();
                    glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
                    glReadBuffer(GL_BACK);
                    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
                    drop(saved);
                    assert_eq!(glGetError(), 0, "restoring state introduced a GL error");
                    let mut value = 0;
                    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &mut value);
                    assert_eq!(value as u32, read);
                    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut value);
                    assert_eq!(value as u32, fbo);
                    for cap in [GL_SCISSOR_TEST, GL_FRAMEBUFFER_SRGB] {
                        assert_eq!(glIsEnabled(cap) != 0, enabled);
                    }
                    glGetIntegerv(GL_READ_BUFFER, &mut value);
                    assert_eq!(
                        value as u32,
                        if read == 0 {
                            0x0404
                        } else {
                            GL_COLOR_ATTACHMENT0
                        }
                    );
                    glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
                    glGetIntegerv(GL_READ_BUFFER, &mut value);
                    assert_eq!(value, 0x0404, "default framebuffer read buffer changed");
                }
            }
            glDeleteFramebuffers(1, &fbo);
        }
    }
}
