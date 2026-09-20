// mtl4clear smoke test: the same clear as mtlclear, presented through metal 4
// metal 4 has no presentDrawable: and no commit on the command buffer; the queue waits on and
// signals the drawable and the drawable is presented directly, which is what d3dmetal 4 does
// MTL4TEST_FPS paces the source rate, MTL4TEST_WIDTH and MTL4TEST_HEIGHT size the window, argv[1] = frame count
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
    MTL4CommandAllocator, MTL4CommandBuffer, MTL4CommandEncoder, MTL4CommandQueue,
    MTL4RenderPassDescriptor, MTLClearColor, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable,
    MTLLoadAction, MTLPixelFormat, MTLStoreAction,
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
        env_f64("MTL4TEST_WIDTH").unwrap_or(640.0),
        env_f64("MTL4TEST_HEIGHT").unwrap_or(480.0),
    );
    let fps = env_f64("MTL4TEST_FPS");

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
    window.setTitle(&NSString::from_str("mtl4clear"));
    let device = MTLCreateSystemDefaultDevice().expect("metal device");
    let layer = CAMetalLayer::new();
    layer.setDevice(Some(&device));
    layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    layer.setDrawableSize(CGSize::new(w, h));
    layer.setFramebufferOnly(false);
    let view = window.contentView().expect("content view");
    view.setWantsLayer(true);
    view.setLayer(Some(&layer));
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    // the render loop runs off the main thread so the app can pump events
    let layer_ptr = Retained::into_raw(layer) as usize;
    std::thread::spawn(move || {
        let layer = unsafe { Retained::from_raw(layer_ptr as *mut CAMetalLayer) }.unwrap();
        render(&layer, &device, frames, fps);
        std::process::exit(0);
    });
    app.run();
}

fn render(
    layer: &CAMetalLayer,
    device: &ProtocolObject<dyn MTLDevice>,
    frames: usize,
    fps: Option<f64>,
) {
    let queue = device.newMTL4CommandQueue().expect("mtl4 queue");
    let allocator = device.newCommandAllocator().expect("command allocator");
    let cb = device.newCommandBuffer().expect("mtl4 command buffer");
    let start = Instant::now();
    let mut shown = 0;
    for i in 0..frames {
        // drawables come back autoreleased, a real game's frame loop drains its pool too
        objc2::rc::autoreleasepool(|_| {
            let Some(drawable) = layer.nextDrawable() else {
                return;
            };
            allocator.reset();
            cb.beginCommandBufferWithAllocator(&allocator);
            let pass = MTL4RenderPassDescriptor::new();
            let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            att.setTexture(Some(&drawable.texture()));
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
            cb.endCommandBuffer();
            let d: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
            queue.waitForDrawable(d);
            let mut one = NonNull::from(&*cb);
            unsafe { queue.commit_count(NonNull::from(&mut one), 1) };
            queue.signalDrawable(d);
            d.present();
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
