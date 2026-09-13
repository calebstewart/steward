//! A service's environment: the user's, built fresh from the registry for
//! every start (`CreateEnvironmentBlock`), so a PATH changed after sign-in
//! reaches the services started after it, and nothing leaks in from whatever
//! started the manager. The unit's `Environment=` is laid over it.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::check;

/// The user's environment, as a fresh sign-in would have it.
pub fn user_environment() -> io::Result<Vec<(String, String)>> {
    unsafe {
        let mut token: HANDLE = null_mut();
        check(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut token,
        ))?;
        let token = OwnedHandle::from_raw_handle(token);
        let mut block: *mut c_void = null_mut();
        check(CreateEnvironmentBlock(
            &mut block,
            std::os::windows::io::AsRawHandle::as_raw_handle(&token),
            0,
        ))?;
        let vars = parse_block(block as *const u16);
        DestroyEnvironmentBlock(block);
        Ok(vars)
    }
}

/// NAME=value\0NAME=value\0\0
unsafe fn parse_block(mut p: *const u16) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    loop {
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        if len == 0 {
            return vars;
        }
        let entry = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        // A name may start with '=' (the per-drive current directories, "=C:"),
        // so the separator is the first '=' after the first character. Byte
        // index 1 is inside that character when it is multibyte, so walk the
        // chars instead of slicing.
        if let Some((split, _)) = entry.char_indices().skip(1).find(|&(_, c)| c == '=') {
            let (name, value) = entry.split_at(split);
            vars.push((name.to_owned(), value[1..].to_owned()));
        }
        p = p.add(len + 1);
    }
}

/// `base` with `overrides` laid over it; names compare case-insensitively.
pub fn merge(
    mut base: Vec<(String, String)>,
    overrides: &[(String, String)],
) -> Vec<(String, String)> {
    for (name, value) in overrides {
        base.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        base.push((name.clone(), value.clone()));
    }
    base
}

/// The UTF-16 block `CreateProcessW` takes, sorted by name as Windows keeps it.
pub fn block(vars: &[(String, String)]) -> Vec<u16> {
    let mut sorted: Vec<&(String, String)> = vars.iter().collect();
    sorted.sort_by_key(|(name, _)| name.to_uppercase());
    let mut out = Vec::new();
    for (name, value) in sorted {
        out.extend(name.encode_utf16());
        out.push(u16::from(b'='));
        out.extend(value.encode_utf16());
        out.push(0);
    }
    out.push(0);
    if vars.is_empty() {
        out.push(0);
    }
    out
}

/// A variable's value, by case-insensitive name.
pub fn get<'a>(vars: &'a [(String, String)], name: &str) -> Option<&'a str> {
    vars.iter()
        .rev()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_round_trip() {
        let vars = vec![
            ("Path".to_string(), r"C:\a;C:\b".to_string()),
            ("=C:".to_string(), r"C:\x".to_string()),
            ("A".to_string(), "1=2".to_string()),
            ("Übung".to_string(), "ü=é".to_string()),
        ];
        let block = block(&vars);
        let parsed = unsafe { parse_block(block.as_ptr()) };
        assert_eq!(
            parsed,
            [
                ("=C:".to_string(), r"C:\x".to_string()),
                ("A".to_string(), "1=2".to_string()),
                ("Path".to_string(), r"C:\a;C:\b".to_string()),
                ("Übung".to_string(), "ü=é".to_string()),
            ]
        );
    }

    #[test]
    fn overrides_replace_whatever_the_case() {
        let merged = merge(
            vec![("Path".into(), "a".into()), ("X".into(), "1".into())],
            &[("PATH".into(), "b".into())],
        );
        assert_eq!(get(&merged, "path"), Some("b"));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn the_users_environment_has_a_profile() {
        let vars = user_environment().unwrap();
        assert!(get(&vars, "USERPROFILE").is_some());
    }
}
