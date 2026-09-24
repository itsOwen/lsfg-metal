// swizzles on CAMetalLayer and the driver's command buffer class
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, Once, OnceLock};

use ash::vk::{self, Handle};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Imp, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{define_class, ffi, msg_send, sel, AnyThread, ClassType, DefinedClass, Message};
use objc2_foundation::NSObject;
use objc2_core_graphics::CGColorSpace;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLEvent,
    MTLPixelFormat,
};
use objc2_quartz_core::CAMetalLayer;

use super::drawable::ProxyDrawable;
use super::generator::{Generator, Job};
use super::{enabled, now, set_enabled};
use crate::generator::signature::{Colour, Store};
use crate::log;

type NextDrawableFn = unsafe extern "C-unwind" fn(*mut CAMetalLayer, Sel) -> *mut AnyObject;
type PresentFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);
type PresentTimedFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, f64);
type CommitFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);

struct CbHooks {
    present: PresentFn,
    present_min: PresentTimedFn,
    present_at: PresentTimedFn,
    commit: CommitFn,
}

static NEXT_DRAWABLE: OnceLock<NextDrawableFn> = OnceLock::new();
static CB_HOOKS: OnceLock<CbHooks> = OnceLock::new();
static INSTALL: Mutex<()> = Mutex::new(());
static INIT: Once = Once::new();
// layers owned by vulkan surfaces and the surface -> layer map
static VULKAN_LAYERS: Mutex<Option<HashSet<usize>>> = Mutex::new(None);
static SURFACES: Mutex<Option<HashMap<u64, usize>>> = Mutex::new(None);
type CommandBuffer = Retained<ProtocolObject<dyn MTLCommandBuffer>>;
// pending present attached to the command buffer as an associated object; dies with it
static PENDING_KEY: u8 = 0;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "LSFGMPendingPresent"]
    #[ivars = RefCell<Vec<(Retained<ProxyDrawable>, f64)>>]
    struct PendingPresent;

    unsafe impl NSObjectProtocol for PendingPresent {}
);

thread_local! {
    static IS_WORKER: Cell<bool> = const { Cell::new(false) };
    // last command buffer this thread committed, for direct presents on our drawables
    static LAST_COMMITTED: RefCell<Option<CommandBuffer>> = const { RefCell::new(None) };
}

pub(crate) fn mark_worker() {
    IS_WORKER.with(|w| w.set(true));
}


// swizzle nextDrawable once; the command buffer hooks follow on first use
pub fn install() {
    let _g = INSTALL.lock().unwrap();
    if NEXT_DRAWABLE.get().is_some() {
        return;
    }
    let Some(m) = CAMetalLayer::class().instance_method(sel!(nextDrawable)) else {
        return;
    };
    // publish the original before swapping: a caller racing the swap must find it
    let _ = NEXT_DRAWABLE.set(unsafe { std::mem::transmute::<Imp, NextDrawableFn>(m.implementation()) });
    unsafe {
        m.set_implementation(std::mem::transmute::<*const (), Imp>(
            next_drawable_hook as *const (),
        ))
    };
}

// probe a command buffer of the layer's device (or the default device) and hook its concrete class
pub(crate) fn install_cb_hooks(layer: &CAMetalLayer) {
    let _g = INSTALL.lock().unwrap();
    if CB_HOOKS.get().is_some() {
        return;
    }
    let device = layer.device().or_else(|| MTLCreateSystemDefaultDevice());
    let Some(cb) = device
        .and_then(|d| d.newCommandQueue())
        .and_then(|q| q.commandBuffer())
    else {
        return;
    };
    let cls = unsafe { &*(Retained::as_ptr(&cb) as *const AnyObject) }.class();
    let sels = [
        sel!(presentDrawable:),
        sel!(presentDrawable:afterMinimumDuration:),
        sel!(presentDrawable:atTime:),
        sel!(commit),
    ];
    let hooks = [
        present_hook as *const (),
        present_min_hook as *const (),
        present_at_hook as *const (),
        commit_hook as *const (),
    ];
    // all four or nothing: a partial swizzle would leave presents without a commit
    let Some(methods) = sels
        .iter()
        .map(|s| cls.instance_method(*s))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    // publish the originals before swapping: a commit racing the swap must find them
    let imp: Vec<Imp> = methods.iter().map(|m| m.implementation()).collect();
    unsafe {
        let _ = CB_HOOKS.set(CbHooks {
            present: std::mem::transmute::<Imp, PresentFn>(imp[0]),
            present_min: std::mem::transmute::<Imp, PresentTimedFn>(imp[1]),
            present_at: std::mem::transmute::<Imp, PresentTimedFn>(imp[2]),
            commit: std::mem::transmute::<Imp, CommitFn>(imp[3]),
        });
        for (m, h) in methods.iter().zip(hooks) {
            m.set_implementation(std::mem::transmute::<*const (), Imp>(h));
        }
    }
    log::info(&format!(
        "Metal presentation hooks installed on {}",
        cls.name().to_string_lossy()
    ));
}

// the layer's real nextDrawable, for the worker and the fallbacks
pub(crate) fn original_next_drawable(
    layer: &CAMetalLayer,
) -> Option<Retained<ProtocolObject<dyn objc2_quartz_core::CAMetalDrawable>>> {
    match NEXT_DRAWABLE.get() {
        Some(f) => unsafe {
            Retained::retain_autoreleased(f(layer as *const _ as *mut _, sel!(nextDrawable)).cast())
        },
        None => layer.nextDrawable(),
    }
}

pub(crate) fn last_committed() -> Option<CommandBuffer> {
    LAST_COMMITTED.with(|c| c.borrow().clone())
}

pub(crate) fn is_vulkan_layer(layer: &CAMetalLayer) -> bool {
    VULKAN_LAYERS
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|s| s.contains(&(layer as *const _ as usize)))
}

pub(crate) fn layer_of_surface(surface: vk::SurfaceKHR) -> Option<Retained<CAMetalLayer>> {
    let p = *SURFACES.lock().unwrap().as_ref()?.get(&surface.as_raw())?;
    unsafe { Retained::retain(p as *mut CAMetalLayer) }
}

// remember a layer vulkan presents to, and (surface -> layer) once the surface exists
pub fn register_layer(layer: *const c_void, surface: Option<vk::SurfaceKHR>) {
    if layer.is_null() {
        return;
    }
    VULKAN_LAYERS
        .lock()
        .unwrap()
        .get_or_insert_with(HashSet::new)
        .insert(layer as usize);
    if let Some(s) = surface {
        SURFACES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(s.as_raw(), layer as usize);
    }
}

// drop the (surface -> layer) entry, and the layer once no other surface maps to it
pub fn forget_surface(surface: vk::SurfaceKHR) {
    let mut surfaces = SURFACES.lock().unwrap();
    let Some(layer) = surfaces
        .as_mut()
        .and_then(|m| m.remove(&surface.as_raw()))
    else {
        return;
    };
    let shared = surfaces
        .as_ref()
        .is_some_and(|m| m.values().any(|&l| l == layer));
    drop(surfaces);
    if !shared {
        if let Some(s) = VULKAN_LAYERS.lock().unwrap().as_mut() {
            s.remove(&layer);
        }
    }
}

// every layer format a game can present, with the colour space deciding only the colour kind; None presents natively
pub(crate) fn layer_supported(layer: &CAMetalLayer) -> bool {
    // the xr formats need the native generator; with it switched off they present natively like any unsupported format
    let ok = colour(layer.pixelFormat(), layer.colorspace().as_deref())
        .is_some_and(|c| c.store != Store::Bgr10Xr || !super::generator::native_off());
    if !ok {
        // warn once per format and colour space, nextDrawable runs every frame
        static SEEN: Mutex<Vec<(usize, Option<String>)>> = Mutex::new(Vec::new());
        let key = (
            layer.pixelFormat().0,
            CGColorSpace::name(layer.colorspace().as_deref()).map(|n| n.to_string()),
        );
        let mut seen = SEEN.lock().unwrap();
        if !seen.contains(&key) {
            log::warn(&format!(
                "Frame generation disabled for this Metal layer (pixel format {}, colour space {:?}); presenting natively",
                key.0, key.1
            ));
            seen.push(key);
        }
    }
    ok
}

// storage that keeps the layer's precision at 32 bits a pixel, and the colour kind from whether its colour space is linear
pub(crate) fn colour(format: MTLPixelFormat, space: Option<&CGColorSpace>) -> Option<Colour> {
    let (linear, extended) = space.map_or((false, false), |s| {
        // a linear space linearizes to itself; the legacy generic linear one comes back unnamed, so its name decides
        let named = CGColorSpace::name(Some(s)).is_some_and(|n| n.to_string().contains("Linear"));
        (named || s.linearized().is_some_and(|l| &*l == s), s.uses_extended_range())
    });
    let kind = match (linear, extended) {
        (false, _) => 0,
        (true, false) => 1,
        (true, true) => 2,
    };
    let store = match format {
        MTLPixelFormat::BGRA8Unorm
        | MTLPixelFormat::BGRA8Unorm_sRGB
        | MTLPixelFormat::RGBA8Unorm
        | MTLPixelFormat::RGBA8Unorm_sRGB => Store::Rgba8,
        MTLPixelFormat::RGB10A2Unorm | MTLPixelFormat::BGR10A2Unorm => Store::Rgb10a2,
        MTLPixelFormat::BGR10_XR
        | MTLPixelFormat::BGR10_XR_sRGB
        | MTLPixelFormat::BGRA10_XR
        | MTLPixelFormat::BGRA10_XR_sRGB => Store::Bgr10Xr,
        // linear hdr keeps half float, as it always has; encoded float needs range above 1 but not half float's cost
        MTLPixelFormat::RGBA16Float if kind == 2 => Store::Rgba16f,
        MTLPixelFormat::RGBA16Float => Store::Rgb9e5,
        _ => return None,
    };
    Some(Colour { store, kind })
}

// hand the game one of our drawables instead of the layer's real one
unsafe extern "C-unwind" fn next_drawable_hook(
    this: *mut CAMetalLayer,
    sel: Sel,
) -> *mut AnyObject {
    let layer = &*this;
    INIT.call_once(|| {
        if super::setup().is_some_and(|s| s.profile.multiplier > 1) {
            set_enabled(true);
            log::info("lsfg-metal metal front end active");
        }
    });
    if enabled() && !is_vulkan_layer(layer) && layer_supported(layer) {
        install_cb_hooks(layer);
        if layer.maximumDrawableCount() < 3 {
            layer.setMaximumDrawableCount(3);
        }
        // vsync mode presents on refresh; with the override off the game's own setting stands
        if super::setup().is_some_and(|s| s.profile.override_present_mode)
            && !layer.displaySyncEnabled()
        {
            layer.setDisplaySyncEnabled(true);
        }
        // the worker blits into the real drawables
        if layer.framebufferOnly() {
            layer.setFramebufferOnly(false);
        }
        if let Some(gen) = Generator::get(layer) {
            let t0 = now();
            let d = gen.acquire_drawable();
            gen.add_blocked(now() - t0);
            if let Some(d) = d {
                return Retained::autorelease_return(d).cast();
            }
        }
    }
    (NEXT_DRAWABLE.get().unwrap())(this, sel)
}

fn ours(drawable: *mut AnyObject) -> Option<Retained<ProxyDrawable>> {
    unsafe {
        if drawable.is_null() || (*drawable).class() != ProxyDrawable::class() {
            return None;
        }
        Some((*(drawable as *mut ProxyDrawable)).retain())
    }
}

// a command buffer can carry several presents; each one is kept in order
fn attach(cb: *mut AnyObject, drawable: Retained<ProxyDrawable>, duration: f64) {
    let key = &PENDING_KEY as *const u8 as *const c_void;
    let existing = unsafe { ffi::objc_getAssociatedObject(cb, key) as *const PendingPresent };
    if let Some(p) = unsafe { existing.as_ref() } {
        p.ivars().borrow_mut().push((drawable, duration));
        return;
    }
    let list = RefCell::new(vec![(drawable, duration)]);
    let p: Retained<PendingPresent> =
        unsafe { msg_send![super(PendingPresent::alloc().set_ivars(list)), init] };
    unsafe {
        ffi::objc_setAssociatedObject(
            cb,
            &PENDING_KEY as *const u8 as *const c_void,
            Retained::as_ptr(&p) as *mut AnyObject,
            ffi::OBJC_ASSOCIATION_RETAIN,
        )
    };
}

// take the pending presents off the command buffer
fn detach(cb: *mut AnyObject) -> Vec<(Retained<ProxyDrawable>, f64)> {
    let key = &PENDING_KEY as *const u8 as *const c_void;
    unsafe {
        let p = ffi::objc_getAssociatedObject(cb, key) as *const PendingPresent;
        let Some(p) = Retained::retain(p as *mut PendingPresent) else {
            return Vec::new();
        };
        ffi::objc_setAssociatedObject(cb, key, std::ptr::null_mut(), ffi::OBJC_ASSOCIATION_RETAIN);
        p.ivars().take()
    }
}

// present hooks: attach a pending present for our drawables, else the original
unsafe extern "C-unwind" fn present_hook(this: *mut AnyObject, sel: Sel, drawable: *mut AnyObject) {
    match ours(drawable) {
        Some(d) => attach(this, d, 0.0),
        None => (CB_HOOKS.get().unwrap().present)(this, sel, drawable),
    }
}

unsafe extern "C-unwind" fn present_min_hook(
    this: *mut AnyObject,
    sel: Sel,
    drawable: *mut AnyObject,
    duration: f64,
) {
    match ours(drawable) {
        Some(d) => attach(this, d, duration),
        None => (CB_HOOKS.get().unwrap().present_min)(this, sel, drawable, duration),
    }
}

unsafe extern "C-unwind" fn present_at_hook(
    this: *mut AnyObject,
    sel: Sel,
    drawable: *mut AnyObject,
    time: f64,
) {
    match ours(drawable) {
        Some(d) => attach(this, d, 0.0),
        None => (CB_HOOKS.get().unwrap().present_at)(this, sel, drawable, time),
    }
}

// commit: signal the game event on the command buffer and hand the frame to the worker
unsafe extern "C-unwind" fn commit_hook(this: *mut AnyObject, sel: Sel) {
    let orig = CB_HOOKS.get().unwrap().commit;
    let cb = &*(this as *const ProtocolObject<dyn MTLCommandBuffer>);
    let pending: Vec<_> = detach(this)
        .into_iter()
        .filter_map(|(d, dur)| d.owner().map(|g| (d, dur, g)))
        .map(|(drawable, duration, gen)| {
            let serial = if enabled() {
                let serial = gen.next_serial();
                cb.encodeSignalEvent_value(
                    ProtocolObject::<dyn MTLEvent>::from_ref(gen.game_event()),
                    serial,
                );
                serial
            } else {
                0
            };
            (drawable, duration, gen, serial)
        })
        .collect();
    orig(this, sel);
    for (drawable, duration, gen, serial) in pending {
        gen.enqueue(Job {
            latency: Default::default(),
            cb: Some(cb.retain()),
            drawable,
            duration,
            serial,
            sample: gen.frame_sample(),
        });
    }
    if !IS_WORKER.with(|w| w.get()) {
        let _ = LAST_COMMITTED.try_with(|c| *c.borrow_mut() = Some(cb.retain()));
    }
}

#[cfg(test)]
mod tests {
    use super::colour;
    use crate::generator::signature::{Colour, Store};
    use objc2_core_graphics::*;
    use objc2_metal::MTLPixelFormat;

    #[test]
    fn layer_formats_keep_their_precision_and_linear_light_picks_the_kind() {
        let c = |f, name: Option<&objc2_core_foundation::CFString>| {
            let space = name.and_then(|n| CGColorSpace::with_name(Some(n)));
            colour(f, space.as_deref())
        };
        let s = |store, kind| Some(Colour { store, kind });
        unsafe {
            assert_eq!(c(MTLPixelFormat::BGRA8Unorm, None), Some(Colour::SDR));
            assert_eq!(c(MTLPixelFormat::BGRA8Unorm_sRGB, Some(kCGColorSpaceSRGB)), Some(Colour::SDR));
            assert_eq!(c(MTLPixelFormat::RGB10A2Unorm, Some(kCGColorSpaceSRGB)), s(Store::Rgb10a2, 0));
            assert_eq!(c(MTLPixelFormat::BGR10A2Unorm, Some(kCGColorSpaceITUR_2100_PQ)), s(Store::Rgb10a2, 0));
            // scrgb stays exactly as before: half float and the hdr kind
            assert_eq!(c(MTLPixelFormat::RGBA16Float, Some(kCGColorSpaceExtendedLinearSRGB)), Some(Colour::HDR));
            assert_eq!(c(MTLPixelFormat::RGBA16Float, Some(kCGColorSpaceExtendedLinearDisplayP3)), Some(Colour::HDR));
            // gamma-encoded float, as unity presents it: range above 1 at 32 bits a pixel
            assert_eq!(c(MTLPixelFormat::RGBA16Float, Some(kCGColorSpaceExtendedSRGB)), s(Store::Rgb9e5, 0));
            assert_eq!(c(MTLPixelFormat::RGBA16Float, Some(kCGColorSpaceITUR_2100_PQ)), s(Store::Rgb9e5, 0));
            assert_eq!(c(MTLPixelFormat::RGBA16Float, Some(kCGColorSpaceLinearSRGB)), s(Store::Rgb9e5, 1));
            assert_eq!(c(MTLPixelFormat::RGBA16Float, Some(kCGColorSpaceGenericRGBLinear)), s(Store::Rgb9e5, 1));
            assert_eq!(c(MTLPixelFormat::BGR10_XR_sRGB, Some(kCGColorSpaceExtendedSRGB)), s(Store::Bgr10Xr, 0));
            assert_eq!(c(MTLPixelFormat::BGRA10_XR, Some(kCGColorSpaceExtendedDisplayP3)), s(Store::Bgr10Xr, 0));
            assert_eq!(c(MTLPixelFormat::R8Unorm, None), None);
        }
    }
}
