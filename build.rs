// version string: LSFGM_VERSION override, else git describe, else 0.8.0
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn git_version() -> Option<String> {
    let sha = git(&["rev-parse", "--short=7", "HEAD"])?;
    let dirty = if git(&["status", "--porcelain", "--untracked-files=no"]).is_some() {
        "-dirty"
    } else {
        ""
    };
    let (tag, count) = match git(&["describe", "--tags", "--abbrev=0"]) {
        Some(tag) => {
            let count = git(&["rev-list", "--count", &format!("{tag}..HEAD")]).unwrap_or_default();
            (tag, count)
        }
        None => (
            "unknown".into(),
            git(&["rev-list", "--count", "HEAD"]).unwrap_or_default(),
        ),
    };
    let base = if count == "0" {
        tag
    } else {
        format!("{tag}.r{count}.g{sha}")
    };
    Some(format!("{base}{dirty}"))
}

fn main() {
    let v = std::env::var("LSFGM_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(git_version)
        .unwrap_or_else(|| "0.8.0".into());
    println!("cargo:rustc-env=LSFGM_VERSION={v}");
    // the shim stands in for the driver, so it carries the driver's install name (cdylib only; package.sh checks the export list)
    println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libMoltenVK.dylib");
    // the forwarded driver helpers (src/shim/forward.rs) are asm, outside rustc's export list
    println!("cargo:rustc-cdylib-link-arg=-Wl,-exported_symbol,_mvk*");
    println!("cargo:rustc-cdylib-link-arg=-Wl,-exported_symbol,_vk*");
    println!("cargo:rerun-if-env-changed=LSFGM_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    // an unstaged source edit touches no git file, so -dirty is rechecked whenever src changes
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
}
