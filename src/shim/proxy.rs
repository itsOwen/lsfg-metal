// seam into the metal module: the vulkan surface registry and the proxy swapchain
use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use ash::vk::{self, Handle};

use super::{swapchain_of, Created, DeviceEntry, Error, Swapchain};
use crate::metal;

// the metal presenter must never handle a layer that vulkan presents to
pub fn register_layer(layer: *const c_void) {
    metal::register_layer(layer, None);
}

// record (surface -> layer) so proxy creation can find the layer behind a surface
pub fn register_surface(surface: vk::SurfaceKHR, layer: *const c_void) {
    metal::register_layer(layer, Some(surface));
}

struct Proxy(metal::ProxySwapchain);

impl Swapchain for Proxy {
    unsafe fn present(
        &mut self,
        queue: vk::Queue,
        info: &vk::PresentInfoKHR,
        _real: vk::PFN_vkQueuePresentKHR,
    ) -> Result<vk::Result, Error> {
        Ok(self.0.queue_present(queue, info))
    }
    unsafe fn get_images(
        &self,
        count: *mut u32,
        images: *mut vk::Image,
    ) -> Result<vk::Result, Error> {
        Ok(self.0.get_swapchain_images(count, images))
    }
    unsafe fn acquire(
        &self,
        timeout: u64,
        semaphore: vk::Semaphore,
        fence: vk::Fence,
        index: *mut u32,
    ) -> Result<vk::Result, Error> {
        let mut i = 0;
        let r = self.0.acquire_next_image(timeout, semaphore, fence, &mut i);
        if matches!(r, vk::Result::SUCCESS | vk::Result::SUBOPTIMAL_KHR) {
            *index = i;
        }
        Ok(r)
    }
    fn as_proxy(&self) -> Option<&metal::ProxySwapchain> {
        Some(&self.0)
    }
}

// Ok(None) = not applicable (fixed wrapper), Ok(Some) = proxy created, Err = attempt failed
// the proxy has no driver swapchain, its handle is the wrapper's own heap address
pub unsafe fn create(
    dev: &Arc<DeviceEntry>,
    info: &vk::SwapchainCreateInfoKHR,
) -> Result<Option<Created>, Error> {
    if !metal::proxy_supported() {
        return Ok(None);
    }
    let h = dev.hook.as_ref().ok_or("device not hooked")?;
    let instance = h
        .inst
        .hook
        .as_ref()
        .ok_or("instance not hooked")?
        .instance
        .clone();
    let game = metal::GameDevice {
        instance,
        physical_device: h.physical,
        device: h.device.clone(),
        queue: h.queue,
        family: h.family,
    };
    let d = dev.clone();
    let lock: metal::QueueLock = Arc::new(move |f: &mut dyn FnMut()| {
        let _q = d.hook.as_ref().expect("hooked device").queue_mutex.lock();
        f()
    });
    let Some(p) = metal::ProxySwapchain::create(game, lock, info.surface, info)? else {
        return Ok(None);
    };
    let b: Box<dyn Swapchain> = Box::new(Proxy(p));
    let handle = vk::SwapchainKHR::from_raw(&*b as *const dyn Swapchain as *const u8 as u64);
    Ok(Some((handle, b)))
}

// released images return to the proxy's pool; false when the swapchain is not a proxy
pub unsafe fn release(info: &vk::ReleaseSwapchainImagesInfoEXT) -> bool {
    let Some(e) = swapchain_of(info.swapchain).filter(|e| e.proxy) else {
        return false;
    };
    let indices = std::slice::from_raw_parts(info.p_image_indices, info.image_index_count as usize);
    if let Some(p) = e.wrapper.as_ref().expect("proxy").read().unwrap().as_proxy() {
        p.release_images(indices);
    }
    true
}

pub fn is_proxy(sc: vk::SwapchainKHR) -> bool {
    swapchain_of(sc).is_some_and(|e| e.proxy)
}

// true when any swapchain in the present was a proxy, none of it reaches the driver
pub unsafe fn invalidate_proxies(queue: vk::Queue, info: &vk::PresentInfoKHR) -> bool {
    let handles = std::slice::from_raw_parts(info.p_swapchains, info.swapchain_count as usize);
    let entries: Vec<_> = handles
        .iter()
        .filter_map(|&s| swapchain_of(s))
        .filter(|e| e.proxy)
        .collect();
    if entries.is_empty() {
        return false;
    }
    let guards: Vec<_> = entries
        .iter()
        .map(|e| e.wrapper.as_ref().expect("proxy").read().unwrap())
        .collect();
    let proxies: Vec<_> = guards.iter().filter_map(|g| g.as_proxy()).collect();
    for e in &entries {
        if let Some(h) = &e.device.hook {
            h.proxy_supported.store(false, Ordering::Relaxed);
        }
    }
    metal::invalidate_proxies(queue, info, &proxies)
}
