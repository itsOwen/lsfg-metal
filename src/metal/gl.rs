// opengl front end: frames from the game's buffer swap go through shared iosurfaces
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::mpsc::{channel, TryRecvError};
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
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice,
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use super::generator::{image_info, import_texture, native_off, Backend, Built, Native, Unshared, Wrapper, GPU_TIMEOUT};
use super::{enabled, metalfx, now, set_enabled, setup, Setup};
use crate::generator::signature::{Colour, Signature};
use crate::log;
use crate::pacer::{display_refresh, Estimator, Pacer, Sample};
use crate::settings::{PacingMode, ScalerMode};
use crate::vkutil::{self, check};

type FlushFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);

#[link(name = "OpenGL", kind = "framework")]
extern "C" {
    fn CGLGetCurrentContext() -> *mut c_void;
    fn CGLSetParameter(ctx: *mut c_void, pname: i32, params: *const i32) -> i32;
    fn CGLGetParameter(ctx: *mut c_void, pname: i32, params: *mut i32) -> i32;
    fn CGLEnable(ctx: *mut c_void, pname: i32) -> i32;
    fn CGLDisable(ctx: *mut c_void, pname: i32) -> i32;
    fn CGLIsEnabled(ctx: *mut c_void, pname: i32, enabled: *mut i32) -> i32;
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
const CGL_SURFACE_BACKING_SIZE: i32 = 304;
const CGL_ENABLE_SURFACE_BACKING_SIZE: i32 = 305;
const BGRA: i32 = i32::from_be_bytes(*b"BGRA");

static FLUSH: OnceLock<FlushFn> = OnceLock::new();

// swizzle -[NSOpenGLContext flushBuffer]; the original is published before the swap
pub fn install() {
    if FLUSH.get().is_some() || setup().is_none_or(|s| !s.profile.active()) {
        return;
    }
    let Some(m) =
        AnyClass::get(c"NSOpenGLContext").and_then(|c| c.instance_method(sel!(flushBuffer)))
    else {
        return;
    };
    // the vblank clock starts now, so it has ticked by the first swap
    let _ = vblank_period();
    let _ = FLUSH.set(unsafe { std::mem::transmute::<Imp, FlushFn>(m.implementation()) });
    unsafe {
        m.set_implementation(std::mem::transmute::<*const (), Imp>(
            flush_hook as *const (),
        ))
    };
    set_enabled(true);
}

// one shared surface: an iosurface seen by opengl, metal and, on moltenvk, vulkan
struct Surface {
    _surface: CFRetained<IOSurfaceRef>,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    gl_texture: u32,
    fbo: u32,
    // null on the native generator
    image: vk::Image,
}

// the generator behind one context: native metal, or the moltenvk context with its own sync
enum Engine {
    Native(Box<Native>),
    Vulkan(Box<Vk>),
}

struct Vk {
    wrapper: Option<Wrapper>,
    cmd: Vec<vk::CommandBuffer>,
    gate: vk::Semaphore,
    mark: vk::Semaphore,
    mark_value: u64,
    fence: vk::Fence,
}

// per opengl context: index 0 holds the source frame, the rest the generated frames
struct Context {
    extent: (u32, u32),
    surfaces: Vec<Surface>,
    engine: Engine,
    pacer: Option<Pacer>,
    estimator: Estimator,
    // seconds the game waited in our swaps during the previous frame
    held: f64,
    stats: Stats,
    up: Option<Up>,
}

// metalfx for one context: the surface is pinned to the shown size, the game keeps drawing its own size in its corner
struct Up {
    shown: (u32, u32),
    out: Surface,
    mid: Retained<ProtocolObject<dyn MTLTexture>>,
    scaler: metalfx::Scaler,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    // the backing size set before ours, restored on release; nothing to restore until pinned
    prior: Option<[i32; 2]>,
    pinned: bool,
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
    // set once the native generator is ruled out, by LSFGM_NATIVE=0 or a failed build
    native_off: Option<bool>,
    contexts: HashMap<usize, Context>,
    // native generators being built, per context, with the size they are for
    pending: HashMap<usize, ((u32, u32), Built)>,
    // contexts that fell back to native swaps, including ones whose setup failed
    failed: HashSet<usize>,
    // upscale only, at multiplier 1, per context: the sizes it was set up for, and the game's frame with its scaler unless that failed
    plain: HashMap<usize, Plain>,
}

// game size, shown size, and the game's frame with its scaler unless that failed
type Plain = ((u32, u32), (u32, u32), Option<(Surface, Up)>);

// metal textures and iosurfaces are thread-safe objects; every access goes through FRONT's lock
unsafe impl Send for Front {}

// one lock for every gl context; per-context locks if two contexts ever swap at once
static FRONT: Mutex<Option<Front>> = Mutex::new(None);

// the display refresh our swaps are spaced by, read again when a context is rebuilt
static REFRESH: Mutex<Option<f64>> = Mutex::new(None);
// the vblank our previous swap shows at, on the host clock; without a display link, when it went out
static SHOWN_AT: Mutex<f64> = Mutex::new(0.0);

#[repr(C)]
struct CVTimeStamp {
    version: u32,
    video_time_scale: i32,
    video_time: i64,
    host_time: u64,
}

type CVOutput = unsafe extern "C" fn(*mut c_void, *const CVTimeStamp, *const CVTimeStamp, u64, *mut u64, *mut c_void) -> i32;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVDisplayLinkCreateWithActiveCGDisplays(link: *mut *mut c_void) -> i32;
    fn CVDisplayLinkSetOutputCallback(link: *mut c_void, cb: Option<CVOutput>, ctx: *mut c_void) -> i32;
    fn CVDisplayLinkStart(link: *mut c_void) -> i32;
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
}

// host ticks to seconds, the clock display link timestamps use
fn host_secs(ticks: u64) -> f64 {
    static RATIO: OnceLock<f64> = OnceLock::new();
    let r = *RATIO.get_or_init(|| {
        let mut tb = [0u32; 2];
        unsafe { mach_timebase_info(&mut tb) };
        if tb[1] == 0 {
            1e-9
        } else {
            tb[0] as f64 / tb[1] as f64 * 1e-9
        }
    });
    ticks as f64 * r
}

// the next vblank the display link announced and the period between its announcements, as f64 bits; zero until it ticks
static VBLANK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PERIOD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

unsafe extern "C" fn vblank(_: *mut c_void, _: *const CVTimeStamp, out: *const CVTimeStamp, _: u64, _: *mut u64, _: *mut c_void) -> i32 {
    use std::sync::atomic::Ordering::Relaxed;
    let next = host_secs((*out).host_time);
    let gap = next - f64::from_bits(VBLANK.swap(next.to_bits(), Relaxed));
    // one refresh apart; a missed callback or the first one leaves the period as it was
    if gap > 0.002 && gap < 0.05 {
        PERIOD.store(gap.to_bits(), Relaxed);
    }
    0
}

// the main display's vblank period, once a display link runs and has ticked twice; none where there is no link
fn vblank_period() -> Option<f64> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    let started = *STARTED.get_or_init(|| unsafe {
        let mut link = std::ptr::null_mut();
        if CVDisplayLinkCreateWithActiveCGDisplays(&mut link) != 0 || link.is_null() {
            return false;
        }
        CVDisplayLinkSetOutputCallback(link, Some(vblank), std::ptr::null_mut());
        // the link lives as long as the process
        CVDisplayLinkStart(link) == 0
    });
    let p = f64::from_bits(PERIOD.load(std::sync::atomic::Ordering::Relaxed));
    (started && p > 0.0).then_some(p)
}

// the first vblank at or after `t`
fn next_vblank(t: f64, period: f64) -> f64 {
    let base = f64::from_bits(VBLANK.load(std::sync::atomic::Ordering::Relaxed));
    base + ((t - base) / period).ceil() * period
}

// gl swaps burst past swap interval 1 and a vblank shows only the last, so each waits out the previous one's vblank
fn spaced(present: &impl Fn()) {
    let refresh = *REFRESH.lock().unwrap().get_or_insert_with(display_refresh);
    let mut shown = SHOWN_AT.lock().unwrap();
    // on the vblank grid when the target is the display's own rate; LSFGM_TARGET_FPS below it spaces by time
    let grid = vblank_period().filter(|p| (p - refresh).abs() < refresh * 0.05);
    // said once, a second in, when the link has had time to tick
    static SAID: std::sync::Once = std::sync::Once::new();
    static FIRST: OnceLock<f64> = OnceLock::new();
    if now() - *FIRST.get_or_init(now) > 1.0 {
        SAID.call_once(|| match grid {
            Some(p) => log::info(&format!("OpenGL swaps paced on the display's vblank every {:.2} ms", p * 1e3)),
            None => log::info(&format!("OpenGL swaps paced every {:.2} ms, without the display's vblank", refresh * 1e3)),
        });
    }
    match grid {
        Some(period) => {
            let host = || host_secs(unsafe { mach_absolute_time() });
            // just past the vblank that shows the previous swap, so this one lands on the next
            let wait = *shown + 0.001 - host();
            if wait > 0.0 {
                std::thread::sleep(std::time::Duration::from_secs_f64(wait));
            }
            // a swap needs a few milliseconds before a vblank to make it
            let at = next_vblank(host() + 0.003, period);
            present();
            *shown = at;
        }
        None => {
            let wait = *shown + refresh * 0.95 - now();
            if wait > 0.0 {
                std::thread::sleep(std::time::Duration::from_secs_f64(wait));
            }
            present();
            *shown = now();
        }
    }
}

unsafe extern "C-unwind" fn flush_hook(this: *mut AnyObject, sel: Sel) {
    let orig = *FLUSH.get().unwrap();
    let cgl = CGLGetCurrentContext();
    if !enabled() || cgl.is_null() {
        return orig(this, sel);
    }
    let Some((extent, shown)) = drawable_size(this) else {
        return orig(this, sel);
    };
    let entered = now();
    let mut guard = FRONT.lock().unwrap();
    let front = guard.get_or_insert_with(Front::default);
    // the game's gl thread may have no pool, and the native generator autoreleases its encoders
    let made = objc2::rc::autoreleasepool(|_| generate(front, cgl, extent, shown, entered, || orig(this, sel)));
    if let Err(e) = made {
        log::error("OpenGL frame generation failed, presenting natively from now on:");
        log::error(&format!("- {e}"));
        if let Some(c) = front.contexts.remove(&(cgl as usize)) {
            c.destroy(front.backend.as_ref());
        }
        if let Some((_, _, Some((src, u)))) = front.plain.remove(&(cgl as usize)) {
            u.release(cgl);
            src.delete();
        }
        front.failed.insert(cgl as usize);
        orig(this, sel);
    }
}

// once per process: the scaler is set but this context shows at the game's size
fn not_upscaled(extent: (u32, u32)) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        log::info(&format!(
            "MetalFX upscaling not used for OpenGL: the view is not larger than {}x{} in pixels, or MetalFX cannot run here",
            extent.0, extent.1
        ))
    });
}

// the default framebuffer's pixel size, which follows the view's backing store when it asks for one, and the view in physical pixels
unsafe fn drawable_size(ctx: *mut AnyObject) -> Option<((u32, u32), (u32, u32))> {
    let view: *mut AnyObject = msg_send![ctx, view];
    if view.is_null() {
        return None;
    }
    let mut r: CGRect = msg_send![view, bounds];
    let best: Bool = msg_send![view, wantsBestResolutionOpenGLSurface];
    let scaling = setup().is_some_and(|s| s.profile.scaler != ScalerMode::Off);
    let scale = if best.as_bool() || !scaling { 1.0 } else { metalfx::screen_scale() };
    if best.as_bool() {
        r = msg_send![view, convertRectToBacking: r];
    }
    let (w, h) = (r.size.width.round() as u32, r.size.height.round() as u32);
    let shown = ((r.size.width * scale).round() as u32, (r.size.height * scale).round() as u32);
    (w > 0 && h > 0).then_some(((w, h), shown))
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
    shown: (u32, u32),
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
    let upscale = p.scaler == ScalerMode::MetalFx && shown.0 > extent.0 && shown.1 > extent.1;
    // every frame, also while the context builds: the game or wine can reset the swap interval
    if p.override_present_mode {
        CGLSetParameter(cgl, CGL_SWAP_INTERVAL, &1);
    }
    // with vsync forced, every frame we present is held to the display's rate; returns how long the game waited
    let swap = || {
        let from = now();
        if p.override_present_mode {
            spaced(&present)
        } else {
            present()
        }
        now() - from
    };
    // multiplier 1 only upscales: the game's frame through metalfx, no generator
    if p.multiplier < 2 {
        if front.plain.get(&key).is_none_or(|(e, s, _)| (*e, *s) != (extent, shown)) {
            if let Some((_, _, Some((src, u)))) = front.plain.remove(&key) {
                u.release(cgl);
                src.delete();
            }
            let made = match upscale.then(|| Up::new(cgl, extent, shown)).flatten() {
                Some(u) => match Surface::new(None, cgl, extent) {
                    Ok(src) => Some((src, u)),
                    Err(e) => {
                        u.release(cgl);
                        return Err(e);
                    }
                },
                None => {
                    not_upscaled(extent);
                    None
                }
            };
            front.plain.insert(key, (extent, shown, made));
        }
        let Some((_, _, Some((src, u)))) = front.plain.get_mut(&key) else {
            present();
            return Ok(());
        };
        let saved = Saved::take();
        blit(0, src.fbo, extent);
        glFinish();
        u.pin(cgl);
        u.show(&src.texture)?;
        drop(saved);
        swap();
        return Ok(());
    }
    if front.contexts.get(&key).is_none_or(|c| c.extent != extent) {
        *REFRESH.lock().unwrap() = None;
        if let Some(old) = front.contexts.remove(&key) {
            old.destroy(front.backend.as_ref());
        }
        if !Signature::new(p.performance_mode).fits(extent.0, extent.1, p.flow_for(extent.1)) {
            present();
            return Ok(());
        }
        let Some(c) = Context::build(front, s, cgl, extent)? else {
            present();
            return Ok(());
        };
        let adaptive = p.pacing_mode == PacingMode::Adaptive;
        let mode = if adaptive {
            "adaptive up to"
        } else {
            "multiplier"
        };
        log::info(&format!(
            "OpenGL presentation {}x{}, {mode} {}, flow {:.2}",
            extent.0, extent.1, p.multiplier, p.flow_for(extent.1)
        ));
        let mut c = c;
        c.up = upscale.then(|| Up::new(cgl, extent, shown)).flatten();
        if p.scaler != ScalerMode::Off && c.up.is_none() {
            not_upscaled(extent);
        }
        front.contexts.insert(key, c);
    }
    let c = front.contexts.get_mut(&key).unwrap();
    // the game waited in our swaps for the display; without that time the interval is how fast the game itself goes
    let blocked = std::mem::take(&mut c.held);
    let raw = c.estimator.sample(entered, blocked);
    let sample = Sample {
        interval: (raw.interval - blocked).max(0.0),
        trusted: raw.interval > 0.0,
    };
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
    // glfinish waits on the game's frame; a gl sync object would let the generator start earlier
    glFinish();
    if let Some(u) = &mut c.up {
        u.pin(cgl);
    }
    match &mut c.engine {
        Engine::Native(n) => {
            n.begin();
            let cb = n.cb()?;
            n.pipeline.copy_in(&cb, &c.surfaces[0].texture, n.iteration % 2)?;
            n.pipeline.encode(&cb, false, n.iteration, n.timestamp)?;
            n.commit(cb);
            if inserted == 0 {
                // the next swap writes the same surface
                n.settle()?;
            }
        }
        Engine::Vulkan(v) => {
            let b = front.backend.as_ref().ok_or("backend missing")?;
            let w = v.wrapper.as_mut().unwrap();
            v.mark_value += 1;
            w.dispatch(
                b,
                v.cmd[0],
                c.surfaces[0].image,
                (v.gate, 0),
                inserted as u32,
                (v.mark, v.mark_value),
            )?;
            if inserted == 0 {
                // nothing waits on this iteration's copy, and the next swap writes the same surface
                w.idle(b);
            }
        }
    }
    let mut held = 0.0;
    for (i, &ts) in slots.iter().take(inserted).enumerate() {
        match &mut c.engine {
            Engine::Native(n) => {
                n.produce(&c.surfaces[1 + i].texture, ts, None)?;
                n.settle()?;
            }
            Engine::Vulkan(v) => {
                let b = front.backend.as_ref().ok_or("backend missing")?;
                let d = &b.device;
                v.mark_value += 1;
                v.wrapper.as_mut().unwrap().acquire(
                    b,
                    v.cmd[1 + i],
                    c.surfaces[1 + i].image,
                    ts,
                    (v.mark, v.mark_value),
                    v.fence,
                )?;
                d.wait_for_fences(&[v.fence], true, GPU_TIMEOUT.as_nanos() as u64)
                    .map_err(|_| "OpenGL generated frame did not complete")?;
                check(d.reset_fences(&[v.fence]), "vkResetFences")?;
            }
        }
        c.show(1 + i, extent)?;
        if i + 1 == inserted && !show_original {
            drop(saved);
            held += swap();
            c.stats.generated += 1;
            c.held = held;
            c.count_source();
            return Ok(());
        }
        held += swap();
        c.stats.generated += 1;
    }
    // the back buffer is undefined after a swap, so the original frame comes back from its copy
    c.show(0, extent)?;
    drop(saved);
    held += swap();
    c.held = held;
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
    // native generator first; moltenvk when it is ruled out; none yet while the native one builds
    unsafe fn build(
        front: &mut Front,
        s: &'static Setup,
        cgl: *mut c_void,
        extent: (u32, u32),
    ) -> Result<Option<Context>, String> {
        // only a native generator that cannot be built turns it off; a surface error fails this context alone
        if !*front.native_off.get_or_insert_with(native_off) {
            let key = cgl as usize;
            // built on its own thread, so the game's swaps never wait on it
            match front.pending.get(&key).filter(|(e, _)| *e == extent).map(|(_, rx)| rx.try_recv()) {
                Some(Err(TryRecvError::Empty)) => return Ok(None),
                Some(Ok(Ok(n))) => {
                    front.pending.remove(&key);
                    return Context::new(None, Engine::Native(Box::new(n.get())), s, cgl, extent).map(Some);
                }
                Some(result) => {
                    front.pending.remove(&key);
                    let e = match result {
                        Ok(Err(e)) => e,
                        _ => "the build thread ended".into(),
                    };
                    log::warn(&format!("Native Metal generator unavailable ({e}); using MoltenVK"));
                    front.native_off = Some(true);
                }
                None => {
                    let (tx, rx) = channel();
                    std::thread::Builder::new()
                        .name("lsfg-metal native build".into())
                        .spawn(move || {
                            let built = objc2::rc::autoreleasepool(|_| {
                                MTLCreateSystemDefaultDevice()
                                    .ok_or_else(|| "no Metal device for the OpenGL context".to_string())
                                    .and_then(|d| Native::new(&d, s, extent, Colour::SDR))
                            });
                            let _ = tx.send(built.map(Unshared));
                        })
                        .map_err(|e| format!("no thread for the native build: {e}"))?;
                    front.pending.insert(key, (extent, rx));
                    return Ok(None);
                }
            }
        }
        if front.backend.is_none() {
            // no metal layer here to take a gpu from, so the first device stands
            front.backend = Some(Backend::create(s, None)?);
        }
        let b = front.backend.as_ref().ok_or("backend missing")?;
        let engine = Engine::Vulkan(Box::new(Vk {
            wrapper: None,
            cmd: Vec::new(),
            gate: vkutil::create_semaphore(&b.device, true)?,
            mark: vk::Semaphore::null(),
            mark_value: 0,
            fence: vk::Fence::null(),
        }));
        Context::new(Some(b), engine, s, cgl, extent).map(Some)
    }

    // on moltenvk with a backend, natively without one
    unsafe fn new(
        b: Option<&Backend>,
        engine: Engine,
        s: &'static Setup,
        cgl: *mut c_void,
        extent: (u32, u32),
    ) -> Result<Context, String> {
        let p = &s.profile;
        let m = p.multiplier;
        let adaptive = p.pacing_mode == PacingMode::Adaptive;
        let mut c = Context {
            extent,
            surfaces: Vec::new(),
            engine,
            pacer: adaptive
                .then(|| Pacer::new(objc2::rc::autoreleasepool(|_| display_refresh()), m)),
            estimator: Estimator::default(),
            held: 0.0,
            stats: Stats::default(),
            up: None,
        };
        let built = (|| {
            if let (Engine::Vulkan(v), Some(b)) = (&mut c.engine, b) {
                v.mark = vkutil::create_semaphore(&b.device, true)?;
                v.fence = vkutil::create_fence(&b.device)?;
            }
            for _ in 0..=m {
                c.surfaces.push(Surface::new(b, cgl, extent)?);
            }
            if let (Engine::Vulkan(v), Some(b)) = (&mut c.engine, b) {
                for _ in 0..=m {
                    v.cmd.push(vkutil::allocate_command_buffer(&b.device, b.pool)?);
                }
                v.wrapper = Some(Wrapper::new(b, extent, p.flow_for(extent.1), p.performance_mode, Colour::SDR)?);
            }
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

    // a frame into the default framebuffer, through metalfx when upscaling
    unsafe fn show(&self, k: usize, extent: (u32, u32)) -> Result<(), String> {
        match &self.up {
            Some(u) => u.show(&self.surfaces[k].texture),
            None => {
                blit(self.surfaces[k].fbo, 0, extent);
                Ok(())
            }
        }
    }

    // the backend is none only for a native context
    fn destroy(mut self, b: Option<&Backend>) {
        if let Some(u) = self.up.take() {
            unsafe { u.release(CGLGetCurrentContext()) };
        }
        match (&mut self.engine, b) {
            (Engine::Native(n), _) => {
                let _ = n.settle();
            }
            (Engine::Vulkan(v), Some(b)) => {
                let d = &b.device;
                if let Some(w) = v.wrapper.take() {
                    w.destroy(b);
                }
                unsafe {
                    for s in &self.surfaces {
                        d.destroy_image(s.image, None);
                    }
                    if !v.cmd.is_empty() {
                        d.free_command_buffers(b.pool, &v.cmd);
                    }
                    d.destroy_semaphore(v.gate, None);
                    d.destroy_semaphore(v.mark, None);
                    d.destroy_fence(v.fence, None);
                }
            }
            (Engine::Vulkan(_), None) => {}
        }
        unsafe {
            for s in self.surfaces.drain(..) {
                glDeleteFramebuffers(1, &s.fbo);
                glDeleteTextures(1, &s.gl_texture);
            }
        }
    }
}

impl Up {
    // none when metalfx cannot run here; the surface stays as the game set it
    unsafe fn new(cgl: *mut c_void, extent: (u32, u32), shown: (u32, u32)) -> Option<Up> {
        let device = MTLCreateSystemDefaultDevice()?;
        let scaler = metalfx::Scaler::new(&device, extent, shown, MTLPixelFormat::BGRA8Unorm, 0)?;
        let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            shown.0 as usize,
            shown.1 as usize,
            false,
        );
        desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite | MTLTextureUsage::RenderTarget);
        desc.setStorageMode(MTLStorageMode::Private);
        let mid = device.newTextureWithDescriptor(&desc)?;
        let queue = device.newCommandQueue()?;
        let out = Surface::new(None, cgl, shown).ok()?;
        log::info(&format!(
            "MetalFX upscaling {}x{} to {}x{}",
            extent.0, extent.1, shown.0, shown.1
        ));
        Some(Up {
            shown,
            out,
            mid,
            scaler,
            queue,
            prior: None,
            pinned: false,
        })
    }

    // the surface at the shown size, checked every frame: wine drops its own pin when a window is hidden or a pbuffer is made current
    unsafe fn pin(&mut self, cgl: *mut c_void) {
        let mut on = 0;
        let mut size = [0i32; 2];
        let enabled = CGLIsEnabled(cgl, CGL_ENABLE_SURFACE_BACKING_SIZE, &mut on) == 0 && on != 0;
        if enabled && CGLGetParameter(cgl, CGL_SURFACE_BACKING_SIZE, size.as_mut_ptr()) != 0 {
            size = [0, 0];
        }
        let shown = [self.shown.0 as i32, self.shown.1 as i32];
        if enabled && size == shown {
            return;
        }
        log::debug(&format!(
            "OpenGL surface pinned to {}x{} again (it was {})",
            shown[0],
            shown[1],
            if enabled { format!("{}x{}", size[0], size[1]) } else { "unpinned".into() }
        ));
        // whatever stands now is what the game or wine wants back later
        self.prior = enabled.then_some(size);
        self.pinned = true;
        CGLSetParameter(cgl, CGL_SURFACE_BACKING_SIZE, shown.as_ptr());
        CGLEnable(cgl, CGL_ENABLE_SURFACE_BACKING_SIZE);
    }

    // the frame scaled into the shared surface, then drawn over the whole default framebuffer
    unsafe fn show(&self, src: &ProtocolObject<dyn MTLTexture>) -> Result<(), String> {
        // the previous frame's gl copy out of the shared surface must be done before metal writes it again
        glFinish();
        let cb = self.queue.commandBuffer().ok_or("no Metal command buffer")?;
        self.scaler.encode(&cb, src, &self.mid);
        let copy = cb.blitCommandEncoder().ok_or("no Metal blit encoder")?;
        copy.copyFromTexture_toTexture(&self.mid, &self.out.texture);
        copy.endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        blit(self.out.fbo, 0, self.shown);
        Ok(())
    }

    // the surface back to the size it had before
    unsafe fn release(self, cgl: *mut c_void) {
        if self.pinned && !cgl.is_null() {
            match self.prior {
                Some(p) => {
                    CGLSetParameter(cgl, CGL_SURFACE_BACKING_SIZE, p.as_ptr());
                }
                None => {
                    CGLDisable(cgl, CGL_ENABLE_SURFACE_BACKING_SIZE);
                }
            }
        }
        self.out.delete();
    }
}

impl Surface {
    unsafe fn delete(self) {
        glDeleteFramebuffers(1, &self.fbo);
        glDeleteTextures(1, &self.gl_texture);
    }

    unsafe fn new(b: Option<&Backend>, cgl: *mut c_void, (w, h): (u32, u32)) -> Result<Surface, String> {
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
        // the game's own rectangle texture stays bound
        let mut bound = 0i32;
        glGetIntegerv(GL_TEXTURE_BINDING_RECTANGLE, &mut bound);
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
        glBindTexture(GL_TEXTURE_RECTANGLE, bound as u32);
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
        } else if let Some(b) = b {
            import_texture(
                &b.device,
                &texture,
                image_info(
                    vk::Format::B8G8R8A8_UNORM,
                    (w, h),
                    vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
                ),
            )
        } else {
            Ok(vk::Image::null())
        };
        match image {
            Ok(image) => Ok(Surface {
                _surface: surface,
                texture,
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
