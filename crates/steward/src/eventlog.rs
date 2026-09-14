//! `steward provision-eventlog`: the Event Log channel each signed-in user's
//! units write their output to.
//!
//! Creating a channel is administrative -- a manifest imported by an
//! administrator, and an access descriptor only an administrator may set --
//! and the manager is not. Users appear after the install, too, which is the
//! other half of the problem: the one elevated step that registers the
//! service does not know what accounts will ever sign in to the machine. So
//! something runs as SYSTEM at the logon of any user, and creates the
//! channels then. That something is a Scheduled Task.
//!
//! **The task is not registered here.** It is declared, like the service
//! beside it, by whatever installs steward: `windows.scheduledTasks` in
//! `nix/winpkgs/system.nix`, or by hand as the README sets out. This program
//! is only ever the thing the task runs. That division is deliberate --
//! winpkgs deletes a task it declared once it leaves a configuration, where
//! a program that registered its own would leave it behind forever -- and it
//! is why nothing here talks to the Task Scheduler.
//!
//! What the task's declaration must get right, wherever it is written: run
//! as SYSTEM, trigger at the logon of *any* user, queue a second run rather
//! than dropping it (two people signing in at once would otherwise cost one
//! of them a channel), start on battery, and carry a security descriptor
//! granting ordinary users read and execute but not write. That last one
//! matters because the task runs as SYSTEM: a user who could rewrite its
//! action could run anything as SYSTEM. It is safe to grant because whoever
//! runs the task cannot change what it does. It takes one argument,
//! `--channel-size`, but as a literal in the action the administrator
//! declared: the action has no `$(Arg0)` for a caller's parameters to be
//! substituted into, so running it on demand runs exactly that.
//!
//! Three things happen in a run, in order, and each is skipped when it has
//! nothing to do:
//!
//! 1. the sessions signed in are enumerated and resolved to SIDs;
//! 2. those, plus every SID the manifest already names, are written back as
//!    the manifest, and it is imported if it changed in anything but its
//!    size, or if a channel it names has gone missing;
//! 3. each channel's access descriptor and size are checked, and set if
//!    they are wrong.
//!
//! Running it again with the same sessions writes nothing at all.
//!
//! One channel that cannot be enabled does not cost the others their run.
//! The import then installs everything and still fails, and the channel
//! cannot be listed or set afterwards; the run carries on with everyone
//! else's, names the one it could not reach, and exits non-zero at the end
//! so that the task's last result shows it.

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use steward_eventlog::{channel_access, channel_name, manifest, sids_in, size_in, ChannelSize};

use crate::sys::session;

/// Where the manifest lives: machine-wide state, beside no binaries.
///
/// Not in the install directory, which an upgrade replaces wholesale
/// (`nix/winpkgs/system.nix`) and which would take the record of every
/// channel ever created with it. The file is that record: it accumulates,
/// and it is the one thing the uninstall hands `wevtutil um`.
fn manifest_path() -> io::Result<PathBuf> {
    let data = std::env::var_os("ProgramData")
        .ok_or_else(|| io::Error::other("%ProgramData% is not set"))?;
    Ok(PathBuf::from(data).join("steward").join("channels.man"))
}

/// The last run's account of itself, beside the manifest. Rewritten each
/// run rather than appended to: a run says the same few lines as the one
/// before unless something changed, the interesting one is always the
/// latest, and a file written at every logon forever should not be able to
/// grow.
fn report_path() -> io::Result<PathBuf> {
    Ok(manifest_path()?.with_file_name("provision-eventlog.log"))
}

/// `%SystemRoot%`, where `wevtutil` and every channel's file live.
fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .unwrap_or_else(|| r"C:\Windows".into())
        .into()
}

/// `wevtutil.exe` by its full path. Named absolutely rather than found on
/// the PATH because this runs as SYSTEM and the PATH is not ours.
fn wevtutil() -> PathBuf {
    system_root().join("System32").join("wevtutil.exe")
}

/// `ERROR_WMI_INSTANCE_NOT_FOUND`: what `wevtutil im` exits with when it
/// installed every publisher and channel but could not enable one of them,
/// and what `wevtutil gl` and `sl` then exit with for that channel.
///
/// Seen for one channel (2026-09-14, #33) after an hour of load tests on
/// it had filled its trace session's backing file again and again, and it
/// outlasted the channel: removed and imported again, the channel under the
/// same name still could not be enabled, while channels for other SIDs in
/// the same import were fine. Not reproduced on purpose -- a second of
/// three million events, `um` in the middle of one, `um` and `im` with the
/// provider still registered all leave a channel that imports cleanly --
/// and the session cannot be stopped from outside to force it, since the
/// Event Log's own sessions refuse even an administrator's `logman stop`.
const NOT_ENABLED: i32 = 4201;

/// `wevtutil` ran and failed. Its exit code is a Win32 error code, kept so
/// that one failure can be told from another; see [`exit_code`].
#[derive(Debug)]
struct Failed {
    code: Option<i32>,
    message: String,
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failed {}

/// The exit code of the `wevtutil` behind an error from [`run`], if it ran.
fn exit_code(e: &io::Error) -> Option<i32> {
    e.get_ref()?.downcast_ref::<Failed>()?.code
}

/// Run `wevtutil` and give back what it said, or what went wrong.
fn run(args: &[&str]) -> io::Result<String> {
    let output = Command::new(wevtutil()).args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        // On one line: it goes in the report, a line to a thing done, and
        // `wevtutil` says most things over two.
        let said = String::from_utf8_lossy(&output.stderr);
        let said: Vec<&str> = said
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        Err(io::Error::other(Failed {
            code: output.status.code(),
            message: format!(
                "wevtutil {} failed ({}){}{}",
                args.join(" "),
                output.status,
                if said.is_empty() { "" } else { ": " },
                said.join(" ")
            ),
        }))
    }
}

/// What a run did, in the order it did it.
#[derive(Default)]
pub struct Report(Vec<String>);

impl Report {
    fn say(&mut self, line: impl Into<String>) {
        let line = line.into();
        // `writeln!` and not `println!`: the Task Scheduler starts a process
        // with no console and no redirection, so `GetStdHandle` gives it
        // nothing, and `println!` *panics* when the write fails. The whole
        // job would then be a panic at the first line it tried to say.
        let _ = writeln!(io::stdout(), "{line}");
        self.0.push(line);
    }

    /// Leave the account of the run beside the manifest. Best effort: a run
    /// that provisioned the channels and could not say so still provisioned
    /// them.
    fn keep(&self) {
        if let Ok(path) = report_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(
                path,
                format!("{}\n{}\n", crate::log::timestamp(), self.0.join("\n")),
            );
        }
    }
}

/// What the task does at every logon: a channel of `size` for everyone who
/// is signed in or ever has been.
pub fn provision(size: ChannelSize) -> io::Result<Report> {
    let mut report = Report::default();
    let path = manifest_path()?;
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy();

    // Who is signed in. A session that signs out while this runs is passed
    // over and named; it is not a failure, and the next logon will bring it
    // back with a channel of its own.
    let found = session::signed_in()?;
    for note in &found.passed_over {
        report.say(format!("no user to name in {note}"));
    }

    // Plus everyone who has had a channel before, so that signing out never
    // takes a channel -- or its history -- away.
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    let mut sids = sids_in(&before);
    let known = sids.len();
    sids.extend(found.sids.iter().cloned());

    let text = manifest(&sids, &exe, size);
    let after = sids_in(&text);
    report.say(format!(
        "{} signed in, {known} known already, {} channels",
        found.sids.len(),
        after.len()
    ));

    // Nobody to give a channel to and none ever given: importing a manifest
    // that declares nothing would register nothing and leave a file saying
    // so. This is what a run by hand without `SeTcbPrivilege` looks like --
    // every session passed over -- and it should change nothing rather than
    // write an empty record over a machine's history.
    if after.is_empty() {
        report.say("no channels to provision");
        report.keep();
        return Ok(report);
    }

    // Import only when there is something to import: when the manifest
    // changed, or when a channel it names has gone from the machine. An
    // import that only adds a provider leaves the others' registrations
    // exactly as they were, and an import that would change nothing does not
    // happen at all.
    //
    // Nor does one that would change only the size. If the file on disk is
    // what this run would write at the size the file already has, the size
    // is all that differs, and it is set on each channel below as the access
    // descriptor is: re-importing every channel to change one number would
    // be a heavier way of doing the same thing. The manifest is still
    // written, because an import sets every channel it names to the size it
    // says, existing channels included (seen, 2026-09-14): the next import --
    // the next new user -- must not put the old size back.
    let was = size_in(&before).unwrap_or(size);
    let changed = manifest(&sids, &exe, was) != before;
    let missing: Vec<&String> = after.iter().filter(|sid| !exists(sid)).collect();
    let mut not_imported = None;
    if text != before {
        report.say(format!(
            "manifest {} {}",
            path.display(),
            if before.is_empty() {
                "written".to_string()
            } else if changed {
                "updated".to_string()
            } else {
                format!("updated in its size alone, {was} to {size}")
            }
        ));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, &text)?;
    }
    if changed || !missing.is_empty() {
        if !missing.is_empty() {
            report.say(format!("{} channels to re-create", missing.len()));
        }
        match run(&["im", &path.to_string_lossy()]) {
            Ok(_) => report.say("manifest imported"),
            // Everything is installed, and some channel is not enabled: the
            // pass below finds which, and does everyone else's regardless.
            // Returning here would leave every channel's descriptor unchecked
            // for the sake of one, at every logon for as long as it lasts.
            Err(e) if exit_code(&e) == Some(NOT_ENABLED) => {
                report.say("manifest imported, but not every channel could be enabled");
                not_imported = Some(e);
            }
            Err(e) => return Err(e),
        }
    } else {
        report.say("nothing to import");
    }

    // And the access descriptors and sizes, read back rather than assumed:
    // an import sets both from the manifest, but a channel that was already
    // there -- whose descriptor or size was changed by hand or by policy, or
    // whose size changed in the manifest alone, above -- is put right here
    // without one. So a size set by hand with `wevtutil sl /ms:` lasts until
    // the next run, and the size to make last is the one the task is given.
    //
    // A size smaller than a channel's file has already grown to is set all
    // the same (seen, 2026-09-14, #34): `sl` succeeds and `gl` reads the new
    // size back at once, but the file keeps its size and its records, and
    // wraps at the size it had reached until the channel is cleared. So the
    // size reads as right from the next run on, while the file is larger.
    //
    // A channel that cannot be listed or set is named and passed over, and
    // the rest are still done.
    let mut access_set = 0;
    let mut size_set = 0;
    let mut passed_over = Vec::new();
    for sid in &after {
        let channel = channel_name(sid);
        let access = channel_access(sid);
        let access_arg = format!("/ca:{access}");
        let size_arg = format!("/ms:{}", size.bytes());
        let done = Listing::of(&channel).and_then(|listed| {
            let access_wrong = listed.access.as_deref() != Some(access.as_str());
            let size_wrong = listed.size != Some(size.bytes());
            let mut args = vec!["sl", channel.as_str()];
            if access_wrong {
                args.push(&access_arg);
            }
            if size_wrong {
                args.push(&size_arg);
            }
            if args.len() > 2 {
                run(&args)?;
            }
            access_set += usize::from(access_wrong);
            size_set += usize::from(size_wrong);
            Ok(())
        });
        if let Err(e) = done {
            report.say(format!("{channel} passed over: {e}"));
            passed_over.push(channel);
        }
    }
    report.say(match access_set {
        0 => "access descriptors already right".to_string(),
        n => format!("{n} access descriptors set"),
    });
    report.say(match size_set {
        0 => format!("sizes already {size}"),
        n => format!("{n} channels resized to {size}"),
    });

    report.keep();
    if !passed_over.is_empty() {
        return Err(io::Error::other(format!(
            "{} of {} channels not provisioned: {}",
            passed_over.len(),
            after.len(),
            passed_over.join(", ")
        )));
    }
    match not_imported {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

/// Whether `sid`'s channel is registered on this machine.
fn exists(sid: &str) -> bool {
    run(&["gl", &channel_name(sid)]).is_ok()
}

/// What `wevtutil gl` says of a channel, as much of it as a run checks.
/// Each is `None` when the listing does not say.
#[derive(Default, Debug, PartialEq)]
struct Listing {
    /// `channelAccess`, the channel's SDDL.
    access: Option<String>,
    /// `maxSize`, in bytes.
    size: Option<u64>,
    /// `logFileName`, the channel's `.evtx`, unexpanded.
    file: Option<String>,
}

impl Listing {
    /// The channel's listing, or why there is none: no such channel, or one
    /// whose session is broken ([`NOT_ENABLED`]).
    fn of(channel: &str) -> io::Result<Listing> {
        run(&["gl", channel]).map(|listed| Listing::read(&listed))
    }

    /// The lines that matter, out of the rest of it.
    fn read(listed: &str) -> Listing {
        let field = |name: &str| {
            listed
                .lines()
                .find_map(|line| line.trim().strip_prefix(name))
                .map(str::trim)
        };
        Listing {
            access: field("channelAccess:").map(str::to_string),
            size: field("maxSize:").and_then(|bytes| bytes.parse().ok()),
            file: field("logFileName:")
                .filter(|file| !file.is_empty())
                .map(str::to_string),
        }
    }
}

/// The `.evtx` a channel keeps its records in.
///
/// As `wevtutil gl` says, when it can. When it cannot -- a channel whose
/// session is broken fails to list at all ([`NOT_ENABLED`]) -- the path is
/// the one the Event Log gives every channel whose manifest names none,
/// which steward's never do: the channel's name, its `/` written `%4`, in
/// `%SystemRoot%\System32\winevt\Logs`.
fn log_file(channel: &str) -> PathBuf {
    match Listing::of(channel).ok().and_then(|listed| listed.file) {
        Some(file) => PathBuf::from(expand(&file, |name| std::env::var(name).ok())),
        None => system_root()
            .join(r"System32\winevt\Logs")
            .join(format!("{}.evtx", channel.replace('/', "%4"))),
    }
}

/// `%NAME%` replaced by the variable `var` gives for `NAME`, as
/// `ExpandEnvironmentStrings` does it. A `%` that does not begin one is left
/// as it is, which matters here: a channel's file is named with a `%4` for
/// the `/` in the channel's name, so `logFileName` is
/// `%SystemRoot%\System32\Winevt\Logs\Steward%4<SID>.evtx`.
fn expand(text: &str, var: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let named = after
            .find('%')
            .and_then(|end| Some((end, var(&after[..end])?)));
        match named {
            Some((end, value)) => {
                out.push_str(&value);
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Delete a channel's file, giving the Event Log service a moment if it
/// still has it open. It holds the file for as long as the channel exists,
/// and in every test so far (2026-09-14) let go of it by the time `wevtutil
/// um` returned, flooded or not; five seconds of retrying is cheap beside a
/// file left behind if one day it does not. `Ok(false)` if there was no file
/// to delete.
fn delete(file: &Path) -> io::Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_file(file) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// The uninstall: every channel steward made, and everything in them.
///
/// `wevtutil um` removes a channel and leaves its `.evtx` where it was
/// (seen 2026-09-14, #33), so the files are deleted here, after it. They
/// are found before it, while each channel can still say where its file is.
///
/// Not the task: that is winpkgs' to prune, or the administrator's to delete
/// (`nix/winpkgs/system.nix`, and the README). Channels cannot be resources
/// the way the task is -- they appear at a logon winpkgs never sees, one per
/// account -- so removing them stays here.
pub fn uninstall() -> io::Result<Report> {
    let mut report = Report::default();
    let path = manifest_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            report.say("no manifest: no channels of ours to remove");
            return Ok(report);
        }
        Err(e) => return Err(e),
    };
    let sids = sids_in(&text);
    let files: Vec<PathBuf> = sids
        .iter()
        .map(|sid| log_file(&channel_name(sid)))
        .collect();

    run(&["um", &path.to_string_lossy()])?;
    std::fs::remove_file(&path)?;
    let _ = std::fs::remove_file(report_path()?);
    report.say(format!("{} channels removed", sids.len()));

    let mut deleted = 0;
    let mut left = 0;
    for file in &files {
        match delete(file) {
            Ok(true) => deleted += 1,
            Ok(false) => {}
            Err(e) => {
                report.say(format!("{} left behind: {e}", file.display()));
                left += 1;
            }
        }
    }
    report.say(format!(
        "{deleted} of their .evtx files deleted, records and all"
    ));
    if left > 0 {
        return Err(io::Error::other(format!(
            "{left} .evtx files left behind; delete them by hand"
        )));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{expand, Listing};

    /// The descriptor, the size and the file, off a listing laid out as
    /// `wevtutil gl` lays one out (the Application channel's, 2026-09-14,
    /// with the descriptor shortened).
    #[test]
    fn the_descriptor_size_and_file_are_read_off_the_listing() {
        let listing = "name: Steward/S-1-5-18\r\nenabled: true\r\ntype: Operational\r\n\
             owningPublisher: \r\nisolation: Custom\r\n\
             channelAccess: O:BAG:SYD:(A;;0x7;;;BA)\r\nlogging:\r\n  \
             logFileName: %SystemRoot%\\System32\\Winevt\\Logs\\Steward%4S-1-5-18.evtx\r\n  \
             retention: false\r\n  autoBackup: false\r\n  maxSize: 67108864\r\n\
             publishing:\r\n  fileMax: 1\r\n";
        assert_eq!(
            Listing::read(listing),
            Listing {
                access: Some("O:BAG:SYD:(A;;0x7;;;BA)".to_string()),
                size: Some(64 << 20),
                file: Some(r"%SystemRoot%\System32\Winevt\Logs\Steward%4S-1-5-18.evtx".to_string()),
            }
        );
    }

    /// A listing missing any of them reads as wrong, and so is set; a file
    /// it does not name is the default one.
    #[test]
    fn what_is_not_there_is_none() {
        assert_eq!(Listing::read(""), Listing::default());
        assert_eq!(Listing::read("logging:\n  maxSize: lots\n").size, None);
        assert_eq!(Listing::read("logging:\n  logFileName: \n").file, None);
    }

    /// `%SystemRoot%` is expanded, and the `%4` that stands for the channel
    /// name's `/` is not mistaken for the start of another.
    #[test]
    fn the_file_name_is_expanded() {
        let var = |name: &str| (name == "SystemRoot").then(|| r"C:\Windows".to_string());
        assert_eq!(
            expand(
                r"%SystemRoot%\System32\Winevt\Logs\Steward%4S-1-5-18.evtx",
                var
            ),
            r"C:\Windows\System32\Winevt\Logs\Steward%4S-1-5-18.evtx"
        );
        assert_eq!(expand("a%4b%c%SystemRoot%", var), r"a%4b%cC:\Windows");
        assert_eq!(expand("%unset%%", var), "%unset%%");
        assert_eq!(expand("", var), "");
    }
}
