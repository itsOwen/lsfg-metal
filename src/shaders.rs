// shader package: pe resource walk, resource keys, dll discovery, shader library
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ash::vk;

use crate::vkutil::check;

pub const MIP_KEY: u32 = 0x8000_1348;
pub const GEN8_KEY: u32 = 1;
pub const GEN16_KEY: u32 = 2;

// per-shader base keys in library load order: (log name, base key)
pub const SHADERS: [(&str, u32); 24] = [
    ("alpha0", 13),
    ("alpha1", 14),
    ("alpha2", 15),
    ("alpha3", 16),
    ("beta0", 22),
    ("beta1", 23),
    ("beta2", 24),
    ("beta3", 25),
    ("beta4", 26),
    ("gamma0", 3),
    ("gamma1", 4),
    ("gamma2", 5),
    ("gamma3", 6),
    ("gamma4", 7),
    ("delta0", 8),
    ("delta1", 9),
    ("delta2", 10),
    ("delta3", 11),
    ("delta4", 12),
    ("epsilon0", 17),
    ("epsilon1", 18),
    ("epsilon2", 19),
    ("epsilon3", 20),
    ("epsilon4", 21),
];

// perf and fp16 variants offset a per-shader base key by 24 and 48
pub fn key(base: u32, perf: bool, fp16: bool) -> u32 {
    base + if perf { 24 } else { 0 } + if fp16 { 48 } else { 0 }
}

// resource key of any shader by log name (base shaders ignore perf/fp16)
pub fn key_of(name: &str, perf: bool, fp16: bool) -> Option<u32> {
    match name {
        "mipmaps" => Some(MIP_KEY),
        "generate_8bit" => Some(GEN8_KEY),
        "generate_16bit" => Some(GEN16_KEY),
        _ => SHADERS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, b)| key(b, perf, fp16)),
    }
}

// alignment is checked against the size of the field being read
fn rd<const N: usize>(b: &[u8], off: u64) -> Result<[u8; N], String> {
    let end = off.checked_add(N as u64).filter(|&e| e <= b.len() as u64);
    let end = end.ok_or("Read outside the buffer")?;
    if !off.is_multiple_of(N as u64) {
        return Err("Unaligned read offset".into());
    }
    Ok(b[off as usize..end as usize].try_into().unwrap())
}
fn u16(b: &[u8], off: u64) -> Result<u16, String> {
    rd(b, off).map(u16::from_le_bytes)
}
fn u32(b: &[u8], off: u64) -> Result<u32, String> {
    rd(b, off).map(u32::from_le_bytes)
}

// walk the pe32+ resource directory: rcdata id or name-key -> spir-v words
pub fn parse(b: &[u8]) -> Result<HashMap<u32, Vec<u32>>, String> {
    if u16(b, 0)? != 0x5A4D {
        return Err("Bad DOS header magic".into());
    }
    let pe = i32::from_le_bytes(rd(b, 60)?);
    let pe = u64::try_from(pe).map_err(|_| "Read outside the buffer")?;
    if u32(b, pe)? != 0x0000_4550 {
        return Err("Bad PE header signature".into());
    }
    let nsect = u16(b, pe + 6)? as u64;
    let optsize = u16(b, pe + 20)? as u64;
    let opt = pe + 24;
    if u16(b, opt)? != 0x20B {
        return Err("Optional header is not PE32+".into());
    }
    let rsrc_rva = u32(b, opt + 128)?;
    let rsrc_size = u32(b, opt + 132)?;
    let mut rsrc_off = None;
    for i in 0..nsect {
        let s = opt + optsize + i * 40;
        let (vsize, vaddr) = (u32(b, s + 8)?, u32(b, s + 12)?);
        let rawoff = u32(b, s + 20)?;
        if vaddr <= rsrc_rva && rsrc_rva as u64 <= vaddr as u64 + vsize as u64 {
            rsrc_off = Some((rsrc_rva - vaddr) as u64 + rawoff as u64);
            break;
        }
    }
    let rsrc_off = rsrc_off.ok_or("No section contains the resource directory")?;
    let sub = |off: u32| rsrc_off + (off & 0x7FFF_FFFF) as u64;
    let named = u16(b, rsrc_off + 12)? as u64;
    let ids = u16(b, rsrc_off + 14)? as u64;
    if ids < 3 {
        return Err("Too few entries in the root resource directory".into());
    }
    let mut rcdata = None;
    for i in 0..named + ids {
        let e = rsrc_off + 16 + i * 8;
        if u32(b, e)? == 10 {
            rcdata = Some(u32(b, e + 4)?);
        }
    }
    let rcdata = rcdata.ok_or("No RT_RCDATA entry in the root directory")?;
    if rcdata & 0x8000_0000 == 0 {
        return Err("Wanted a subdirectory here, got a data entry".into());
    }
    let tdir = sub(rcdata);
    let named = u16(b, tdir + 12)? as u64;
    let ids = u16(b, tdir + 14)? as u64;
    if ids < 1 {
        return Err("Too few entries in the RT_RCDATA directory".into());
    }
    let mut out = HashMap::new();
    for i in 0..named + ids {
        let e = tdir + 16 + i * 8;
        let (id, off) = (u32(b, e)?, u32(b, e + 4)?);
        if off & 0x8000_0000 == 0 {
            return Err("Wanted a subdirectory here, got a data entry".into());
        }
        let ldir = sub(off);
        if u16(b, ldir + 14)? < 1 {
            return Err("Language directory has no entries".into());
        }
        let doff = u32(b, ldir + 16 + 4)?;
        if doff & 0x8000_0000 != 0 {
            return Err("Wanted a data entry here, got a subdirectory".into());
        }
        let de = sub(doff);
        let (data_rva, size) = (u32(b, de)?, u32(b, de + 4)?);
        if data_rva < rsrc_rva || data_rva as u64 > rsrc_rva as u64 + rsrc_size as u64 {
            return Err("Data entry lies outside the resource section".into());
        }
        let start = (data_rva - rsrc_rva) as u64 + rsrc_off;
        let words = (0..size as u64 / 4)
            .map(|w| u32(b, start + w * 4))
            .collect::<Result<Vec<_>, _>>()?;
        out.insert(id, words);
    }
    Ok(out)
}

// automatic discovery: steam on the host, then steam inside the wine prefix, then the working directory
pub fn find_dll() -> Option<PathBuf> {
    let common = "steamapps/common/Lossless Scaling/lsfg-vk.dll";
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
    let hosted = var("HOME").map(|h| Path::new(&h).join("Library/Application Support/Steam").join(common));
    let bottled = var("WINEPREFIX").map(|w| Path::new(&w).join("drive_c/Program Files (x86)/Steam").join(common));
    hosted
        .into_iter()
        .chain(bottled)
        .chain(std::iter::once(PathBuf::from("lsfg-vk.dll")))
        .find(|c| c.is_file())
}


// path correction: a "Lossless Scaling" directory gains the dll name, a wrong dll name is replaced; file_name ignores a trailing slash
pub fn fix_dll_path(p: &Path) -> PathBuf {
    match p.file_name().and_then(|n| n.to_str()) {
        Some("Lossless Scaling") => p.join("lsfg-vk.dll"),
        Some("Lossless.dll") | Some("LosslessScaling.dll") => p.with_file_name("lsfg-vk.dll"),
        _ => p.to_path_buf(),
    }
}

// one precision's 51 modules: 3 base (stored under both perf keys) + 24 quality + 24 performance
pub struct Library {
    modules: HashMap<(&'static str, bool), vk::ShaderModule>,
    pub fp16: bool,
}

impl Library {
    pub fn load(
        device: &ash::Device,
        fp16: bool,
        path: &Path,
        log: fn(&str),
    ) -> Result<Library, String> {
        log(&format!("Loading shader library from {}", path.display()));
        if !path.exists() {
            return Err("No shader DLL at the given path".into());
        }
        let res = parse(&std::fs::read(path).map_err(|e| e.to_string())?)?;
        let mut lib = Library {
            modules: HashMap::new(),
            fp16,
        };
        let mut make = |name: &'static str, perf: bool, words: &[u32]| -> Result<(), String> {
            if words.len() < 5 || words[0] != 0x0723_0203 {
                return Err(format!("Shader '{name}' in the DLL is not SPIR-V"));
            }
            // moltenvk's parser crashes on a malformed module, so it is parsed here first
            spirv_cross2::Compiler::<spirv_cross2::targets::Msl>::new(spirv_cross2::Module::from_words(words))
                .map_err(|e| format!("Shader '{name}' in the DLL is not valid SPIR-V: {e}"))?;
            let info = vk::ShaderModuleCreateInfo::default().code(words);
            let m = check(
                unsafe { device.create_shader_module(&info, None) },
                "vkCreateShaderModule",
            )?;
            lib.modules.insert((name, perf), m);
            Ok(())
        };
        // the library has no drop, so modules made before a failure are destroyed here
        let loaded = (|| -> Result<(), String> {
            let base = [
                ("mipmaps", MIP_KEY),
                ("generate_8bit", GEN8_KEY),
                ("generate_16bit", GEN16_KEY),
            ];
            for (i, (name, k)) in base.iter().enumerate() {
                let w = res
                    .get(k)
                    .ok_or_else(|| format!("Base shader '{name}' missing from DLL"))?;
                log(&format!(
                    "  {i:2}: name={name}, rid={k:#x}, size={} bytes",
                    w.len()
                ));
                make(name, false, w)?;
                make(name, true, w)?;
            }
            for (name, b) in SHADERS {
                let (q, p) = (key(b, false, fp16), key(b, true, fp16));
                let missing = || format!("Shader '{name}' missing from DLL");
                let (wq, wp) = (
                    res.get(&q).ok_or_else(missing)?,
                    res.get(&p).ok_or_else(missing)?,
                );
                log(&format!(
                    "  {b:2}: name={name:>8}, [Q] rid={q:2}, size={:5} bytes, [P] rid={p:2}, size={:5} bytes",
                    wq.len(),
                    wp.len()
                ));
                make(name, false, wq)?;
                make(name, true, wp)?;
            }
            Ok(())
        })();
        if let Err(e) = loaded {
            lib.destroy(device);
            return Err(e);
        }
        log("Shader library ready");
        Ok(lib)
    }

    pub fn shader(&self, name: &str, perf: bool) -> Option<vk::ShaderModule> {
        self.modules
            .iter()
            .find(|((n, p), _)| *n == name && *p == perf)
            .map(|(_, &m)| m)
    }

    pub fn destroy(&self, device: &ash::Device) {
        for m in self.modules.values() {
            unsafe { device.destroy_shader_module(*m, None) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // LSFGM_TEST_DLL points at an lsfg-vk.dll; unset or missing skips the test
    fn dll() -> Option<String> {
        std::env::var("LSFGM_TEST_DLL").ok()
    }

    #[test]
    fn parses_real_dll() {
        let Ok(bytes) = std::fs::read(dll().unwrap_or_default()) else {
            return;
        };
        let res = parse(&bytes).unwrap();
        assert!(res.contains_key(&MIP_KEY));
        for k in 1..=98 {
            assert!(res.contains_key(&k), "key {k}");
            assert_eq!(res[&k][0], 0x0723_0203, "magic of {k}");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(
            parse(b"nope").unwrap_err(),
            "Bad DOS header magic"
        );
        assert_eq!(
            parse(b"MZ").unwrap_err(),
            "Read outside the buffer"
        );
    }

    #[test]
    fn fixes_paths() {
        assert_eq!(
            fix_dll_path(Path::new("/a/Lossless Scaling/")),
            Path::new("/a/Lossless Scaling/lsfg-vk.dll")
        );
        assert_eq!(
            fix_dll_path(Path::new("/a/Lossless.dll")),
            Path::new("/a/lsfg-vk.dll")
        );
        assert_eq!(fix_dll_path(Path::new("/a/x.dll")), Path::new("/a/x.dll"));
    }
}
