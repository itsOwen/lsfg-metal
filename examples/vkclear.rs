// vulkan smoke test: load a driver by path, clear alternating colours into a CAMetalLayer window, present n frames
use std::path::Path;

use ash::{ext, khr, vk};
use objc2::encode::{Encode, Encoding};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_quartz_core::CAMetalLayer;

// the helpers below mirror the crate's vkutil; the example must not link the crate (its class statics would register twice under the shim)
fn check<T>(r: Result<T, vk::Result>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what} failed: {e:?}"))
}

fn load_driver(path: &Path) -> Result<(libloading::Library, ash::Entry), String> {
    unsafe {
        let lib = libloading::Library::new(path)
            .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        let gipa: vk::PFN_vkGetInstanceProcAddr = *lib
            .get(b"vkGetInstanceProcAddr\0")
            .map_err(|e| format!("vkGetInstanceProcAddr: {e}"))?;
        let entry = ash::Entry::from_static_fn(ash::StaticFn {
            get_instance_proc_addr: gipa,
        });
        Ok((lib, entry))
    }
}

fn graphics_family(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Result<u32, String> {
    unsafe { instance.get_physical_device_queue_family_properties(pd) }
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .map(|i| i as u32)
        .ok_or_else(|| "No graphics queue family found".into())
}

unsafe fn create_semaphore(device: &ash::Device) -> Result<vk::Semaphore, String> {
    check(
        device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None),
        "vkCreateSemaphore",
    )
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn image_barrier(
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    image: vk::Image,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_range())
}

#[repr(C)]
struct Rect(f64, f64, f64, f64);

unsafe impl Encode for Rect {
    const ENCODING: Encoding = Encoding::Struct(
        "CGRect",
        &[
            Encoding::Struct("CGPoint", &[Encoding::Double, Encoding::Double]),
            Encoding::Struct("CGSize", &[Encoding::Double, Encoding::Double]),
        ],
    );
}

unsafe fn window(w: f64, h: f64) -> *const CAMetalLayer {
    let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    let _: bool = msg_send![app, setActivationPolicy: 0isize];
    let win: *mut AnyObject = msg_send![class!(NSWindow), alloc];
    let win: *mut AnyObject = msg_send![win, initWithContentRect: Rect(100.0, 100.0, w, h), styleMask: 15usize, backing: 2usize, defer: false];
    let view: *mut AnyObject = msg_send![win, contentView];
    let layer = CAMetalLayer::new();
    let _: () = msg_send![view, setLayer: &*layer];
    let _: () = msg_send![view, setWantsLayer: true];
    let _: () = msg_send![win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
    let _: () = msg_send![app, finishLaunching];
    objc2::rc::Retained::into_raw(layer)
}

fn run(driver: &str, frames: u32) -> Result<(), String> {
    let (_lib, entry) = load_driver(Path::new(driver))?;
    let layer = unsafe { window(640.0, 480.0) };
    unsafe {
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
        let iexts = [
            khr::surface::NAME.as_ptr(),
            ext::metal_surface::NAME.as_ptr(),
        ];
        let instance = check(
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app)
                    .enabled_extension_names(&iexts),
                None,
            ),
            "vkCreateInstance",
        )?;
        let surface_fn = khr::surface::Instance::new(&entry, &instance);
        let sci = vk::MetalSurfaceCreateInfoEXT {
            p_layer: layer.cast(),
            ..Default::default()
        };
        let surface = check(
            ext::metal_surface::Instance::new(&entry, &instance).create_metal_surface(&sci, None),
            "vkCreateMetalSurfaceEXT",
        )?;
        let pd = check(
            instance.enumerate_physical_devices(),
            "vkEnumeratePhysicalDevices",
        )?[0];
        let family = graphics_family(&instance, pd)?;
        if !check(
            surface_fn.get_physical_device_surface_support(pd, family, surface),
            "vkGetPhysicalDeviceSurfaceSupportKHR",
        )? {
            return Err("surface not supported on the graphics family".into());
        }
        let prio = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&prio)];
        let dexts = [khr::swapchain::NAME.as_ptr()];
        // a chained vulkan 1.2 structure exercises the shim's feature-chain copy like dxvk does
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&dexts)
            .push_next(&mut f12);
        let device = check(instance.create_device(pd, &dci, None), "vkCreateDevice")?;
        assert_eq!(
            (f12.timeline_semaphore, f12.shader_float16),
            (0, 0),
            "caller's feature chain must stay untouched"
        );
        let queue = device.get_device_queue(family, 0);
        let swap_fn = khr::swapchain::Device::new(&instance, &device);
        let caps = check(
            surface_fn.get_physical_device_surface_capabilities(pd, surface),
            "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
        )?;
        let extent = if caps.current_extent.width == u32::MAX {
            vk::Extent2D {
                width: 640,
                height: 480,
            }
        } else {
            caps.current_extent
        };
        let sc_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(caps.min_image_count.max(2))
            .image_format(vk::Format::B8G8R8A8_UNORM)
            .image_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true);
        let swapchain = check(
            swap_fn.create_swapchain(&sc_info, None),
            "vkCreateSwapchainKHR",
        )?;
        let images = check(
            swap_fn.get_swapchain_images(swapchain),
            "vkGetSwapchainImagesKHR",
        )?;
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = check(device.create_command_pool(&pool_info, None), "vkCreateCommandPool")?;
        let cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = check(device.allocate_command_buffers(&cb_info), "vkAllocateCommandBuffers")?[0];
        let (acquired, rendered, fence) = (
            create_semaphore(&device)?,
            create_semaphore(&device)?,
            check(device.create_fence(&vk::FenceCreateInfo::default(), None), "vkCreateFence")?,
        );
        let mut presented = 0;
        let start = std::time::Instant::now();
        for i in 0..frames {
            let (idx, _) = match swap_fn.acquire_next_image(
                swapchain,
                u64::MAX,
                acquired,
                vk::Fence::null(),
            ) {
                Ok(x) => x,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => break,
                Err(e) => return Err(format!("vkAcquireNextImageKHR failed: {e:?}")),
            };
            let image = images[idx as usize];
            check(
                device.begin_command_buffer(
                    cb,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                ),
                "vkBeginCommandBuffer",
            )?;
            use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
            device.cmd_pipeline_barrier(
                cb,
                P::TOP_OF_PIPE,
                P::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[image_barrier(
                    A::NONE,
                    A::TRANSFER_WRITE,
                    L::UNDEFINED,
                    L::TRANSFER_DST_OPTIMAL,
                    image,
                )],
            );
            let color = if i % 2 == 0 {
                [1.0, 0.1, 0.1, 1.0]
            } else {
                [0.1, 0.1, 1.0, 1.0]
            };
            device.cmd_clear_color_image(
                cb,
                image,
                L::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: color },
                &[color_range()],
            );
            device.cmd_pipeline_barrier(
                cb,
                P::TRANSFER,
                P::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[image_barrier(
                    A::TRANSFER_WRITE,
                    A::NONE,
                    L::TRANSFER_DST_OPTIMAL,
                    L::PRESENT_SRC_KHR,
                    image,
                )],
            );
            check(device.end_command_buffer(cb), "vkEndCommandBuffer")?;
            let (waits, stages, cbs, signals) = ([acquired], [P::TRANSFER], [cb], [rendered]);
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cbs)
                .signal_semaphores(&signals);
            check(
                device.queue_submit(queue, &[submit], fence),
                "vkQueueSubmit",
            )?;
            let (swapchains, indices) = ([swapchain], [idx]);
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signals)
                .swapchains(&swapchains)
                .image_indices(&indices);
            match swap_fn.queue_present(queue, &present) {
                Ok(_) => presented += 1,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => break,
                Err(e) => return Err(format!("vkQueuePresentKHR failed: {e:?}")),
            }
            check(
                device.wait_for_fences(&[fence], true, u64::MAX),
                "vkWaitForFences",
            )?;
            check(device.reset_fences(&[fence]), "vkResetFences")?;
            // VKTEST_FPS paces the source like a capped game
            if let Some(fps) = std::env::var("VKTEST_FPS").ok().and_then(|v| v.parse::<f64>().ok()).filter(|f| *f > 0.0) {
                let due = start + std::time::Duration::from_secs_f64((i + 1) as f64 / fps);
                if let Some(left) = due.checked_duration_since(std::time::Instant::now()) {
                    std::thread::sleep(left);
                }
            }
        }
        check(device.device_wait_idle(), "vkDeviceWaitIdle")?;
        device.destroy_semaphore(acquired, None);
        device.destroy_semaphore(rendered, None);
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        swap_fn.destroy_swapchain(swapchain, None);
        device.destroy_device(None);
        surface_fn.destroy_surface(surface, None);
        instance.destroy_instance(None);
        println!("vkclear: presented {presented} frames");
    }
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(driver) = args.next() else {
        eprintln!("usage: vkclear <libMoltenVK.dylib> [frames]");
        std::process::exit(2);
    };
    let frames = args.next().and_then(|n| n.parse().ok()).unwrap_or(240);
    if let Err(e) = run(&driver, frames) {
        eprintln!("vkclear: {e}");
        std::process::exit(1);
    }
}
