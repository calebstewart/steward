//! What a changed unit file asks of a unit that runs: nothing, a reload or a
//! restart -- decided on the file as written, key by key, as NixOS's
//! `switch-to-configuration` does in `compare_units` (and home-manager's
//! `sd-switch` after it), not on what steward makes of the keys. The keys the
//! parser skips count too: `X-Restart-Triggers=` is there to be compared, and
//! so is any other `X-` key.
//!
//! - A difference only in `[Unit] X-Reload-Triggers=` or `[Service]
//!   ExecReload=` is a reload. Removing either is nothing: the process is
//!   untouched, and there is nothing new to reload it for, or no longer
//!   anything to reload it with.
//! - A difference only in a `[Unit]` key that does not reach the running
//!   process ([`HARMLESS`]) is nothing.
//! - Any other difference, another `X-` key included, is a restart.
//!
//! The order of different keys does not matter; the order of one key's
//! values does, as they accumulate in that order.

use std::collections::BTreeMap;

use crate::syntax::UnitFile;

/// `[Unit]` keys a change to which leaves the unit as it runs: NixOS's list,
/// `X-Reload-Triggers=` aside, which asks for a reload.
pub const HARMLESS: [&str; 12] = [
    "Description",
    "Documentation",
    "OnFailure",
    "OnSuccess",
    "OnFailureJobMode",
    "IgnoreOnIsolate",
    "StopWhenUnneeded",
    "RefuseManualStart",
    "RefuseManualStop",
    "AllowIsolate",
    "CollectMode",
    "SourcePath",
];

/// A unit file's entries: each section's keys, each key's values in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entries(BTreeMap<String, BTreeMap<String, Vec<String>>>);

impl Entries {
    pub fn of(file: &UnitFile) -> Entries {
        let mut entries = Entries::default();
        for section in &file.sections {
            let keys = entries.0.entry(section.name.clone()).or_default();
            for entry in &section.entries {
                keys.entry(entry.key.clone())
                    .or_default()
                    .push(entry.value.clone());
            }
        }
        entries
    }

    /// A key's last value read as a systemd boolean; `None` if it is not
    /// there or is not a boolean.
    pub fn flag(&self, section: &str, key: &str) -> Option<bool> {
        let value = self.0.get(section)?.get(key)?.last()?;
        match value.to_ascii_lowercase().as_str() {
            "1" | "yes" | "y" | "true" | "t" | "on" => Some(true),
            "0" | "no" | "n" | "false" | "f" | "off" => Some(false),
            _ => None,
        }
    }
}

/// What a new unit file asks of the unit, were it running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// Nothing that reaches the running process changed.
    Nothing,
    /// Only what a reload runs, or what it is triggered by.
    Reload,
    Restart,
}

/// What going from the `old` file to the `new` one asks of the unit.
pub fn compare(old: &Entries, new: &Entries) -> Change {
    let empty = BTreeMap::new();
    let mut change = Change::Nothing;
    let sections = old
        .0
        .keys()
        .chain(new.0.keys().filter(|s| !old.0.contains_key(*s)));
    for section in sections {
        let before = old.0.get(section).unwrap_or(&empty);
        let after = new.0.get(section).unwrap_or(&empty);
        let keys = before
            .keys()
            .chain(after.keys().filter(|k| !before.contains_key(*k)));
        for key in keys {
            let (was, is) = (before.get(key), after.get(key));
            if was == is {
                continue;
            }
            let this = match (section.as_str(), key.as_str()) {
                ("Unit", "X-Reload-Triggers") if is.is_none() => Change::Nothing,
                ("Unit", "X-Reload-Triggers") => Change::Reload,
                ("Unit", key) if HARMLESS.contains(&key) => Change::Nothing,
                ("Service", "ExecReload") if is.is_none() => Change::Nothing,
                ("Service", "ExecReload") => Change::Reload,
                _ => return Change::Restart,
            };
            change = change.max(this);
        }
    }
    change
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax;

    fn entries(text: &str) -> Entries {
        Entries::of(&syntax::parse(text).unwrap())
    }

    fn change(old: &str, new: &str) -> Change {
        compare(&entries(old), &entries(new))
    }

    const BASE: &str = "[Unit]\nDescription=Hotkeys\n[Service]\nExecStart=whkd.exe\n";

    #[test]
    fn the_same_file_is_nothing_whatever_its_layout() {
        assert_eq!(change(BASE, BASE), Change::Nothing);
        assert_eq!(
            change(
                "[Unit]\nA=1\nB=2\n[Service]\nExecStart=x\n",
                "# a comment\n[Service]\nExecStart=x\n\n[Unit]\nB=2\nA=1\n"
            ),
            Change::Nothing
        );
    }

    /// home-manager's restart trigger is a key the parser skips; it is what
    /// a changed dependency looks like, and it restarts the unit.
    #[test]
    fn a_restart_trigger_or_any_other_x_key_restarts() {
        let with =
            |section: &str, key: &str, value: &str| format!("{BASE}[{section}]\n{key}={value}\n");
        let old = with("Unit", "X-Restart-Triggers", "aaa");
        assert_eq!(
            change(&old, &with("Unit", "X-Restart-Triggers", "bbb")),
            Change::Restart
        );
        assert_eq!(change(BASE, &old), Change::Restart);
        assert_eq!(change(&old, BASE), Change::Restart);
        assert_eq!(
            change(BASE, &with("Service", "X-Mine", "1")),
            Change::Restart
        );
        assert_eq!(change(BASE, &with("X-Custom", "Key", "1")), Change::Restart);
    }

    #[test]
    fn a_reload_trigger_or_exec_reload_reloads() {
        let trigger = |value: &str| format!("{BASE}[Unit]\nX-Reload-Triggers={value}\n");
        assert_eq!(change(BASE, &trigger("aaa")), Change::Reload);
        assert_eq!(change(&trigger("aaa"), &trigger("bbb")), Change::Reload);
        let reload = |line: &str| format!("{BASE}ExecReload={line}\n");
        assert_eq!(change(BASE, &reload("whkd.exe --reload")), Change::Reload);
        assert_eq!(
            change(&reload("whkd.exe --reload"), &reload("whkd.exe -r")),
            Change::Reload
        );
        // Both at once is still a reload.
        assert_eq!(
            change(
                &trigger("aaa"),
                &format!("{BASE}ExecReload=r\n[Unit]\nX-Reload-Triggers=bbb\n")
            ),
            Change::Reload
        );
    }

    #[test]
    fn dropping_a_reload_trigger_or_exec_reload_is_nothing() {
        assert_eq!(
            change(&format!("{BASE}ExecReload=r.exe\n"), BASE),
            Change::Nothing
        );
        // A trigger that goes away changes nothing the process uses.
        assert_eq!(
            change(&format!("{BASE}[Unit]\nX-Reload-Triggers=aaa\n"), BASE),
            Change::Nothing
        );
    }

    #[test]
    fn a_description_or_documentation_is_nothing() {
        assert_eq!(
            change(
                BASE,
                "[Unit]\nDescription=Hotkey daemon\n[Service]\nExecStart=whkd.exe\n"
            ),
            Change::Nothing
        );
        assert_eq!(
            change(BASE, "[Service]\nExecStart=whkd.exe\n"),
            Change::Nothing
        );
        assert_eq!(
            change(
                BASE,
                &format!("{BASE}[Unit]\nDocumentation=https://x\nRefuseManualStop=yes\n")
            ),
            Change::Nothing
        );
    }

    #[test]
    fn anything_else_restarts_and_outweighs_a_reload() {
        assert_eq!(
            change(
                BASE,
                "[Unit]\nDescription=Hotkeys\n[Service]\nExecStart=whkd.exe -v\n"
            ),
            Change::Restart
        );
        assert_eq!(
            change(BASE, &format!("{BASE}ExecReload=r\nRestart=always\n")),
            Change::Restart
        );
        assert_eq!(
            change(BASE, &format!("{BASE}[Install]\nWantedBy=default.target\n")),
            Change::Restart
        );
        // A key the parser warns about and ignores still restarts.
        assert_eq!(change(BASE, &format!("{BASE}Nice=5\n")), Change::Restart);
        // So does a Description= somewhere other than [Unit].
        assert_eq!(
            change(BASE, &format!("{BASE}Description=x\n")),
            Change::Restart
        );
    }

    #[test]
    fn a_list_s_order_is_a_change() {
        let env = |a: &str, b: &str| format!("{BASE}Environment={a}\nEnvironment={b}\n");
        assert_eq!(
            change(&env("A=1", "B=2"), &env("B=2", "A=1")),
            Change::Restart
        );
    }

    #[test]
    fn flags_are_systemd_booleans_and_the_last_one_counts() {
        let e = entries("[Service]\nX-ReloadIfChanged=no\nX-ReloadIfChanged=yes\nX-RestartIfChanged=0\nX-Odd=maybe\n");
        assert_eq!(e.flag("Service", "X-ReloadIfChanged"), Some(true));
        assert_eq!(e.flag("Service", "X-RestartIfChanged"), Some(false));
        assert_eq!(e.flag("Service", "X-Odd"), None);
        assert_eq!(e.flag("Service", "X-Missing"), None);
        assert_eq!(e.flag("Unit", "X-ReloadIfChanged"), None);
    }
}
