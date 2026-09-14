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
//! action could run anything as SYSTEM. It is safe to grant because this
//! program takes no arguments and does the same thing every time.
//!
//! Three things happen in a run, in order, and each is skipped when it has
//! nothing to do:
//!
//! 1. the sessions signed in are enumerated and resolved to SIDs;
//! 2. those, plus every SID the manifest already names, are written back as
//!    the manifest, and it is imported if it changed or if a channel it
//!    names has gone missing;
//! 3. each channel's access descriptor is checked and set if it is wrong.
//!
//! Running it again with the same sessions writes nothing at all.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use steward_eventlog::{channel_access, channel_name, manifest, sids_in};

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

/// `wevtutil.exe` by its full path. Named absolutely rather than found on
/// the PATH because this runs as SYSTEM and the PATH is not ours.
fn wevtutil() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join("wevtutil.exe")
}

/// Run `wevtutil` and give back what it said, or what went wrong.
fn run(args: &[&str]) -> io::Result<String> {
    let output = Command::new(wevtutil()).args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim();
        Err(io::Error::other(format!(
            "wevtutil {} failed ({}){}{}",
            args.join(" "),
            output.status,
            if said.is_empty() { "" } else { ": " },
            said
        )))
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

/// The argument-free run: what the task does at every logon.
pub fn provision() -> io::Result<Report> {
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

    let text = manifest(&sids, &exe);
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
    let missing: Vec<&String> = after.iter().filter(|sid| !exists(sid)).collect();
    if text != before || !missing.is_empty() {
        if text != before {
            report.say(format!(
                "manifest {} {}",
                path.display(),
                if before.is_empty() {
                    "written"
                } else {
                    "updated"
                }
            ));
        }
        if !missing.is_empty() {
            report.say(format!("{} channels to re-create", missing.len()));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, &text)?;
        run(&["im", &path.to_string_lossy()])?;
        report.say("manifest imported");
    } else {
        report.say("manifest unchanged, nothing to import");
    }

    // And the access descriptors, read back rather than assumed: the import
    // sets them from the manifest, but a channel that was already there and
    // whose descriptor was changed by hand or by policy is put right here
    // without an import.
    let mut set = 0;
    for sid in &after {
        let channel = channel_name(sid);
        let wanted = channel_access(sid);
        if access(&channel)?.as_deref() != Some(wanted.as_str()) {
            run(&["sl", &channel, &format!("/ca:{wanted}")])?;
            set += 1;
        }
    }
    report.say(match set {
        0 => "access descriptors already right".to_string(),
        n => format!("{n} access descriptors set"),
    });

    report.keep();
    Ok(report)
}

/// Whether `sid`'s channel is registered on this machine.
fn exists(sid: &str) -> bool {
    run(&["gl", &channel_name(sid)]).is_ok()
}

/// A channel's access descriptor as `wevtutil gl` reports it, or `None` if
/// there is no such channel.
fn access(channel: &str) -> io::Result<Option<String>> {
    let Ok(listed) = run(&["gl", channel]) else {
        return Ok(None);
    };
    Ok(listed
        .lines()
        .find_map(|line| line.trim().strip_prefix("channelAccess:"))
        .map(|sddl| sddl.trim().to_string()))
}

/// The uninstall: every channel steward made, and everything in them.
///
/// Not the task: that is winpkgs' to prune, or the administrator's to delete
/// (`nix/winpkgs/system.nix`, and the README). Channels cannot be resources
/// the way the task is -- they appear at a logon winpkgs never sees, one per
/// account -- so removing them stays here.
pub fn uninstall() -> io::Result<Report> {
    let mut report = Report::default();
    let path = manifest_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let channels = sids_in(&text).len();
            run(&["um", &path.to_string_lossy()])?;
            std::fs::remove_file(&path)?;
            let _ = std::fs::remove_file(report_path()?);
            report.say(format!("{channels} channels removed, with their records"));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            report.say("no manifest: no channels of ours to remove");
        }
        Err(e) => return Err(e),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    /// The one line of `wevtutil gl` that matters, out of the rest of it.
    #[test]
    fn the_descriptor_is_read_off_the_listing() {
        let listing = "name: Steward/S-1-5-18\n  enabled: true\n  type: Operational\n  \
             isolation: Custom\n  channelAccess: O:BAG:SYD:(A;;0x7;;;BA)\n  logging:\n    \
             retention: false\n";
        let found = listing
            .lines()
            .find_map(|line| line.trim().strip_prefix("channelAccess:"))
            .map(str::trim);
        assert_eq!(found, Some("O:BAG:SYD:(A;;0x7;;;BA)"));
    }
}
