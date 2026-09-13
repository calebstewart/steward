//! Job objects: steward's cgroups. Each start of a service gets a fresh job.
//!
//! Jobs are not named: a job's name disappears with its last handle, even while
//! its processes run on, so a name could not lead a restarted manager back to
//! it. A restarted manager instead puts the processes it finds recorded into a
//! new job (see `assign`), which Windows nests inside the orphaned one.
//!
//! A job is created *without* `KILL_ON_JOB_CLOSE`: closing the manager's
//! handle -- including by the manager crashing -- leaves the services running.
//! It *has* `DIE_ON_UNHANDLED_EXCEPTION`, so a program that crashes dies at
//! once rather than waiting on an error-reporting dialog nobody will answer.

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::null;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
};

use super::port::Port;
use super::{check, owned};

pub struct Job {
    handle: OwnedHandle,
}

impl Job {
    /// A new job. With `children_break_away`, processes the service's own
    /// processes start are not part of it (`KillMode=process`).
    pub fn create(children_break_away: bool) -> io::Result<Job> {
        let handle = unsafe { owned(CreateJobObjectW(null(), null()))? };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | if children_break_away {
                JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
            } else {
                0
            };
        check(unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })?;
        Ok(Job { handle })
    }

    /// Put a running process in this job. A process already in a job can join
    /// a new, empty one, which becomes nested in the old: how a restarted
    /// manager takes back the processes of a job it has no handle to.
    pub fn assign(&self, process: HANDLE) -> io::Result<()> {
        check(unsafe { AssignProcessToJobObject(self.raw(), process) })
    }

    pub fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    /// Have Windows post this job's notifications to `port` under `key`.
    pub fn notify(&self, port: &Port, key: usize) -> io::Result<()> {
        let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: key as *mut c_void,
            CompletionPort: port.raw(),
        };
        check(unsafe {
            SetInformationJobObject(
                self.raw(),
                JobObjectAssociateCompletionPortInformation,
                &association as *const _ as *const c_void,
                size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        })
    }

    pub fn active_processes(&self) -> io::Result<u32> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        check(unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut c_void,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        })?;
        Ok(info.ActiveProcesses)
    }

    /// The IDs of the processes in the job.
    pub fn pids(&self) -> io::Result<Vec<u32>> {
        self.pids_with_room(1024)
    }

    /// `pids`, first trying a buffer with room for `room` IDs. Windows fails
    /// a query whose buffer is too small with `ERROR_MORE_DATA`, but reports
    /// in it how many processes the job holds, so the buffer is regrown to
    /// fit and the query repeated. Past a cap of a million IDs, the partial
    /// list is returned rather than nothing: a runaway unit still gets its
    /// processes recorded and asked to exit.
    fn pids_with_room(&self, mut room: usize) -> io::Result<Vec<u32>> {
        // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts, then usize IDs.
        const HEADER: usize = (2 * size_of::<u32>()).div_ceil(size_of::<usize>());
        const MOST: usize = 1 << 20;
        loop {
            let mut buffer = vec![0usize; HEADER + room];
            let result = check(unsafe {
                QueryInformationJobObject(
                    self.raw(),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr() as *mut c_void,
                    (buffer.len() * size_of::<usize>()) as u32,
                    std::ptr::null_mut(),
                )
            });
            let counts = buffer.as_ptr() as *const u32;
            let assigned = unsafe { *counts } as usize;
            let listed = unsafe { *counts.add(1) } as usize;
            match result {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) => {
                    if room < MOST {
                        // Double past what was there: the job may still be
                        // growing, and each query is a fresh snapshot.
                        room = assigned.max(room).saturating_mul(2).min(MOST);
                        continue;
                    }
                }
                Err(e) => return Err(e),
            }
            return Ok(buffer[HEADER..HEADER + listed.min(room)]
                .iter()
                .map(|&pid| pid as u32)
                .collect());
        }
    }

    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        check(unsafe { TerminateJobObject(self.raw(), exit_code) })
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command, Stdio};

    use super::*;

    /// Processes that sit until the job is terminated: `pause` reads from
    /// a pipe nobody writes to.
    fn sitters(job: &Job, count: usize) -> Vec<Child> {
        (0..count)
            .map(|_| {
                let child = Command::new("cmd")
                    .args(["/c", "pause"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                job.assign(child.as_raw_handle()).unwrap();
                child
            })
            .collect()
    }

    #[test]
    fn lists_more_processes_than_the_first_buffer_holds() {
        let job = Job::create(false).unwrap();
        let mut children = sitters(&job, 5);
        let mut expected: Vec<u32> = children.iter().map(|c| c.id()).collect();
        expected.sort_unstable();

        // Room for one ID, so the first query fails with ERROR_MORE_DATA.
        let mut pids = job.pids_with_room(1).unwrap();
        pids.sort_unstable();
        assert_eq!(pids, expected);

        let mut pids = job.pids().unwrap();
        pids.sort_unstable();
        assert_eq!(pids, expected);

        job.terminate(0).unwrap();
        for child in &mut children {
            child.wait().unwrap();
        }
    }

    #[test]
    fn an_empty_job_lists_nothing() {
        let job = Job::create(false).unwrap();
        assert!(job.pids_with_room(1).unwrap().is_empty());
        assert!(job.pids().unwrap().is_empty());
    }
}
