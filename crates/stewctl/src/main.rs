//! stewctl: the command line for steward, after `systemctl --user`.
//!
//! Everything but `verify` and `logs` asks the running manager, over its
//! pipe. `logs` asks it where the logs are and which units exist, then reads
//! the file itself -- falling back to this shell's LOCALAPPDATA when no
//! manager runs; `verify` needs no manager at all.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use steward_ipc::{Request, Response, UnitStatus};
use steward_unit::{LoadedUnit, Severity};

/// Everything stewctl prints goes through this rather than std's `println!`,
/// which panics when the reader has gone -- `stewctl logs whkd | Select-Object
/// -First 3`, or a pager that is quit. Defined here, it is the `println!` the
/// whole file uses.
macro_rules! println {
    () => {
        $crate::print_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::print_line(format_args!($($arg)*))
    };
}

fn print_line(args: std::fmt::Arguments) {
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_fmt(args).and_then(|()| out.write_all(b"\n")) {
        stdout_failed(e);
    }
}

/// The reader stopped reading: stop too, successfully, as `head` ends `cat`
/// on Unix. Any other failure to write is reported.
fn stdout_failed(e: std::io::Error) -> ! {
    // ERROR_NO_DATA ("the pipe is being closed") and ERROR_BROKEN_PIPE.
    if e.kind() == std::io::ErrorKind::BrokenPipe || matches!(e.raw_os_error(), Some(232 | 109)) {
        std::process::exit(0);
    }
    eprintln!("stewctl: cannot write the output: {e}");
    std::process::exit(1);
}

#[derive(Parser)]
#[command(
    version,
    about = "Control steward, the per-user service manager for Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List the units, their state, and their main process (the default).
    #[command(visible_aliases = ["list", "ls"])]
    ListUnits,
    /// Show the manager, or units in detail with the end of their logs.
    Status { units: Vec<String> },
    /// Start units, and what they want or require.
    Start {
        #[arg(required = true)]
        units: Vec<String>,
        /// Return at once instead of waiting for them to be up.
        #[arg(long)]
        no_block: bool,
    },
    /// Stop units; they stay stopped until started again or the next sign-in.
    Stop {
        #[arg(required = true)]
        units: Vec<String>,
    },
    /// Stop and start units; a changed unit starts with its new definition.
    Restart {
        #[arg(required = true)]
        units: Vec<String>,
        #[arg(long)]
        no_block: bool,
    },
    /// Exit 0 if every unit is active, 3 otherwise; print their states.
    IsActive {
        #[arg(required = true)]
        units: Vec<String>,
    },
    /// Read the unit files again. Removed units are stopped; changed units
    /// keep running as they are until restarted.
    #[command(visible_alias = "reload")]
    DaemonReload,
    /// Read the unit files again and make what runs match them, as sd-switch
    /// does: restart the changed, start the new, stop the removed; a unit
    /// stopped on purpose stays stopped. What an apply runs.
    Switch {
        /// Succeed, doing nothing, when no manager runs in this session:
        /// the next one to start reads the units as they are.
        #[arg(long)]
        if_running: bool,
    },
    /// Show a unit's log: its output and steward's lines about it.
    Logs {
        unit: String,
        /// How many lines from the end.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Keep printing what is appended.
        #[arg(short, long)]
        follow: bool,
    },
    /// Check unit files: every *.service in the unit directory, or the files given.
    Verify { files: Vec<PathBuf> },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command.unwrap_or(Command::ListUnits) {
        Command::ListUnits => list(),
        Command::Status { units } => status(names(units)),
        Command::Start { units, no_block } => start(names(units), no_block),
        Command::Stop { units } => simple(Request::Stop {
            units: names(units),
        }),
        Command::Restart { units, no_block } => restart(names(units), no_block),
        Command::IsActive { units } => is_active(names(units)),
        Command::DaemonReload => simple(Request::Reload { apply: false }),
        Command::Switch { if_running } => switch(if_running),
        Command::Logs {
            unit,
            lines,
            follow,
        } => logs(&name(unit), lines, follow),
        Command::Verify { files } => Ok(verify(files)),
    };
    result.unwrap_or_else(|message| {
        eprintln!("stewctl: {message}");
        ExitCode::FAILURE
    })
}

/// `whkd` means `whkd.service`.
fn name(unit: String) -> String {
    if unit.contains('.') {
        unit
    } else {
        format!("{unit}.service")
    }
}

fn names(units: Vec<String>) -> Vec<String> {
    units.into_iter().map(name).collect()
}

type Outcome = Result<ExitCode, String>;

#[cfg(windows)]
fn ask(request: Request) -> Result<Response, String> {
    let response = steward_ipc::pipe::request(&request).map_err(|e| e.to_string())?;
    match response.error {
        Some(error) => Err(error),
        None => Ok(response),
    }
}

#[cfg(not(windows))]
fn ask(_request: Request) -> Result<Response, String> {
    Err("steward runs only on Windows".into())
}

fn simple(request: Request) -> Outcome {
    for message in ask(request)?.messages {
        println!("{message}");
    }
    Ok(ExitCode::SUCCESS)
}

fn ago(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}min {}s ago", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {}min ago", s / 3600, s % 3600 / 60),
        s => format!("{}d {}h ago", s / 86_400, s % 86_400 / 3600),
    }
}

fn list() -> Outcome {
    let response = ask(Request::Status { units: Vec::new() })?;
    if response.units.is_empty() {
        let dir = response.manager.map(|m| m.unit_dir).unwrap_or_default();
        println!("no units in {dir}");
        return Ok(ExitCode::SUCCESS);
    }
    let width = response
        .units
        .iter()
        .map(|u| u.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<width$}  {:<12}  {:>8}  {:>8}  DESCRIPTION",
        "UNIT", "STATE", "PID", "RESTARTS"
    );
    for unit in &response.units {
        let state = if unit.changed {
            format!("{}*", unit.state)
        } else {
            unit.state.clone()
        };
        println!(
            "{:<width$}  {:<12}  {:>8}  {:>8}  {}",
            unit.name,
            state,
            unit.main_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            unit.restarts,
            unit.description.as_deref().unwrap_or(""),
        );
    }
    if response.units.iter().any(|u| u.changed) {
        println!("\n* changed on disk; restart it (or `stewctl switch`) to use the new definition");
    }
    Ok(ExitCode::SUCCESS)
}

fn status(units: Vec<String>) -> Outcome {
    let response = ask(Request::Status {
        units: units.clone(),
    })?;
    if units.is_empty() {
        let Some(manager) = response.manager else {
            return Err("steward sent no status".into());
        };
        let count = |state: &str| response.units.iter().filter(|u| u.state == state).count();
        println!(
            "steward {} (pid {}, session {})",
            manager.version, manager.pid, manager.session
        );
        println!(
            "   Shell: {}",
            if manager.graphical_session {
                "ready (graphical-session.target reached)"
            } else {
                "not ready yet"
            }
        );
        println!("   Units: {} in {}", response.units.len(), manager.unit_dir);
        println!(
            "          {} active, {} failed, {} restarting",
            count("active"),
            count("failed"),
            count("auto-restart")
        );
        println!("    Logs: {}", manager.log_dir);
        return Ok(ExitCode::SUCCESS);
    }
    // The manager's log directory, not this shell's idea of one.
    let log_dir = response.manager.as_ref().map(|m| PathBuf::from(&m.log_dir));
    for (i, unit) in response.units.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_unit(unit);
        println!();
        if let Some(dir) = &log_dir {
            for line in tail(&dir.join(format!("{}.log", unit.name)), 10) {
                println!("{line}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_unit(unit: &UnitStatus) {
    let mark = match unit.state.as_str() {
        "active" => "●",
        "failed" => "×",
        _ => "○",
    };
    match &unit.description {
        Some(description) => println!("{mark} {} - {description}", unit.name),
        None => println!("{mark} {}", unit.name),
    }
    println!(
        "     Loaded: {}{}",
        unit.path,
        if unit.changed {
            " (changed on disk since it started)"
        } else {
            ""
        }
    );
    let since = match (&unit.since, unit.for_secs) {
        (Some(since), Some(secs)) => format!(" since {since} ({})", ago(secs)),
        _ => String::new(),
    };
    println!("     Active: {}{since}", unit.state);
    if let Some(secs) = unit.restart_in_secs {
        println!("    Restart: in {secs:.1} s");
    }
    if let Some(pid) = unit.main_pid {
        println!("   Main PID: {pid}");
    }
    if !unit.pids.is_empty() {
        let pids: Vec<String> = unit.pids.iter().map(u32::to_string).collect();
        println!("  Processes: {}", pids.join(" "));
    }
    match &unit.last_outcome {
        Some(outcome) => println!("   Restarts: {} (last ended: it {outcome})", unit.restarts),
        None => println!("   Restarts: {}", unit.restarts),
    }
    if !unit.wanted_by.is_empty() {
        println!("  Wanted by: {}", unit.wanted_by.join(" "));
    }
}

/// Wait until none of `units` is on its way up; report where each ended.
fn wait_until_settled(units: &[String]) -> Outcome {
    let give_up = Instant::now() + Duration::from_secs(90);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let response = ask(Request::Status {
            units: units.to_vec(),
        })?;
        let settled = response
            .units
            .iter()
            .all(|u| !u.is_starting() && u.state != "stop" && !u.state.starts_with("stop-"));
        if settled || Instant::now() > give_up {
            let mut code = ExitCode::SUCCESS;
            for unit in &response.units {
                match unit.state.as_str() {
                    "active" => println!("{}: active", unit.name),
                    "inactive" if unit.last_outcome.as_deref() == Some("exited cleanly") => {
                        println!("{}: ran and exited cleanly", unit.name)
                    }
                    state => {
                        let why = unit
                            .last_outcome
                            .as_deref()
                            .map(|o| format!(": it {o}"))
                            .unwrap_or_default();
                        println!(
                            "{}: {state}{why}; see stewctl status {}",
                            unit.name, unit.name
                        );
                        code = ExitCode::FAILURE;
                    }
                }
            }
            return Ok(code);
        }
    }
}

fn start(units: Vec<String>, no_block: bool) -> Outcome {
    for message in ask(Request::Start {
        units: units.clone(),
    })?
    .messages
    {
        println!("{message}");
    }
    if no_block {
        return Ok(ExitCode::SUCCESS);
    }
    wait_until_settled(&units)
}

fn restart(units: Vec<String>, no_block: bool) -> Outcome {
    for message in ask(Request::Restart {
        units: units.clone(),
    })?
    .messages
    {
        println!("{message}");
    }
    if no_block {
        return Ok(ExitCode::SUCCESS);
    }
    wait_until_settled(&units)
}

fn is_active(units: Vec<String>) -> Outcome {
    let response = ask(Request::Status { units })?;
    let mut all = true;
    for unit in &response.units {
        println!("{}", unit.state);
        all &= unit.is_active();
    }
    Ok(if all {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

// ---- logs ------------------------------------------------------------------

/// Where the logs are and which units there are.
struct LogView {
    dir: PathBuf,
    units: BTreeSet<String>,
    from_manager: bool,
}

/// The running manager's view if there is one -- its log directory is the one
/// that counts, whatever this shell's LOCALAPPDATA says -- otherwise this
/// shell's: the logs under its LOCALAPPDATA and the units under its APPDATA.
fn log_view() -> Result<LogView, String> {
    if let Some(response) = manager_status()? {
        let manager = response.manager.ok_or("steward sent no status")?;
        return Ok(LogView {
            dir: PathBuf::from(manager.log_dir),
            units: response.units.into_iter().map(|u| u.name).collect(),
            from_manager: true,
        });
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .ok_or("steward is not running and LOCALAPPDATA is not set")?;
    let dir = PathBuf::from(local).join("steward").join("logs");
    let mut units = BTreeSet::new();
    let names = |d: &std::path::Path, suffix: &str| -> Vec<String> {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(suffix).map(str::to_owned))
            .collect()
    };
    units.extend(names(&dir, ".log"));
    if let Some(unit_dir) = steward_unit::user_unit_dir() {
        units.extend(
            names(&unit_dir, ".service")
                .into_iter()
                .map(|n| n + ".service"),
        );
    }
    Ok(LogView {
        dir,
        units,
        from_manager: false,
    })
}

/// The status of every unit, or `None` if no manager is running.
fn manager_status() -> Result<Option<Response>, String> {
    ask_if_running(Request::Status { units: Vec::new() })
}

/// The manager's answer, or `None` if no manager is running.
#[cfg(windows)]
fn ask_if_running(request: Request) -> Result<Option<Response>, String> {
    use steward_ipc::pipe::ClientError;
    match steward_ipc::pipe::request(&request) {
        Ok(Response {
            error: Some(error), ..
        }) => Err(error),
        Ok(response) => Ok(Some(response)),
        Err(ClientError::NotRunning) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(not(windows))]
fn ask_if_running(_request: Request) -> Result<Option<Response>, String> {
    Ok(None)
}

/// `switch`; with `if_running`, no manager is not an error: the next one to
/// start reads the units as they are. What an apply runs.
fn switch(if_running: bool) -> Outcome {
    let request = Request::Reload { apply: true };
    if !if_running {
        return simple(request);
    }
    match ask_if_running(request)? {
        Some(response) => {
            for message in response.messages {
                println!("{message}");
            }
        }
        None => println!(
            "steward is not running in this session; the next manager reads the units as they are"
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// Edit distance, for suggesting the unit someone meant.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous + usize::from(ca != *cb);
            previous = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(previous + 1);
        }
    }
    row[b.len()]
}

/// The candidate `wanted` is probably a misspelling or a shortening of.
fn closest<'a>(wanted: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let stem = |s: &str| s.strip_suffix(".service").unwrap_or(s).to_lowercase();
    let wanted = stem(wanted);
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let other = stem(candidate);
            let d = distance(&wanted, &other);
            let near = d <= 2.max(other.len() / 3);
            let part = !wanted.is_empty() && (other.contains(&wanted) || wanted.contains(&other));
            (near || part).then_some((d, candidate))
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, candidate)| candidate)
}

/// The last `n` lines of a file, if it can be read.
fn tail(path: &PathBuf, n: usize) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

fn logs(unit: &str, lines: usize, follow: bool) -> Outcome {
    let view = log_view()?;
    if !view.units.contains(unit) {
        let mut message = format!("no unit named {unit}");
        match closest(unit, view.units.iter().map(String::as_str)) {
            Some(near) => {
                message += &format!("; did you mean {}?", near.trim_end_matches(".service"))
            }
            None if !view.from_manager => {
                message += &format!(
                    " (steward is not running; looked in {})",
                    view.dir.display()
                )
            }
            None => {}
        }
        return Err(message);
    }
    let path = view.dir.join(format!("{unit}.log"));
    // On stderr, so that the output itself can be piped clean.
    let note = if view.from_manager {
        ""
    } else {
        " (steward is not running)"
    };
    eprintln!("-- {}{note}", path.display());
    if !path.exists() {
        if !follow {
            return Err(format!("{unit} has no log yet"));
        }
        eprintln!("-- no log yet; waiting for one");
    }
    for line in tail(&path, lines) {
        println!("{line}");
    }
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }
    let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let stdout = std::io::stdout();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let Ok(mut file) = std::fs::File::open(&path) else {
            continue;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < offset {
            // Set aside and begun again.
            offset = 0;
        }
        if len == offset {
            continue;
        }
        let mut buffer = Vec::new();
        if file.seek(SeekFrom::Start(offset)).is_ok() && file.read_to_end(&mut buffer).is_ok() {
            offset += buffer.len() as u64;
            let mut out = stdout.lock();
            // Following ends with the reader, not never.
            if let Err(e) = out
                .write_all(String::from_utf8_lossy(&buffer).as_bytes())
                .and_then(|()| out.flush())
            {
                stdout_failed(e);
            }
        }
    }
}

// ---- verify ----------------------------------------------------------------

fn verify(files: Vec<PathBuf>) -> ExitCode {
    let units: Vec<LoadedUnit> = if files.is_empty() {
        let Some(dir) = steward_unit::user_unit_dir() else {
            eprintln!("stewctl: APPDATA is not set; name the unit files to check");
            return ExitCode::from(2);
        };
        match steward_unit::load_dir(&dir) {
            Ok(units) if units.is_empty() => {
                println!("no units in {}", dir.display());
                return ExitCode::SUCCESS;
            }
            Ok(units) => units,
            Err(e) => {
                eprintln!("stewctl: cannot read {}: {e}", dir.display());
                return ExitCode::from(2);
            }
        }
    } else {
        files.into_iter().map(steward_unit::load_file).collect()
    };

    let mut errors = 0;
    let mut warnings = 0;
    for unit in &units {
        let has = |severity| {
            unit.parsed
                .diagnostics
                .iter()
                .any(|d| d.severity == severity)
        };
        if unit.parsed.diagnostics.is_empty() {
            println!("{}: ok", unit.path.display());
            continue;
        }
        println!("{}:", unit.path.display());
        for diagnostic in &unit.parsed.diagnostics {
            println!("  {diagnostic}");
        }
        if has(Severity::Error) {
            errors += 1;
        } else if has(Severity::Warning) {
            warnings += 1;
        }
    }
    println!(
        "{} unit(s): {errors} with errors, {warnings} with warnings only",
        units.len()
    );
    if errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_is_a_service() {
        assert_eq!(name("whkd".into()), "whkd.service");
        assert_eq!(name("whkd.service".into()), "whkd.service");
    }

    #[test]
    fn suggestions() {
        let units = ["pinger.service", "whkd.service", "komorebi.service"];
        assert_eq!(closest("ping.service", units), Some("pinger.service"));
        assert_eq!(
            closest("komorebbi.service", units),
            Some("komorebi.service")
        );
        assert_eq!(closest("WHKD.service", units), Some("whkd.service"));
        assert_eq!(closest("flow-launcher.service", units), None);
        assert_eq!(distance("kitten", "sitting"), 3);
    }
}
