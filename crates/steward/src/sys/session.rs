//! Who is signed in, as SIDs: what the Event Log provisioning needs a
//! channel for.
//!
//! `WTSEnumerateSessions` lists every session on the machine, the ones with
//! nobody in them included -- session 0 where the services live, the
//! sign-in screen, and the listener a Remote Desktop server keeps. A session
//! becomes a SID by way of its user's token, which is exact and needs
//! nothing resolved by name: [`WTSQueryUserToken`] wants `SeTcbPrivilege`,
//! which SYSTEM holds and an administrator does not, and the provisioning
//! runs as SYSTEM for this among other reasons.
//!
//! Nothing here fails because one session did. Sessions are enumerated and
//! then asked about one at a time, and a user can sign out in between -- a
//! race with no upper bound, since the list is a snapshot -- so a session
//! that has gone is passed over and named, not raised. The same goes for one
//! that never had a user.

use std::io;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, PSID, TOKEN_USER};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSActive, WTSDisconnected, WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken,
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
};

use super::{check, owned};

/// What an enumeration found.
pub struct SignedIn {
    /// The SID of every session that resolved to a user, in session order
    /// and with duplicates left in: one user may hold more than one session.
    pub sids: Vec<String>,
    /// A line for each session that did not resolve, and why. Ordinary, not
    /// alarming: the sign-in screen has no user yet, and a session that
    /// signed out while this ran has none any more.
    pub passed_over: Vec<String>,
}

/// The SIDs of the users signed in to this machine now.
///
/// Fails only if the machine cannot be asked at all. Per-session failures
/// land in [`SignedIn::passed_over`].
pub fn signed_in() -> io::Result<SignedIn> {
    let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    check(unsafe {
        WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count)
    })?;
    // Copied out before the list is freed; the strings in it are not used.
    let sessions: Vec<(u32, i32)> = unsafe { std::slice::from_raw_parts(info, count as usize) }
        .iter()
        .map(|session| (session.SessionId, session.State))
        .collect();
    unsafe { WTSFreeMemory(info.cast()) };

    let mut found = SignedIn {
        sids: Vec::new(),
        passed_over: Vec::new(),
    };
    for (id, state) in sessions {
        // Session 0 is the services'; it has a token (SYSTEM's) and no user.
        // Of the rest, only a session someone is signed in to has one:
        // active, or disconnected, which a Remote Desktop session that was
        // closed rather than signed out of still is -- that user is signed
        // in and wants their channel. Every other state (the listener, the
        // sign-in screen, one shutting down) has nobody to name.
        if id == 0 || (state != WTSActive && state != WTSDisconnected) {
            continue;
        }
        match sid_of(id) {
            Ok(sid) => found.sids.push(sid),
            Err(e) => found.passed_over.push(format!("session {id}: {e}")),
        }
    }
    Ok(found)
}

/// The SID of whoever is signed in to `session`.
fn sid_of(session: u32) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    check(unsafe { WTSQueryUserToken(session, &mut token) })?;
    let token = unsafe { owned(token)? };

    // TOKEN_USER is a header and a pointer into the same buffer, so the
    // buffer has to be asked for by size and kept alive under the reference.
    // As `u64`s: a `Vec<u8>` is aligned for bytes, and this holds a pointer.
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    })?;
    let user: &TOKEN_USER = unsafe { &*buffer.as_ptr().cast() };
    string_sid(user.User.Sid)
}

/// A SID as `S-1-5-21-...`, the only form the manifest and the access
/// descriptors are written in.
fn string_sid(sid: PSID) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    check(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    let length = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}
