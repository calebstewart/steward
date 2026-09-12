//! stewctl: the command line for steward, after `systemctl --user`.
//!
//! Everything but `verify` and `logs` asks the running manager, over its
//! pipe. `logs` reads the unit's log file directly, so it works with the
//! manager down; `verify` needs no manager at all.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use steward_ipc::{Request, Response, UnitStatus};
use steward_unit::{LoadedUnit, Severity};

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
    /// Read the unit files again and make what runs match them: restart the
    /// changed, start the wanted, stop the removed. What an apply runs.
    Switch,
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
        Command::Switch => simple(Request::Reload { apply: true }),
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
        println!("steward {} (pid {})", manager.version, manager.pid);
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
    for (i, unit) in response.units.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_unit(unit);
        println!();
        for line in tail(&log_path(&unit.name)?, 10) {
            println!("{line}");
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

fn log_path(unit: &str) -> Result<PathBuf, String> {
    let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local)
        .join("steward")
        .join("logs")
        .join(format!("{unit}.log")))
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
    let path = log_path(unit)?;
    if !path.exists() && !follow {
        return Err(format!("no log for {unit} yet ({})", path.display()));
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
            let _ = out.write_all(String::from_utf8_lossy(&buffer).as_bytes());
            let _ = out.flush();
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
