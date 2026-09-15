// deep copy of the caller's device feature chain, forcing on synchronization2, timeline semaphores and (when asked) shader float16
use std::ffi::c_void;

use ash::vk;

use super::chain_sizes::SIZES;

pub struct Chain {
    pub head: *mut c_void,
    _nodes: Vec<Box<[u64]>>,
}

// every vulkan structure keeps its pNext link at offset 8; nodes are 8-byte aligned boxes
unsafe fn next_of(node: *mut c_void) -> *mut *mut c_void {
    node.cast::<u8>().add(8).cast()
}

unsafe fn alloc(size: usize) -> Box<[u64]> {
    vec![0u64; size.div_ceil(8)].into_boxed_slice()
}

unsafe fn prepend<T>(nodes: &mut Vec<Box<[u64]>>, head: &mut *mut c_void, value: T) {
    let mut b = alloc(std::mem::size_of::<T>());
    let node = b.as_mut_ptr().cast::<T>();
    std::ptr::write(node, value);
    *next_of(node.cast()) = *head;
    *head = node.cast();
    nodes.push(b);
}

// an unknown sType ends the deep copy: the rest of the caller's chain is borrowed as is (never written),
// and any of the five feature structs behind it counts as absent, so our own copy is prepended instead
pub unsafe fn copy(p_next: *const c_void, fp16: bool) -> Chain {
    use vk::StructureType as S;
    let mut nodes = Vec::new();
    let mut head: *mut c_void = std::ptr::null_mut();
    let mut tail: *mut *mut c_void = &mut head;
    let (mut sync2, mut f16, mut timeline) = (false, false, false);
    let mut p = p_next.cast::<vk::BaseInStructure>();
    while !p.is_null() {
        let st = (*p).s_type;
        let Some(size) = SIZES.iter().find(|(t, _)| *t == st).map(|(_, s)| *s) else {
            *tail = p.cast_mut().cast();
            break;
        };
        let mut b = alloc(size);
        let node = b.as_mut_ptr().cast::<c_void>();
        std::ptr::copy_nonoverlapping(p.cast::<u8>(), node.cast::<u8>(), size);
        *next_of(node) = std::ptr::null_mut();
        *tail = node;
        tail = next_of(node);
        match st {
            S::PHYSICAL_DEVICE_VULKAN_1_3_FEATURES => {
                (*node.cast::<vk::PhysicalDeviceVulkan13Features>()).synchronization2 = vk::TRUE;
                sync2 = true;
            }
            S::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES => {
                let f = node.cast::<vk::PhysicalDeviceVulkan12Features>();
                (*f).timeline_semaphore = vk::TRUE;
                if fp16 {
                    (*f).shader_float16 = vk::TRUE;
                }
                timeline = true;
                f16 = true;
            }
            S::PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES => {
                (*node.cast::<vk::PhysicalDeviceSynchronization2Features>()).synchronization2 =
                    vk::TRUE;
                sync2 = true;
            }
            S::PHYSICAL_DEVICE_SHADER_FLOAT16_INT8_FEATURES => {
                if fp16 {
                    (*node.cast::<vk::PhysicalDeviceShaderFloat16Int8Features>()).shader_float16 =
                        vk::TRUE;
                }
                f16 = true;
            }
            S::PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES => {
                (*node.cast::<vk::PhysicalDeviceTimelineSemaphoreFeatures>()).timeline_semaphore =
                    vk::TRUE;
                timeline = true;
            }
            _ => {}
        }
        nodes.push(b);
        p = (*p).p_next;
    }
    if !sync2 {
        prepend(
            &mut nodes,
            &mut head,
            vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true),
        );
    }
    if !f16 && fp16 {
        prepend(
            &mut nodes,
            &mut head,
            vk::PhysicalDeviceShaderFloat16Int8Features::default().shader_float16(true),
        );
    }
    if !timeline {
        prepend(
            &mut nodes,
            &mut head,
            vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true),
        );
    }
    Chain {
        head,
        _nodes: nodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn walk(head: *mut c_void) -> Vec<(vk::StructureType, *const c_void)> {
        let mut out = vec![];
        let mut p = head.cast::<vk::BaseInStructure>().cast_const();
        while !p.is_null() {
            out.push(((*p).s_type, p.cast()));
            p = (*p).p_next;
        }
        out
    }

    #[test]
    fn copies_enables_and_leaves_caller_untouched() {
        use vk::StructureType as S;
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut f2 = vk::PhysicalDeviceFeatures2 {
            p_next: (&mut f12 as *mut vk::PhysicalDeviceVulkan12Features).cast(),
            ..Default::default()
        };
        let chain = unsafe { copy((&f2 as *const vk::PhysicalDeviceFeatures2).cast(), true) };
        let nodes = unsafe { walk(chain.head) };
        let types: Vec<_> = nodes.iter().map(|n| n.0).collect();
        assert_eq!(
            types,
            [
                S::PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES,
                S::PHYSICAL_DEVICE_FEATURES_2,
                S::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES
            ]
        );
        let c = unsafe { &*nodes[2].1.cast::<vk::PhysicalDeviceVulkan12Features>() };
        assert_eq!(
            (c.timeline_semaphore, c.shader_float16),
            (vk::TRUE, vk::TRUE)
        );
        assert_eq!((f12.timeline_semaphore, f12.shader_float16), (0, 0));
        f2.p_next = std::ptr::null_mut();
        let chain = unsafe { copy((&f2 as *const vk::PhysicalDeviceFeatures2).cast(), false) };
        let types: Vec<_> = unsafe { walk(chain.head) }.iter().map(|n| n.0).collect();
        assert_eq!(
            types,
            [
                S::PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES,
                S::PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES,
                S::PHYSICAL_DEVICE_FEATURES_2
            ]
        );
    }

    // features2 (copied) -> unknown (borrowed, with the vulkan 1.2 features behind it left alone and re-added at the head)
    #[test]
    fn borrows_from_the_first_unknown_node() {
        use vk::StructureType as S;
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut bogus = vk::BaseInStructure {
            s_type: S::APPLICATION_INFO,
            p_next: (&mut f12 as *mut vk::PhysicalDeviceVulkan12Features).cast(),
            _marker: Default::default(),
        };
        let f2 = vk::PhysicalDeviceFeatures2 {
            p_next: (&mut bogus as *mut vk::BaseInStructure).cast(),
            ..Default::default()
        };
        let chain = unsafe { copy((&f2 as *const vk::PhysicalDeviceFeatures2).cast(), true) };
        let nodes = unsafe { walk(chain.head) };
        let types: Vec<_> = nodes.iter().map(|n| n.0).collect();
        assert_eq!(
            types,
            [
                S::PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES,
                S::PHYSICAL_DEVICE_SHADER_FLOAT16_INT8_FEATURES,
                S::PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES,
                S::PHYSICAL_DEVICE_FEATURES_2,
                S::APPLICATION_INFO,
                S::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES
            ]
        );
        assert_ne!(nodes[3].1, (&f2 as *const vk::PhysicalDeviceFeatures2).cast());
        assert_eq!(nodes[4].1, (&bogus as *const vk::BaseInStructure).cast());
        assert_eq!(nodes[5].1, (&f12 as *const vk::PhysicalDeviceVulkan12Features).cast());
        assert_eq!((f12.timeline_semaphore, f12.shader_float16), (0, 0));
    }
}
