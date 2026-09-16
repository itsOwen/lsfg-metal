// pipeline construction and recording
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ash::vk;

use super::signature::*;
use super::Instance;
use crate::vkutil::{self, check};

struct Img {
    handles: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    layers: u32,
}

pub struct Pipeline {
    inst: Arc<Instance>,
    pub sig: Signature,
    pub extent: (u32, u32),
    pub flow: f32,
    pub hdr: bool,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    imgs: Vec<Img>,
    memories: Vec<vk::DeviceMemory>,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    block: vk::Buffer,
    block_mem: vk::DeviceMemory,
    block_ptr: *mut u32,
    samplers: Vec<vk::Sampler>,
    cache: vk::PipelineCache,
    pipelines: Vec<vk::Pipeline>,
    cmd_pool: vk::CommandPool,
    pub cmd: [vk::CommandBuffer; 2],
    params: HashMap<u64, vk::CommandBuffer>,
    pub source: vk::Image,
    pub destination: vk::Image,
}

// the only raw pointer is the persistently mapped uniform block, written from whichever thread drives the context
unsafe impl Send for Pipeline {}
unsafe impl Sync for Pipeline {}

// greedy lifetime aliasing: items are (size, lifetime or none when pinned); returns (transient size, pinned size, (allocation, offset) per item)
pub fn plan(items: &[(u64, Option<(usize, usize)>)]) -> (u64, u64, Vec<(usize, u64)>) {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(items[i].0));
    let (mut transient, mut pinned) = (0u64, 0u64);
    let mut placed: Vec<(u64, u64, (usize, usize))> = vec![];
    let mut out = vec![(0usize, 0u64); items.len()];
    for i in order {
        let (size, life) = items[i];
        let Some(life) = life else {
            out[i] = (1, pinned);
            pinned += size;
            continue;
        };
        let mut off = 0;
        for &(o, end, other) in &placed {
            if life.0 > other.1 || life.1 < other.0 || off >= end || o >= off + size {
                continue;
            }
            off = end;
        }
        transient = transient.max(off + size);
        let pos = placed.partition_point(|p| p.0 <= off);
        placed.insert(pos, (off, off + size, life));
        out[i] = (0, off);
    }
    (transient, pinned, out)
}

// one cache file per device uuid and quality mode; XDG_CACHE_HOME overrides the mac location
// no home directory means no cache rather than a shared path under /tmp
fn cache_path(perf: bool, uuid: [u8; 16]) -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let dir = env("XDG_CACHE_HOME")
        .map(|c| PathBuf::from(c).join("lsfg-metal"))
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join("Library/Caches/lsfg-metal")))?;
    let _ = std::fs::create_dir_all(&dir);
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    let u = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    Some(dir.join(format!(
        "cache_{}_{u}.bin",
        if perf { "performance" } else { "quality" }
    )))
}

// one dispatch: (sub-iteration, groups, special)
type Dispatch = (u32, (u32, u32), bool);

struct StageTab {
    sampled: Vec<usize>,
    stored: Vec<usize>,
    // (shader, dispatches) runs
    subs: Vec<(usize, Vec<Dispatch>)>,
}

impl Pipeline {
    pub fn new(
        inst: Arc<Instance>,
        w: u32,
        h: u32,
        flow: f32,
        perf: bool,
        hdr: bool,
    ) -> Result<Pipeline, String> {
        let mut p = Pipeline {
            inst,
            sig: Signature::new(perf),
            extent: (w, h),
            flow,
            hdr,
            set_layout: Default::default(),
            layout: Default::default(),
            imgs: vec![],
            memories: vec![],
            pool: Default::default(),
            set: Default::default(),
            block: Default::default(),
            block_mem: Default::default(),
            block_ptr: std::ptr::null_mut(),
            samplers: vec![],
            cache: Default::default(),
            pipelines: vec![],
            cmd_pool: Default::default(),
            cmd: Default::default(),
            params: HashMap::new(),
            source: Default::default(),
            destination: Default::default(),
        };
        // a failure drops the partial pipeline, which destroys whatever was created
        p.build()?;
        Ok(p)
    }

    fn build(&mut self) -> Result<(), String> {
        let inst = self.inst.clone();
        let (d, pd, log) = (&inst.device, inst.physical_device, inst.log);
        let ((w, h), flow, hdr, perf) = (self.extent, self.flow, self.hdr, self.sig.perf);
        log(&format!(
            "Building pipeline for {w}x{h} at {flow:.2} flow ({})",
            if perf { "performance" } else { "quality" }
        ));

        // layouts
        let (mut sampled, mut storage) = (0u32, 0u32);
        let bindings: Vec<vk::DescriptorSetLayoutBinding> = self
            .sig
            .bindings
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let n = self.sig.count(b);
                let ty = match b {
                    Binding::Uniform => vk::DescriptorType::UNIFORM_BUFFER,
                    Binding::Sampler => vk::DescriptorType::SAMPLER,
                    Binding::Storage(_) => {
                        storage += n;
                        vk::DescriptorType::STORAGE_IMAGE
                    }
                    Binding::Sampled(_) => {
                        sampled += n;
                        vk::DescriptorType::SAMPLED_IMAGE
                    }
                };
                vk::DescriptorSetLayoutBinding::default()
                    .binding(i as u32)
                    .descriptor_type(ty)
                    .descriptor_count(n)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            })
            .collect();
        let info = vk::DescriptorSetLayoutCreateInfo::default()
            .bindings(&bindings)
            .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL);
        self.set_layout = check(
            unsafe { d.create_descriptor_set_layout(&info, None) },
            "vkCreateDescriptorSetLayout",
        )?;
        let push = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::COMPUTE,
            offset: 0,
            size: 8,
        }];
        let layouts = [self.set_layout];
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&layouts)
            .push_constant_ranges(&push);
        self.layout = check(
            unsafe { d.create_pipeline_layout(&info, None) },
            "vkCreatePipelineLayout",
        )?;
        log(&format!(
            "  Built descriptor set layout with {} bindings ({sampled} sampled, {storage} storage)",
            bindings.len()
        ));

        // images
        let (mut alignment, mut types) = (1u64, u32::MAX);
        let mut internal: Vec<usize> = vec![];
        let mut sizes: Vec<Vec<u64>> = vec![];
        for (idx, im) in self.sig.images.iter().enumerate() {
            let mut usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED;
            if im.is(I) {
                usage |= vk::ImageUsageFlags::TRANSFER_DST;
            }
            if im.is(O) {
                usage |= vk::ImageUsageFlags::TRANSFER_SRC;
            }
            let mut img = Img {
                handles: vec![],
                views: vec![],
                layers: im.layers(),
            };
            let mut sub_sizes = vec![];
            for sub in 0..im.sub_images() {
                let image = vkutil::create_image(
                    d,
                    im.format(hdr),
                    im.extent(sub, w, h, flow),
                    im.layers(),
                    usage,
                )?;
                img.handles.push(image);
                let r = unsafe { d.get_image_memory_requirements(image) };
                if im.is(I | O) {
                    let mem = vkutil::allocate(
                        &inst.instance,
                        pd,
                        d,
                        r.size,
                        r.memory_type_bits,
                        false,
                        Some(image),
                    )?;
                    self.memories.push(mem);
                    check(
                        unsafe { d.bind_image_memory(image, mem, 0) },
                        "vkBindImageMemory",
                    )?;
                    log(&format!(
                        "  Allocated memory of size {} for external image {idx}",
                        r.size
                    ));
                    if im.is(I) {
                        self.source = image;
                    } else {
                        self.destination = image;
                    }
                } else {
                    alignment = alignment.max(r.alignment);
                    types &= r.memory_type_bits;
                    sub_sizes.push(r.size);
                }
            }
            self.imgs.push(img);
            if !im.is(I | O) {
                internal.push(idx);
                sizes.push(sub_sizes);
            }
        }
        log(&format!(
            "  Created {} images with alignment {alignment} and memory type bits {types:x}",
            self.sig.images.len()
        ));
        if types == 0 {
            return Err("No memory type suits the pipeline images".into());
        }

        // memory planning
        let items: Vec<(u64, Option<(usize, usize)>)> = internal
            .iter()
            .zip(&sizes)
            .map(|(&idx, s)| {
                (
                    s.iter().map(|&z| vkutil::align_up(z, alignment)).sum(),
                    self.sig.images[idx].lifetime,
                )
            })
            .collect();
        let (transient, pinned, offsets) = plan(&items);
        log("  Computed 2 memory allocations");
        let mut allocs = vec![];
        for (alloc, size) in [transient, pinned].into_iter().enumerate() {
            let mem = vkutil::allocate(&inst.instance, pd, d, size, types, false, None)?;
            self.memories.push(mem);
            allocs.push(mem);
            let k = offsets.iter().filter(|o| o.0 == alloc).count();
            log(&format!(
                "  Allocated memory of size {size} for {k} segments"
            ));
        }
        for ((&idx, s), &(alloc, off)) in internal.iter().zip(&sizes).zip(&offsets) {
            let mut o = off;
            for (&image, &z) in self.imgs[idx].handles.iter().zip(s) {
                check(
                    unsafe { d.bind_image_memory(image, allocs[alloc], o) },
                    "vkBindImageMemory",
                )?;
                o += vkutil::align_up(z, alignment);
            }
        }

        // views
        for (idx, im) in self.sig.images.iter().enumerate() {
            let img = &mut self.imgs[idx];
            for &image in &img.handles {
                img.views.push(vkutil::create_view(
                    d,
                    image,
                    im.format(hdr),
                    im.layers(),
                    im.array_view(),
                )?);
            }
        }

        // pool and set
        let size = |ty, n| vk::DescriptorPoolSize {
            ty,
            descriptor_count: n,
        };
        let pool_sizes = [
            size(vk::DescriptorType::SAMPLER, 3),
            size(vk::DescriptorType::SAMPLED_IMAGE, sampled),
            size(vk::DescriptorType::STORAGE_IMAGE, storage),
            size(vk::DescriptorType::UNIFORM_BUFFER, 1),
        ];
        let info = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        self.pool = check(
            unsafe { d.create_descriptor_pool(&info, None) },
            "vkCreateDescriptorPool",
        )?;
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.pool)
            .set_layouts(&layouts);
        self.set = check(
            unsafe { d.allocate_descriptor_sets(&info) },
            "vkAllocateDescriptorSets",
        )?[0];

        // uniform block: timestamp, iteration, colour kind, hdr flag, inverse flow scale, ui threshold
        let block = [
            0u32,
            0,
            if hdr { 2 } else { 0 },
            if hdr { 1 } else { 0 },
            (1.0 / flow).to_bits(),
            0.5f32.to_bits(),
        ];
        let bytes: Vec<u8> = block.iter().flat_map(|v| v.to_le_bytes()).collect();
        let usage = vk::BufferUsageFlags::UNIFORM_BUFFER | vk::BufferUsageFlags::TRANSFER_DST;
        (self.block, self.block_mem) = vkutil::create_buffer(&inst.instance, pd, d, usage, &bytes)?;
        self.block_ptr = check(
            unsafe { d.map_memory(self.block_mem, 0, 24, vk::MemoryMapFlags::empty()) },
            "vkMapMemory",
        )? as *mut u32;

        // samplers
        use vk::{BorderColor as B, CompareOp as C, SamplerAddressMode as S};
        for (addr, op, border) in [
            (S::CLAMP_TO_BORDER, C::NEVER, B::FLOAT_TRANSPARENT_BLACK),
            (S::CLAMP_TO_BORDER, C::NEVER, B::FLOAT_OPAQUE_WHITE),
            (S::CLAMP_TO_EDGE, C::ALWAYS, B::FLOAT_TRANSPARENT_BLACK),
        ] {
            self.samplers
                .push(vkutil::create_sampler(d, addr, op, border)?);
        }

        // writes
        let buf = [vk::DescriptorBufferInfo {
            buffer: self.block,
            offset: 0,
            range: vk::WHOLE_SIZE,
        }];
        let sampler_infos: Vec<[vk::DescriptorImageInfo; 1]> = self
            .samplers
            .iter()
            .map(|&s| {
                [vk::DescriptorImageInfo {
                    sampler: s,
                    ..Default::default()
                }]
            })
            .collect();
        let image_infos: Vec<Vec<vk::DescriptorImageInfo>> = self
            .sig
            .bindings
            .iter()
            .map(|b| match b {
                Binding::Storage(v) | Binding::Sampled(v) => v
                    .iter()
                    .flat_map(|&i| {
                        self.imgs[i]
                            .views
                            .iter()
                            .map(|&view| vk::DescriptorImageInfo {
                                sampler: Default::default(),
                                image_view: view,
                                image_layout: vk::ImageLayout::GENERAL,
                            })
                    })
                    .collect(),
                _ => vec![],
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet> = self
            .sig
            .bindings
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let w = vk::WriteDescriptorSet::default()
                    .dst_set(self.set)
                    .dst_binding(i as u32);
                match b {
                    Binding::Uniform => w
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .buffer_info(&buf),
                    Binding::Sampler => w
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(&sampler_infos[i - 1]),
                    Binding::Storage(_) => w
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&image_infos[i]),
                    Binding::Sampled(_) => w
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&image_infos[i]),
                }
            })
            .collect();
        unsafe { d.update_descriptor_sets(&writes, &[]) };
        log(&format!(
            "  Updated descriptor set with {} bindings",
            writes.len()
        ));

        // pipelines with the cache file
        let uuid = unsafe { inst.instance.get_physical_device_properties(pd) }.pipeline_cache_uuid;
        let path = cache_path(perf, uuid);
        // the cache is only an optimisation, so i/o failures are warnings and not errors
        let data = match path.as_ref().map(std::fs::read).transpose() {
            Ok(data) => data.unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => {
                crate::log::warn(&format!("Ignoring unreadable pipeline cache file: {e}"));
                vec![]
            }
        };
        let info = vk::PipelineCacheCreateInfo::default().initial_data(&data);
        self.cache = match unsafe { d.create_pipeline_cache(&info, None) } {
            Ok(c) => c,
            // a corrupt cache file (for example truncated by a crash mid-write) is rebuilt instead of failing every context build
            Err(e) if !data.is_empty() => {
                crate::log::warn(&format!(
                    "Pipeline cache file is corrupt ({e:?}), rebuilding it"
                ));
                let empty = vk::PipelineCacheCreateInfo::default();
                check(
                    unsafe { d.create_pipeline_cache(&empty, None) },
                    "vkCreatePipelineCache",
                )?
            }
            Err(e) => return Err(format!("vkCreatePipelineCache failed: {e:?}")),
        };
        let modules = (0..SHADERS.len())
            .map(|sh| {
                let name = if sh != GEN {
                    SHADERS[sh]
                } else if hdr {
                    "generate_16bit"
                } else {
                    "generate_8bit"
                };
                inst.shaders
                    .shader(name, perf)
                    .ok_or_else(|| format!("Shader '{name}' missing from library"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let stages: Vec<vk::PipelineShaderStageCreateInfo> = modules
            .iter()
            .map(|&m| {
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::COMPUTE)
                    .module(m)
                    .name(c"main")
            })
            .collect();
        let infos: Vec<vk::ComputePipelineCreateInfo> = stages
            .iter()
            .map(|&s| {
                vk::ComputePipelineCreateInfo::default()
                    .stage(s)
                    .layout(self.layout)
            })
            .collect();
        match unsafe { d.create_compute_pipelines(self.cache, &infos, None) } {
            Ok(p) => self.pipelines = p,
            Err((p, e)) => {
                self.pipelines = p;
                return Err(format!("vkCreateComputePipelines failed: {e:?}"));
            }
        }
        if let Some(path) = path {
            let bytes = check(
                unsafe { d.get_pipeline_cache_data(self.cache) },
                "vkGetPipelineCacheData",
            )?;
            // rewrite when the driver's data changed; per-process temp name so two contexts do not interleave
            if bytes != data {
                let tmp = path.with_extension(format!("tmp{}", std::process::id()));
                if let Err(e) =
                    std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path))
                {
                    crate::log::warn(&format!("Could not save pipeline cache file: {e}"));
                }
            }
        }
        log(&format!("  Created {} pipelines", self.pipelines.len()));

        // stage tables
        let tabs: Vec<StageTab> = self
            .sig
            .stages
            .iter()
            .map(|stage| {
                let mut t = StageTab {
                    sampled: vec![],
                    stored: vec![],
                    subs: vec![],
                };
                for &p in stage {
                    let pass = &self.sig.passes[p];
                    for &i in pass.inputs.iter().flatten() {
                        if !t.sampled.contains(&i) {
                            t.sampled.push(i);
                        }
                    }
                    t.stored.push(pass.output);
                    let entry = (
                        self.sig.subiter[p],
                        pass.rule.eval(w, h, flow),
                        pass.flags & SPECIAL != 0,
                    );
                    match t.subs.last_mut() {
                        Some((sh, v)) if *sh == pass.shader => v.push(entry),
                        _ => t.subs.push((pass.shader, vec![entry])),
                    }
                }
                t
            })
            .collect();
        log(&format!("  Built {} pipeline stages", tabs.len()));

        // one-time transition to general
        // a resettable pool, so the parameter-update buffers can be re-recorded in place
        self.cmd_pool = vkutil::create_command_pool(d, inst.family, true)?;
        let cb = vkutil::allocate_command_buffer(d, self.cmd_pool)?;
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        let barriers: Vec<vk::ImageMemoryBarrier2> = self
            .imgs
            .iter()
            .flat_map(|im| {
                im.handles.iter().map(move |&h| {
                    vk::ImageMemoryBarrier2::default()
                        .new_layout(vk::ImageLayout::GENERAL)
                        .image(h)
                        .subresource_range(vkutil::color_range(0, im.layers))
                })
            })
            .collect();
        unsafe {
            inst.sync2.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            )
        };
        check(unsafe { d.end_command_buffer(cb) }, "vkEndCommandBuffer")?;
        let fence = vkutil::create_fence(d)?;
        let cbs = [cb];
        let submit = [vk::SubmitInfo::default().command_buffers(&cbs)];
        let submitted = unsafe { d.queue_submit(inst.queue, &submit, fence) };
        let waited =
            submitted.and_then(|_| unsafe { d.wait_for_fences(&[fence], true, 1_000_000_000) });
        if submitted.is_ok() && waited.is_err() {
            // the transition is still in flight after a timeout; do not destroy what it uses
            let _ = unsafe { d.queue_wait_idle(inst.queue) };
        }
        unsafe {
            d.destroy_fence(fence, None);
            d.free_command_buffers(self.cmd_pool, &cbs);
        }
        waited.map_err(|_| "Wait on the layout transition fence did not complete")?;
        log(&format!(
            "  Transitioned all {} images into general layout",
            self.sig.images.len()
        ));

        // the two execution command buffers
        for cb in self.cmd.iter_mut() {
            *cb = vkutil::allocate_command_buffer(d, self.cmd_pool)?;
            vkutil::begin(d, *cb, vk::CommandBufferUsageFlags::SIMULTANEOUS_USE)?;
            unsafe {
                d.cmd_bind_descriptor_sets(
                    *cb,
                    vk::PipelineBindPoint::COMPUTE,
                    self.layout,
                    0,
                    &[self.set],
                    &[],
                )
            };
        }
        use vk::AccessFlags2 as AF;
        let mut pending: HashMap<vk::Image, (usize, AF, AF)> = HashMap::new();
        for (st, tab) in tabs.iter().enumerate() {
            let cb = self.cmd[(st >= self.sig.split) as usize];
            for (list, access) in [
                (&tab.sampled, AF::SHADER_READ),
                (&tab.stored, AF::SHADER_WRITE),
            ] {
                for &i in list {
                    for &h in &self.imgs[i].handles {
                        pending.entry(h).and_modify(|e| e.2 = access).or_insert((
                            i,
                            AF::NONE,
                            access,
                        ));
                    }
                }
            }
            // "no layout change" is expressed as general -> general, which is valid vulkan and equally a no-op
            let barriers: Vec<vk::ImageMemoryBarrier2> = pending
                .iter()
                .map(|(&h, &(i, src, dst))| {
                    vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .src_access_mask(src)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(dst)
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .image(h)
                        .subresource_range(vkutil::color_range(0, self.imgs[i].layers))
                })
                .collect();
            unsafe {
                inst.sync2.cmd_pipeline_barrier2(
                    cb,
                    &vk::DependencyInfo::default().image_memory_barriers(&barriers),
                )
            };
            pending.clear();
            for (sh, passes) in &tab.subs {
                unsafe {
                    d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipelines[*sh])
                };
                for &(sub, (x, y), special) in passes {
                    let pc = [special as u32, sub];
                    let bytes = unsafe { std::slice::from_raw_parts(pc.as_ptr() as *const u8, 8) };
                    unsafe {
                        d.cmd_push_constants(
                            cb,
                            self.layout,
                            vk::ShaderStageFlags::COMPUTE,
                            0,
                            bytes,
                        );
                        d.cmd_dispatch(cb, x, y, 1);
                    }
                }
            }
            if st + 1 != self.sig.split && st + 1 != tabs.len() {
                for (list, src) in [
                    (&tab.sampled, AF::SHADER_READ),
                    (&tab.stored, AF::SHADER_WRITE),
                ] {
                    for &i in list {
                        for &h in &self.imgs[i].handles {
                            pending.insert(h, (i, src, AF::SHADER_READ));
                        }
                    }
                }
            }
        }
        for cb in self.cmd {
            check(unsafe { d.end_command_buffer(cb) }, "vkEndCommandBuffer")?;
        }
        log("  Execution command buffers recorded");
        log("Pipeline build complete");
        Ok(())
    }

    // host write of the iteration counter
    pub fn set_iteration(&self, iteration: u32) {
        unsafe { self.block_ptr.add(1).write_volatile(iteration) }
    }

    // parameter-update command buffer for one main-pass
    pub fn param_update(
        &mut self,
        index: u32,
        total: u32,
        timestamp: f32,
    ) -> Result<vk::CommandBuffer, String> {
        let d = &self.inst.device;
        let key = index as u64;
        // the buffer is reset and re-recorded instead of freed and reallocated every call
        let cb = match self.params.get(&key) {
            Some(&cb) => {
                check(
                    unsafe { d.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty()) },
                    "vkResetCommandBuffer",
                )?;
                cb
            }
            None => vkutil::allocate_command_buffer(d, self.cmd_pool)?,
        };
        let ts = if timestamp != 0.0 {
            timestamp
        } else {
            (index + 1) as f32 / (total + 1) as f32
        };
        vkutil::begin(d, cb, vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)?;
        let barrier = |ss, sa, ds, da| {
            [vk::BufferMemoryBarrier2::default()
                .src_stage_mask(ss)
                .src_access_mask(sa)
                .dst_stage_mask(ds)
                .dst_access_mask(da)
                .buffer(self.block)
                .offset(0)
                .size(4)]
        };
        use vk::{AccessFlags2 as AF, PipelineStageFlags2 as PS};
        unsafe {
            let b = barrier(
                PS::COMPUTE_SHADER,
                AF::UNIFORM_READ,
                PS::TRANSFER,
                AF::TRANSFER_WRITE,
            );
            self.inst.sync2.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().buffer_memory_barriers(&b),
            );
            d.cmd_update_buffer(cb, self.block, 0, &ts.to_le_bytes());
            let b = barrier(
                PS::TRANSFER,
                AF::TRANSFER_WRITE,
                PS::COMPUTE_SHADER,
                AF::UNIFORM_READ,
            );
            self.inst.sync2.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().buffer_memory_barriers(&b),
            );
            check(d.end_command_buffer(cb), "vkEndCommandBuffer")?;
        }
        self.params.insert(key, cb);
        Ok(cb)
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let d = &self.inst.device;
        unsafe {
            d.destroy_command_pool(self.cmd_pool, None);
            for &p in &self.pipelines {
                d.destroy_pipeline(p, None);
            }
            d.destroy_pipeline_cache(self.cache, None);
            for &s in &self.samplers {
                d.destroy_sampler(s, None);
            }
            if !self.block_ptr.is_null() {
                d.unmap_memory(self.block_mem);
            }
            d.destroy_buffer(self.block, None);
            d.free_memory(self.block_mem, None);
            d.destroy_descriptor_pool(self.pool, None);
            for im in &self.imgs {
                for &v in &im.views {
                    d.destroy_image_view(v, None);
                }
                for &i in &im.handles {
                    d.destroy_image(i, None);
                }
            }
            for &m in &self.memories {
                d.free_memory(m, None);
            }
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_never_overlaps_live_segments() {
        // deterministic pseudo-random sizes and lifetimes
        let mut x = 0x2545_f491u64;
        let mut items = vec![];
        for _ in 0..200 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let size = (x % 50 + 1) * 256;
            let a = (x >> 8) % 46;
            let b = a + (x >> 16) % 8;
            items.push((
                size,
                if x.is_multiple_of(7) {
                    None
                } else {
                    Some((a as usize, b.min(45) as usize))
                },
            ));
        }
        let (transient, pinned, out) = plan(&items);
        for (i, &(alloc, off)) in out.iter().enumerate() {
            let (size, life) = items[i];
            assert!(off + size <= if alloc == 0 { transient } else { pinned });
            for (j, &(alloc2, off2)) in out.iter().enumerate() {
                if i == j || alloc != alloc2 {
                    continue;
                }
                let (size2, life2) = items[j];
                let disjoint = off + size <= off2 || off2 + size2 <= off;
                let live = match (life, life2) {
                    (Some(a), Some(b)) => a.0 <= b.1 && b.0 <= a.1,
                    _ => true,
                };
                assert!(disjoint || !live, "items {i} and {j} overlap");
            }
        }
        // the real signature at 1080p must fit both allocations
        let s = Signature::new(false);
        let real: Vec<_> = s
            .images
            .iter()
            .filter(|im| !im.is(I | O))
            .map(|im| (im.sub_images() as u64 * 4096, im.lifetime))
            .collect();
        let (t, p, _) = plan(&real);
        assert!(t > 0 && p > 0 && t < real.iter().map(|r| r.0).sum::<u64>());
    }
}
