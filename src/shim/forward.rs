// stubs for the driver's other exports, so libraries linked against moltenvk (gstreamer) still load
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicPtr, Ordering};

use super::{dladdr, DlInfo};

extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> i32;
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(index: u32) -> *const c_char;
}
const RTLD_LAZY: i32 = 0x1;
const RTLD_NOLOAD: i32 = 0x10;

fn dl_info(addr: *const c_void) -> Option<DlInfo> {
    let mut info = DlInfo {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    (unsafe { dladdr(addr, &mut info) } != 0 && !info.fname.is_null()).then_some(info)
}

// two copies of the shim must never forward to each other
unsafe fn in_shim(addr: *const c_void) -> bool {
    let Some(info) = dl_info(addr) else {
        return false;
    };
    let h = dlopen(info.fname, RTLD_LAZY | RTLD_NOLOAD);
    if h.is_null() {
        return false;
    }
    let marker = dlsym(h, c"LSFGM_SHIM".as_ptr());
    dlclose(h);
    !marker.is_null() && dl_info(marker).is_some_and(|m| m.fbase == info.fbase)
}

// injected with no driver beside the shim: any loaded image, even one the app dlopened RTLD_LOCAL
unsafe fn loaded(name: &CStr) -> *mut c_void {
    for i in 0.._dyld_image_count() {
        let h = dlopen(_dyld_get_image_name(i), RTLD_LAZY | RTLD_NOLOAD);
        if h.is_null() {
            continue;
        }
        let f = dlsym(h, name.as_ptr());
        dlclose(h);
        if !f.is_null() && !in_shim(f) {
            return f;
        }
    }
    std::ptr::null_mut()
}

// a missing absolute path means injected with no driver, so skip the load and its error line
fn driver_configured() -> bool {
    let path = super::driver_path();
    !path.is_empty() && !(path.starts_with('/') && !std::path::Path::new(&path).exists())
}

// a configured driver is the only answer, so handles never cross into a second moltenvk
unsafe fn lookup(name: &CStr) -> *mut c_void {
    if !driver_configured() {
        return loaded(name);
    }
    super::driver()
        .and_then(|d| d.lib.get::<*mut c_void>(name.to_bytes_with_nul()).ok().map(|s| *s))
        .unwrap_or(std::ptr::null_mut())
}

pub(super) unsafe fn real<F>(name: &CStr) -> Option<F> {
    let f = lookup(name);
    (!f.is_null()).then(|| std::mem::transmute_copy(&f))
}

// slot is the stub's cached address followed by its nul-terminated name
unsafe extern "C" fn resolve(slot: *const AtomicPtr<c_void>) -> *mut c_void {
    let name = CStr::from_ptr(slot.add(1).cast::<c_char>());
    let f = lookup(name);
    if f.is_null() {
        let name = name.to_string_lossy();
        let why = if !driver_configured() {
            format!("no MoltenVK is loaded to forward {name} to")
        } else if let Some(d) = super::driver() {
            format!("real MoltenVK '{}' has no {name}", d.path)
        } else {
            format!("real MoltenVK did not load, so {name} has nowhere to go")
        };
        crate::log::log_fmt(crate::log::Level::Error, format_args!("lsfg-metal shim: {why}"));
        std::process::abort();
    }
    (*slot).store(f, Ordering::Release);
    f
}

macro_rules! forward {
    ($($name:ident)*) => {
        // slow path: keep the argument registers, resolve the slot in x16 / r11, jump to the result
        #[cfg(target_arch = "aarch64")]
        std::arch::global_asm!(
            ".text",
            ".p2align 2",
            ".private_extern _lsfgm_forward_slow",
            "_lsfgm_forward_slow:",
            ".cfi_startproc",
            "stp x29, x30, [sp, #-16]!",
            "mov x29, sp",
            ".cfi_def_cfa w29, 16",
            ".cfi_offset w30, -8",
            ".cfi_offset w29, -16",
            "sub sp, sp, #208",
            "stp x0, x1, [sp, #0]",
            "stp x2, x3, [sp, #16]",
            "stp x4, x5, [sp, #32]",
            "stp x6, x7, [sp, #48]",
            "str x8, [sp, #64]",
            "stp q0, q1, [sp, #80]",
            "stp q2, q3, [sp, #112]",
            "stp q4, q5, [sp, #144]",
            "stp q6, q7, [sp, #176]",
            "mov x0, x16",
            "bl {resolve}",
            "mov x16, x0",
            "ldp x0, x1, [sp, #0]",
            "ldp x2, x3, [sp, #16]",
            "ldp x4, x5, [sp, #32]",
            "ldp x6, x7, [sp, #48]",
            "ldr x8, [sp, #64]",
            "ldp q0, q1, [sp, #80]",
            "ldp q2, q3, [sp, #112]",
            "ldp q4, q5, [sp, #144]",
            "ldp q6, q7, [sp, #176]",
            "add sp, sp, #208",
            "ldp x29, x30, [sp], #16",
            "br x16",
            ".cfi_endproc",
            $(
                concat!(".globl _", stringify!($name)),
                ".p2align 2",
                concat!("_", stringify!($name), ":"),
                concat!("adrp x16, lfw_", stringify!($name), "@PAGE"),
                concat!("add x16, x16, lfw_", stringify!($name), "@PAGEOFF"),
                "ldr x17, [x16]",
                "cbz x17, 1f",
                "br x17",
                "1: b _lsfgm_forward_slow",
            )*
            ".data",
            $(
                ".p2align 3",
                concat!("lfw_", stringify!($name), ": .quad 0"),
                concat!(".asciz \"", stringify!($name), "\""),
            )*
            ".text",
            resolve = sym resolve,
        );

        #[cfg(target_arch = "x86_64")]
        std::arch::global_asm!(
            ".text",
            ".p2align 4",
            ".private_extern _lsfgm_forward_slow",
            "_lsfgm_forward_slow:",
            ".cfi_startproc",
            "pushq %rbp",
            ".cfi_def_cfa_offset 16",
            ".cfi_offset %rbp, -16",
            "movq %rsp, %rbp",
            ".cfi_def_cfa_register %rbp",
            "pushq %rdi",
            "pushq %rsi",
            "pushq %rdx",
            "pushq %rcx",
            "pushq %r8",
            "pushq %r9",
            "pushq %rax",
            "subq $136, %rsp",
            "movdqu %xmm0, 0(%rsp)",
            "movdqu %xmm1, 16(%rsp)",
            "movdqu %xmm2, 32(%rsp)",
            "movdqu %xmm3, 48(%rsp)",
            "movdqu %xmm4, 64(%rsp)",
            "movdqu %xmm5, 80(%rsp)",
            "movdqu %xmm6, 96(%rsp)",
            "movdqu %xmm7, 112(%rsp)",
            "movq %r11, %rdi",
            "call {resolve}",
            "movq %rax, %r11",
            "movdqu 0(%rsp), %xmm0",
            "movdqu 16(%rsp), %xmm1",
            "movdqu 32(%rsp), %xmm2",
            "movdqu 48(%rsp), %xmm3",
            "movdqu 64(%rsp), %xmm4",
            "movdqu 80(%rsp), %xmm5",
            "movdqu 96(%rsp), %xmm6",
            "movdqu 112(%rsp), %xmm7",
            "addq $136, %rsp",
            "popq %rax",
            "popq %r9",
            "popq %r8",
            "popq %rcx",
            "popq %rdx",
            "popq %rsi",
            "popq %rdi",
            "popq %rbp",
            "jmpq *%r11",
            ".cfi_endproc",
            $(
                concat!(".globl _", stringify!($name)),
                ".p2align 4",
                concat!("_", stringify!($name), ":"),
                concat!("movq lfw_", stringify!($name), "(%rip), %r10"),
                "testq %r10, %r10",
                "jz 1f",
                "jmpq *%r10",
                concat!("1: leaq lfw_", stringify!($name), "(%rip), %r11"),
                "jmp _lsfgm_forward_slow",
            )*
            ".data",
            $(
                ".p2align 3",
                concat!("lfw_", stringify!($name), ": .quad 0"),
                concat!(".asciz \"", stringify!($name), "\""),
            )*
            ".text",
            resolve = sym resolve,
            options(att_syntax),
        );
    };
}

// every c function the moltenvk builds we meet export, minus the shim's own and the vk_icd* ones a loader prefers
forward! {
    mvkCGPointFromVkOffset2D
    mvkCGRectFromVkRectLayerKHR
    mvkCGSizeFromVkExtent2D
    mvkFormatTypeFromMTLPixelFormat
    mvkFormatTypeFromVkFormat
    mvkMipmapBaseSizeFromLevelSize2D
    mvkMipmapBaseSizeFromLevelSize3D
    mvkMipmapLevels
    mvkMipmapLevels2D
    mvkMipmapLevels3D
    mvkMipmapLevelSizeFromBaseSize2D
    mvkMipmapLevelSizeFromBaseSize3D
    mvkMTLBarrierScopeFromVkAccessFlags
    mvkMTLBlendFactorFromVkBlendFactor
    mvkMTLBlendOperationFromVkBlendOp
    mvkMTLClearColorFromVkClearValue
    mvkMTLClearDepthFromVkClearValue
    mvkMTLClearStencilFromVkClearValue
    mvkMTLColorWriteMaskFromVkChannelFlags
    mvkMTLCompareFunctionFromVkCompareOp
    mvkMTLCPUCacheModeFromVkMemoryPropertyFlags
    mvkMTLCullModeFromVkCullModeFlags
    mvkMTLIndexTypeFromVkIndexType
    mvkMTLIndexTypeSizeInBytes
    mvkMTLLoadActionFromVkAttachmentLoadOp
    mvkMTLLogicOperationFromVkLogicOp
    mvkMTLMultisampleDepthResolveFilterFromVkResolveModeFlagBits
    mvkMTLMultisampleStencilResolveFilterFromVkResolveModeFlagBits
    mvkMTLPixelFormatBlockTexelSize
    mvkMTLPixelFormatBytesPerBlock
    mvkMTLPixelFormatBytesPerLayer
    mvkMTLPixelFormatBytesPerRow
    mvkMTLPixelFormatBytesPerTexel
    mvkMTLPixelFormatFromVkFormat
    mvkMTLPixelFormatIsDepthFormat
    mvkMTLPixelFormatIsPVRTCFormat
    mvkMTLPixelFormatIsStencilFormat
    mvkMTLPixelFormatIsSupported
    mvkMTLPixelFormatName
    mvkMTLPrimitiveTopologyClassFromVkPrimitiveTopology
    mvkMTLPrimitiveTypeFromVkPrimitiveTopology
    mvkMTLProvokingVertexModeFromVkProvokingVertexMode
    mvkMTLRenderStagesFromVkPipelineStageFlags
    mvkMTLResourceOptions
    mvkMTLSamplerAddressModeFromVkSamplerAddressMode
    mvkMTLSamplerBorderColorFromVkBorderColor
    mvkMTLSamplerMinMagFilterFromVkFilter
    mvkMTLSamplerMipFilterFromVkSamplerMipmapMode
    mvkMTLScissorRectFromVkRect2D
    mvkMTLStencilOperationFromVkStencilOp
    mvkMTLStepFunctionFromVkVertexInputRate
    mvkMTLStoreActionFromVkAttachmentStoreOp
    mvkMTLTessellationPartitionModeFromSpvExecutionMode
    mvkMTLTextureSwizzleChannelsFromVkComponentMapping
    mvkMTLTextureSwizzleFromVkComponentSwizzle
    mvkMTLTextureTypeFromVkImageType
    mvkMTLTextureTypeFromVkImageViewType
    mvkMTLTextureUsageFromVkImageUsageFlags
    mvkMTLTriangleFillModeFromVkPolygonMode
    mvkMTLVertexFormatFromVkFormat
    mvkMTLVertexStepFunctionFromVkVertexInputRate
    mvkMTLViewportFromVkViewport
    mvkMTLWindingFromSpvExecutionMode
    mvkMTLWindingFromVkFrontFace
    mvkPrimRestartIndexFromVkIndexType
    mvkSampleCountFromVkSampleCountFlagBits
    mvkShaderStageFromVkShaderStageFlagBits
    mvkVkClearColorFloatValueFromVkComponentSwizzle
    mvkVkClearColorIntValueFromVkComponentSwizzle
    mvkVkClearColorUIntValueFromVkComponentSwizzle
    mvkVkExtent2DFromCGSize
    mvkVkFormatBlockTexelSize
    mvkVkFormatBytesPerBlock
    mvkVkFormatBytesPerLayer
    mvkVkFormatBytesPerRow
    mvkVkFormatBytesPerTexel
    mvkVkFormatFromMTLPixelFormat
    mvkVkFormatIsSupported
    mvkVkFormatName
    mvkVkFormatProperties
    mvkVkImageTypeFromMTLTextureType
    mvkVkImageUsageFlagsFromMTLTextureUsage
    mvkVkRect2DFromMTLScissorRect
    mvkVkSampleCountFlagBitsFromSampleCount
    mvkVkShaderStageFlagBitsFromMVKShaderStage
    vkAcquireNextImage2KHR
    vkAcquireNextImageKHR
    vkAllocateCommandBuffers
    vkAllocateDescriptorSets
    vkAllocateMemory
    vkBeginCommandBuffer
    vkBindBufferMemory
    vkBindBufferMemory2
    vkBindBufferMemory2KHR
    vkBindImageMemory
    vkBindImageMemory2
    vkBindImageMemory2KHR
    vkCmdBeginDebugUtilsLabelEXT
    vkCmdBeginQuery
    vkCmdBeginQueryIndexedEXT
    vkCmdBeginRendering
    vkCmdBeginRenderingKHR
    vkCmdBeginRenderPass
    vkCmdBeginRenderPass2
    vkCmdBeginRenderPass2KHR
    vkCmdBeginTransformFeedbackEXT
    vkCmdBindDescriptorSets
    vkCmdBindDescriptorSets2
    vkCmdBindDescriptorSets2KHR
    vkCmdBindIndexBuffer
    vkCmdBindIndexBuffer2
    vkCmdBindIndexBuffer2KHR
    vkCmdBindPipeline
    vkCmdBindTransformFeedbackBuffersEXT
    vkCmdBindVertexBuffers
    vkCmdBindVertexBuffers2
    vkCmdBindVertexBuffers2EXT
    vkCmdBlitImage
    vkCmdBlitImage2
    vkCmdBlitImage2KHR
    vkCmdClearAttachments
    vkCmdClearColorImage
    vkCmdClearDepthStencilImage
    vkCmdCopyBuffer
    vkCmdCopyBuffer2
    vkCmdCopyBuffer2KHR
    vkCmdCopyBufferToImage
    vkCmdCopyBufferToImage2
    vkCmdCopyBufferToImage2KHR
    vkCmdCopyImage
    vkCmdCopyImage2
    vkCmdCopyImage2KHR
    vkCmdCopyImageToBuffer
    vkCmdCopyImageToBuffer2
    vkCmdCopyImageToBuffer2KHR
    vkCmdCopyQueryPoolResults
    vkCmdDebugMarkerBeginEXT
    vkCmdDebugMarkerEndEXT
    vkCmdDebugMarkerInsertEXT
    vkCmdDispatch
    vkCmdDispatchBase
    vkCmdDispatchBaseKHR
    vkCmdDispatchIndirect
    vkCmdDraw
    vkCmdDrawIndexed
    vkCmdDrawIndexedIndirect
    vkCmdDrawIndexedIndirectCount
    vkCmdDrawIndexedIndirectCountAMD
    vkCmdDrawIndexedIndirectCountKHR
    vkCmdDrawIndirect
    vkCmdDrawIndirectByteCountEXT
    vkCmdDrawIndirectCount
    vkCmdDrawIndirectCountAMD
    vkCmdDrawIndirectCountKHR
    vkCmdEndDebugUtilsLabelEXT
    vkCmdEndQuery
    vkCmdEndQueryIndexedEXT
    vkCmdEndRendering
    vkCmdEndRenderingKHR
    vkCmdEndRenderPass
    vkCmdEndRenderPass2
    vkCmdEndRenderPass2KHR
    vkCmdEndTransformFeedbackEXT
    vkCmdExecuteCommands
    vkCmdFillBuffer
    vkCmdInsertDebugUtilsLabelEXT
    vkCmdNextSubpass
    vkCmdNextSubpass2
    vkCmdNextSubpass2KHR
    vkCmdPipelineBarrier
    vkCmdPipelineBarrier2
    vkCmdPipelineBarrier2KHR
    vkCmdPushConstants
    vkCmdPushConstants2
    vkCmdPushConstants2KHR
    vkCmdPushDescriptorSet
    vkCmdPushDescriptorSet2
    vkCmdPushDescriptorSet2KHR
    vkCmdPushDescriptorSetKHR
    vkCmdPushDescriptorSetWithTemplate
    vkCmdPushDescriptorSetWithTemplate2
    vkCmdPushDescriptorSetWithTemplate2KHR
    vkCmdPushDescriptorSetWithTemplateKHR
    vkCmdResetEvent
    vkCmdResetEvent2
    vkCmdResetEvent2KHR
    vkCmdResetQueryPool
    vkCmdResolveImage
    vkCmdResolveImage2
    vkCmdResolveImage2KHR
    vkCmdSetAlphaToCoverageEnableEXT
    vkCmdSetAlphaToOneEnableEXT
    vkCmdSetBlendConstants
    vkCmdSetColorBlendAdvancedEXT
    vkCmdSetColorBlendEnableEXT
    vkCmdSetColorBlendEquationEXT
    vkCmdSetColorWriteMaskEXT
    vkCmdSetConservativeRasterizationModeEXT
    vkCmdSetCullMode
    vkCmdSetCullModeEXT
    vkCmdSetDepthBias
    vkCmdSetDepthBiasEnable
    vkCmdSetDepthBiasEnableEXT
    vkCmdSetDepthBounds
    vkCmdSetDepthBoundsTestEnable
    vkCmdSetDepthBoundsTestEnableEXT
    vkCmdSetDepthClampEnableEXT
    vkCmdSetDepthClipEnableEXT
    vkCmdSetDepthClipNegativeOneToOneEXT
    vkCmdSetDepthCompareOp
    vkCmdSetDepthCompareOpEXT
    vkCmdSetDepthTestEnable
    vkCmdSetDepthTestEnableEXT
    vkCmdSetDepthWriteEnable
    vkCmdSetDepthWriteEnableEXT
    vkCmdSetDeviceMask
    vkCmdSetDeviceMaskKHR
    vkCmdSetEvent
    vkCmdSetEvent2
    vkCmdSetEvent2KHR
    vkCmdSetExtraPrimitiveOverestimationSizeEXT
    vkCmdSetFrontFace
    vkCmdSetFrontFaceEXT
    vkCmdSetLineRasterizationModeEXT
    vkCmdSetLineStipple
    vkCmdSetLineStippleEnableEXT
    vkCmdSetLineStippleEXT
    vkCmdSetLineStippleKHR
    vkCmdSetLineWidth
    vkCmdSetLogicOpEnableEXT
    vkCmdSetLogicOpEXT
    vkCmdSetPatchControlPointsEXT
    vkCmdSetPolygonModeEXT
    vkCmdSetPrimitiveRestartEnable
    vkCmdSetPrimitiveRestartEnableEXT
    vkCmdSetPrimitiveTopology
    vkCmdSetPrimitiveTopologyEXT
    vkCmdSetProvokingVertexModeEXT
    vkCmdSetRasterizationSamplesEXT
    vkCmdSetRasterizationStreamEXT
    vkCmdSetRasterizerDiscardEnable
    vkCmdSetRasterizerDiscardEnableEXT
    vkCmdSetRenderingAttachmentLocations
    vkCmdSetRenderingAttachmentLocationsKHR
    vkCmdSetRenderingInputAttachmentIndices
    vkCmdSetRenderingInputAttachmentIndicesKHR
    vkCmdSetSampleLocationsEnableEXT
    vkCmdSetSampleMaskEXT
    vkCmdSetScissor
    vkCmdSetScissorWithCount
    vkCmdSetScissorWithCountEXT
    vkCmdSetStencilCompareMask
    vkCmdSetStencilOp
    vkCmdSetStencilOpEXT
    vkCmdSetStencilReference
    vkCmdSetStencilTestEnable
    vkCmdSetStencilTestEnableEXT
    vkCmdSetStencilWriteMask
    vkCmdSetTessellationDomainOriginEXT
    vkCmdSetViewport
    vkCmdSetViewportWithCount
    vkCmdSetViewportWithCountEXT
    vkCmdUpdateBuffer
    vkCmdWaitEvents
    vkCmdWaitEvents2
    vkCmdWaitEvents2KHR
    vkCmdWriteTimestamp
    vkCmdWriteTimestamp2
    vkCmdWriteTimestamp2KHR
    vkCopyImageToImage
    vkCopyImageToImageEXT
    vkCopyImageToMemory
    vkCopyImageToMemoryEXT
    vkCopyMemoryToImage
    vkCopyMemoryToImageEXT
    vkCreateBuffer
    vkCreateBufferView
    vkCreateCommandPool
    vkCreateComputePipelines
    vkCreateDebugReportCallbackEXT
    vkCreateDebugUtilsMessengerEXT
    vkCreateDeferredOperationKHR
    vkCreateDescriptorPool
    vkCreateDescriptorSetLayout
    vkCreateDescriptorUpdateTemplate
    vkCreateDescriptorUpdateTemplateKHR
    vkCreateDevice
    vkCreateEvent
    vkCreateFence
    vkCreateFramebuffer
    vkCreateGraphicsPipelines
    vkCreateHeadlessSurfaceEXT
    vkCreateImage
    vkCreateImageView
    vkCreatePipelineCache
    vkCreatePipelineLayout
    vkCreatePrivateDataSlot
    vkCreatePrivateDataSlotEXT
    vkCreateQueryPool
    vkCreateRenderPass
    vkCreateRenderPass2
    vkCreateRenderPass2KHR
    vkCreateSampler
    vkCreateSamplerYcbcrConversion
    vkCreateSamplerYcbcrConversionKHR
    vkCreateSemaphore
    vkCreateShaderModule
    vkCreateSwapchainKHR
    vkDebugMarkerSetObjectNameEXT
    vkDebugMarkerSetObjectTagEXT
    vkDebugReportMessageEXT
    vkDeferredOperationJoinKHR
    vkDestroyBuffer
    vkDestroyBufferView
    vkDestroyCommandPool
    vkDestroyDebugReportCallbackEXT
    vkDestroyDebugUtilsMessengerEXT
    vkDestroyDeferredOperationKHR
    vkDestroyDescriptorPool
    vkDestroyDescriptorSetLayout
    vkDestroyDescriptorUpdateTemplate
    vkDestroyDescriptorUpdateTemplateKHR
    vkDestroyEvent
    vkDestroyFence
    vkDestroyFramebuffer
    vkDestroyImage
    vkDestroyImageView
    vkDestroyPipeline
    vkDestroyPipelineCache
    vkDestroyPipelineLayout
    vkDestroyPrivateDataSlot
    vkDestroyPrivateDataSlotEXT
    vkDestroyQueryPool
    vkDestroyRenderPass
    vkDestroySampler
    vkDestroySamplerYcbcrConversion
    vkDestroySamplerYcbcrConversionKHR
    vkDestroySemaphore
    vkDestroyShaderModule
    vkDestroySurfaceKHR
    vkDestroySwapchainKHR
    vkDeviceWaitIdle
    vkEndCommandBuffer
    vkEnumerateDeviceLayerProperties
    vkEnumerateInstanceLayerProperties
    vkEnumeratePhysicalDeviceGroups
    vkEnumeratePhysicalDeviceGroupsKHR
    vkEnumeratePhysicalDevices
    vkExportMetalObjectsEXT
    vkFlushMappedMemoryRanges
    vkFreeCommandBuffers
    vkFreeDescriptorSets
    vkFreeMemory
    vkGetBufferDeviceAddress
    vkGetBufferDeviceAddressEXT
    vkGetBufferDeviceAddressKHR
    vkGetBufferMemoryRequirements
    vkGetBufferMemoryRequirements2
    vkGetBufferMemoryRequirements2KHR
    vkGetBufferOpaqueCaptureAddress
    vkGetBufferOpaqueCaptureAddressKHR
    vkGetCalibratedTimestampsEXT
    vkGetCalibratedTimestampsKHR
    vkGetDeferredOperationMaxConcurrencyKHR
    vkGetDeferredOperationResultKHR
    vkGetDescriptorSetLayoutSupport
    vkGetDescriptorSetLayoutSupportKHR
    vkGetDeviceBufferMemoryRequirements
    vkGetDeviceBufferMemoryRequirementsKHR
    vkGetDeviceGroupPeerMemoryFeatures
    vkGetDeviceGroupPeerMemoryFeaturesKHR
    vkGetDeviceGroupPresentCapabilitiesKHR
    vkGetDeviceGroupSurfacePresentModesKHR
    vkGetDeviceImageMemoryRequirements
    vkGetDeviceImageMemoryRequirementsKHR
    vkGetDeviceImageSparseMemoryRequirements
    vkGetDeviceImageSparseMemoryRequirementsKHR
    vkGetDeviceImageSubresourceLayout
    vkGetDeviceImageSubresourceLayoutKHR
    vkGetDeviceMemoryCommitment
    vkGetDeviceMemoryOpaqueCaptureAddress
    vkGetDeviceMemoryOpaqueCaptureAddressKHR
    vkGetDeviceQueue
    vkGetDeviceQueue2
    vkGetEventStatus
    vkGetFenceStatus
    vkGetImageMemoryRequirements
    vkGetImageMemoryRequirements2
    vkGetImageMemoryRequirements2KHR
    vkGetImageSparseMemoryRequirements
    vkGetImageSparseMemoryRequirements2
    vkGetImageSparseMemoryRequirements2KHR
    vkGetImageSubresourceLayout
    vkGetImageSubresourceLayout2
    vkGetImageSubresourceLayout2EXT
    vkGetImageSubresourceLayout2KHR
    vkGetIOSurfaceMVK
    vkGetMemoryHostPointerPropertiesEXT
    vkGetMemoryMetalHandleEXT
    vkGetMemoryMetalHandlePropertiesEXT
    vkGetMoltenVKConfigurationMVK
    vkGetMTLBufferMVK
    vkGetMTLCommandQueueMVK
    vkGetMTLDeviceMVK
    vkGetMTLTextureMVK
    vkGetPastPresentationTimingGOOGLE
    vkGetPerformanceStatisticsMVK
    vkGetPhysicalDeviceCalibrateableTimeDomainsEXT
    vkGetPhysicalDeviceCalibrateableTimeDomainsKHR
    vkGetPhysicalDeviceExternalBufferProperties
    vkGetPhysicalDeviceExternalBufferPropertiesKHR
    vkGetPhysicalDeviceExternalFenceProperties
    vkGetPhysicalDeviceExternalFencePropertiesKHR
    vkGetPhysicalDeviceExternalSemaphoreProperties
    vkGetPhysicalDeviceExternalSemaphorePropertiesKHR
    vkGetPhysicalDeviceFeatures
    vkGetPhysicalDeviceFeatures2
    vkGetPhysicalDeviceFeatures2KHR
    vkGetPhysicalDeviceFormatProperties
    vkGetPhysicalDeviceFormatProperties2
    vkGetPhysicalDeviceFormatProperties2KHR
    vkGetPhysicalDeviceImageFormatProperties
    vkGetPhysicalDeviceImageFormatProperties2
    vkGetPhysicalDeviceImageFormatProperties2KHR
    vkGetPhysicalDeviceMemoryProperties
    vkGetPhysicalDeviceMemoryProperties2
    vkGetPhysicalDeviceMemoryProperties2KHR
    vkGetPhysicalDeviceMetalFeaturesMVK
    vkGetPhysicalDevicePresentRectanglesKHR
    vkGetPhysicalDeviceProperties
    vkGetPhysicalDeviceProperties2
    vkGetPhysicalDeviceProperties2KHR
    vkGetPhysicalDeviceQueueFamilyProperties
    vkGetPhysicalDeviceQueueFamilyProperties2
    vkGetPhysicalDeviceQueueFamilyProperties2KHR
    vkGetPhysicalDeviceSparseImageFormatProperties
    vkGetPhysicalDeviceSparseImageFormatProperties2
    vkGetPhysicalDeviceSparseImageFormatProperties2KHR
    vkGetPhysicalDeviceSurfaceCapabilities2KHR
    vkGetPhysicalDeviceSurfaceCapabilitiesKHR
    vkGetPhysicalDeviceSurfaceFormats2KHR
    vkGetPhysicalDeviceSurfaceFormatsKHR
    vkGetPhysicalDeviceSurfacePresentModesKHR
    vkGetPhysicalDeviceSurfaceSupportKHR
    vkGetPhysicalDeviceToolProperties
    vkGetPhysicalDeviceToolPropertiesEXT
    vkGetPipelineCacheData
    vkGetPrivateData
    vkGetPrivateDataEXT
    vkGetQueryPoolResults
    vkGetRefreshCycleDurationGOOGLE
    vkGetRenderAreaGranularity
    vkGetRenderingAreaGranularity
    vkGetRenderingAreaGranularityKHR
    vkGetSemaphoreCounterValue
    vkGetSemaphoreCounterValueKHR
    vkGetSwapchainImagesKHR
    vkGetVersionStringsMVK
    vkInvalidateMappedMemoryRanges
    vkMapMemory
    vkMapMemory2
    vkMapMemory2KHR
    vkMergePipelineCaches
    vkQueueBeginDebugUtilsLabelEXT
    vkQueueBindSparse
    vkQueueEndDebugUtilsLabelEXT
    vkQueueInsertDebugUtilsLabelEXT
    vkQueuePresentKHR
    vkQueueSubmit
    vkQueueSubmit2
    vkQueueSubmit2KHR
    vkQueueWaitIdle
    vkReleaseSwapchainImagesEXT
    vkReleaseSwapchainImagesKHR
    vkResetCommandBuffer
    vkResetCommandPool
    vkResetDescriptorPool
    vkResetEvent
    vkResetFences
    vkResetQueryPool
    vkResetQueryPoolEXT
    vkSetDebugUtilsObjectNameEXT
    vkSetDebugUtilsObjectTagEXT
    vkSetEvent
    vkSetHdrMetadataEXT
    vkSetMoltenVKConfigurationMVK
    vkSetMTLTextureMVK
    vkSetPrivateData
    vkSetPrivateDataEXT
    vkSetWorkgroupSizeMVK
    vkSignalSemaphore
    vkSignalSemaphoreKHR
    vkSubmitDebugUtilsMessageEXT
    vkTransitionImageLayout
    vkTransitionImageLayoutEXT
    vkTrimCommandPool
    vkTrimCommandPoolKHR
    vkUnmapMemory
    vkUnmapMemory2
    vkUnmapMemory2KHR
    vkUpdateDescriptorSets
    vkUpdateDescriptorSetWithTemplate
    vkUpdateDescriptorSetWithTemplateKHR
    vkUseIOSurfaceMVK
    vkWaitForFences
    vkWaitForPresent2KHR
    vkWaitForPresentKHR
    vkWaitSemaphores
    vkWaitSemaphoresKHR
}
