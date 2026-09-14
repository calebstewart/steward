//! The manager's own log: `%LOCALAPPDATA%\steward\steward.log`, and stderr as
//! well when running in a console; and what keeps every log bounded, the
//! units' (`manager.rs`) as well as this one.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// A log over this many bytes is set aside as `<name>.log.1`.
pub const CAP: u64 = 8 << 20;

struct Sink {
    file: Option<File>,
    path: Option<PathBuf>,
    /// The file's size, kept up to date by this process's own writes.
    size: u64,
    console: bool,
}

static SINK: Mutex<Sink> = Mutex::new(Sink {
    file: None,
    path: None,
    size: 0,
    console: false,
});

pub fn state_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("steward"))
}

/// The manager's log file, `%LOCALAPPDATA%\steward\steward.log`: also
/// where a `steward-cat`'s own few words go, since they are about the
/// manager's machinery rather than a unit's doing.
pub fn path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("steward.log"))
}

/// Where a log goes when it is set aside: `steward.log` to `steward.log.1`.
pub fn aside(path: &Path) -> PathBuf {
    path.with_extension("log.1")
}

/// A log's size, read through a handle. Its size as its directory lists it,
/// which `std::fs::metadata` on a path reads, goes stale while another
/// process holds the file open for writing -- as a unit's processes hold
/// their log for as long as they run -- and can lag by megabytes.
pub fn size(path: &Path) -> io::Result<u64> {
    Ok(File::open(path)?.metadata()?.len())
}

/// Set a log aside as `<name>.log.1` and begin it again, in place: copy it,
/// then cut it to nothing. The previous `<name>.log.1` is replaced.
///
/// In place, because a unit's processes write to its log through an
/// inherited handle, and would go on writing to a renamed file. The handle
/// is append-only, and an append-only write lands at the file's current end
/// whatever the handle's own position, so what they write next begins the
/// emptied file. Lines written between the copy and the cut are lost; the
/// caller says so in the log.
pub fn set_aside(path: &Path) -> io::Result<u64> {
    let mut from = File::open(path)?;
    let mut to = File::create(aside(path))?;
    let copied = io::copy(&mut from, &mut to)?;
    OpenOptions::new().write(true).open(path)?.set_len(0)?;
    Ok(copied)
}

/// Open the log file; `console` also copies every line to stderr.
pub fn init(console: bool) {
    let path = state_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join("steward.log"))
    });
    let mut sink = SINK.lock().unwrap_or_else(|p| p.into_inner());
    sink.path = path;
    sink.console = console;
    sink.open();
}

impl Sink {
    fn open(&mut self) {
        self.file = self
            .path
            .as_ref()
            .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());
        self.size = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }

    /// The manager is this log's only writer, so it is simply renamed, and
    /// begun again with a line that says so.
    fn set_aside(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.file = None;
        let renamed = std::fs::rename(&path, aside(&path));
        self.open();
        if let Err(e) = renamed {
            let _ = self.line(&format!(
                "{} WARN  [{}] cannot set {} aside: {e}",
                timestamp(),
                std::process::id(),
                path.display()
            ));
        }
    }

    fn line(&mut self, line: &str) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(format!("{line}\n").as_bytes())?;
            self.size += line.len() as u64 + 1;
        }
        Ok(())
    }
}

/// The local time, to the millisecond.
pub fn timestamp() -> String {
    let mut t = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

pub fn write(level: &str, message: &str) {
    let line = format!(
        "{} {level:<5} [{}] {message}",
        timestamp(),
        std::process::id()
    );
    let mut sink = SINK.lock().unwrap_or_else(|p| p.into_inner());
    let _ = sink.line(&line);
    if sink.size > CAP {
        sink.set_aside();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("steward-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn read(path: &Path) -> String {
        let mut text = String::new();
        File::open(path).unwrap().read_to_string(&mut text).unwrap();
        text
    }

    /// What a unit's processes rely on: a writer that holds an append-only
    /// handle across a set-aside goes on writing into the emptied file, not
    /// past a hole where the old contents were, and not into the copy.
    #[test]
    fn an_append_only_writer_continues_in_the_emptied_log() {
        let path = scratch("unit.log");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(aside(&path));
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for i in 0..1000 {
            writeln!(writer, "line {i}").unwrap();
        }
        let before = std::fs::metadata(&path).unwrap().len();

        assert_eq!(set_aside(&path).unwrap(), before);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(aside(&path)).unwrap().len(), before);

        writeln!(writer, "after").unwrap();
        assert_eq!(read(&path), "after\n");
        assert!(read(&aside(&path)).ends_with("line 999\n"));

        // Again: the previous copy is replaced, not appended to.
        set_aside(&path).unwrap();
        assert_eq!(read(&aside(&path)), "after\n");
        writeln!(writer, "later").unwrap();
        assert_eq!(read(&path), "later\n");
    }

    #[test]
    fn aside_keeps_the_name() {
        assert_eq!(
            aside(Path::new(r"C:\x\logs\whkd.service.log")),
            PathBuf::from(r"C:\x\logs\whkd.service.log.1")
        );
        assert_eq!(
            aside(Path::new(r"C:\x\steward.log")),
            PathBuf::from(r"C:\x\steward.log.1")
        );
    }
}
