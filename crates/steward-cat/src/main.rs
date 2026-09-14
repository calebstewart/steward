//! `steward-cat UNIT STDOUT STDERR`: writes what arrives on two inherited
//! pipe handles to the user's Event Log channel as unit `UNIT`'s output, and
//! exits once both are closed. See the crate's documentation.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    app::main()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("steward-cat: runs on Windows only");
    std::process::ExitCode::FAILURE
}

#[cfg(windows)]
mod app {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::process::ExitCode;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::thread;

    use steward_cat::etw::{Channel, Provider};
    use steward_cat::pending::Pending;
    use steward_cat::{lines, utf16, Stream, HOLD_MAX, LINE_MAX, UNIT_MAX};
    use steward_eventlog::{provider_guid, provider_name, CHANNEL_KEYWORD, CHANNEL_VALUE};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetHandleInformation, GetLastError, ERROR_BROKEN_PIPE, HANDLE,
    };
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, ReadFile, FILE_TYPE_PIPE};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, SetEvent, WaitForSingleObject, INFINITE,
    };

    const USAGE: &str = "\
usage: steward-cat UNIT STDOUT STDERR

Writes what arrives on the inherited handles STDOUT and STDERR (numbers,
decimal or 0x-hex) to the Event Log channel of the user it runs as, one
event per line with the fields unit, stream and bytes. Output is held
while nobody listens and written once someone does. Exits when both
handles are closed.";

    /// How often main looks again while output is held, beside being woken
    /// when a session enables the provider: in case an enable is missed, and
    /// for a lost-bytes count ETW refused, which is tried until it goes.
    const RETRY_MS: u32 = 1000;

    pub fn main() -> ExitCode {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        let (unit, stdout, stderr) = match parse(&args) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("steward-cat: {err}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        };
        match run(&unit, stdout, stderr) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("steward-cat: {unit}: {err}");
                ExitCode::FAILURE
            }
        }
    }

    fn parse(args: &[OsString]) -> Result<(String, Input, Input), String> {
        let [unit, stdout, stderr] = args else {
            return Err(format!("expected 3 arguments, got {}", args.len()));
        };
        let unit = unit
            .to_str()
            .filter(|u| !u.is_empty() && u.len() <= UNIT_MAX)
            .ok_or(format!("UNIT must be a name of 1 to {UNIT_MAX} bytes"))?
            .to_string();
        let (out, err) = (handle(stdout)?, handle(stderr)?);
        if out == err {
            return Err("STDOUT and STDERR must be different handles".into());
        }
        // SAFETY: both are open handles this process was given to own, and
        // they are different.
        unsafe { Ok((unit, Input::new(out), Input::new(err))) }
    }

    /// A handle given as a number, checked to be open.
    fn handle(arg: &OsString) -> Result<RawHandle, String> {
        let text = arg.to_string_lossy();
        let value = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(hex) => usize::from_str_radix(hex, 16),
            None => text.parse(),
        }
        .map_err(|_| format!("{text}: not a handle"))?;
        let handle = value as RawHandle;
        let mut flags = 0;
        if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
            return Err(format!("{text}: {}", io::Error::last_os_error()));
        }
        Ok(handle)
    }

    /// The read end of one of a unit's streams.
    struct Input {
        handle: OwnedHandle,
        /// A pipe's writer closing is its end; a file's end is reading
        /// nothing.
        pipe: bool,
    }

    impl Input {
        /// # Safety
        /// `handle` is open and this process's to close.
        unsafe fn new(handle: RawHandle) -> Input {
            let pipe = unsafe { GetFileType(handle) } == FILE_TYPE_PIPE;
            Input {
                handle: unsafe { OwnedHandle::from_raw_handle(handle) },
                pipe,
            }
        }

        /// One read: `None` at the end.
        fn read(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
            let mut n = 0u32;
            let len = buf.len().min(u32::MAX as usize) as u32;
            let ok = unsafe {
                ReadFile(
                    self.handle.as_raw_handle(),
                    buf.as_mut_ptr(),
                    len,
                    &mut n,
                    null_mut(),
                )
            };
            if ok == 0 {
                return match unsafe { GetLastError() } {
                    ERROR_BROKEN_PIPE => Ok(None),
                    err => Err(io::Error::from_raw_os_error(err as i32)),
                };
            }
            // A zero-byte write to a byte-mode pipe completes a read with
            // nothing; only the writer closing ends a pipe.
            if n == 0 && !self.pipe {
                return Ok(None);
            }
            Ok(Some(n as usize))
        }
    }

    /// An auto-reset event: set when there may be something to flush.
    struct Wake(HANDLE);

    // SAFETY: an event handle may be used from any thread.
    unsafe impl Send for Wake {}
    unsafe impl Sync for Wake {}

    impl Wake {
        fn new() -> io::Result<Wake> {
            let event = unsafe { CreateEventW(null(), 0, 0, null()) };
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Wake(event))
        }

        fn set(&self) {
            unsafe { SetEvent(self.0) };
        }
    }

    impl Drop for Wake {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// What main and the two readers share. The provider is dropped, and so
    /// unregistered, before `wake`, which its enable callback sets, is
    /// closed: fields drop in order.
    struct Shared {
        provider: Provider,
        pending: Mutex<Held>,
        wake: Wake,
        /// Readers whose pipe has closed.
        finished: AtomicUsize,
    }

    /// What the lock guards.
    struct Held {
        pending: Pending,
        /// Where a line becomes UTF-16 to be written, `LINE_MAX + 1` units
        /// long from the start.
        text: Vec<u16>,
    }

    impl Held {
        /// Writes one line.
        fn write(provider: &Provider, text: &mut Vec<u16>, stream: Stream, data: &[u8]) -> bool {
            utf16::encode(data, text);
            provider.output(stream, text)
        }

        /// Writes what is held, if a session listens. Returns whether nothing
        /// is left to write.
        fn flush(&mut self, provider: &Provider) -> bool {
            let Held { pending, text } = self;
            pending.is_empty()
                || (provider.listening()
                    && pending.drain(
                        |stream, data| Held::write(provider, text, stream, data),
                        |stream, bytes| provider.dropped(stream, bytes),
                    ))
        }
    }

    impl Shared {
        fn held(&self) -> MutexGuard<'_, Held> {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Writes a line now if a session listens, after everything held
        /// before it; holds it otherwise. Under the lock, so what is held and
        /// what is written stay in the order they were read.
        fn emit(&self, held: &mut Held, stream: Stream, data: &[u8]) {
            if self.provider.listening() && held.flush(&self.provider) {
                if !Held::write(&self.provider, &mut held.text, stream, data) {
                    // Said so by the next event that gets through.
                    held.pending.lose(stream, data.len() as u64);
                }
                return;
            }
            held.pending.push(stream, data);
        }

        fn flush(&self) {
            self.held().flush(&self.provider);
        }
    }

    fn run(unit: &str, stdout: Input, stderr: Input) -> io::Result<()> {
        let sid = steward_ipc::pipe::user_sid()?;
        let name = provider_name(&sid);
        let channel = Channel {
            guid: provider_guid(&sid).to_u128(),
            name: &name,
            channel: CHANNEL_VALUE,
            keyword: CHANNEL_KEYWORD,
        };
        let wake = Wake::new()?;
        let provider = Provider::register(&channel, unit, wake.0)?;
        let shared = Shared {
            provider,
            pending: Mutex::new(Held {
                pending: Pending::with_capacity(HOLD_MAX),
                text: Vec::with_capacity(LINE_MAX + 1),
            }),
            wake,
            finished: AtomicUsize::new(0),
        };

        thread::scope(|scope| -> io::Result<()> {
            for (stream, input) in [(Stream::Stdout, stdout), (Stream::Stderr, stderr)] {
                let shared = &shared;
                thread::Builder::new()
                    .name(stream.name().into())
                    .spawn_scoped(scope, move || {
                        pump(shared, stream, &input);
                        shared.finished.fetch_add(1, SeqCst);
                        shared.wake.set();
                    })?;
            }
            loop {
                let timeout = if shared.held().pending.is_empty() {
                    INFINITE
                } else {
                    RETRY_MS
                };
                unsafe { WaitForSingleObject(shared.wake.0, timeout) };
                let done = shared.finished.load(SeqCst) == 2;
                shared.flush();
                if done {
                    return Ok(());
                }
            }
        })?;

        // Both pipes are closed and there is nothing more to read. What is
        // still held had nobody to go to; say so where the manager can see.
        let (refused, err) = shared.provider.refused();
        if refused != 0 {
            eprintln!(
                "steward-cat: {unit}: ETW refused {refused} writes, the last with error {err}"
            );
        }
        let unwritten = shared.held().pending.unwritten();
        if unwritten != 0 {
            eprintln!(
                "steward-cat: {unit}: {unwritten} bytes of output were never written: no session \
                 enabled {name} before the unit's output closed"
            );
        }
        Ok(())
    }

    /// Reads one stream until its pipe closes, an event per line. The buffer
    /// is on this thread's stack: nothing here allocates.
    fn pump(shared: &Shared, stream: Stream, input: &Input) {
        let mut buf = [0u8; LINE_MAX];
        // The start of a line still to come, at the front.
        let mut len = 0;
        loop {
            let n = match input.read(&mut buf[len..]) {
                Ok(Some(n)) => n,
                Ok(None) => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    eprintln!("steward-cat: reading {}: {err}", stream.name());
                    break;
                }
            };
            len += n;
            // Every line of one read under one lock. A full buffer always
            // gives up at least a byte, so the next read has room.
            let used = {
                let mut held = shared.held();
                lines::split(&buf[..len], len == buf.len(), |line| {
                    shared.emit(&mut held, stream, line)
                })
            };
            buf.copy_within(used..len, 0);
            len -= used;
        }
        if let Some(line) = lines::last(&buf[..len]) {
            shared.emit(&mut shared.held(), stream, line);
        }
    }
}
