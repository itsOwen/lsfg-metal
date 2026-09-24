// doctor: the install checks that otherwise only show up as a passive shim and a log line
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

use ash::vk;
use lsfg_metal::vkutil;
use lsfg_metal::{settings, shaders};

// one report line per check; a fail sets the exit code, a warn does not
struct Report {
    failed: bool,
}

impl Report {
    fn ok(&mut self, what: &str, detail: &str) {
        println!("ok    {what}: {detail}");
    }
    fn warn(&mut self, what: &str, detail: &str) {
        println!("warn  {what}: {detail}");
    }
    fn fail(&mut self, what: &str, detail: &str) {
        println!("FAIL  {what}: {detail}");
        self.failed = true;
    }
    // Ok prints the value, Err fails the check
    fn res(&mut self, what: &str, r: Result<String, String>) {
        match r {
            Ok(d) => self.ok(what, &d),
            Err(e) => self.fail(what, &e),
        }
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

// dlsym through a handle, returning whether the symbol is there at all
fn has_symbol(lib: &libloading::Library, name: &[u8]) -> bool {
    unsafe { lib.get::<*const ()>(name) }.is_ok()
}

// the shim exports the LSFGM_SHIM marker; a shim from before the marker is the one without vkCreateDevice
fn is_shim(path: &Path) -> Result<bool, String> {
    let lib = unsafe { libloading::Library::new(path) }
        .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
    if !has_symbol(&lib, b"vkGetInstanceProcAddr\0") {
        return Err(format!("{} has no vkGetInstanceProcAddr", path.display()));
    }
    let shim = has_symbol(&lib, b"LSFGM_SHIM\0") || !has_symbol(&lib, b"vkCreateDevice\0");
    // the initializer leaves a panic hook, and maybe a swizzle, pointing into this image
    std::mem::forget(lib);
    Ok(shim)
}

// the shim installs as libMoltenVK.dylib with the driver beside it as libMoltenVK.real.dylib
fn check_install(r: &mut Report, shim: &Path) -> Option<PathBuf> {
    match is_shim(shim) {
        Err(e) => {
            r.fail("shim", &e);
            return None;
        }
        Ok(false) => {
            r.fail(
                "shim",
                &format!(
                    "{} is the real MoltenVK, not the shim (a CrossOver or runtime update restores it; reinstall)",
                    shim.display()
                ),
            );
            return None;
        }
        Ok(true) => {
            let version = std::fs::read_to_string(shim.with_file_name("source.txt"))
                .ok()
                .and_then(|t| t.lines().next().map(str::to_string))
                .unwrap_or_else(|| "version unknown, no source.txt beside it".into());
            r.ok("shim", &format!("{} ({version})", shim.display()));
        }
    }
    if shim.file_name().and_then(|n| n.to_str()) != Some("libMoltenVK.dylib") {
        r.warn(
            "shim name",
            "the shim only loads when it is named libMoltenVK.dylib",
        );
    }
    let real = env("LSFGM_MOLTENVK")
        .map(PathBuf::from)
        .unwrap_or_else(|| shim.with_file_name("libMoltenVK.real.dylib"));
    // a symlink into a runtime that moved leaves the name in place and nothing behind it
    if !real.exists() {
        let dangling = real.symlink_metadata().is_ok();
        r.fail(
            "real driver",
            &format!(
                "{} {}",
                real.display(),
                if dangling {
                    "is a dangling symlink"
                } else {
                    "does not exist"
                }
            ),
        );
        return None;
    }
    match is_shim(&real) {
        Err(e) => {
            r.fail("real driver", &e);
            return None;
        }
        Ok(true) => {
            r.fail(
                "real driver",
                &format!(
                    "{} is the shim again, not MoltenVK; it would recurse",
                    real.display()
                ),
            );
            return None;
        }
        Ok(false) => r.ok(
            "real driver",
            &real
                .canonicalize()
                .unwrap_or(real.clone())
                .display()
                .to_string(),
        ),
    }
    Some(real)
}

// a leaf-name lookup finds the first libMoltenVK.dylib on this path, the shim's own included
fn check_dyld(r: &mut Report) {
    // a leaf name here is resolved through that same search, so it can find the shim itself
    if let Some(v) = env("LSFGM_MOLTENVK").filter(|v| !v.contains('/')) {
        r.warn(
            "LSFGM_MOLTENVK",
            &format!(
                "'{v}' has no directory, so it is resolved by leaf name and can find the shim"
            ),
        );
    }
    let Some(paths) = env("DYLD_LIBRARY_PATH") else {
        return r.ok("DYLD_LIBRARY_PATH", "unset");
    };
    let Some(first) = paths
        .split(':')
        .filter(|d| !d.is_empty())
        .find(|d| Path::new(d).join("libMoltenVK.dylib").exists())
    else {
        return r.ok("DYLD_LIBRARY_PATH", &paths);
    };
    let found = Path::new(first).join("libMoltenVK.dylib");
    match is_shim(&found) {
        // this is how a launcher installs the shim without touching the runtime, so it is expected
        Ok(true) => r.ok(
            "DYLD_LIBRARY_PATH",
            &format!("{first} (a leaf-name lookup of libMoltenVK.dylib finds the shim there)"),
        ),
        Ok(false) => r.warn(
            "DYLD_LIBRARY_PATH",
            &format!("{first} holds a real MoltenVK, which wins the leaf-name lookup; the shim will not load"),
        ),
        Err(e) => r.warn("DYLD_LIBRARY_PATH", &e),
    }
}

// version and the extension the Metal paths need, from the driver itself
fn check_driver(r: &mut Report, real: &Path) {
    // set when the driver loads but is too old or lacks metal_objects, which limits generation on moltenvk only
    let limited = std::cell::Cell::new(false);
    let run = || -> Result<String, String> {
        let (_lib, entry) = vkutil::load_driver(real)?;
        let instance =
            vkutil::create_instance(&entry, c"lsfg-metal doctor", vk::API_VERSION_1_2, true)?;
        let out = (|| {
            let pd = vkutil::select_physical_device(&instance, "")?;
            let gipa = entry.static_fn().get_instance_proc_addr;
            vkutil::check_driver(gipa, instance.handle(), pd).inspect_err(|_| limited.set(true))?;
            let exts = vkutil::check(
                unsafe { instance.enumerate_device_extension_properties(pd) },
                "vkEnumerateDeviceExtensionProperties",
            )?;
            let metal = exts
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(ash::ext::metal_objects::NAME));
            if !metal {
                limited.set(true);
                return Err("the driver has no VK_EXT_metal_objects, so the proxy path and generation on MoltenVK cannot import textures".into());
            }
            let mut props = vk::PhysicalDeviceProperties2::default();
            unsafe { instance.get_physical_device_properties2(pd, &mut props) };
            let v = props.properties.driver_version;
            let mut name = props.properties.device_name;
            name[255] = 0;
            let name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy();
            Ok(format!(
                "MoltenVK {}.{}.{} on {name}, VK_EXT_metal_objects present",
                v / 10000,
                v / 100 % 100,
                v % 100
            ))
        })();
        unsafe { instance.destroy_instance(None) };
        out
    };
    // the native generator needs neither, so on apple silicon those two only limit moltenvk
    let native = std::env::var_os("LSFGM_NATIVE").is_none_or(|v| v != "0")
        && objc2_metal::MTLCreateSystemDefaultDevice()
            .is_some_and(|d| objc2_metal::MTLDevice::supportsFamily(&*d, objc2_metal::MTLGPUFamily::Apple7));
    match run() {
        Err(e) if native && limited.get() => r.warn(
            "driver",
            &format!("{e}; the native Metal generator does not need it, so Metal and OpenGL games and Vulkan games on the proxy can still generate"),
        ),
        res => r.res("driver", res),
    }
}

// the shader package: found, parsable, and the frame-generation build rather than the DXBC one
fn check_dll(r: &mut Report, dll: Option<PathBuf>) {
    let Some(dll) = dll else {
        // discovery looks inside WINEPREFIX, which the launcher sets and a shell does not
        return r.warn(
            "shaders",
            "no lsfg-vk.dll here: pass --dll, set LSFGM_DLL_PATH, or set WINEPREFIX to the bottle that has Lossless Scaling installed",
        );
    };
    if dll.file_name().and_then(|n| n.to_str()) != Some("lsfg-vk.dll") {
        // Lossless.dll parses and yields modules, but they are DXBC and generation would fail later
        return r.fail(
            "shaders",
            &format!(
                "{} is not lsfg-vk.dll; the default branch ships DXBC shaders, so pick the lsfg-vk beta branch in Steam",
                dll.display()
            ),
        );
    }
    let run = || -> Result<String, String> {
        let bytes = std::fs::read(&dll).map_err(|e| format!("{}: {e}", dll.display()))?;
        let res = shaders::parse(&bytes)?;
        Ok(format!("{} ({} modules)", dll.display(), res.len()))
    };
    r.res("shaders", run());
}

// the hardened runtime, library validation, restrict and sip each drop DYLD_INSERT_LIBRARIES
fn check_entitlements(r: &mut Report, app: &Path) {
    let out = Command::new("codesign")
        .args(["-dv", "--entitlements", "-", "--xml"])
        .arg(app)
        .output();
    let Ok(out) = out else {
        return r.warn("entitlements", "codesign not available");
    };
    // the signature summary goes to stderr, the entitlement plist to stdout
    let info = String::from_utf8_lossy(&out.stderr);
    let ents = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return r.warn(
            "entitlements",
            &format!(
                "{}: {}",
                app.display(),
                info.lines().next().unwrap_or("codesign failed")
            ),
        );
    }
    // "flags=0x10000(runtime)" or a comma-separated list of several
    let flags = info
        .lines()
        .find_map(|l| l.split("flags=").nth(1))
        .and_then(|f| f.split_once('('))
        .and_then(|(_, rest)| rest.split(')').next())
        .unwrap_or("")
        .to_string();
    // /usr/local is the documented hole in SIP's /usr coverage, and it is where intel homebrew lives
    let protected = ["/System/", "/bin/", "/sbin/", "/usr/"];
    if protected.iter().any(|p| app.starts_with(p)) && !app.starts_with("/usr/local") {
        return r.warn(
            "entitlements",
            &format!(
                "{}: SIP protects this path, so dyld drops DYLD_INSERT_LIBRARIES whatever the entitlements say",
                app.display()
            ),
        );
    }
    let blocking: Vec<&str> = ["runtime", "library-validation", "restrict"]
        .into_iter()
        .filter(|f| flags.contains(f))
        .collect();
    if blocking.is_empty() {
        return r.ok(
            "entitlements",
            &format!("{}: no flag blocks injection", app.display()),
        );
    }
    if flags.contains("restrict") {
        return r.fail(
            "entitlements",
            &format!(
                "{} is signed restrict, which no entitlement can waive; DYLD_INSERT_LIBRARIES is dropped on exec",
                app.display()
            ),
        );
    }
    // measured on macos 27: hardened needs allow-dyld-environment-variables or get-task-allow and disable-library-validation
    let hardened = flags.contains("runtime");
    let mut missing = vec![];
    if hardened && !["allow-dyld-environment-variables", "get-task-allow"].iter().any(|k| entitled(&ents, k)) {
        missing.push("allow-dyld-environment-variables (or get-task-allow)");
    }
    if (hardened || flags.contains("library-validation")) && !entitled(&ents, "disable-library-validation") {
        missing.push("disable-library-validation");
    }
    if missing.is_empty() {
        r.ok(
            "entitlements",
            &format!(
                "{}: {} set, and entitled for injection",
                app.display(),
                blocking.join(" + ")
            ),
        );
    } else {
        r.fail(
            "entitlements",
            &format!(
                "{} is signed {} without {}; the shim is not injected",
                app.display(),
                blocking.join(" + "),
                missing.join(" and ")
            ),
        );
    }
}

// an entitlement counts only when its value is true
fn entitled(ents: &str, key: &str) -> bool {
    ents.split(key).skip(1).any(|rest| {
        rest.trim_start()
            .trim_start_matches("</key>")
            .trim_start()
            .starts_with("<true/>")
    })
}

// the pipeline cache is optional, but an unwritable directory costs the whole rebuild every launch
fn check_cache(r: &mut Report) {
    let dir = env("XDG_CACHE_HOME")
        .map(|c| PathBuf::from(c).join("lsfg-metal"))
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join("Library/Caches/lsfg-metal")));
    let Some(dir) = dir else {
        return r.warn("pipeline cache", "no HOME, so no cache is kept");
    };
    let probe = dir.join(".doctor");
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&probe, b"")) {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            r.ok("pipeline cache", &dir.display().to_string());
        }
        Err(e) => r.warn(
            "pipeline cache",
            &format!(
                "{} is not writable ({e}); pipelines rebuild every launch",
                dir.display()
            ),
        ),
    }
}

// the profile that would be picked in this process, which is the one question a launcher cannot answer
fn check_profile(r: &mut Report, disabled: bool) {
    match settings::load() {
        Err(e) => r.fail("config", &e),
        Ok(cfg) => {
            r.ok(
                "config",
                &format!(
                    "{} ({} profile{})",
                    settings::config_path(&settings::os_env).display(),
                    cfg.profiles.len(),
                    if cfg.profiles.len() == 1 { "" } else { "s" }
                ),
            );
            if disabled {
                return r.warn(
                    "profile",
                    "the kill switch is set in this environment, so the shim stays passive in every process that inherits it",
                );
            }
            // doctor disables itself before dlopening the shim, so ask without that override
            let env = |k: &str| match k {
                "LSFGM_DISABLE" | "DISABLE_LSFGM" => None,
                _ => settings::os_env(k),
            };
            match settings::identify_with(&cfg, &env) {
                Some((i, m)) => r.ok(
                    "profile",
                    &format!(
                        "'{}' via {} (as seen from doctor, not from the game)",
                        cfg.profiles[i].name,
                        m.name()
                    ),
                ),
                None => r.warn(
                    "profile",
                    "nothing matches doctor itself; run the game and check its log line instead",
                ),
            }
        }
    }
}

const USAGE: &str =
    "usage: doctor [--shim libMoltenVK.dylib] [--dll lsfg-vk.dll] [--app the-binary-that-is-injected]";

fn main() {
    // moltenvk prints its whole extension list at info level; the report is the output here
    std::env::set_var("MVK_CONFIG_LOG_LEVEL", "1");
    // --shim loads the shim here, so disable it first and its initializer arms nothing
    let disabled = settings::disabled(&settings::os_env);
    std::env::set_var("LSFGM_DISABLE", "1");
    let mut shim = None;
    let mut dll = None;
    let mut app = None;
    let mut args = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = args.next() {
        let mut val = || {
            args.next().unwrap_or_else(|| {
                eprintln!("{a} needs a value");
                exit(2)
            })
        };
        match a.as_str() {
            "--shim" => shim = Some(PathBuf::from(val())),
            "--dll" => dll = Some(PathBuf::from(val())),
            "--app" => app = Some(PathBuf::from(val())),
            "-h" | "--help" => {
                println!("{USAGE}");
                exit(0)
            }
            _ => {
                eprintln!("unknown argument '{a}'\n{USAGE}");
                exit(2)
            }
        }
    }

    let mut r = Report { failed: false };
    check_dyld(&mut r);
    let real = match shim {
        Some(s) => check_install(&mut r, &s),
        None => {
            r.warn(
                "shim",
                "not checked: pass --shim <the installed libMoltenVK.dylib>",
            );
            env("LSFGM_MOLTENVK").map(PathBuf::from)
        }
    };
    match real {
        Some(real) => check_driver(&mut r, &real),
        None => r.warn("driver", "not checked: no real driver to load"),
    }
    // the shim's own order: the config's dll (which LSFGM_DLL_PATH overrides), the path fix-up, then discovery
    let dll = dll
        .or_else(|| settings::load().ok().and_then(|c| c.dll).map(PathBuf::from))
        .or_else(|| env("LSFGM_DLL_PATH").map(PathBuf::from))
        .map(|d| shaders::fix_dll_path(&d))
        .or_else(shaders::find_dll);
    check_dll(&mut r, dll);
    check_cache(&mut r);
    check_profile(&mut r, disabled);
    if let Some(app) = app {
        check_entitlements(&mut r, &app);
    }
    if r.failed {
        println!("\nfailed checks above will stop frame generation");
        exit(1);
    }
}
