// tiny logger: "(lsfg-metal) [LEVEL]: msg" to stderr plus LSFGM_LOG_FILE / set_file append
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

static LEVEL: AtomicU8 = AtomicU8::new(0);
static FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn open(path: &str) -> Option<File> {
    match OpenOptions::new().append(true).create(true).open(path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("(lsfg-metal) [WARN]: cannot open log file '{path}': {e}");
            None
        }
    }
}

fn file() -> &'static Mutex<Option<File>> {
    FILE.get_or_init(|| {
        let env = std::env::var("LSFGM_LOG_FILE")
            .ok()
            .filter(|p| !p.is_empty());
        Mutex::new(env.and_then(|p| open(&p)))
    })
}

pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

// redirect (append) to a log file; a bad path keeps stderr only
pub fn set_file(path: &str) {
    *file().lock().unwrap() = open(path);
}

pub fn enabled(level: Level) -> bool {
    (level as u8) >= LEVEL.load(Ordering::Relaxed)
}

// hot-path entry: the caller passes format_args! so nothing is formatted when the level is off
pub fn log_fmt(level: Level, args: std::fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    let tag = match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    };
    let line = format!("(lsfg-metal) [{tag}]: {args}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Some(f) = file().lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn log(level: Level, msg: &str) {
    log_fmt(level, format_args!("{msg}"))
}

pub fn debug(msg: &str) {
    log(Level::Debug, msg)
}
pub fn info(msg: &str) {
    log(Level::Info, msg)
}
pub fn warn(msg: &str) {
    log(Level::Warn, msg)
}
pub fn error(msg: &str) {
    log(Level::Error, msg)
}
