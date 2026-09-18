// metal presenter and proxy swapchain
mod drawable;
mod generator;
mod gl;
mod hooks;
mod latency;
mod proxy;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use ash::vk;
use objc2_metal::MTLPixelFormat;

use crate::settings::Profile;

pub use hooks::{forget_surface, register_layer};

// metal layer swizzles plus the opengl buffer swap
pub fn install() {
    hooks::install();
    gl::install();
}

// only the opengl buffer swap, for processes whose metal layers belong to a vulkan driver
pub fn install_opengl() {
    gl::install();
}
pub use proxy::{
    invalidate_proxies, proxy_supported, set_proxy_supported, GameDevice, ProxySwapchain, QueueLock,
};

// front end enabled; a failure clears it for good
static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed)
}

// the active profile plus the global options the presenter needs
pub struct Setup {
    pub profile: Profile,
    pub allow_fp16: bool,
    pub dll: PathBuf,
}

impl Setup {
    // front ends pass their raw config values; the dll path is resolved here so none of them repeats it
    pub fn new(profile: Profile, allow_fp16: bool, dll: Option<String>) -> Setup {
        let dll = dll
            .as_deref()
            .map(|d| crate::shaders::fix_dll_path(std::path::Path::new(d)))
            .or_else(crate::shaders::find_dll)
            .unwrap_or_default();
        Setup {
            profile,
            allow_fp16,
            dll,
        }
    }
}

static SETUP: OnceLock<Setup> = OnceLock::new();

// pushed by a front end as it activates; the first push wins
pub fn init(setup: Setup) {
    let _ = SETUP.set(setup);
}

// None until a front end has pushed its configuration
pub fn setup() -> Option<&'static Setup> {
    SETUP.get()
}

// metal to vulkan format map
pub(crate) fn vk_format(f: MTLPixelFormat) -> Result<vk::Format, String> {
    Ok(match f {
        MTLPixelFormat::BGRA8Unorm => vk::Format::B8G8R8A8_UNORM,
        MTLPixelFormat::BGRA8Unorm_sRGB => vk::Format::B8G8R8A8_SRGB,
        MTLPixelFormat::RGBA8Unorm => vk::Format::R8G8B8A8_UNORM,
        MTLPixelFormat::RGBA8Unorm_sRGB => vk::Format::R8G8B8A8_SRGB,
        MTLPixelFormat::RGBA16Float => vk::Format::R16G16B16A16_SFLOAT,
        MTLPixelFormat::RGB10A2Unorm => vk::Format::A2B10G10R10_UNORM_PACK32,
        other => return Err(format!("unsupported drawable pixel format {}", other.0)),
    })
}

// the layer pixel format a swapchain format presents through; only formats the proxy can generate from
pub(crate) fn mtl_format(f: vk::Format) -> Option<MTLPixelFormat> {
    Some(match f {
        vk::Format::B8G8R8A8_UNORM => MTLPixelFormat::BGRA8Unorm,
        vk::Format::B8G8R8A8_SRGB => MTLPixelFormat::BGRA8Unorm_sRGB,
        vk::Format::R8G8B8A8_UNORM => MTLPixelFormat::RGBA8Unorm,
        vk::Format::R8G8B8A8_SRGB => MTLPixelFormat::RGBA8Unorm_sRGB,
        vk::Format::R16G16B16A16_SFLOAT => MTLPixelFormat::RGBA16Float,
        vk::Format::A2B10G10R10_UNORM_PACK32 => MTLPixelFormat::RGB10A2Unorm,
        _ => return None,
    })
}

// monotonic seconds
pub(crate) fn now() -> f64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}
