//! `steward provision-eventlog`: the Event Log channel each signed-in user's
//! units write their output to, and the Scheduled Task that keeps the set of
//! them up to date.
//!
//! Creating a channel is administrative -- a manifest imported by an
//! administrator, and an access descriptor only an administrator may set --
//! and the manager is not. Users appear after the install, too, which is the
//! other half of the problem: the one elevated step that registers the
//! service does not know what accounts will ever sign in to the machine. So
//! the install registers a task instead of a channel, and the task creates
//! the channels, as SYSTEM, at the logon of any user.
//!
//! The task takes no arguments and does the same thing every time, which is
//! what lets it be given a security descriptor that ordinary users may run
//! but not modify: a user can ask for their channel without being given
//! anything else, and cannot turn a task that runs as SYSTEM into a task
//! that runs something of theirs.
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

use crate::sys::{session, task};

/// The task's name, in the root folder. The manager will want to run it on
/// demand one day (a channel that does not exist yet is output the shim has
/// to hold), so the name is part of the interface and not decoration.
pub const TASK: &str = "steward-provision-eventlog";

/// Who may do what with the task.
///
/// Administrators and SYSTEM get all of it. Authenticated users get
/// `0x1200a9`, which is `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE` and which
/// the Task Scheduler reads as "may see it and may run it"; it is the right
/// Windows itself grants on a task meant to be runnable by whoever is signed
/// in (`\Microsoft\Windows\Defrag\ScheduledDefrag` grants Local Service
/// exactly this). What is deliberately not in it is `FILE_GENERIC_WRITE`:
/// this task runs as SYSTEM, so a user who could change its action could run
/// anything as SYSTEM. Inheritance from the root folder is left alone, as on
/// every task Windows ships, and adds nothing beyond administrators and
/// SYSTEM again.
const TASK_ACCESS: &str = "D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;0x1200a9;;;AU)";

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

/// The elevated install step: register the task, then do what it does.
///
/// The run at install is what gives whoever is signed in right now a
/// channel; without it they would wait for their next sign-in.
pub fn install() -> io::Result<Report> {
    let exe = std::env::current_exe()?;
    task::register(TASK, &task_xml(&exe.to_string_lossy()), TASK_ACCESS)?;
    let mut report = Report::default();
    report.say(format!("task {TASK} registered, running {}", exe.display()));

    let run = provision()?;
    report.0.extend(run.0);
    report.keep();
    Ok(report)
}

/// The uninstall: the task goes, and so do the channels and everything in
/// them.
pub fn uninstall() -> io::Result<Report> {
    let mut report = Report::default();
    report.say(match task::remove(TASK)? {
        true => format!("task {TASK} deleted"),
        false => format!("task {TASK} was not registered"),
    });

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

/// The task: run `exe provision-eventlog`, as SYSTEM, when anybody logs on.
///
/// The settings that are not the defaults, and why each is not:
///
/// - `MultipleInstancesPolicy` is `Queue`. The default drops a run that
///   begins while one is still going, and two people signing in at once is
///   exactly when that happens -- the second would be the one left without a
///   channel.
/// - `DisallowStartIfOnBatteries` is false. The default is true, so on a
///   laptop away from its charger the task would simply not run at logon.
/// - `AllowStartOnDemand` is true, so the task can be run by hand, and by
///   the manager when it has a reason to.
/// - `ExecutionTimeLimit` is three minutes rather than the default three
///   days: this either works in a second or is not going to.
fn task_xml(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>steward</Author>
    <Description>Creates the Windows Event Log channel that each signed-in user's steward units write their output to, one channel per user, named by SID. Takes no arguments and does the same thing every time.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>Queue</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT3M</ExecutionTimeLimit>
    <Priority>7</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
      <Arguments>provision-eventlog</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        escape(exe)
    )
}

/// The characters XML reserves, for the one value interpolated into the task:
/// the path steward was installed as.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings a logon-triggered SYSTEM task gets wrong by default, and
    /// the shape of the thing: SYSTEM, at anyone's logon, running the
    /// subcommand with nothing else on the command line.
    #[test]
    fn the_task_says_what_it_must() {
        let xml = task_xml(r"C:\Program Files\steward\steward.exe");
        assert!(xml.contains("<UserId>S-1-5-18</UserId>"));
        assert!(xml.contains("<LogonTrigger>"));
        // Any user: a LogonTrigger with no UserId of its own.
        assert!(!xml.contains("<UserId>S-1-5-21"));
        assert!(xml.contains("<MultipleInstancesPolicy>Queue</MultipleInstancesPolicy>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<AllowStartOnDemand>true</AllowStartOnDemand>"));
        assert!(xml.contains(r"<Command>C:\Program Files\steward\steward.exe</Command>"));
        assert!(xml.contains("<Arguments>provision-eventlog</Arguments>"));
    }

    /// A path with a reserved character in it still gives well-formed XML.
    #[test]
    fn the_path_is_escaped() {
        let xml = task_xml(r"C:\a & b\steward.exe");
        assert!(xml.contains(r"<Command>C:\a &amp; b\steward.exe</Command>"));
        assert!(!xml.contains("& b"));
    }

    /// Users may run the task and may not change it; if they could, a task
    /// that runs as SYSTEM would be a way to become SYSTEM.
    #[test]
    fn users_may_run_the_task_and_not_write_it() {
        // 0x1200a9 is FILE_GENERIC_READ | FILE_GENERIC_EXECUTE. FILE_GENERIC_WRITE
        // (0x120116) shares no bit with it beyond SYNCHRONIZE and READ_CONTROL.
        let users = 0x1200a9u32;
        assert_eq!(users & 0x0002, 0, "FILE_WRITE_DATA");
        assert_eq!(users & 0x0004, 0, "FILE_APPEND_DATA");
        assert_eq!(users & 0x0010, 0, "FILE_WRITE_EA");
        assert_eq!(users & 0x10000, 0, "DELETE");
        assert_eq!(users & 0x40000, 0, "WRITE_DAC");
        assert_ne!(users & 0x0001, 0, "FILE_READ_DATA");
        assert_ne!(users & 0x0020, 0, "FILE_EXECUTE");
        assert!(TASK_ACCESS.contains(&format!("(A;;0x{users:x};;;AU)")));
        assert!(TASK_ACCESS.contains("(A;;FA;;;BA)"));
        assert!(TASK_ACCESS.contains("(A;;FA;;;SY)"));
    }

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
