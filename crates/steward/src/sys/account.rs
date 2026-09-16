//! An account named by the install, as a SID: what `steward
//! provision-eventlog --account` needs a channel for.
//!
//! The sessions ([`super::session`]) are who is signed in *now*; these are
//! who the install knows the machine is for, whether they are signed in or
//! not, and whether they have ever signed in or not. Resolving one is
//! `LookupAccountName`, which asks the local SAM and then, on a domain
//! machine, the domain: it needs no privilege beyond being able to ask, so
//! unlike the session enumeration it works for an ordinary administrator as
//! well as for SYSTEM.
//!
//! Two answers are not errors here, and the caller passes both over and
//! names them: a name nothing maps (a local account the install declares and
//! has not created yet) and a name that maps to something that is not a user
//! (a group, a domain, an alias). A channel is a per-account thing; a SID
//! that is not an account's would get one nobody could ever write to.

use std::io;

use windows_sys::Win32::Security::{
    LookupAccountNameW, SidTypeAlias, SidTypeComputer, SidTypeDeletedAccount, SidTypeDomain,
    SidTypeGroup, SidTypeInvalid, SidTypeLabel, SidTypeLogonSession, SidTypeUnknown, SidTypeUser,
    SidTypeWellKnownGroup, SID_NAME_USE,
};

use super::{check, string_sid, wide};

/// The SID of the account `name`: `CALEB`, `DOMAIN\caleb`, `caleb@domain`,
/// or a display name with spaces in it, as Windows itself takes them.
///
/// Fails if the name maps to nothing -- `ERROR_NONE_MAPPED`, which is what
/// an account that has not been created yet gives -- or if what it maps to
/// is not a user.
pub fn sid_of(name: &str) -> io::Result<String> {
    let name = wide(name);
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut kind: SID_NAME_USE = SidTypeUnknown;

    // Asked twice, as every Win32 call that returns a variable-length thing
    // is: the first tells the sizes and fails, and only the second can
    // succeed. A name that maps to nothing fails the first with both sizes
    // left at zero, which is the error worth telling apart, so the first
    // call's failure is returned rather than swallowed.
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    if sid_len == 0 {
        return Err(io::Error::last_os_error());
    }

    // A SID is a structure, not bytes: `u64`s so that the buffer is aligned
    // for one however large Windows says it is.
    let mut sid = vec![0u64; (sid_len as usize).div_ceil(8).max(1)];
    // The domain is not wanted, but the call writes it and fails without
    // somewhere to write it to.
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    check(unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    })?;

    if kind != SidTypeUser {
        return Err(io::Error::other(format!(
            "names {}, not a user",
            what(kind)
        )));
    }
    string_sid(sid.as_mut_ptr().cast())
}

/// A `SID_NAME_USE` as the report should say it: what the name turned out to
/// be, when it was not a user.
///
/// A table and not a `match`, because `SID_NAME_USE`'s values are constants
/// in Windows' own casing, and a constant in a pattern with a lower-case
/// letter in it is a warning (and `-D warnings` in CI).
fn what(kind: SID_NAME_USE) -> &'static str {
    const NAMES: [(SID_NAME_USE, &str); 9] = [
        (SidTypeGroup, "a group"),
        (SidTypeDomain, "a domain"),
        (SidTypeAlias, "an alias"),
        (SidTypeWellKnownGroup, "a well-known group"),
        (SidTypeDeletedAccount, "a deleted account"),
        (SidTypeInvalid, "nothing valid"),
        (SidTypeComputer, "a computer"),
        (SidTypeLabel, "an integrity label"),
        (SidTypeLogonSession, "a logon session"),
    ];
    NAMES
        .iter()
        .find(|(value, _)| *value == kind)
        .map_or("something Windows would not name", |(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An account that certainly exists -- the one running the tests -- as a
    /// SID the manifest would accept. Needs no privilege, which is the point
    /// of this module: it passes for an ordinary user.
    #[test]
    fn the_account_this_runs_as() {
        let name = std::env::var("USERNAME").expect("%USERNAME%");
        let sid = sid_of(&name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(steward_eventlog::is_sid(&sid), "{name} gave {sid}");
        // A machine account, not one of Windows' own: those are the S-1-5-21
        // SIDs, and they are what a home configuration names.
        assert!(sid.starts_with("S-1-5-21-"), "{name} gave {sid}");
    }

    /// A name nothing maps is an error, not a panic and not a SID: this is
    /// the local account an install declares before anyone has created it,
    /// and the caller passes it over and says so.
    #[test]
    fn a_name_that_maps_to_nothing() {
        let e = sid_of("no-such-account-cf3a1d").unwrap_err();
        // ERROR_NONE_MAPPED, 1332.
        assert_eq!(e.raw_os_error(), Some(1332), "{e}");
        assert!(sid_of("").is_err());
    }

    /// A name that maps to something that is not a user is refused, and the
    /// report says what it turned out to be. A channel for one of these
    /// would be a channel no account could ever write to as itself.
    ///
    /// Windows' own accounts are among them, which is worth knowing before
    /// anyone writes `--account SYSTEM`: `LookupAccountName` calls SYSTEM
    /// and LOCAL SERVICE well-known *groups* (checked on Windows 11 26200,
    /// 2026-09-15), not users. Only the S-1-5-21 accounts of a machine or a
    /// domain -- the ones a home configuration names -- come back as users.
    #[test]
    fn a_name_that_is_not_a_users() {
        for (name, what) in [
            ("BUILTIN\\Administrators", "an alias"),
            ("Everyone", "a well-known group"),
            ("SYSTEM", "a well-known group"),
            ("LOCAL SERVICE", "a well-known group"),
        ] {
            let e = sid_of(name).unwrap_err().to_string();
            assert_eq!(e, format!("names {what}, not a user"), "{name}");
        }
    }
}
