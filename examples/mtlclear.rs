// mtlclear smoke test: clears a CAMetalLayer drawable and blits a 32x32 white square moving 8 px per frame
// MTLTEST_FPS paces the source rate, MTLTEST_MINDURATION=<fps> presents with afterMinimumDuration, argv[1] = frame count
// MTLTEST_WIDTH and MTLTEST_HEIGHT size the window, default 640x480
// MTLTEST_FORMAT=<raw MTLPixelFormat> sets the layer format; MTLTEST_DOUBLE presents a second layer on the same command buffer
// run with the shim on DYLD_INSERT_LIBRARIES and LSFGM_METAL=1; the example must not link the crate itself
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLRegion, MTLRenderPassDescriptor, MTLSize, MTLStoreAction, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()?
        .parse()
        .ok()
        .filter(|v: &f64| *v > 0.0)
}

fn main() {
    let frames: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(240);
    let (w, h) = (
        env_f64("MTLTEST_WIDTH").unwrap_or(640.0),
        env_f64("MTLTEST_HEIGHT").unwrap_or(480.0),
    );
    let fps = env_f64("MTLTEST_FPS");
    let min_duration =
        env_f64("MTLTEST_MINDURATION").map(|f| if f <= 1.0 { 1.0 / 60.0 } else { 1.0 / f });

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let rect = CGRect::new(CGPoint::new(100.0, 100.0), CGSize::new(w, h));
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("mtlclear"));
    let device = MTLCreateSystemDefaultDevice().expect("metal device");
    let layer = CAMetalLayer::new();
    layer.setDevice(Some(&device));
    let format = std::env::var("MTLTEST_FORMAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(MTLPixelFormat::BGRA8Unorm, MTLPixelFormat);
    layer.setPixelFormat(format);
    layer.setDrawableSize(CGSize::new(w, h));
    layer.setFramebufferOnly(false);
    let view = window.contentView().expect("content view");
    view.setWantsLayer(true);
    view.setLayer(Some(&layer));
    let second = std::env::var_os("MTLTEST_DOUBLE").map(|_| {
        let l = CAMetalLayer::new();
        l.setDevice(Some(&device));
        l.setPixelFormat(format);
        l.setDrawableSize(CGSize::new(w / 2.0, h / 2.0));
        l.setFramebufferOnly(false);
        l.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w / 2.0, h / 2.0)));
        layer.addSublayer(&l);
        Retained::into_raw(l) as usize
    });
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    // the render loop runs off the main thread so the app can pump events
    let layer: Retained<CAMetalLayer> = layer;
    let layer_ptr = Retained::into_raw(layer) as usize;
    std::thread::spawn(move || {
        let layer = unsafe { Retained::from_raw(layer_ptr as *mut CAMetalLayer) }.unwrap();
        let second = second.map(|p| unsafe { Retained::from_raw(p as *mut CAMetalLayer) }.unwrap());
        render(&layer, second.as_deref(), &device, frames, fps, min_duration);
        std::process::exit(0);
    });
    app.run();
}

fn render(
    layer: &CAMetalLayer,
    second: Option<&CAMetalLayer>,
    device: &ProtocolObject<dyn MTLDevice>,
    frames: usize,
    fps: Option<f64>,
    min_duration: Option<f64>,
) {
    let queue = device.newCommandQueue().expect("queue");
    let square = {
        let d = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                32,
                32,
                false,
            )
        };
        d.setUsage(MTLTextureUsage::ShaderRead);
        let t = device.newTextureWithDescriptor(&d).expect("square texture");
        let white = vec![255u8; 32 * 32 * 4];
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: 32,
                height: 32,
                depth: 1,
            },
        };
        unsafe {
            t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(white.as_ptr() as *mut _).unwrap(),
                32 * 4,
            )
        };
        t
    };
    let size = layer.drawableSize();
    let (w, h) = (size.width as usize, size.height as usize);
    let start = Instant::now();
    let mut shown = 0;
    for i in 0..frames {
        // drawables come back autoreleased, a real game's frame loop drains its pool too
        objc2::rc::autoreleasepool(|_| {
            let Some(drawable) = layer.nextDrawable() else {
                return;
            };
            let target = drawable.texture();
            let cb = queue.commandBuffer().expect("command buffer");
            // the second layer is presented first, so a lost first present shows on it
            if let Some(extra) = second.and_then(|l| l.nextDrawable()) {
                let pass = MTLRenderPassDescriptor::new();
                let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
                att.setTexture(Some(&extra.texture()));
                att.setLoadAction(MTLLoadAction::Clear);
                att.setStoreAction(MTLStoreAction::Store);
                att.setClearColor(MTLClearColor { red: 0.8, green: 0.2, blue: 0.2, alpha: 1.0 });
                cb.renderCommandEncoderWithDescriptor(&pass).expect("encoder").endEncoding();
                cb.presentDrawable(ProtocolObject::from_ref(&*extra));
            }
            let pass = MTLRenderPassDescriptor::new();
            let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            att.setTexture(Some(&target));
            att.setLoadAction(MTLLoadAction::Clear);
            att.setStoreAction(MTLStoreAction::Store);
            let c = (i % 60) as f64 / 60.0;
            att.setClearColor(MTLClearColor {
                red: 0.1,
                green: 0.2 + 0.5 * c,
                blue: 0.4,
                alpha: 1.0,
            });
            cb.renderCommandEncoderWithDescriptor(&pass)
                .expect("encoder")
                .endEncoding();
            let x = (i * 8) % (w - 32);
            let y = (i * 8 / (w - 32) * 40) % (h - 32);
            if target.pixelFormat() != MTLPixelFormat::BGRA8Unorm {
                let d: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
                cb.presentDrawable(d);
                cb.commit();
                shown += 1;
                return;
            }
            let blit = cb.blitCommandEncoder().expect("blit");
            unsafe {
                blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                &square,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
                MTLSize { width: 32, height: 32, depth: 1 },
                &target,
                0,
                0,
                MTLOrigin { x, y, z: 0 },
            );
            }
            blit.endEncoding();
            let d: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
            match min_duration {
                Some(m) => cb.presentDrawable_afterMinimumDuration(d, m),
                None => cb.presentDrawable(d),
            }
            cb.commit();
            shown += 1;
        });
        if let Some(fps) = fps {
            let due = start + Duration::from_secs_f64((i + 1) as f64 / fps);
            if let Some(left) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(left);
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "{shown} frames in {secs:.2}s = {:.1} fps",
        shown as f64 / secs
    );
}
