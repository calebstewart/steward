//! The I/O completion port the manager waits on. Everything that can happen
//! arrives here as a packet: a process exiting (posted by a thread-pool wait),
//! a job's notifications (posted by Windows), and a nudge from the SCM's or
//! the console's control handler that there are events in the channel.

use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::null_mut;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus, OVERLAPPED,
};

use super::{check, owned};

/// A packet from the port: which source (`key`), and two words of payload.
#[derive(Debug, Clone, Copy)]
pub struct Packet {
    pub key: usize,
    pub bytes: u32,
    pub value: usize,
}

pub struct Port(OwnedHandle);

impl Port {
    pub fn new() -> io::Result<Port> {
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
        Ok(Port(unsafe { owned(handle)? }))
    }

    pub fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// Something other threads can post to. It must not outlive the port.
    pub fn waker(&self) -> Waker {
        Waker(self.raw() as usize)
    }

    /// The next packet, or `None` once `timeout` has passed.
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<Option<Packet>> {
        let millis = timeout.map_or(u32::MAX, |d| {
            d.as_millis().min(u128::from(u32::MAX - 1)) as u32
        });
        let (mut bytes, mut key, mut overlapped) = (0u32, 0usize, null_mut::<OVERLAPPED>());
        let ok = unsafe {
            GetQueuedCompletionStatus(self.raw(), &mut bytes, &mut key, &mut overlapped, millis)
        };
        if ok == 0 && overlapped.is_null() {
            let error = unsafe { GetLastError() };
            if error == WAIT_TIMEOUT {
                return Ok(None);
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(Some(Packet {
            key,
            bytes,
            value: overlapped as usize,
        }))
    }
}

/// Posts packets to a [`Port`] from any thread.
#[derive(Debug, Clone, Copy)]
pub struct Waker(usize);

impl Waker {
    pub fn post(&self, key: usize, bytes: u32, value: usize) -> io::Result<()> {
        check(unsafe {
            PostQueuedCompletionStatus(self.0 as HANDLE, bytes, key, value as *mut OVERLAPPED)
        })
    }
}
