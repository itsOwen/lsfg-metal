// native metal generator: the dll's spir-v translated to msl, the same signature encoded straight on metal
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};

use ash::vk;
use objc2::rc::Retained;
use objc2::Message;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBarrierScope, MTLCommandBuffer, MTLCompileOptions, MTLComputePipelineDescriptor, MTLLanguageVersion, MTLPipelineOption, MTLCommandEncoder, MTLComputeCommandEncoder,
    MTLComputePipelineState, MTLDevice, MTLDispatchType, MTLFence, MTLHazardTrackingMode, MTLHeap,
    MTLHeapDescriptor, MTLHeapType, MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLSamplerAddressMode, MTLSamplerBorderColor, MTLSamplerDescriptor,
    MTLSamplerMinMagFilter, MTLSamplerMipFilter, MTLSamplerState, MTLSize, MTLStorageMode,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use spirv_cross2::compile::msl::{BindTarget, CompilerOptions, MetalPlatform, MslVersion, ResourceBinding};
use spirv_cross2::reflect::{DecorationValue, ExecutionModeArguments};
use spirv_cross2::spirv::{Decoration, ExecutionMode, ExecutionModel};
use spirv_cross2::{targets::Msl, Compiler, Module};

use super::pipeline::plan;
use super::signature::*;
use crate::shaders;

pub type Device = ProtocolObject<dyn MTLDevice>;
pub type Texture = Retained<ProtocolObject<dyn MTLTexture>>;
type Cb = ProtocolObject<dyn MTLCommandBuffer>;

// fixed slots: the uniform block, the push constants, and sampler binding b at b - 1
const BLOCK: usize = 0;
const PUSH: usize = 1;

// copies in and out of the pipeline; drawables are written by a render pass, which every drawable format allows
const COPY: &str = r#"
#include <metal_stdlib>
using namespace metal;
kernel void copy_in(texture2d<float> src [[texture(0)]], texture2d_array<float, access::write> dst [[texture(1)]],
                    constant uint& layer [[buffer(0)]], uint2 id [[thread_position_in_grid]]) {
    if (id.x < dst.get_width() && id.y < dst.get_height()) dst.write(src.read(id), id, layer);
}
struct V { float4 pos [[position]]; };
vertex V full(uint i [[vertex_id]]) {
    V v;
    v.pos = float4(float2((i << 1) & 2, i & 2) * 2.0 - 1.0, 0.0, 1.0);
    return v;
}
fragment float4 copy_out(V v [[stage_in]], texture2d<float> src [[texture(0)]]) {
    return src.read(uint2(v.pos.xy));
}
"#;

// one translated shader: its pipeline, threadgroup size and (image binding, first texture slot) pairs
struct Kernel {
    pso: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    group: MTLSize,
    textures: Vec<(usize, usize)>,
}

fn pixel_format(f: vk::Format) -> Result<MTLPixelFormat, String> {
    Ok(match f {
        vk::Format::R8_UNORM => MTLPixelFormat::R8Unorm,
        vk::Format::R8G8B8A8_UNORM => MTLPixelFormat::RGBA8Unorm,
        vk::Format::R16G16B16A16_SFLOAT => MTLPixelFormat::RGBA16Float,
        f => return Err(format!("no Metal format for {f:?}")),
    })
}

// kernels write any format, so the source and generated images take the storage that keeps the game's precision
fn store_format(s: Store) -> MTLPixelFormat {
    match s {
        Store::Rgba8 => MTLPixelFormat::RGBA8Unorm,
        Store::Rgb10a2 => MTLPixelFormat::RGB10A2Unorm,
        Store::Rgb9e5 => MTLPixelFormat::RGB9E5Float,
        Store::Bgr10Xr => MTLPixelFormat::BGR10_XR,
        Store::Rgba16f => MTLPixelFormat::RGBA16Float,
    }
}

// spirv-cross output for one shader: its source, threadgroup size and texture slots
#[derive(Clone)]
struct Translated {
    msl: String,
    group: MTLSize,
    textures: Vec<(usize, usize)>,
}

// the translation depends only on the shader and the signature, so a rebuild for a new size reuses it
static TRANSLATED: Mutex<BTreeMap<(u32, bool), Translated>> = Mutex::new(BTreeMap::new());

fn translate(device: &Device, id: u32, words: &[u32], sig: &Signature) -> Result<Kernel, String> {
    let cached = TRANSLATED.lock().unwrap_or_else(PoisonError::into_inner).get(&(id, sig.perf)).cloned();
    let t = match cached {
        Some(t) => t,
        None => {
            let t = to_msl(words, sig)?;
            TRANSLATED.lock().unwrap_or_else(PoisonError::into_inner).insert((id, sig.perf), t.clone());
            t
        }
    };
    let (group, textures) = (t.group, t.textures);
    let lib = library(device, &t.msl)?;
    // without the group size the compiler may cap a register-heavy kernel below it, and the extra threads never run
    let threads = group.width * group.height * group.depth;
    let desc = MTLComputePipelineDescriptor::new();
    desc.setComputeFunction(Some(&*function(&lib, "main0")?));
    desc.setMaxTotalThreadsPerThreadgroup(threads);
    let pso = device
        .newComputePipelineStateWithDescriptor_options_reflection_error(&desc, MTLPipelineOption::None, None)
        .map_err(|e| e.localizedDescription().to_string())?;
    if pso.maxTotalThreadsPerThreadgroup() < threads {
        return Err(format!(
            "pipeline runs {} threads per group, the shader needs {threads}",
            pso.maxTotalThreadsPerThreadgroup()
        ));
    }
    Ok(Kernel {
        pso,
        group,
        textures,
    })
}

fn to_msl(words: &[u32], sig: &Signature) -> Result<Translated, String> {
    let e = |e: spirv_cross2::SpirvCrossError| e.to_string();
    let mut c = Compiler::<Msl>::new(Module::from_words(words)).map_err(e)?;
    let res = c.shader_resources().map_err(e)?.all_resources().map_err(e)?;
    let literal = |c: &Compiler<Msl>, id, d| match c.decoration(id, d) {
        Ok(Some(DecorationValue::Literal(v))) => Some(v),
        _ => None,
    };
    let mut images = vec![];
    for r in res.separate_images.iter().chain(&res.storage_images) {
        let b = literal(&c, r.id, Decoration::Binding).ok_or("image without a binding")?;
        images.push((b, literal(&c, r.id, Decoration::DescriptorSet).unwrap_or(0)));
    }
    images.sort();
    let mut binds = vec![];
    let mut textures = vec![];
    let mut slot = 0u32;
    for (b, set) in images {
        let n = match sig.bindings.get(b as usize) {
            Some(bind @ (Binding::Storage(_) | Binding::Sampled(_))) => sig.count(bind),
            _ => return Err(format!("image binding {b} is not an image in the signature")),
        };
        binds.push((ResourceBinding::from_qualified(set, b), (0, slot, 0)));
        textures.push((b as usize, slot as usize));
        slot += n;
    }
    for r in &res.separate_samplers {
        let b = literal(&c, r.id, Decoration::Binding).ok_or("sampler without a binding")?;
        let set = literal(&c, r.id, Decoration::DescriptorSet).unwrap_or(0);
        let slot = b.checked_sub(1).ok_or("sampler at binding 0")?;
        binds.push((ResourceBinding::from_qualified(set, b), (0, 0, slot)));
    }
    for r in &res.uniform_buffers {
        let b = literal(&c, r.id, Decoration::Binding).ok_or("buffer without a binding")?;
        let set = literal(&c, r.id, Decoration::DescriptorSet).unwrap_or(0);
        binds.push((ResourceBinding::from_qualified(set, b), (BLOCK as u32, 0, 0)));
    }
    binds.push((ResourceBinding::PushConstantBuffer, (PUSH as u32, 0, 0)));
    for (r, (buffer, texture, sampler)) in binds {
        let t = BindTarget {
            buffer,
            texture,
            sampler,
            count: None,
        };
        c.add_resource_binding(ExecutionModel::GLCompute, r, &t).map_err(e)?;
    }
    let group = match c.execution_mode_arguments(ExecutionMode::LocalSize).map_err(e)? {
        Some(ExecutionModeArguments::LocalSize { x, y, z }) => MTLSize {
            width: x as usize,
            height: y as usize,
            depth: z as usize,
        },
        _ => return Err("shader has no literal local size".into()),
    };
    let mut o = CompilerOptions::default();
    // macos 11, the oldest the library supports
    o.version = MslVersion::new(2, 3, 0);
    o.platform = MetalPlatform::MacOS;
    let msl = c.compile(&o).map_err(e)?.to_string();
    Ok(Translated { msl, group, textures })
}

// pinned to msl 2.3, so a newer construct fails here instead of only on an older macos
fn library(device: &Device, source: &str) -> Result<Retained<ProtocolObject<dyn MTLLibrary>>, String> {
    let options = MTLCompileOptions::new();
    options.setLanguageVersion(MTLLanguageVersion::Version2_3);
    device
        .newLibraryWithSource_options_error(&NSString::from_str(source), Some(&options))
        .map_err(|e| e.localizedDescription().to_string())
}

fn function(
    lib: &ProtocolObject<dyn MTLLibrary>,
    name: &str,
) -> Result<Retained<ProtocolObject<dyn objc2_metal::MTLFunction>>, String> {
    lib.newFunctionWithName(&NSString::from_str(name))
        .ok_or_else(|| format!("no function {name} in the translated library"))
}

fn bytes<T>(v: &T) -> NonNull<c_void> {
    NonNull::from(v).cast()
}

pub struct Pipeline {
    device: Retained<Device>,
    pub sig: Signature,
    pub extent: (u32, u32),
    pub flow: f32,
    pub colour: Colour,
    kernels: Vec<Kernel>,
    images: Vec<Vec<Texture>>,
    _heaps: Vec<Retained<ProtocolObject<dyn MTLHeap>>>,
    samplers: Vec<Retained<ProtocolObject<dyn MTLSamplerState>>>,
    fence: Retained<ProtocolObject<dyn MTLFence>>,
    tabs: Vec<StageTab>,
    // timestamp, iteration, colour kind, hdr flag, inverse flow scale, ui threshold
    block: [u32; 6],
    copy_in: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    copy_lib: Retained<ProtocolObject<dyn MTLLibrary>>,
    copy_out: HashMap<MTLPixelFormat, Retained<ProtocolObject<dyn MTLRenderPipelineState>>>,
}

impl Pipeline {
    // `spirv` is the dll's resource map; fp16 picks the half-precision variants
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &Device,
        spirv: &HashMap<u32, Vec<u32>>,
        fp16: bool,
        (w, h): (u32, u32),
        flow: f32,
        perf: bool,
        colour: Colour,
        log: fn(&str),
    ) -> Result<Pipeline, String> {
        log(&format!(
            "Building native pipeline for {w}x{h} at {flow:.2} flow ({})",
            if perf { "performance" } else { "quality" }
        ));
        let sig = Signature::new(perf);
        let t0 = std::time::Instant::now();
        // one thread per shader: a first run compiles every one, and the compiler service works on them in parallel
        let kernels = std::thread::scope(|s| {
            let jobs: Vec<_> = (0..SHADERS.len())
                .map(|sh| {
                    let sig = &sig;
                    // spawn would panic, and so abort the game, when no thread can be made
                    std::thread::Builder::new().spawn_scoped(s, move || {
                        let name = match sh {
                            GEN if colour.float() => "generate_16bit",
                            GEN => "generate_8bit",
                            _ => SHADERS[sh],
                        };
                        let (id, words) = shaders::key_of(name, perf, fp16)
                            .and_then(|k| Some((k, spirv.get(&k)?)))
                            .ok_or_else(|| format!("Shader '{name}' missing from DLL"))?;
                        translate(device, id, words, sig).map_err(|e| format!("shader '{name}': {e}"))
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|j| {
                    j.map_err(|e| format!("no thread for shader translation: {e}"))?
                        .join()
                        .unwrap_or_else(|_| Err("shader translation panicked".into()))
                })
                .collect::<Result<Vec<_>, _>>()
        })?;
        log(&format!(
            "  Translated and compiled {} shaders in {:.0} ms",
            kernels.len(),
            t0.elapsed().as_secs_f64() * 1e3
        ));

        // images: external ones stand alone, internal ones are placed in two heaps by the lifetime planner
        let desc = |im: &Image, sub: u32| -> Result<Retained<MTLTextureDescriptor>, String> {
            let (x, y) = im.extent(sub, w, h, flow);
            let d = MTLTextureDescriptor::new();
            unsafe {
                d.setTextureType(if im.array_view() {
                    MTLTextureType::Type2DArray
                } else {
                    MTLTextureType::Type2D
                });
                d.setPixelFormat(if im.is(H) { store_format(colour.store) } else { pixel_format(im.format(false))? });
                d.setWidth(x as usize);
                d.setHeight(y as usize);
                d.setArrayLength(im.layers() as usize);
            }
            d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
            d.setStorageMode(MTLStorageMode::Private);
            d.setHazardTrackingMode(if im.is(I | O) {
                MTLHazardTrackingMode::Tracked
            } else {
                MTLHazardTrackingMode::Untracked
            });
            Ok(d)
        };
        let mut images: Vec<Vec<Texture>> = vec![vec![]; sig.images.len()];
        let mut items = vec![];
        let mut internal = vec![];
        let mut align = 1u64;
        for (idx, im) in sig.images.iter().enumerate() {
            if im.is(I | O) {
                let t = device
                    .newTextureWithDescriptor(&*desc(im, 0)?)
                    .ok_or("could not create an external texture")?;
                images[idx].push(t);
                continue;
            }
            let mut size = 0;
            for sub in 0..im.sub_images() {
                let sa = device.heapTextureSizeAndAlignWithDescriptor(&*desc(im, sub)?);
                align = align.max(sa.align as u64);
                size += (sa.size as u64).next_multiple_of(sa.align as u64);
            }
            items.push((size, im.lifetime));
            internal.push(idx);
        }
        // every segment starts on the largest alignment, so round each up to it
        let items: Vec<_> = items
            .into_iter()
            .map(|(s, l)| (s.next_multiple_of(align), l))
            .collect();
        let (transient, pinned, offsets) = plan(&items);
        let mut heaps = vec![];
        for size in [transient, pinned] {
            let d = MTLHeapDescriptor::new();
            d.setType(MTLHeapType::Placement);
            d.setStorageMode(MTLStorageMode::Private);
            d.setHazardTrackingMode(MTLHazardTrackingMode::Untracked);
            d.setSize(size.max(align) as usize);
            heaps.push(device.newHeapWithDescriptor(&d).ok_or("could not create a Metal heap")?);
        }
        for (&idx, &(heap, off)) in internal.iter().zip(&offsets) {
            let im = &sig.images[idx];
            let mut o = off;
            for sub in 0..im.sub_images() {
                let d = desc(im, sub)?;
                let sa = device.heapTextureSizeAndAlignWithDescriptor(&d);
                let t = unsafe { heaps[heap].newTextureWithDescriptor_offset(&d, o as usize) }
                    .ok_or("could not place a texture in its heap")?;
                images[idx].push(t);
                o += (sa.size as u64).next_multiple_of(sa.align as u64);
            }
        }
        log(&format!(
            "  Placed {} images in heaps of {transient} and {pinned} bytes",
            internal.len()
        ));

        let mut samplers = vec![];
        for border in [
            Some(MTLSamplerBorderColor::TransparentBlack),
            Some(MTLSamplerBorderColor::OpaqueWhite),
            None,
        ] {
            let d = MTLSamplerDescriptor::new();
            d.setMinFilter(MTLSamplerMinMagFilter::Linear);
            d.setMagFilter(MTLSamplerMinMagFilter::Linear);
            d.setMipFilter(MTLSamplerMipFilter::Linear);
            let mode = match border {
                Some(b) => {
                    d.setBorderColor(b);
                    MTLSamplerAddressMode::ClampToBorderColor
                }
                None => MTLSamplerAddressMode::ClampToEdge,
            };
            d.setSAddressMode(mode);
            d.setTAddressMode(mode);
            d.setRAddressMode(mode);
            samplers.push(
                device
                    .newSamplerStateWithDescriptor(&d)
                    .ok_or("could not create a sampler")?,
            );
        }

        let copy_lib = library(device, COPY)?;
        let copy_in = device
            .newComputePipelineStateWithFunction_error(&*function(&copy_lib, "copy_in")?)
            .map_err(|e| e.localizedDescription().to_string())?;
        let tabs = sig.tabs(w, h, flow);
        Ok(Pipeline {
            fence: device.newFence().ok_or("could not create a Metal fence")?,
            device: device.retain(),
            extent: (w, h),
            flow,
            colour,
            kernels,
            images,
            _heaps: heaps,
            samplers,
            tabs,
            block: [
                0,
                0,
                colour.block()[0],
                colour.block()[1],
                (1.0 / flow).to_bits(),
                0.5f32.to_bits(),
            ],
            copy_in,
            copy_lib,
            copy_out: HashMap::new(),
            sig,
        })
    }

    // the two-layer image the game's frames go into
    pub fn source(&self) -> &Texture {
        let i = self.sig.images.iter().position(|im| im.is(I)).unwrap();
        &self.images[i][0]
    }

    // the generated frame
    pub fn destination(&self) -> &Texture {
        let i = self.sig.images.iter().position(|im| im.is(O)).unwrap();
        &self.images[i][0]
    }

    // the pre-pass (main false) or one main pass; the block is copied into the command buffer, so passes never race on it
    pub fn encode(&self, cb: &Cb, main: bool, iteration: u32, timestamp: f32) -> Result<(), String> {
        let enc = cb
            .computeCommandEncoderWithDispatchType(MTLDispatchType::Concurrent)
            .ok_or("no Metal compute encoder")?;
        enc.waitForFence(&self.fence);
        let mut block = self.block;
        block[0] = timestamp.to_bits();
        block[1] = iteration;
        unsafe {
            enc.setBytes_length_atIndex(bytes(&block), 24, BLOCK);
            for (i, s) in self.samplers.iter().enumerate() {
                enc.setSamplerState_atIndex(Some(s), i);
            }
        }
        let range = if main {
            self.sig.split..self.tabs.len()
        } else {
            0..self.sig.split
        };
        let first = range.start;
        for st in range {
            // every stage reads what the one before wrote
            if st != first {
                enc.memoryBarrierWithScope(MTLBarrierScope::Textures);
            }
            for (sh, dispatches) in &self.tabs[st].subs {
                let k = &self.kernels[*sh];
                enc.setComputePipelineState(&k.pso);
                for &(b, slot) in &k.textures {
                    let (Binding::Storage(v) | Binding::Sampled(v)) = &self.sig.bindings[b] else {
                        continue;
                    };
                    for (j, t) in v.iter().flat_map(|&i| &self.images[i]).enumerate() {
                        unsafe { enc.setTexture_atIndex(Some(t), slot + j) };
                    }
                }
                for &(sub, (x, y), special) in dispatches {
                    // metal rejects an empty grid where vulkan skips it
                    if x == 0 || y == 0 {
                        continue;
                    }
                    let pc = [special as u32, sub];
                    unsafe { enc.setBytes_length_atIndex(bytes(&pc), 8, PUSH) };
                    let groups = MTLSize {
                        width: x as usize,
                        height: y as usize,
                        depth: 1,
                    };
                    enc.dispatchThreadgroups_threadsPerThreadgroup(groups, k.group);
                }
            }
        }
        enc.updateFence(&self.fence);
        enc.endEncoding();
        Ok(())
    }

    // a game frame into source layer `layer`; an srgb texture must come as a unorm view so it is not linearised
    pub fn copy_in(&self, cb: &Cb, src: &ProtocolObject<dyn MTLTexture>, layer: u32) -> Result<(), String> {
        let enc = cb.computeCommandEncoder().ok_or("no Metal compute encoder")?;
        enc.setComputePipelineState(&self.copy_in);
        unsafe {
            enc.setTexture_atIndex(Some(src), 0);
            enc.setTexture_atIndex(Some(self.source()), 1);
            enc.setBytes_length_atIndex(bytes(&layer), 4, 0);
        }
        let (w, h) = self.extent;
        enc.dispatchThreads_threadsPerThreadgroup(
            MTLSize {
                width: w as usize,
                height: h as usize,
                depth: 1,
            },
            MTLSize {
                width: 16,
                height: 16,
                depth: 1,
            },
        );
        enc.endEncoding();
        Ok(())
    }

    // the generated frame into a drawable, converting to its format
    pub fn copy_out(&mut self, cb: &Cb, dst: &ProtocolObject<dyn MTLTexture>) -> Result<(), String> {
        let format = dst.pixelFormat();
        if !self.copy_out.contains_key(&format) {
            let d = MTLRenderPipelineDescriptor::new();
            d.setVertexFunction(Some(&*function(&self.copy_lib, "full")?));
            d.setFragmentFunction(Some(&*function(&self.copy_lib, "copy_out")?));
            unsafe { d.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(format);
            let pso = self
                .device
                .newRenderPipelineStateWithDescriptor_error(&d)
                .map_err(|e| e.localizedDescription().to_string())?;
            self.copy_out.insert(format, pso);
        }
        let pass = MTLRenderPassDescriptor::new();
        let a = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        a.setTexture(Some(dst));
        a.setLoadAction(MTLLoadAction::DontCare);
        a.setStoreAction(MTLStoreAction::Store);
        let enc = cb
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or("no Metal render encoder")?;
        enc.setRenderPipelineState(&self.copy_out[&format]);
        unsafe {
            enc.setFragmentTexture_atIndex(Some(self.destination()), 0);
            enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }
        enc.endEncoding();
        Ok(())
    }
}
