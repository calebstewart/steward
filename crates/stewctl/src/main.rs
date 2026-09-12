//! stewctl: the command line for steward. Only `verify` exists until the
//! control plane does (M2); it needs no running manager.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use steward_unit::{LoadedUnit, Severity};

#[derive(Parser)]
#[command(
    version,
    about = "Control steward, the per-user service manager for Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check unit files: every *.service in the unit directory, or the files given.
    Verify {
        /// Unit files to check instead of the unit directory.
        files: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Verify { files } => verify(files),
    }
}

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
