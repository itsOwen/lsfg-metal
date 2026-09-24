// v3.1 pipeline signature: images, passes, bindings, stages, sub-iterations
// the tables are built by the procedure below; tests pin the counts and sample rows
use std::collections::HashSet;

use ash::vk;

// shader list in first-use order, by log name
pub const SHADERS: [&str; 26] = [
    "mipmaps", "alpha0", "alpha1", "alpha2", "alpha3", "beta0", "beta1", "beta2", "beta3", "beta4",
    "gamma0", "gamma1", "gamma2", "gamma3", "gamma4", "delta0", "delta1", "delta2", "delta3",
    "delta4", "epsilon0", "epsilon1", "epsilon2", "epsilon3", "epsilon4", "generate",
];
pub const MIP: usize = 0;
pub const A0: usize = 1;
pub const B0: usize = 5;
pub const C0: usize = 10;
pub const D0: usize = 15;
pub const E0: usize = 20;
pub const GEN: usize = 25;
pub const SPLIT_PASS: usize = 34;

// image flags
pub const M: u8 = 1;
pub const P: u8 = 2;
pub const I: u8 = 4;
pub const O: u8 = 8;
pub const H: u8 = 16;
pub const A: u8 = 32;
// pass flags
pub const AGG: u8 = 1;
pub const SPECIAL: u8 = 2;

const RGBA8: vk::Format = vk::Format::R8G8B8A8_UNORM;
const R8: vk::Format = vk::Format::R8_UNORM;
const RGBA16F: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

// storage of the source and generated images (flag H); all but half float are 32 bits a pixel and cost what rgba8 does
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Store {
    Rgba8,
    Rgb10a2,
    // shared-exponent float: 9-bit precision and no clipping above 1, for gamma-encoded float sources
    Rgb9e5,
    Bgr10Xr,
    Rgba16f,
}

// a source's colour in the generator: its storage and the dxgi colour kind (0 encoded, 1 linear sdr, 2 linear hdr)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Colour {
    pub store: Store,
    pub kind: u32,
}

impl Colour {
    pub const SDR: Colour = Colour { store: Store::Rgba8, kind: 0 };
    pub const HDR: Colour = Colour { store: Store::Rgba16f, kind: 2 };

    // the vulkan generator has only rgba8 and half float, so anything deeper than 8 bits runs in half float there
    pub fn float(self) -> bool {
        self.store != Store::Rgba8
    }

    // colour kind and hdr support, as the uniform block carries them
    pub fn block(self) -> [u32; 2] {
        [self.kind, (self.kind == 2) as u32]
    }
}

// extent rule: base B or F, then (add, shift) steps
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub flow: bool,
    pub ops: Vec<(u32, u32)>,
}

impl Rule {
    pub fn then(&self, add: u32, shift: u32) -> Rule {
        let mut r = self.clone();
        r.ops.push((add, shift));
        r
    }
    pub fn eval(&self, w: u32, h: u32, flow: f32) -> (u32, u32) {
        let (mut x, mut y) = if self.flow {
            ((w as f32 * flow) as u32, (h as f32 * flow) as u32)
        } else {
            (w, h)
        };
        for &(add, shift) in &self.ops {
            x = x.wrapping_add(add) >> shift;
            y = y.wrapping_add(add) >> shift;
        }
        (x, y)
    }
}

pub struct Image {
    pub sdr: vk::Format,
    pub hdr: vk::Format,
    pub flags: u8,
    pub count: u32,
    pub rule: Rule,
    // [first writing stage, last reading stage]; none for pinned/external
    pub lifetime: Option<(usize, usize)>,
}

impl Image {
    pub fn is(&self, f: u8) -> bool {
        self.flags & f != 0
    }
    pub fn format(&self, hdr: bool) -> vk::Format {
        if hdr && self.is(H) {
            self.hdr
        } else {
            self.sdr
        }
    }
    pub fn sub_images(&self) -> u32 {
        if self.is(M) {
            self.count
        } else {
            1
        }
    }
    pub fn layers(&self) -> u32 {
        if self.is(M) {
            1
        } else {
            self.count
        }
    }
    // 2d-array view when layered or flagged A
    pub fn array_view(&self) -> bool {
        self.layers() > 1 || self.is(A)
    }
    pub fn extent(&self, sub: u32, w: u32, h: u32, flow: f32) -> (u32, u32) {
        let (x, y) = self.rule.eval(w, h, flow);
        ((x >> sub).max(1), (y >> sub).max(1))
    }
}

// inputs keep only the image index; the sub-image of an M input is irrelevant (barriers cover every sub-image)
pub struct Pass {
    pub shader: usize,
    pub flags: u8,
    pub inputs: Vec<Option<usize>>,
    pub output: usize,
    pub rule: Rule,
}

#[derive(Debug, PartialEq)]
pub enum Binding {
    Uniform,
    Sampler,
    Storage(Vec<usize>),
    Sampled(Vec<usize>),
}

// one dispatch: (sub-iteration, groups, special)
pub type Dispatch = (u32, (u32, u32), bool);

pub struct StageTab {
    // (shader, dispatches) runs
    pub subs: Vec<(usize, Vec<Dispatch>)>,
}

pub struct Signature {
    pub perf: bool,
    pub images: Vec<Image>,
    pub passes: Vec<Pass>,
    pub bindings: Vec<Binding>,
    // passes per stage in recorded order
    pub stages: Vec<Vec<usize>>,
    // first stage of command buffer 1
    pub split: usize,
    pub subiter: Vec<u32>,
}

struct Builder {
    images: Vec<Image>,
    passes: Vec<Pass>,
}

impl Builder {
    fn img(
        &mut self,
        sdr: vk::Format,
        hdr: vk::Format,
        flags: u8,
        count: u32,
        rule: &Rule,
    ) -> usize {
        self.images.push(Image {
            sdr,
            hdr,
            flags,
            count,
            rule: rule.clone(),
            lifetime: None,
        });
        self.images.len() - 1
    }
    fn pass(
        &mut self,
        shader: usize,
        flags: u8,
        inputs: Vec<Option<usize>>,
        output: usize,
        rule: &Rule,
    ) {
        self.passes.push(Pass {
            shader,
            flags,
            inputs,
            output,
            rule: rule.clone(),
        });
    }
    // five-pass chain X0..X4: X0 takes `first`, X4 takes x3 plus `last`
    #[allow(clippy::too_many_arguments)]
    fn chain(
        &mut self,
        base: usize,
        special: bool,
        first: Vec<Option<usize>>,
        last: Vec<Option<usize>>,
        ff: u8,
        c0: u32,
        cff: u32,
        e: &Rule,
    ) -> usize {
        let d = e.then(7, 3);
        let sp = if special { SPECIAL } else { 0 };
        let mut prev = self.img(RGBA8, RGBA8, ff, c0, e);
        self.pass(base, AGG | sp, first, prev, &d);
        for k in 1..4 {
            let x = self.img(RGBA8, RGBA8, ff, cff, e);
            self.pass(base + k, AGG, vec![Some(prev)], x, &d);
            prev = x;
        }
        let res = self.img(RGBA16F, RGBA16F, 0, 1, e);
        let mut ins = vec![Some(prev)];
        ins.extend(last);
        self.pass(base + 4, AGG | sp, ins, res, &d);
        res
    }
}

impl Signature {
    pub fn new(perf: bool) -> Signature {
        let mul = if perf { 1 } else { 2 };
        let mut s = Builder {
            images: vec![],
            passes: vec![],
        };
        let b = Rule {
            flow: false,
            ops: vec![],
        };
        let f = Rule {
            flow: true,
            ops: vec![],
        };
        let src = s.img(RGBA8, RGBA16F, P | I | H, 2, &b);
        let mip = s.img(R8, R8, M, 7, &f);
        s.pass(MIP, 0, vec![Some(src)], mip, &f.then(63, 6));
        let mut pyramid = [0usize; 7];
        let mut pext: Vec<Rule> = vec![b.clone(); 7];
        for i in 0..7u32 {
            let level = (6 - i) as usize;
            let e = f.then(0, 6 - i).then(1, 1);
            let d = e.then(7, 3);
            let ff0 = s.img(RGBA8, RGBA8, A, mul, &e);
            s.pass(A0, AGG, vec![Some(mip)], ff0, &d);
            let ff1 = s.img(RGBA8, RGBA8, A, mul, &e);
            s.pass(A0 + 1, AGG, vec![Some(ff0)], ff1, &d);
            let e = e.then(1, 1);
            let d = e.then(7, 3);
            let ff2 = s.img(RGBA8, RGBA8, 0, 2 * mul, &e);
            s.pass(A0 + 2, AGG, vec![Some(ff1)], ff2, &d);
            let res = s.img(RGBA8, RGBA8, P, 6 * mul, &e);
            s.pass(A0 + 3, AGG, vec![Some(ff2)], res, &d);
            pyramid[level] = res;
            pext[level] = e;
        }
        let e = pext[0].clone();
        let d = e.then(7, 3);
        let mut prev = pyramid[0];
        for k in 0..4 {
            let bk = s.img(RGBA8, RGBA8, 0, 2, &e);
            s.pass(B0 + k, 0, vec![Some(prev)], bk, &d);
            prev = bk;
        }
        let bm = s.img(R8, R8, M, 6, &e);
        s.pass(B0 + 4, 0, vec![Some(prev)], bm, &e.then(31, 5));
        // main pass
        let (mut cres, mut dres, mut eres) = (None, None, None);
        for i in 0..7 {
            let level = 6 - i;
            let e = pext[level].clone();
            let (cprev, dprev, eprev) = (cres, dres, eres);
            let pyr = Some(pyramid[level]);
            cres = Some(s.chain(
                C0,
                i == 0,
                vec![pyr, cprev],
                vec![cprev, Some(bm)],
                0,
                3,
                2 * mul,
                &e,
            ));
            if i >= 4 {
                dres = Some(s.chain(
                    D0,
                    i == 4,
                    vec![pyr, dprev],
                    vec![dprev, Some(bm)],
                    0,
                    3,
                    2 * mul,
                    &e,
                ));
                eres = Some(s.chain(
                    E0,
                    i == 4,
                    vec![pyr, cprev, dprev],
                    vec![eprev],
                    A,
                    mul,
                    mul,
                    &e,
                ));
            }
        }
        let dst = s.img(RGBA8, RGBA16F, P | O | H, 1, &b);
        s.pass(
            GEN,
            0,
            vec![Some(src), cres, dres, eres],
            dst,
            &b.then(15, 4),
        );
        let Builder { mut images, passes } = s;

        // stage schedule
        let mut written: HashSet<usize> = images
            .iter()
            .enumerate()
            .filter(|(_, im)| im.is(I))
            .map(|(i, _)| i)
            .collect();
        let mut remaining = vec![true; passes.len()];
        let mut bound = (0, SPLIT_PASS);
        let mut stages: Vec<Vec<usize>> = vec![];
        let mut split = 0;
        while remaining.iter().any(|&r| r) {
            let mut ready: Vec<usize> = (bound.0..bound.1)
                .filter(|&p| {
                    remaining[p]
                        && passes[p]
                            .inputs
                            .iter()
                            .flatten()
                            .all(|i| written.contains(i))
                })
                .collect();
            if ready.is_empty() {
                assert!(bound.1 < passes.len(), "stage schedule stuck");
                bound = (bound.1, passes.len());
                split = stages.len();
                continue;
            }
            ready.sort_by_key(|&p| SHADERS[passes[p].shader]);
            for &p in &ready {
                written.insert(passes[p].output);
                remaining[p] = false;
            }
            stages.push(ready);
        }

        // sub-iterations and lifetimes
        let mut subiter = vec![0u32; passes.len()];
        let mut counters = [0u32; 26];
        for (st, stage) in stages.iter().enumerate() {
            for &p in stage {
                subiter[p] = counters[passes[p].shader];
                counters[passes[p].shader] += 1;
                let out = &mut images[passes[p].output];
                if !out.is(P) {
                    out.lifetime = Some((st, st));
                }
                for &i in passes[p].inputs.iter().flatten() {
                    if let Some(l) = images[i].lifetime.as_mut() {
                        l.1 = l.1.max(st);
                    }
                }
            }
        }

        // descriptor bindings
        let mut bindings = vec![
            Binding::Uniform,
            Binding::Sampler,
            Binding::Sampler,
            Binding::Sampler,
        ];
        bindings.extend(
            images
                .iter()
                .enumerate()
                .filter(|(_, im)| im.is(I))
                .map(|(i, _)| Binding::Sampled(vec![i])),
        );
        for sh in 0..SHADERS.len() {
            let outs: Vec<usize> = passes
                .iter()
                .filter(|p| p.shader == sh)
                .map(|p| p.output)
                .collect();
            let external_out = images[outs[0]].is(O);
            bindings.push(Binding::Storage(outs.clone()));
            if !external_out {
                bindings.push(Binding::Sampled(outs));
            }
        }
        Signature {
            perf,
            images,
            passes,
            bindings,
            stages,
            split,
            subiter,
        }
    }

    // the dispatches of every stage at this size, consecutive passes of one shader grouped
    pub fn tabs(&self, w: u32, h: u32, flow: f32) -> Vec<StageTab> {
        self.stages
            .iter()
            .map(|stage| {
                let mut subs: Vec<(usize, Vec<Dispatch>)> = vec![];
                for &p in stage {
                    let pass = &self.passes[p];
                    let entry = (
                        self.subiter[p],
                        pass.rule.eval(w, h, flow),
                        pass.flags & SPECIAL != 0,
                    );
                    match subs.last_mut() {
                        Some((sh, v)) if *sh == pass.shader => v.push(entry),
                        _ => subs.push((pass.shader, vec![entry])),
                    }
                }
                StageTab { subs }
            })
            .collect()
    }

    // false when a pass would get an empty grid, so later passes would read texels nothing wrote
    pub fn fits(&self, w: u32, h: u32, flow: f32) -> bool {
        self.passes.iter().all(|p| {
            let (x, y) = p.rule.eval(w, h, flow);
            x > 0 && y > 0
        })
    }

    // descriptor count of a binding: images expand to their sub-images
    pub fn count(&self, b: &Binding) -> u32 {
        match b {
            Binding::Uniform | Binding::Sampler => 1,
            Binding::Storage(v) | Binding::Sampled(v) => {
                v.iter().map(|&i| self.images[i].sub_images()).sum()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_sample_rows() {
        for perf in [false, true] {
            let s = Signature::new(perf);
            assert_eq!(s.images.len(), 101);
            assert_eq!(s.passes.len(), 100);
            assert_eq!(s.bindings.len(), 56);
            assert_eq!(s.stages.len(), 46);
            assert_eq!(s.split, 10);
            let sum = |f: fn(&Binding) -> bool| {
                s.bindings
                    .iter()
                    .filter(|b| f(b))
                    .map(|b| s.count(b))
                    .sum::<u32>()
            };
            assert_eq!(sum(|b| matches!(b, Binding::Sampled(_))), 111);
            assert_eq!(sum(|b| matches!(b, Binding::Storage(_))), 111);
            assert_eq!(s.stages[10], vec![59, 34]);
            assert_eq!(s.stages[30], vec![64, 54]);
            assert_eq!(s.stages[45], vec![99]);
            assert_eq!(s.images[34].lifetime, Some((9, 44)));
            assert_eq!(s.images[64].lifetime, Some((14, 35)));
            assert_eq!(s.images[5].count, if perf { 6 } else { 12 });
            assert_eq!(
                s.bindings[25],
                Binding::Storage(vec![35, 40, 45, 50, 55, 70, 85])
            );
            assert_eq!(s.bindings[45], Binding::Storage(vec![65, 80, 95]));
            assert_eq!(s.bindings[55], Binding::Storage(vec![100]));
            assert_eq!(s.passes[64].inputs, vec![Some(21), Some(54), None]);
            assert_eq!(s.passes[38].inputs, vec![Some(38), None, Some(34)]);
            assert_eq!(&s.subiter[..13], &[0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2]);
            assert_eq!(s.subiter[84], 6);
            assert_eq!(s.subiter[89], 2);
            assert_eq!(s.images[2].extent(0, 1920, 1080, 1.0), (15, 8));
            assert_eq!(s.images[9].extent(0, 1920, 1080, 1.0), (15, 9));
            assert_eq!(s.passes[0].rule.eval(1920, 1080, 1.0), (30, 17));
            assert_eq!(s.passes[33].rule.eval(1920, 1080, 1.0), (15, 9));
            // aggregate consistency rules
            let mut writers = vec![0; 101];
            for p in &s.passes {
                writers[p.output] += 1;
                assert!(!p.inputs.contains(&Some(p.output)));
                let same: Vec<&Pass> = s.passes.iter().filter(|q| q.shader == p.shader).collect();
                assert!(same.iter().all(|q| q.inputs.len() == p.inputs.len()));
                assert!(p.flags & AGG != 0 || same.len() == 1);
            }
            for (i, im) in s.images.iter().enumerate() {
                assert_eq!(writers[i], if im.is(I) { 0 } else { 1 }, "image {i}");
                let read = s.passes.iter().any(|p| p.inputs.contains(&Some(i)));
                assert!(im.is(P) || read, "image {i} never read");
            }
        }
    }

    // the coarsest pass works on 1/128 of the flowed size, rounded, so 64 flowed pixels is the floor
    #[test]
    fn fits_from_64_flowed_pixels() {
        for perf in [false, true] {
            let s = Signature::new(perf);
            assert!(s.fits(64, 64, 1.0) && s.fits(1920, 1080, 0.25));
            assert!(!s.fits(63, 1080, 1.0) && !s.fits(1920, 63, 1.0));
            assert!(s.fits(256, 256, 0.25) && !s.fits(255, 1080, 0.25));
            assert!(!s.fits(0, 0, 1.0));
        }
    }
}
