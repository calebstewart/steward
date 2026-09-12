//! The wall clock as the user reads it -- in their time zone -- and the two
//! moments timers count from besides their own: boot and sign-in.

use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use steward_unit::calendar::{Civil, Utc, Zone};
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfo, WTSINFOW, WTS_CURRENT_SERVER_HANDLE,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Time::{
    FileTimeToSystemTime, GetDynamicTimeZoneInformation, SystemTimeToFileTime,
    SystemTimeToTzSpecificLocalTimeEx, TzSpecificLocalTimeToSystemTimeEx,
    DYNAMIC_TIME_ZONE_INFORMATION, TIME_ZONE_ID_INVALID,
};

/// 1970-01-01 as a FILETIME: 100 ns intervals since 1601.
const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

fn to_filetime(t: SystemTime) -> Option<FILETIME> {
    let ticks = t.duration_since(UNIX_EPOCH).ok()?.as_nanos() / 100;
    let ticks = u64::try_from(ticks)
        .ok()?
        .checked_add(UNIX_EPOCH_FILETIME)?;
    Some(FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    })
}

fn from_filetime(ticks: u64) -> Option<SystemTime> {
    let since_epoch = ticks.checked_sub(UNIX_EPOCH_FILETIME)?;
    UNIX_EPOCH.checked_add(Duration::from_nanos(since_epoch.checked_mul(100)?))
}

/// The time zone the user has chosen, as it is when this is made: a change
/// reaches the next one.
pub struct Local(Option<DYNAMIC_TIME_ZONE_INFORMATION>);

impl Local {
    pub fn current() -> Local {
        let mut zone: DYNAMIC_TIME_ZONE_INFORMATION = unsafe { zeroed() };
        let ok = unsafe { GetDynamicTimeZoneInformation(&mut zone) } != TIME_ZONE_ID_INVALID;
        Local(ok.then_some(zone))
    }
}

impl Zone for Local {
    fn civil(&self, t: SystemTime) -> Civil {
        let local = (|| {
            let zone = self.0.as_ref()?;
            let file = to_filetime(t)?;
            let mut utc = SYSTEMTIME::default();
            let mut local = SYSTEMTIME::default();
            unsafe {
                (FileTimeToSystemTime(&file, &mut utc) != 0
                    && SystemTimeToTzSpecificLocalTimeEx(zone, &utc, &mut local) != 0)
                    .then_some(local)
            }
        })();
        // No zone to be had: UTC is at least a time.
        local.map_or_else(
            || Utc.civil(t),
            |s| Civil {
                year: i32::from(s.wYear),
                month: u32::from(s.wMonth),
                day: u32::from(s.wDay),
                hour: u32::from(s.wHour),
                minute: u32::from(s.wMinute),
                second: u32::from(s.wSecond),
            },
        )
    }

    fn moment(&self, c: Civil) -> Option<SystemTime> {
        let Some(zone) = self.0.as_ref() else {
            return Utc.moment(c);
        };
        let local = SYSTEMTIME {
            wYear: u16::try_from(c.year).ok()?,
            wMonth: c.month as u16,
            wDayOfWeek: 0,
            wDay: c.day as u16,
            wHour: c.hour as u16,
            wMinute: c.minute as u16,
            wSecond: c.second as u16,
            wMilliseconds: 0,
        };
        let mut utc = SYSTEMTIME::default();
        let mut file = FILETIME::default();
        let ok = unsafe {
            TzSpecificLocalTimeToSystemTimeEx(zone, &local, &mut utc) != 0
                && SystemTimeToFileTime(&utc, &mut file) != 0
        };
        ok.then(|| {
            from_filetime(u64::from(file.dwHighDateTime) << 32 | u64::from(file.dwLowDateTime))
        })
        .flatten()
    }
}

/// `Sat 2026-09-12 03:00:00`, in `zone`.
pub fn format(t: SystemTime, zone: &dyn Zone) -> String {
    let civil = zone.civil(t);
    format!("{} {civil}", civil.weekday_name())
}

/// When Windows started, time asleep included.
pub fn boot_time() -> SystemTime {
    let up = Duration::from_millis(unsafe { GetTickCount64() });
    let now = SystemTime::now();
    now.checked_sub(up).unwrap_or(now)
}

/// When `session` was signed in to, if Windows says.
pub fn logon_time(session: u32) -> Option<SystemTime> {
    let mut buffer = null_mut();
    let mut bytes = 0;
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session,
            WTSSessionInfo,
            &mut buffer,
            &mut bytes,
        )
    } != 0;
    if !ok || buffer.is_null() {
        return None;
    }
    let logon = (bytes as usize >= size_of::<WTSINFOW>())
        .then(|| unsafe { (*(buffer as *const WTSINFOW)).LogonTime });
    unsafe { WTSFreeMemory(buffer.cast()) };
    from_filetime(u64::try_from(logon?).ok().filter(|&t| t > 0)?)
}
