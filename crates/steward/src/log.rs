//! The manager's own log: `%LOCALAPPDATA%\steward\steward.log`, and stderr as
//! well when running in a console. (The per-service journal is M2.)

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

struct Sink {
    file: Option<File>,
    console: bool,
}

static SINK: Mutex<Sink> = Mutex::new(Sink {
    file: None,
    console: false,
});

pub fn state_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("steward"))
}

/// Open the log file; `console` also copies every line to stderr.
pub fn init(console: bool) {
    let file = state_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("steward.log"))
            .ok()
    });
    let mut sink = SINK.lock().unwrap_or_else(|p| p.into_inner());
    sink.file = file;
    sink.console = console;
}

pub fn write(level: &str, message: &str) {
    let mut t = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut t) };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {level:<5} [{}] {message}",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        t.wMilliseconds,
        std::process::id()
    );
    let mut sink = SINK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(file) = sink.file.as_mut() {
        let _ = writeln!(file, "{line}");
    }
    if sink.console {
        eprintln!("{line}");
    }
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write("INFO", &format!($($arg)*)) };
}
macro_rules! warning {
    ($($arg:tt)*) => { $crate::log::write("WARN", &format!($($arg)*)) };
}
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::write("ERROR", &format!($($arg)*)) };
}
pub(crate) use {error, info, warning};
