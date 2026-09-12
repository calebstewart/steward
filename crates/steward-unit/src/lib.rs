//! Unit files in systemd's syntax, read the way steward reads them on Windows.
//!
//! [`syntax`] is the file format alone; [`parse_service`] gives the keys their
//! meaning and reports everything it could not use. [`load_dir`] reads a unit
//! directory.

mod service;
pub mod syntax;
pub mod time;

use std::path::{Path, PathBuf};

pub use service::{
    parse_service, Command, Diagnostic, KillMode, Parsed, Restart, Service, ServiceType, Severity,
};

/// Where a user's units live: `%APPDATA%\steward\units` -- the XDG config
/// home, as winpkgs lays it out on Windows.
pub fn user_unit_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("steward").join("units"))
}

/// One unit file from a directory, read and parsed.
#[derive(Debug, Clone)]
pub struct LoadedUnit {
    pub path: PathBuf,
    pub parsed: Parsed,
}

/// Every `*.service` file in `dir`, sorted by name. A missing directory is an
/// empty one; a file that cannot be read is an error diagnostic on that unit.
pub fn load_dir(dir: &Path) -> std::io::Result<Vec<LoadedUnit>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "service"))
        .collect();
    paths.sort();
    Ok(paths.into_iter().map(load_file).collect())
}

/// One unit file, named after its file name.
pub fn load_file(path: PathBuf) -> LoadedUnit {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parsed = match std::fs::read_to_string(&path) {
        Ok(text) => parse_service(&name, &text),
        Err(e) => Parsed {
            service: None,
            diagnostics: vec![Diagnostic {
                line: 0,
                severity: Severity::Error,
                message: format!("cannot read: {e}"),
            }],
        },
    };
    LoadedUnit { path, parsed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_examples_load_cleanly() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let units = load_dir(&dir).unwrap();
        assert!(units.len() >= 4, "found {} examples", units.len());
        for unit in units {
            assert!(
                unit.parsed.diagnostics.is_empty(),
                "{}: {:?}",
                unit.path.display(),
                unit.parsed.diagnostics
            );
        }
    }
}
