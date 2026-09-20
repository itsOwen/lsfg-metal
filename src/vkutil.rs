// vulkan utilities: driver loading, device selection, memory, images, commands, sync objects
use std::ffi::CStr;
use std::path::Path;

use ash::{ext, vk};

pub fn check<T>(r: Result<T, vk::Result>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what} failed: {e:?}"))
}

pub fn align_up(size: u64, a: u64) -> u64 {
    (size + a - 1) & !(a - 1)
}

// dlopen a vulkan driver and build an entry from its vkGetInstanceProcAddr; keep the library alive
pub fn load_driver(path: &Path) -> Result<(libloading::Library, ash::Entry), String> {
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

// no layers, no extensions; engine name = name when `engine`
pub fn create_instance(
    entry: &ash::Entry,
    name: &CStr,
    api_version: u32,
    engine: bool,
) -> Result<ash::Instance, String> {
    let v = vk::make_api_version(0, 2, 0, 0);
    let mut app = vk::ApplicationInfo::default()
        .application_name(name)
        .application_version(v)
        .engine_version(v)
        .api_version(api_version);
    if engine {
        app = app.engine_name(name);
    }
    let info = vk::InstanceCreateInfo::default().application_info(&app);
    check(
        unsafe { entry.create_instance(&info, None) },
        "vkCreateInstance",
    )
}

// empty id = first device; else name, vendor:device or pci address
pub fn select_physical_device(
    instance: &ash::Instance,
    id: &str,
) -> Result<vk::PhysicalDevice, String> {
    let devices = check(
        unsafe { instance.enumerate_physical_devices() },
        "vkEnumeratePhysicalDevices",
    )?;
    for pd in devices {
        let exts = check(
            unsafe { instance.enumerate_device_extension_properties(pd) },
            "vkEnumerateDeviceExtensionProperties",
        )?;
        let pci_supported = exts
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(ext::pci_bus_info::NAME));
        let mut pci = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceProperties2::default();
        if pci_supported {
            props = props.push_next(&mut pci);
        }
        unsafe { instance.get_physical_device_properties2(pd, &mut props) };
        let p = props.properties;
        if id.is_empty() {
            return Ok(pd);
        }
        let mut name = p.device_name;
        name[255] = 0;
        let name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy();
        let pci_id = format!(
            "{:04x}:{:02x}:{:02x}.{:x}",
            pci.pci_domain, pci.pci_bus, pci.pci_device, pci.pci_function
        );
        if id == name
            || id == format!("{:04x}:{:04x}", p.vendor_id, p.device_id)
            || (pci_supported && id == pci_id)
        {
            return Ok(pd);
        }
    }
    Err(format!("No physical device matching '{id}' found"))
}

// first family sharing any bit with flags, skipping graphics families when dedicated
pub fn find_queue_family(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    flags: vk::QueueFlags,
    dedicated: bool,
) -> Result<u32, String> {
    let fams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    fams.iter()
        .position(|f| {
            f.queue_flags.intersects(flags)
                && !(dedicated && f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        })
        .map(|i| i as u32)
        .ok_or_else(|| format!("No queue family found matching {flags:?}"))
}

pub fn half_precision_supported(instance: &ash::Instance, pd: vk::PhysicalDevice) -> bool {
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut f = vk::PhysicalDeviceFeatures2::default().push_next(&mut f12);
    unsafe { instance.get_physical_device_features2(pd, &mut f) };
    f12.shader_float16 == vk::TRUE
}

// moltenvk before 1.3 runs the generation shaders but writes black frames
pub fn check_driver(
    gipa: vk::PFN_vkGetInstanceProcAddr,
    instance: vk::Instance,
    pd: vk::PhysicalDevice,
) -> Result<(), String> {
    // the khr name serves vulkan 1.0 instances; a driver with neither predates the versions that work
    let Some(f) = [c"vkGetPhysicalDeviceProperties2", c"vkGetPhysicalDeviceProperties2KHR"]
        .iter()
        .find_map(|n| unsafe { gipa(instance, n.as_ptr()) })
    else {
        return Err(
            "the driver cannot report its version, so it is older than the MoltenVK 1.3 frame generation needs"
                .into(),
        );
    };
    let f: vk::PFN_vkGetPhysicalDeviceProperties2 = unsafe { std::mem::transmute(f) };
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut p = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    unsafe { f(pd, &mut p) };
    let v = p.properties.driver_version;
    if driver.driver_id == vk::DriverId::MOLTENVK && v < 10300 {
        return Err(format!(
            "MoltenVK {}.{}.{} is too old for frame generation (it outputs black frames); MoltenVK 1.3 or newer is needed",
            v / 10000,
            v / 100 % 100,
            v % 100
        ));
    }
    Ok(())
}

// first eligible device-local type, else the last eligible type
pub fn memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    mask: u32,
    host_visible: bool,
) -> Result<u32, String> {
    let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let mut selected = None;
    for i in 0..props.memory_type_count {
        let f = props.memory_types[i as usize].property_flags;
        if mask & (1 << i) == 0 || (host_visible && !f.contains(host)) {
            continue;
        }
        selected = Some(i);
        if f.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL) {
            break;
        }
    }
    selected.ok_or_else(|| "No memory type matches the allocation requirements".into())
}

pub fn allocate(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    device: &ash::Device,
    size: u64,
    mask: u32,
    host_visible: bool,
    dedicated: Option<vk::Image>,
) -> Result<vk::DeviceMemory, String> {
    let props = unsafe { instance.get_physical_device_memory_properties(pd) };
    let ty = memory_type(&props, mask, host_visible)?;
    let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(dedicated.unwrap_or_default());
    let mut info = vk::MemoryAllocateInfo::default()
        .allocation_size(size)
        .memory_type_index(ty);
    if dedicated.is_some() {
        info = info.push_next(&mut ded);
    }
    check(
        unsafe { device.allocate_memory(&info, None) },
        "vkAllocateMemory",
    )
}

pub fn create_image(
    device: &ash::Device,
    format: vk::Format,
    (w, h): (u32, u32),
    layers: u32,
    usage: vk::ImageUsageFlags,
) -> Result<vk::Image, String> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(layers)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    check(unsafe { device.create_image(&info, None) }, "vkCreateImage")
}

pub fn color_range(base_layer: u32, layers: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: base_layer,
        layer_count: layers,
    }
}

// 2d-array view when `array`, else 2d
pub fn create_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    layers: u32,
    array: bool,
) -> Result<vk::ImageView, String> {
    let ty = if array {
        vk::ImageViewType::TYPE_2D_ARRAY
    } else {
        vk::ImageViewType::TYPE_2D
    };
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(ty)
        .format(format)
        .subresource_range(color_range(0, layers));
    check(
        unsafe { device.create_image_view(&info, None) },
        "vkCreateImageView",
    )
}

// linear everything, lod clamp none, compare disabled but op set
pub fn create_sampler(
    device: &ash::Device,
    address: vk::SamplerAddressMode,
    compare: vk::CompareOp,
    border: vk::BorderColor,
) -> Result<vk::Sampler, String> {
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::LINEAR)
        .min_filter(vk::Filter::LINEAR)
        .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
        .address_mode_u(address)
        .address_mode_v(address)
        .address_mode_w(address)
        .compare_op(compare)
        .max_lod(vk::LOD_CLAMP_NONE)
        .border_color(border);
    check(
        unsafe { device.create_sampler(&info, None) },
        "vkCreateSampler",
    )
}

// host-visible buffer with initial contents copied through a temporary mapping
pub fn create_buffer(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    device: &ash::Device,
    usage: vk::BufferUsageFlags,
    data: &[u8],
) -> Result<(vk::Buffer, vk::DeviceMemory), String> {
    unsafe {
        let info = vk::BufferCreateInfo::default()
            .size(data.len() as u64)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = check(device.create_buffer(&info, None), "vkCreateBuffer")?;
        let req = device.get_buffer_memory_requirements(buffer);
        let mem = allocate(
            instance,
            pd,
            device,
            req.size,
            req.memory_type_bits,
            true,
            None,
        )?;
        check(
            device.bind_buffer_memory(buffer, mem, 0),
            "vkBindBufferMemory",
        )?;
        let p = check(
            device.map_memory(mem, 0, data.len() as u64, vk::MemoryMapFlags::empty()),
            "vkMapMemory",
        )?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), p as *mut u8, data.len());
        device.unmap_memory(mem);
        Ok((buffer, mem))
    }
}

pub fn create_command_pool(
    device: &ash::Device,
    family: u32,
    resettable: bool,
) -> Result<vk::CommandPool, String> {
    let flags = if resettable {
        vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER
    } else {
        vk::CommandPoolCreateFlags::empty()
    };
    let info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(family)
        .flags(flags);
    check(
        unsafe { device.create_command_pool(&info, None) },
        "vkCreateCommandPool",
    )
}

pub fn allocate_command_buffer(
    device: &ash::Device,
    pool: vk::CommandPool,
) -> Result<vk::CommandBuffer, String> {
    let info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    Ok(check(
        unsafe { device.allocate_command_buffers(&info) },
        "vkAllocateCommandBuffers",
    )?[0])
}

pub fn begin(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    flags: vk::CommandBufferUsageFlags,
) -> Result<(), String> {
    check(
        unsafe {
            device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(flags))
        },
        "vkBeginCommandBuffer",
    )
}

// full-extent nearest blit, level 0, both images in general layout
pub fn cmd_blit(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    src: vk::Image,
    src_layer: u32,
    dst: vk::Image,
    dst_layer: u32,
    (w, h): (u32, u32),
) {
    let layer = |l| vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: l,
        layer_count: 1,
    };
    let bounds = [
        vk::Offset3D { x: 0, y: 0, z: 0 },
        vk::Offset3D {
            x: w as i32,
            y: h as i32,
            z: 1,
        },
    ];
    let blit = vk::ImageBlit {
        src_subresource: layer(src_layer),
        src_offsets: bounds,
        dst_subresource: layer(dst_layer),
        dst_offsets: bounds,
    };
    unsafe {
        device.cmd_blit_image(
            cb,
            src,
            vk::ImageLayout::GENERAL,
            dst,
            vk::ImageLayout::GENERAL,
            &[blit],
            vk::Filter::NEAREST,
        )
    }
}

// legacy image barrier helper
pub fn image_barrier(
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    image: vk::Image,
    base_layer: u32,
    layers: u32,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_range(base_layer, layers))
}

pub fn create_semaphore(device: &ash::Device, timeline: bool) -> Result<vk::Semaphore, String> {
    let mut ty = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .initial_value(0);
    let mut info = vk::SemaphoreCreateInfo::default();
    if timeline {
        info = info.push_next(&mut ty);
    }
    check(
        unsafe { device.create_semaphore(&info, None) },
        "vkCreateSemaphore",
    )
}

pub fn create_fence(device: &ash::Device) -> Result<vk::Fence, String> {
    check(
        unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) },
        "vkCreateFence",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_type_prefers_device_local_else_last() {
        let mut p = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        p.memory_types[0].property_flags = host;
        p.memory_types[1].property_flags = host;
        p.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        assert_eq!(memory_type(&p, 0b111, false).unwrap(), 2);
        assert_eq!(memory_type(&p, 0b011, false).unwrap(), 1);
        assert_eq!(
            memory_type(&p, 0b100, true).unwrap_err(),
            "No memory type matches the allocation requirements"
        );
        assert_eq!(align_up(13, 8), 16);
    }
}
