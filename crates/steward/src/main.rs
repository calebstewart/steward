//! steward, the manager.
//!
//!   steward              run under the SCM, as an instance of the per-user
//!                        service template (how it runs for real)
//!   steward --console    run in the foreground until Ctrl+C (development)
//!
//! Either way the manager is the same code: `manager::run` reading events from
//! a channel that the SCM's control handler or the console's Ctrl+C handler
//! feeds.

#[cfg(windows)]
mod log;
#[cfg(windows)]
mod manager;
#[cfg(windows)]
mod service;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => service::run(),
        ["--console"] => manager::run_console(),
        ["--version"] => println!("steward {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("usage: steward [--console | --version]");
            eprintln!("  (with no arguments steward expects to be started by the SCM)");
            std::process::exit(2);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("steward manages Windows services and runs only on Windows");
    std::process::exit(1);
}
