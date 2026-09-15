// shader image-view check: spir-v image arrayed flags vs the view types the pipeline binds
use std::collections::HashMap;
use std::process::exit;

use lsfg_metal::generator::signature::{Binding, Signature, GEN, SHADERS};
use lsfg_metal::shaders;

// (binding, arrayed) for every image variable with binding >= 4
fn image_bindings(words: &[u32]) -> Vec<(u32, bool)> {
    let (mut bindings, mut images, mut pointers, mut arrays) = (
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let mut vars = vec![];
    let mut i = 5;
    while i < words.len() {
        let (op, n) = (words[i] & 0xffff, (words[i] >> 16) as usize);
        if n == 0 || i + n > words.len() {
            break;
        }
        let w = &words[i..i + n];
        match op {
            71 if n > 3 && w[2] == 33 => drop(bindings.insert(w[1], w[3])),
            25 => drop(images.insert(w[1], w[5] == 1)),
            32 => drop(pointers.insert(w[1], w[3])),
            28 => drop(arrays.insert(w[1], w[2])),
            59 => vars.push((w[2], w[1])),
            _ => {}
        }
        i += n;
    }
    let mut out = vec![];
    for (id, ty) in vars {
        let Some(&b) = bindings.get(&id).filter(|&&b| b >= 4) else {
            continue;
        };
        let Some(&t) = pointers.get(&ty) else {
            continue;
        };
        let t = arrays.get(&t).copied().unwrap_or(t);
        if let Some(&arrayed) = images.get(&t) {
            out.push((b, arrayed));
        }
    }
    out
}

fn main() {
    let Some(dll) = std::env::args().nth(1) else {
        eprintln!("usage: shader-check <lsfg-vk.dll>");
        exit(1);
    };
    let bytes = std::fs::read(&dll).unwrap_or_else(|e| {
        eprintln!("{dll}: {e}");
        exit(1)
    });
    let res = shaders::parse(&bytes).unwrap_or_else(|e| {
        eprintln!("{e}");
        exit(1)
    });
    let mut checked = 0;
    for perf in [false, true] {
        let sig = Signature::new(perf);
        for fp16 in [false, true] {
            for hdr in [false, true] {
                for (sh, &name) in SHADERS.iter().enumerate() {
                    let name = if sh != GEN {
                        name
                    } else if hdr {
                        "generate_16bit"
                    } else {
                        "generate_8bit"
                    };
                    let key = shaders::key_of(name, perf, fp16).unwrap();
                    let Some(words) = res.get(&key) else {
                        eprintln!("{name}: resource {key} missing");
                        exit(1);
                    };
                    if words.first() != Some(&0x0723_0203) {
                        eprintln!("{name}: resource {key} is not SPIR-V");
                        exit(1);
                    }
                    for (b, arrayed) in image_bindings(words) {
                        let images = match sig.bindings.get(b as usize) {
                            Some(Binding::Storage(v) | Binding::Sampled(v)) => v,
                            _ => {
                                eprintln!(
                                    "{name} binding {b} is not an image binding (resource {key})"
                                );
                                exit(1);
                            }
                        };
                        if images
                            .iter()
                            .any(|&i| sig.images[i].array_view() != arrayed)
                        {
                            eprintln!(
                                "{name} binding {b} has the wrong image view type (resource {key})"
                            );
                            exit(1);
                        }
                        // one check per (binding variable, image element)
                        checked += images.len();
                    }
                }
            }
        }
    }
    println!("Shader image-view checks passed: {checked} bindings across quality, performance, fp16 and hdr");
}
