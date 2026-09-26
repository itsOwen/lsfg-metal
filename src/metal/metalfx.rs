// metalfx spatial scaler, bound at runtime: the framework only exists from macos 13 and the shim loads on 11
use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::msg_send;
use objc2_metal::{MTLCommandBuffer, MTLDevice, MTLPixelFormat, MTLTexture};
use objc2_quartz_core::CAMetalLayer;

extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

fn descriptor_class() -> Option<&'static AnyClass> {
    static LOADED: OnceLock<bool> = OnceLock::new();
    let loaded = *LOADED.get_or_init(|| {
        let path = c"/System/Library/Frameworks/MetalFX.framework/MetalFX";
        !unsafe { dlopen(path.as_ptr(), 0x1) }.is_null()
    });
    loaded.then(|| AnyClass::get(c"MTLFXSpatialScalerDescriptor")).flatten()
}

// the layer in physical pixels, what an upscaled frame is shown at
pub fn shown_size(layer: &CAMetalLayer) -> (u32, u32) {
    let (b, s) = (layer.bounds().size, screen_scale());
    ((b.width * s).round() as u32, (b.height * s).round() as u32)
}

// the main screen's scale, kept once read: a window on a display with another scale is upscaled to the main one's
pub(crate) fn screen_scale() -> f64 {
    static SCALE: OnceLock<f64> = OnceLock::new();
    if let Some(s) = SCALE.get() {
        return *s;
    }
    // appkit may not be loaded yet, and no screen is no answer: both are asked again next time
    let Some(cls) = AnyClass::get(c"NSScreen") else {
        return 1.0;
    };
    let screen: *mut AnyObject = unsafe { msg_send![cls, mainScreen] };
    if screen.is_null() {
        return 1.0;
    }
    *SCALE.get_or_init(|| unsafe { msg_send![screen, backingScaleFactor] })
}

// false before macos 13 and on gpus metalfx does not run on
pub fn supported(device: &ProtocolObject<dyn MTLDevice>) -> bool {
    descriptor_class().is_some_and(|c| unsafe { msg_send![c, supportsDevice: device] })
}

pub struct Scaler(Retained<AnyObject>);

impl Scaler {
    // one input and output format; colour kind 0 encoded, 1 linear, 2 linear hdr, as the generator classifies it
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        input: (u32, u32),
        output: (u32, u32),
        format: MTLPixelFormat,
        kind: u32,
    ) -> Option<Scaler> {
        // metalfx asserts on srgb formats outside perceptual mode; callers pass unorm views
        if srgb(format) || !supported(device) {
            return None;
        }
        let class = descriptor_class()?;
        unsafe {
            let d: Option<Retained<AnyObject>> = msg_send![class, new];
            let d = d?;
            let _: () = msg_send![&d, setColorTextureFormat: format];
            let _: () = msg_send![&d, setOutputTextureFormat: format];
            let _: () = msg_send![&d, setInputWidth: input.0 as usize];
            let _: () = msg_send![&d, setInputHeight: input.1 as usize];
            let _: () = msg_send![&d, setOutputWidth: output.0 as usize];
            let _: () = msg_send![&d, setOutputHeight: output.1 as usize];
            let _: () = msg_send![&d, setColorProcessingMode: kind.min(2) as isize];
            let s: Option<Retained<AnyObject>> = msg_send![&d, newSpatialScalerWithDevice: device];
            s.map(Scaler)
        }
    }

    // the output must be a private texture; drawables are not, so it goes to an intermediate first
    pub fn encode(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        src: &ProtocolObject<dyn MTLTexture>,
        dst: &ProtocolObject<dyn MTLTexture>,
    ) {
        unsafe {
            let _: () = msg_send![&self.0, setColorTexture: src];
            let _: () = msg_send![&self.0, setOutputTexture: dst];
            let _: () = msg_send![&self.0, encodeToCommandBuffer: cb];
        }
    }
}

fn srgb(format: MTLPixelFormat) -> bool {
    matches!(
        format,
        MTLPixelFormat::BGRA8Unorm_sRGB
            | MTLPixelFormat::RGBA8Unorm_sRGB
            | MTLPixelFormat::BGR10_XR_sRGB
            | MTLPixelFormat::BGRA10_XR_sRGB
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_metal::MTLCreateSystemDefaultDevice;

    #[test]
    fn scaler_builds_or_declines_cleanly() {
        let Some(device) = MTLCreateSystemDefaultDevice() else { return };
        let s = Scaler::new(&device, (1470, 920), (2940, 1840), MTLPixelFormat::BGRA8Unorm, 0);
        assert_eq!(s.is_some(), supported(&device));
        assert!(Scaler::new(&device, (1470, 920), (2940, 1840), MTLPixelFormat::BGRA8Unorm_sRGB, 0).is_none());
    }
}
