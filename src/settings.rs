// settings library: env vars, toml v2 file, profile selection, mtime reload

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

// env lookup; callers pass `&os_env`, tests pass a closure over a slice
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn os_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

// variables are only honoured when non-empty
fn nonempty(env: Env, name: &str) -> Option<String> {
    env(name).filter(|v| !v.is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s.to_lowercase().as_str() {
            "debug" => Self::Debug,
            "info" => Self::Info,
            "warning" => Self::Warning,
            "error" => Self::Error,
            other => return Err(format!("Unrecognized log level: {other}")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacingMode {
    Vsync,
    Adaptive,
}

impl PacingMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s.to_lowercase().as_str() {
            "vsync" | "none" => Self::Vsync,
            "adaptive" => Self::Adaptive,
            other => return Err(format!("Unrecognized pacing mode: {other}")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Vsync => "vsync",
            Self::Adaptive => "adaptive",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    pub active_in: Vec<String>,
    pub pacing_mode: PacingMode,
    pub multiplier: u32,
    pub flow_scale: f32,
    pub performance_mode: bool,
    pub override_present_mode: bool,
    pub preserve_swapchain_image_count: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: String::new(),
            active_in: Vec::new(),
            pacing_mode: PacingMode::Vsync,
            multiplier: 2,
            flow_scale: 1.0,
            performance_mode: false,
            override_present_mode: true,
            preserve_swapchain_image_count: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub dll: Option<String>,
    pub allow_half_precision: bool,
    pub profiles: Vec<Profile>,
    pub log_level: LogLevel,
    pub log_file: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dll: None,
            allow_half_precision: true,
            profiles: Vec::new(),
            log_level: LogLevel::Info,
            log_file: None,
        }
    }
}

impl Config {
    // built-in configuration written on first run
    pub fn builtin() -> Self {
        Self {
            profiles: vec![Profile {
                name: "Default 2x".into(),
                active_in: vec!["000000".into()],
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

// config path search; last match wins
pub fn config_path(env: Env) -> PathBuf {
    let mut path = PathBuf::from("/etc/lsfg-metal/conf.toml");
    if let Some(home) = nonempty(env, "HOME") {
        path = Path::new(&home).join(".config/lsfg-metal/conf.toml");
    }
    if let Some(xdg) = nonempty(env, "XDG_CONFIG_HOME") {
        path = Path::new(&xdg).join("lsfg-metal/conf.toml");
    }
    if let Some(cfg) = nonempty(env, "LSFGM_CONFIG") {
        path = PathBuf::from(cfg);
    }
    path
}

// global env overrides, applied in every mode and on reload
pub fn apply_env(cfg: &mut Config, env: Env) -> Result<(), String> {
    if let Some(v) = nonempty(env, "LSFGM_DLL_PATH") {
        cfg.dll = Some(v);
    }
    if let Some(v) = nonempty(env, "LSFGM_NO_FP16") {
        cfg.allow_half_precision = v != "1";
    }
    if let Some(v) = nonempty(env, "LSFGM_LOG_LEVEL") {
        cfg.log_level = LogLevel::parse(&v)?;
    }
    if let Some(v) = nonempty(env, "LSFGM_LOG_FILE") {
        cfg.log_file = Some(v);
    }
    Ok(())
}

// strtof: longest parsable prefix after leading whitespace, junk gives 0
fn strtof(s: &str) -> f32 {
    let s = s.trim_start();
    (1..=s.len())
        .rev()
        .find_map(|n| s.get(..n)?.parse().ok())
        .unwrap_or(0.0)
}

fn env_profile(env: Env) -> Result<Profile, String> {
    let mut p = Profile {
        name: "(environment)".into(),
        ..Default::default()
    };
    if let Some(v) = nonempty(env, "LSFGM_MULTIPLIER") {
        p.multiplier = v
            .parse()
            .ok()
            .filter(|_| v.bytes().all(|b| b.is_ascii_digit()))
            .ok_or("Invalid LSFGM_MULTIPLIER")?;
    }
    if let Some(v) = nonempty(env, "LSFGM_FLOW_SCALE") {
        p.flow_scale = strtof(&v);
    }
    if let Some(v) = nonempty(env, "LSFGM_PERFORMANCE_MODE") {
        p.performance_mode = v == "1";
    }
    if let Some(v) = nonempty(env, "LSFGM_PACING_MODE") {
        p.pacing_mode = PacingMode::parse(&v)?;
    }
    if let Some(v) = nonempty(env, "LSFGM_OVERRIDE_PRESENT_MODE") {
        p.override_present_mode = v == "1";
    }
    if let Some(v) = nonempty(env, "LSFGM_PRESERVE_SWAPCHAIN_IMAGE_COUNT") {
        p.preserve_swapchain_image_count = v == "1";
    }
    if p.multiplier > 4 {
        return Err("The macOS shim supports multipliers from 2 to 4".into());
    }
    if p.multiplier <= 1 {
        return Err("LSFGM_MULTIPLIER must be greater than 1".into());
    }
    if !(0.25..=1.0).contains(&p.flow_scale) {
        return Err("LSFGM_FLOW_SCALE must be between 0.25 and 1.0".into());
    }
    Ok(p)
}

// load the config, writing the built-in default when no file exists
pub fn load() -> Result<Config, String> {
    load_with(&os_env)
}

pub fn load_with(env: Env) -> Result<Config, String> {
    let mut cfg = Config::default();
    if env("LSFGM_ENV").is_some() {
        apply_env(&mut cfg, env)?;
        cfg.profiles.push(env_profile(env)?);
        return Ok(cfg);
    }
    let path = config_path(env);
    if path.exists() {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg = parse(&text, env)?;
    } else if nonempty(env, "LSFGM_CONFIG").is_some() {
        return Err(format!(
            "LSFGM_CONFIG is set but file does not exist: {}",
            path.display()
        ));
    } else {
        cfg = Config::builtin();
        write(&cfg, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // env overrides are applied to the freshly written default too, so a first run honours them
    apply_env(&mut cfg, env)?;
    Ok(cfg)
}

enum Val {
    Str(String),
    Bool(bool),
    Int(i64),
    Float(f64),
    Arr(Vec<Val>),
}

// single-line toml subset (basic/literal strings, bool, int, float, one-line arrays)
fn value(s: &str) -> Result<(Val, &str), String> {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut it = r.char_indices();
        while let Some((i, c)) = it.next() {
            match c {
                '"' => return Ok((Val::Str(out), &r[i + 1..])),
                '\\' => out.push(match it.next().map(|(_, c)| c) {
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('r') => '\r',
                    Some('"') => '"',
                    Some('\\') => '\\',
                    _ => return Err("invalid escape sequence".into()),
                }),
                c => out.push(c),
            }
        }
        return Err("unterminated string".into());
    }
    if let Some(r) = s.strip_prefix('\'') {
        let n = r.find('\'').ok_or("unterminated string")?;
        return Ok((Val::Str(r[..n].into()), &r[n + 1..]));
    }
    if let Some(mut r) = s.strip_prefix('[') {
        let mut items = Vec::new();
        loop {
            r = r.trim_start();
            if let Some(rest) = r.strip_prefix(']') {
                return Ok((Val::Arr(items), rest));
            }
            if !items.is_empty() {
                r = r
                    .strip_prefix(',')
                    .ok_or("expected ',' or ']' in array")?
                    .trim_start();
                if let Some(rest) = r.strip_prefix(']') {
                    return Ok((Val::Arr(items), rest));
                }
            }
            let (v, rest) = value(r)?;
            items.push(v);
            r = rest;
        }
    }
    let n = s
        .find(|c: char| c.is_whitespace() || matches!(c, ',' | ']' | '#'))
        .unwrap_or(s.len());
    let (tok, rest) = s.split_at(n);
    let t = tok.replace('_', "");
    let v = match tok {
        "true" => Val::Bool(true),
        "false" => Val::Bool(false),
        _ if t.parse::<i64>().is_ok() => Val::Int(t.parse().unwrap()),
        _ if !t.is_empty() && t.parse::<f64>().is_ok() => Val::Float(t.parse().unwrap()),
        _ => return Err(format!("invalid value '{tok}'")),
    };
    Ok((v, rest))
}

fn expand_home(s: String, env: Env) -> String {
    match (s.strip_prefix('~'), nonempty(env, "HOME")) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => home + rest,
        _ => s,
    }
}

fn typed<T>(v: Option<T>, key: &str) -> Result<T, String> {
    v.ok_or_else(|| format!("Invalid type for value '{key}'"))
}

fn as_str(v: Val, key: &str) -> Result<String, String> {
    typed(if let Val::Str(s) = v { Some(s) } else { None }, key)
}

fn as_bool(v: Val, key: &str) -> Result<bool, String> {
    typed(if let Val::Bool(b) = v { Some(b) } else { None }, key)
}

// parse the config text; a version marker and a [global] section are required
pub fn parse(text: &str, env: Env) -> Result<Config, String> {
    #[derive(PartialEq)]
    enum Section {
        Top,
        Global,
        Profile,
    }
    // a missing profile array is accepted as empty
    let mut cfg = Config::default();
    let (mut section, mut version_ok, mut saw_global) = (Section::Top, false, false);
    // editors on windows like to prefix a utf-8 bom
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let syntax = |what: &str| format!("{what}\n    at line {}", n + 1);
        if line.starts_with('[') {
            let header = line.split('#').next().unwrap().trim();
            let (name, array) = match header.strip_prefix("[[").and_then(|h| h.strip_suffix("]]")) {
                Some(name) => (name.trim(), true),
                None => (
                    header[1..]
                        .strip_suffix(']')
                        .ok_or_else(|| syntax("malformed table header"))?
                        .trim(),
                    false,
                ),
            };
            section = match (name, array) {
                ("global", false) => Section::Global,
                ("global", true) => return Err("Malformed global section in config".into()),
                ("profile", true) => Section::Profile,
                ("profile", false) => {
                    return Err("Malformed profiles section in config".into())
                }
                _ => return Err(format!("Unknown key in configuration: {name}")),
            };
            if array {
                cfg.profiles.push(Profile::default());
            } else {
                saw_global = true;
            }
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .ok_or_else(|| syntax("expected key = value"))?;
        let key = key.trim();
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(syntax("invalid key"));
        }
        let (val, rest) = value(rest).map_err(|e| syntax(&e))?;
        if !rest.trim().is_empty() && !rest.trim_start().starts_with('#') {
            return Err(syntax("unexpected text after value"));
        }
        match section {
            Section::Top => match key {
                "version" => version_ok = matches!(val, Val::Int(2)),
                "global" | "profile" => {
                    return Err(format!(
                        "Malformed {} section in config",
                        if key == "global" {
                            "global"
                        } else {
                            "profiles"
                        }
                    ))
                }
                _ => return Err(format!("Unknown key in configuration: {key}")),
            },
            Section::Global => match key {
                "allow_fp16" => cfg.allow_half_precision = as_bool(val, key)?,
                "dll" => cfg.dll = Some(expand_home(as_str(val, key)?, env)),
                "log_level" => cfg.log_level = LogLevel::parse(&as_str(val, key)?)?,
                "log_file" => cfg.log_file = Some(expand_home(as_str(val, key)?, env)),
                _ => return Err(format!("Unknown key in [global] section: {key}")),
            },
            Section::Profile => {
                let p = cfg.profiles.last_mut().unwrap();
                match key {
                    "name" => p.name = as_str(val, key)?,
                    "active_in" => {
                        p.active_in = match val {
                            Val::Str(s) => vec![s],
                            Val::Arr(items) => items
                                .into_iter()
                                .map(|v| if let Val::Str(s) = v { Some(s) } else { None })
                                .collect::<Option<_>>()
                                .ok_or("Wrong type for active_in")?,
                            _ => return Err("Wrong type for active_in".into()),
                        }
                    }
                    "pacing_mode" | "pacing" => {
                        p.pacing_mode = PacingMode::parse(&as_str(val, key)?)?
                    }
                    // out of range integers land in the range checks below
                    "multiplier" => {
                        p.multiplier =
                            typed(if let Val::Int(i) = val { Some(i) } else { None }, key)?
                                .clamp(0, u32::MAX as i64) as u32
                    }
                    "flow_scale" => {
                        p.flow_scale = typed(
                            match val {
                                Val::Float(f) => Some(f as f32),
                                Val::Int(i) => Some(i as f32),
                                _ => None,
                            },
                            key,
                        )?
                    }
                    "performance_mode" => p.performance_mode = as_bool(val, key)?,
                    "override_present_mode" => p.override_present_mode = as_bool(val, key)?,
                    "preserve_swapchain_image_count" => {
                        p.preserve_swapchain_image_count = as_bool(val, key)?
                    }
                    _ => return Err(format!("Unknown key in profile section: {key}")),
                }
            }
        }
    }
    if !version_ok {
        return Err("Config version not supported; expected 2.".into());
    }
    if !saw_global {
        return Err("Malformed global section in config".into());
    }
    for p in &cfg.profiles {
        if p.multiplier > 4 {
            return Err("Profile multipliers must be 1 to 4".into());
        }
        if p.multiplier < 1 {
            return Err(format!("Profile '{}' has multiplier < 1", p.name));
        }
        if !(0.25..=1.0).contains(&p.flow_scale) {
            return Err(format!(
                "Profile '{}' has flow_scale out of range (must be between 0.25 and 1.0)",
                p.name
            ));
        }
    }
    Ok(cfg)
}

pub fn to_toml(cfg: &Config) -> String {
    fn q(s: &str) -> String {
        let s = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            "\"{}\"",
            s.replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t")
        )
    }
    let mut out = String::from(
        "# active_in lists Steam App IDs ($SteamAppId), not executable names\nversion = 2\n\n[global]\n",
    );
    if let Some(dll) = &cfg.dll {
        let _ = writeln!(out, "dll = {}", q(dll));
    }
    let _ = writeln!(
        out,
        "allow_fp16 = {}\nlog_level = {}",
        cfg.allow_half_precision,
        q(cfg.log_level.name())
    );
    if let Some(file) = &cfg.log_file {
        let _ = writeln!(out, "log_file = {}", q(file));
    }
    for p in &cfg.profiles {
        let _ = writeln!(out, "\n[[profile]]\nname = {}", q(&p.name));
        match p.active_in.as_slice() {
            [] => {}
            [one] => {
                let _ = writeln!(out, "active_in = {}", q(one));
            }
            many => {
                let _ = writeln!(
                    out,
                    "active_in = [{}]",
                    many.iter().map(|s| q(s)).collect::<Vec<_>>().join(", ")
                );
            }
        }
        let _ = writeln!(
            out,
            "pacing_mode = {}\nmultiplier = {}\nflow_scale = {:?}\nperformance_mode = {}\noverride_present_mode = {}\npreserve_swapchain_image_count = {}",
            q(p.pacing_mode.name()),
            p.multiplier,
            p.flow_scale,
            p.performance_mode,
            p.override_present_mode,
            p.preserve_swapchain_image_count
        );
    }
    out
}

pub fn write(cfg: &Config, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, to_toml(cfg))
}

// mtime polling; a failed parse keeps the old mtime so the next call retries, but the error is reported once per mtime
pub struct Watcher {
    path: PathBuf,
    mtime: (i64, i64),
    failed: Option<(i64, i64)>,
}

fn mtime(path: &Path) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| (m.mtime(), m.mtime_nsec()))
        .unwrap_or((-1, -1))
}

impl Watcher {
    pub fn new(path: PathBuf) -> Self {
        let mtime = mtime(&path);
        Self {
            path,
            mtime,
            failed: None,
        }
    }

    pub fn check_and_reload(&mut self, env: Env) -> Result<Option<Config>, String> {
        let now = mtime(&self.path);
        if now == self.mtime || now == (-1, -1) {
            return Ok(None);
        }
        match self.reload(env) {
            Ok(cfg) => {
                self.mtime = now;
                self.failed = None;
                Ok(Some(cfg))
            }
            Err(_) if self.failed == Some(now) => Ok(None),
            Err(e) => {
                self.failed = Some(now);
                Err(e)
            }
        }
    }

    fn reload(&self, env: Env) -> Result<Config, String> {
        let text = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut cfg = parse(&text, env)?;
        apply_env(&mut cfg, env)?;
        Ok(cfg)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Environment,
    SteamAppId,
}

impl Method {
    // wording used in the profile selection log line
    pub fn name(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::SteamAppId => "Steam App ID",
        }
    }
}

// profile index and how it was identified
pub fn identify(cfg: &Config) -> Option<(usize, Method)> {
    identify_with(cfg, &os_env)
}

pub fn identify_with(cfg: &Config, env: Env) -> Option<(usize, Method)> {
    if env("LSFGM_ENV").is_some() {
        return cfg.profiles.first().map(|_| (0, Method::Environment));
    }
    // an unmatched LSFGM_PROFILE falls through to the steam match
    if let Some(name) = nonempty(env, "LSFGM_PROFILE") {
        if let Some(i) = cfg.profiles.iter().position(|p| p.name == name) {
            return Some((i, Method::Environment));
        }
    }
    if let Some(id) = nonempty(env, "SteamAppId") {
        if let Some(i) = cfg.profiles.iter().position(|p| p.active_in.contains(&id)) {
            return Some((i, Method::SteamAppId));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lsfg-metal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const VALID: &str = "version = 2\n[global]\nallow_fp16 = true\nlog_level = \"info\"\n[[profile]]\nname = \"audit\"\nactive_in = \"x\"\nmultiplier = 2\n";

    #[test]
    fn env_mode_reads_every_variable() {
        let e = env(&[
            ("LSFGM_ENV", ""),
            ("LSFGM_MULTIPLIER", "3"),
            ("LSFGM_FLOW_SCALE", "0.5abc"),
            ("LSFGM_PERFORMANCE_MODE", "1"),
            ("LSFGM_PACING_MODE", "ADAPTIVE"),
            ("LSFGM_OVERRIDE_PRESENT_MODE", "0"),
            ("LSFGM_PRESERVE_SWAPCHAIN_IMAGE_COUNT", "1"),
            ("LSFGM_DLL_PATH", "/d.dll"),
            ("LSFGM_NO_FP16", "1"),
            ("LSFGM_LOG_LEVEL", "Warning"),
            ("LSFGM_LOG_FILE", "/l.log"),
        ]);
        let cfg = load_with(&e).unwrap();
        assert_eq!(cfg.dll.as_deref(), Some("/d.dll"));
        assert!(!cfg.allow_half_precision);
        assert_eq!(cfg.log_level, LogLevel::Warning);
        assert_eq!(cfg.log_file.as_deref(), Some("/l.log"));
        let p = &cfg.profiles[0];
        assert_eq!(p.name, "(environment)");
        assert_eq!(
            (p.multiplier, p.flow_scale, p.pacing_mode),
            (3, 0.5, PacingMode::Adaptive)
        );
        assert!(p.performance_mode && !p.override_present_mode && p.preserve_swapchain_image_count);
        assert_eq!(identify_with(&cfg, &e), Some((0, Method::Environment)));
    }

    #[test]
    fn env_mode_defaults_and_empty_values() {
        let e = env(&[
            ("LSFGM_ENV", "1"),
            ("LSFGM_PERFORMANCE_MODE", ""),
            ("LSFGM_NO_FP16", "0"),
            ("LSFGM_LOG_LEVEL", ""),
        ]);
        let cfg = load_with(&e).unwrap();
        assert!(cfg.allow_half_precision && cfg.dll.is_none() && cfg.log_file.is_none());
        assert_eq!(cfg.log_level, LogLevel::Info);
        assert_eq!(
            cfg.profiles[0],
            Profile {
                name: "(environment)".into(),
                ..Default::default()
            }
        );
        let e = env(&[
            ("LSFGM_ENV", "1"),
            ("LSFGM_PERFORMANCE_MODE", "yes"),
            ("LSFGM_PACING_MODE", "None"),
        ]);
        let p = &load_with(&e).unwrap().profiles[0];
        assert!(!p.performance_mode);
        assert_eq!(p.pacing_mode, PacingMode::Vsync);
    }

    #[test]
    fn env_mode_errors() {
        let err = |vars: &[(&str, &str)]| {
            let mut all = vec![("LSFGM_ENV", "1")];
            all.extend_from_slice(vars);
            load_with(&env(&all)).unwrap_err()
        };
        for bad in ["4294967298", "2junk", "-1", "+2", " 2"] {
            assert_eq!(
                err(&[("LSFGM_MULTIPLIER", bad)]),
                "Invalid LSFGM_MULTIPLIER",
                "{bad}"
            );
        }
        assert_eq!(
            err(&[("LSFGM_MULTIPLIER", "5")]),
            "The macOS shim supports multipliers from 2 to 4"
        );
        assert_eq!(
            err(&[("LSFGM_MULTIPLIER", "1")]),
            "LSFGM_MULTIPLIER must be greater than 1"
        );
        assert_eq!(
            err(&[("LSFGM_MULTIPLIER", "0")]),
            "LSFGM_MULTIPLIER must be greater than 1"
        );
        assert_eq!(
            err(&[("LSFGM_FLOW_SCALE", "0.2")]),
            "LSFGM_FLOW_SCALE must be between 0.25 and 1.0"
        );
        assert_eq!(
            err(&[("LSFGM_FLOW_SCALE", "junk")]),
            "LSFGM_FLOW_SCALE must be between 0.25 and 1.0"
        );
        assert_eq!(
            err(&[("LSFGM_PACING_MODE", "Bogus")]),
            "Unrecognized pacing mode: bogus"
        );
        assert_eq!(
            err(&[("LSFGM_LOG_LEVEL", "Loud")]),
            "Unrecognized log level: loud"
        );
    }

    #[test]
    fn config_path_precedence() {
        assert_eq!(
            config_path(&env(&[])),
            PathBuf::from("/etc/lsfg-metal/conf.toml")
        );
        assert_eq!(
            config_path(&env(&[("HOME", "/h")])),
            PathBuf::from("/h/.config/lsfg-metal/conf.toml")
        );
        assert_eq!(
            config_path(&env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "/x")])),
            PathBuf::from("/x/lsfg-metal/conf.toml")
        );
        assert_eq!(
            config_path(&env(&[("HOME", "/h"), ("LSFGM_CONFIG", "/c.toml")])),
            PathBuf::from("/c.toml")
        );
        assert_eq!(
            config_path(&env(&[("HOME", ""), ("LSFGM_CONFIG", "")])),
            PathBuf::from("/etc/lsfg-metal/conf.toml")
        );
    }

    #[test]
    fn missing_config_file() {
        let dir = tmp("missing");
        let path = dir.join("nope.toml");
        let e = env(&[("LSFGM_CONFIG", path.to_str().unwrap())]);
        assert_eq!(
            load_with(&e).unwrap_err(),
            format!(
                "LSFGM_CONFIG is set but file does not exist: {}",
                path.display()
            )
        );
        // without LSFGM_CONFIG the default file is written and read back
        let e = env(&[
            ("HOME", dir.to_str().unwrap()),
            ("LSFGM_LOG_LEVEL", "debug"),
        ]);
        let cfg = load_with(&e).unwrap();
        assert_eq!(cfg.log_level, LogLevel::Debug);
        assert_eq!(cfg.profiles, Config::builtin().profiles);
        let written = dir.join(".config/lsfg-metal/conf.toml");
        assert_eq!(
            parse(&std::fs::read_to_string(&written).unwrap(), &env(&[])).unwrap(),
            Config::builtin()
        );
        let cfg2 = load_with(&env(&[("HOME", dir.to_str().unwrap())])).unwrap();
        assert_eq!(cfg2, Config::builtin());
    }

    #[test]
    fn toml_roundtrip_and_home_expansion() {
        let mut cfg = Config::builtin();
        cfg.dll = Some("/a b/\"q\".dll".into());
        cfg.log_file = Some("/l.log".into());
        cfg.profiles.push(Profile {
            name: "solo".into(),
            active_in: vec![],
            ..Default::default()
        });
        assert_eq!(parse(&to_toml(&cfg), &env(&[])).unwrap(), cfg);
        let text = "version = 2\n[global]\ndll = \"~/x.dll\"\nlog_file = '~/l.log' # c\n[[profile]]\nname = \"p\"\nactive_in = [\"a\", 'b']\npacing = \"adaptive\"\nflow_scale = 1\n";
        let cfg = parse(text, &env(&[("HOME", "/h")])).unwrap();
        assert_eq!(cfg.dll.as_deref(), Some("/h/x.dll"));
        assert_eq!(cfg.log_file.as_deref(), Some("/h/l.log"));
        let p = &cfg.profiles[0];
        assert_eq!(p.active_in, ["a", "b"]);
        assert_eq!((p.pacing_mode, p.flow_scale), (PacingMode::Adaptive, 1.0));
        // no profiles at all is accepted
        assert!(parse("version = 2\n[global]\n", &env(&[]))
            .unwrap()
            .profiles
            .is_empty());
    }

    #[test]
    fn toml_errors() {
        let e = env(&[]);
        let err = |t: &str| parse(t, &e).unwrap_err();
        assert_eq!(
            err("version = 2\n[global]\nallow_fp16 = \"x\n"),
            "unterminated string\n    at line 3"
        );
        assert_eq!(
            err("version = 2\n[global]\nallow_fp16 = true junk\n"),
            "unexpected text after value\n    at line 3"
        );
        assert_eq!(
            err("version = 2\nfoo = 1\n[global]\n"),
            "Unknown key in configuration: foo"
        );
        assert_eq!(
            err("version = 2\n[global]\n[other]\n"),
            "Unknown key in configuration: other"
        );
        assert_eq!(
            err("version = 1\n[global]\n"),
            "Config version not supported; expected 2."
        );
        assert_eq!(
            err("[global]\n"),
            "Config version not supported; expected 2."
        );
        assert_eq!(
            err("version = 2\n"),
            "Malformed global section in config"
        );
        assert_eq!(
            err("version = 2\n[[global]]\n"),
            "Malformed global section in config"
        );
        assert_eq!(
            err("version = 2\n[global]\nfoo = 1\n"),
            "Unknown key in [global] section: foo"
        );
        assert_eq!(
            err("version = 2\n[global]\nallow_fp16 = 1\n"),
            "Invalid type for value 'allow_fp16'"
        );
        assert_eq!(
            err("version = 2\n[global]\nlog_level = \"Loud\"\n"),
            "Unrecognized log level: loud"
        );
        assert_eq!(
            err("version = 2\n[global]\n[profile]\n"),
            "Malformed profiles section in config"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nfoo = 1\n"),
            "Unknown key in profile section: foo"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nactive_in = 1\n"),
            "Wrong type for active_in"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nactive_in = [\"a\", 1]\n"),
            "Wrong type for active_in"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nmultiplier = \"2\"\n"),
            "Invalid type for value 'multiplier'"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\npacing_mode = \"x\"\n"),
            "Unrecognized pacing mode: x"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nmultiplier = 5\n"),
            "Profile multipliers must be 1 to 4"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nmultiplier = 0\nname = \"p\"\n"),
            "Profile 'p' has multiplier < 1"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nmultiplier = -3\nname = \"p\"\n"),
            "Profile 'p' has multiplier < 1"
        );
        assert_eq!(
            err("version = 2\n[global]\n[[profile]]\nname = \"p\"\nflow_scale = 1.5\n"),
            "Profile 'p' has flow_scale out of range (must be between 0.25 and 1.0)"
        );
        // multiplier 1 is valid in a file
        assert_eq!(
            parse("version = 2\n[global]\n[[profile]]\nmultiplier = 1\n", &e)
                .unwrap()
                .profiles[0]
                .multiplier,
            1
        );
    }

    #[test]
    fn identify_order() {
        let mut cfg = Config::builtin();
        cfg.profiles.push(Profile {
            name: "named".into(),
            active_in: vec!["480".into()],
            ..Default::default()
        });
        assert_eq!(
            identify_with(&cfg, &env(&[("LSFGM_ENV", ""), ("SteamAppId", "480")])),
            Some((0, Method::Environment))
        );
        assert_eq!(
            identify_with(
                &cfg,
                &env(&[("LSFGM_PROFILE", "named"), ("SteamAppId", "480")])
            ),
            Some((1, Method::Environment))
        );
        assert_eq!(
            identify_with(
                &cfg,
                &env(&[("LSFGM_PROFILE", "NAMED"), ("SteamAppId", "480")])
            ),
            Some((1, Method::SteamAppId))
        );
        assert_eq!(
            identify_with(&cfg, &env(&[("SteamAppId", "000000")])),
            Some((0, Method::SteamAppId))
        );
        assert_eq!(identify_with(&cfg, &env(&[("SteamAppId", "999")])), None);
        assert_eq!(
            identify_with(&cfg, &env(&[("LSFGM_PROFILE", ""), ("SteamAppId", "")])),
            None
        );
        assert_eq!(
            identify_with(&Config::default(), &env(&[("LSFGM_ENV", "1")])),
            None
        );
        assert_eq!(Method::SteamAppId.name(), "Steam App ID");
    }

    #[test]
    fn reload_on_mtime_change() {
        let dir = tmp("reload");
        let path = dir.join("conf.toml");
        let e = env(&[("LSFGM_LOG_LEVEL", "error")]);
        std::fs::write(&path, VALID).unwrap();
        let mut w = Watcher::new(path.clone());
        assert!(
            w.check_and_reload(&e).unwrap().is_none(),
            "Unchanged configuration reloaded"
        );
        let set_mtime = |secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap()
        };
        std::fs::write(&path, VALID.replace("multiplier = 2", "multiplier = 3")).unwrap();
        set_mtime(2208988800);
        let cfg = w
            .check_and_reload(&e)
            .unwrap()
            .expect("Post-2038 configuration was ignored");
        assert_eq!(cfg.profiles[0].multiplier, 3);
        assert_eq!(cfg.log_level, LogLevel::Error);
        std::fs::write(&path, "version = 2\n[global\n").unwrap();
        set_mtime(2208988801);
        assert!(
            w.check_and_reload(&e).is_err(),
            "Invalid configuration was accepted"
        );
        assert!(
            w.check_and_reload(&e).unwrap().is_none(),
            "Same broken file reported twice"
        );
        std::fs::write(&path, VALID).unwrap();
        set_mtime(2208988801);
        let cfg = w
            .check_and_reload(&e)
            .unwrap()
            .expect("Failed reload consumed the update timestamp");
        assert_eq!(cfg.profiles[0].multiplier, 2);
        assert!(w.check_and_reload(&e).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(w.check_and_reload(&e).unwrap().is_none());
    }
}
