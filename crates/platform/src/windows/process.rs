//! Processes: the snapshot, start times, usage, priorities, GPU use, and the job objects that
//! follow a protected tree.
//!
//! The job object is used for following and counting only. It is created without "kill on
//! job close", so dropping the tracker or the daemon dying leaves every process running, and
//! with "breakaway OK", so a program that asks to start a child outside its job still can.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::Mutex;

use daemon::sys::{ProcessEntry, ProcessInfo, ProcessPriority, TreeTracker, Usage, access_denied};
use proto::{ApiError, ErrorCode};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, FILETIME, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAndIoAccountingInformation, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass, GetProcessIoCounters,
    GetProcessTimes, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, IO_COUNTERS, NORMAL_PRIORITY_CLASS,
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_TERMINATE, SetPriorityClass,
};

use super::ffi::{os_error, process_image_path};
use super::sensors::Counter;

struct Handle(HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn not_found(pid: u32) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("no process {pid}"))
}

// Not declared by windows-sys without the Wdk and Shell features.
windows_link::link!("ntdll.dll" "system" fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut std::ffi::c_void, length: u32, returned: *mut u32) -> i32);
windows_link::link!("shell32.dll" "system" fn CommandLineToArgvW(line: *const u16, count: *mut i32) -> *mut *mut u16);
const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

/// A running process's command line, split the way the C runtime would. It may hold secrets or
/// a prompt: callers keep what they need and pass none of it on.
pub fn process_command_line(pid: u32) -> Result<Vec<String>, ApiError> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION, "read command line")?;
    let mut needed = 0u32;
    // SAFETY: a zero-length query only reports the size in `needed`.
    unsafe { NtQueryInformationProcess(h.0, PROCESS_COMMAND_LINE_INFORMATION, null_mut(), 0, &mut needed) };
    if (needed as usize) < size_of::<UnicodeString>() {
        return Err(ApiError::internal(format!("no command line for process {pid}")));
    }
    // u64 keeps the buffer aligned for the UNICODE_STRING at its start.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buffer` holds `needed` writable bytes.
    let status = unsafe {
        NtQueryInformationProcess(h.0, PROCESS_COMMAND_LINE_INFORMATION, buffer.as_mut_ptr().cast(), needed, &mut needed)
    };
    if status < 0 {
        return Err(ApiError::internal(format!("cannot read the command line of process {pid}: status {status:#x}")));
    }
    // SAFETY: on success the buffer starts with a UNICODE_STRING pointing into the same buffer.
    let line = unsafe {
        let text = &*buffer.as_ptr().cast::<UnicodeString>();
        std::slice::from_raw_parts(text.buffer, usize::from(text.length) / 2).to_vec()
    };
    split_command_line(&line)
}

pub(crate) fn split_command_line(line: &[u16]) -> Result<Vec<String>, ApiError> {
    let wide: Vec<u16> = line.iter().copied().chain(std::iter::once(0)).collect();
    let mut count = 0i32;
    // SAFETY: `wide` is NUL-terminated; the result is freed below.
    let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
    if argv.is_null() {
        return Err(os_error("CommandLineToArgvW"));
    }
    // SAFETY: `argv` holds `count` NUL-terminated strings.
    let out = (0..count as usize)
        .map(|i| unsafe {
            let arg = *argv.add(i);
            let n = (0..).take_while(|&k| *arg.add(k) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(arg, n))
        })
        .collect();
    // SAFETY: allocated by CommandLineToArgvW, freed once.
    unsafe { windows_sys::Win32::Foundation::LocalFree(argv.cast()) };
    Ok(out)
}

fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS, what: &str) -> Result<Handle, ApiError> {
    // SAFETY: plain Win32 call; a null result is handled.
    let h = unsafe { OpenProcess(access, 0, pid) };
    if !h.is_null() {
        return Ok(Handle(h));
    }
    // SAFETY: right after the failing call.
    match unsafe { GetLastError() } {
        ERROR_ACCESS_DENIED => Err(access_denied(&format!("{what} (pid {pid})"))),
        ERROR_INVALID_PARAMETER => Err(not_found(pid)),
        _ => Err(os_error(&format!("OpenProcess({pid})"))),
    }
}

fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

pub fn processes() -> Result<Vec<ProcessEntry>, ApiError> {
    // SAFETY: plain Win32 call; failure is handled.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(os_error("CreateToolhelp32Snapshot"));
    }
    let snap = Handle(snap);
    let mut e = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: live snapshot handle, dwSize set.
    let mut more = unsafe { Process32FirstW(snap.0, &mut e) } != 0;
    while more {
        let len = e
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(e.szExeFile.len());
        out.push(ProcessEntry {
            pid: e.th32ProcessID,
            parent_pid: e.th32ParentProcessID,
            name: String::from_utf16_lossy(&e.szExeFile[..len]),
        });
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap.0, &mut e) } != 0;
    }
    Ok(out)
}

/// Creation time and CPU time (100 ns ticks).
fn times(h: &Handle) -> Result<(u64, u64), ApiError> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: live handle with query rights, valid out-pointers.
    if unsafe { GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(os_error("GetProcessTimes"));
    }
    Ok((ticks(created), ticks(kernel) + ticks(user)))
}

pub fn process_info(pid: u32) -> Result<ProcessInfo, ApiError> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION, "process_info")?;
    // A parent may retain a handle after exit, keeping the PID object queryable.
    // A nonzero exit timestamp proves this process has finished even if its image is unreadable.
    let (mut created_time, mut exited_time, mut kernel, mut user) = Default::default();
    if unsafe { GetProcessTimes(h.0, &mut created_time, &mut exited_time, &mut kernel, &mut user) } == 0 {
        return Err(os_error("GetProcessTimes"));
    }
    if ticks(exited_time) != 0 {
        return Err(not_found(pid));
    }
    let created = ticks(created_time);
    // FILETIME counts 100 ns ticks since 1601-01-01.
    let started_at = (created / 10_000) as i64 - 11_644_473_600_000;
    let own_user = super::ffi::process_user_sid(pid)
        .ok()
        .zip(super::ffi::current_user_sid().ok())
        .map(|(a, b)| a == b);
    Ok(ProcessInfo {
        pid,
        exe: process_image_path(pid),
        started_at,
        own_user,
    })
}

fn private_mb(h: &Handle) -> u64 {
    let mut m = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: live handle; the EX struct starts with the plain one and `cb` says which.
    let ok = unsafe {
        GetProcessMemoryInfo(
            h.0,
            (&mut m as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            m.cb,
        )
    };
    if ok == 0 {
        0
    } else {
        (m.PrivateUsage / (1024 * 1024)) as u64
    }
}

pub fn process_usage(pid: u32) -> Result<Usage, ApiError> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION, "process_usage")?;
    let (_, cpu) = times(&h)?;
    let mut io = IO_COUNTERS::default();
    // SAFETY: live handle, valid out-pointer.
    if unsafe { GetProcessIoCounters(h.0, &mut io) } == 0 {
        return Err(os_error("GetProcessIoCounters"));
    }
    Ok(Usage {
        cpu_ms: cpu / 10_000,
        io_ops: io.ReadOperationCount + io.WriteOperationCount + io.OtherOperationCount,
        io_bytes: io.ReadTransferCount + io.WriteTransferCount + io.OtherTransferCount,
        memory_mb: private_mb(&h),
    })
}

pub fn process_priority(pid: u32) -> Result<Option<ProcessPriority>, ApiError> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION, "process_priority")?;
    // SAFETY: live handle with query rights.
    let class = unsafe { GetPriorityClass(h.0) };
    Ok(match class {
        0 => return Err(os_error("GetPriorityClass")),
        IDLE_PRIORITY_CLASS => Some(ProcessPriority::Idle),
        BELOW_NORMAL_PRIORITY_CLASS => Some(ProcessPriority::BelowNormal),
        NORMAL_PRIORITY_CLASS => Some(ProcessPriority::Normal),
        ABOVE_NORMAL_PRIORITY_CLASS => Some(ProcessPriority::AboveNormal),
        HIGH_PRIORITY_CLASS => Some(ProcessPriority::High),
        // Real-time, or something newer: not ours to touch.
        _ => None,
    })
}

pub fn set_process_priority(pid: u32, p: ProcessPriority) -> Result<(), ApiError> {
    // There is no real-time value to map to: ProcessPriority cannot express it.
    let class = match p {
        ProcessPriority::Idle => IDLE_PRIORITY_CLASS,
        ProcessPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
        ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        ProcessPriority::High => HIGH_PRIORITY_CLASS,
    };
    let h = open(pid, PROCESS_SET_INFORMATION, "set_process_priority")?;
    // SAFETY: live handle with set-information rights.
    if unsafe { SetPriorityClass(h.0, class) } == 0 {
        // SAFETY: right after the failing call.
        return Err(if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            access_denied(&format!("set_process_priority (pid {pid})"))
        } else {
            os_error("SetPriorityClass")
        });
    }
    Ok(())
}

// ---- job objects ---------------------------------------------------------------------------

pub struct JobTracker {
    job: Handle,
}

fn assign(job: &Handle, pid: u32) -> Result<(), ApiError> {
    // AssignProcessToJobObject requires these two access rights on the process; nothing
    // here ever ends a process.
    let h = open(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE, "track_tree")?;
    // SAFETY: both handles are live.
    if unsafe { AssignProcessToJobObject(job.0, h.0) } == 0 {
        // SAFETY: right after the failing call.
        return Err(if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            access_denied(&format!("track_tree (pid {pid})"))
        } else {
            os_error("AssignProcessToJobObject")
        });
    }
    Ok(())
}

pub fn track_tree(root: u32, members: &[u32]) -> Result<Box<dyn TreeTracker>, ApiError> {
    Ok(Box::new(track(root, members)?))
}

fn track(root: u32, members: &[u32]) -> Result<JobTracker, ApiError> {
    // SAFETY: anonymous job with default security.
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(os_error("CreateJobObject"));
    }
    let job = Handle(job);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // Only "breakaway OK". Never JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    // SAFETY: live job handle; the struct matches the information class.
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(os_error("SetInformationJobObject"));
    }
    assign(&job, root)?;
    for pid in members {
        // A member that cannot be taken in is followed no further; the rest still are.
        let _ = assign(&job, *pid);
    }
    Ok(JobTracker { job })
}

impl JobTracker {
    #[cfg(test)]
    fn limit_flags(&self) -> u32 {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: live job handle; the struct matches the information class.
        unsafe {
            QueryInformationJobObject(
                self.job.0,
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        };
        limits.BasicLimitInformation.LimitFlags
    }
}

impl TreeTracker for JobTracker {
    fn members(&self) -> Result<Vec<u32>, ApiError> {
        let mut room = 256usize;
        loop {
            // Two u32 counts (8 bytes: one usize on 64-bit, two on 32-bit), then the ids.
            let header = 8 / size_of::<usize>();
            let mut buf = vec![0usize; header + room];
            // SAFETY: live job handle; `buf` is large enough for the header and `room` ids.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * size_of::<usize>()) as u32,
                    null_mut(),
                )
            };
            // SAFETY: right after the call.
            if ok == 0 && unsafe { GetLastError() } != ERROR_MORE_DATA {
                return Err(os_error("QueryInformationJobObject(process ids)"));
            }
            // SAFETY: the header is two u32 counts at the start of `buf`.
            let (assigned, listed) = unsafe {
                let p = buf.as_ptr().cast::<u32>();
                (*p as usize, *p.add(1) as usize)
            };
            if ok != 0 && listed >= assigned {
                return Ok(buf[header..header + listed].iter().map(|p| *p as u32).collect());
            }
            room = assigned.max(room * 2);
        }
    }

    fn usage(&self) -> Result<Usage, ApiError> {
        let mut info = JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION::default();
        // SAFETY: live job handle; the struct matches the information class.
        let ok = unsafe {
            QueryInformationJobObject(
                self.job.0,
                JobObjectBasicAndIoAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(os_error("QueryInformationJobObject(accounting)"));
        }
        let b = &info.BasicInfo;
        let io = &info.IoInfo;
        let memory_mb = self
            .members()?
            .into_iter()
            .filter_map(|pid| open(pid, PROCESS_QUERY_LIMITED_INFORMATION, "memory").ok())
            .map(|h| private_mb(&h))
            .sum();
        Ok(Usage {
            cpu_ms: (b.TotalUserTime.max(0) as u64 + b.TotalKernelTime.max(0) as u64) / 10_000,
            io_ops: io.ReadOperationCount + io.WriteOperationCount + io.OtherOperationCount,
            io_bytes: io.ReadTransferCount + io.WriteTransferCount + io.OtherTransferCount,
            memory_mb,
        })
    }
}

// ---- GPU -----------------------------------------------------------------------------------

static GPU: Mutex<Option<Counter>> = Mutex::new(None);

/// `pid_1234_luid_0x…_phys_0_eng_0_engtype_3D` → 1234.
fn gpu_instance_pid(name: &str) -> Option<u32> {
    name.strip_prefix("pid_")?.split('_').next()?.parse().ok()
}

pub fn gpu_usage() -> Result<BTreeMap<u32, f64>, ApiError> {
    let mut slot = GPU.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(Counter::open(&[r"\GPU Engine(*)\Utilization Percentage"])?.0);
    }
    let mut out: BTreeMap<u32, f64> = BTreeMap::new();
    for (name, value) in slot.as_ref().expect("just set").read()? {
        if let Some(pid) = gpu_instance_pid(&name) {
            let e = out.entry(pid).or_default();
            *e = e.max(value);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    use super::*;

    /// A child this test starts, ended (with its own children) when the test is done.
    struct Own(Child);

    impl Drop for Own {
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &self.0.id().to_string()])
                .output();
            let _ = self.0.wait();
        }
    }

    fn spawn(cmd: &str, args: &[&str]) -> Own {
        Own(Command::new(cmd)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap())
    }

    #[test]
    fn the_snapshot_and_own_process_readings_make_sense() {
        let me = std::process::id();
        let all = processes().unwrap();
        assert!(all.iter().any(|p| p.pid == me), "own process missing");
        let info = process_info(me).unwrap();
        assert!(
            info.started_at > 1_600_000_000_000 && info.exe.is_some(),
            "{info:?}"
        );
        let u = process_usage(me).unwrap();
        assert!(u.memory_mb > 0, "{u:?}");
        assert!(process_priority(me).unwrap().is_some());
        assert_eq!(process_info(u32::MAX - 3).unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(
            gpu_instance_pid("pid_1234_luid_0x0_0x1_phys_0_eng_0_engtype_3D"),
            Some(1234)
        );
        // Machines without GPU counters say so; otherwise a map (possibly empty at first).
        if let Err(e) = gpu_usage() {
            assert_eq!(e.code, ErrorCode::NotImplemented, "{e:?}");
        }
    }

    /// Only processes this test starts. The job follows them, never ends them.
    #[test]
    fn a_tracked_tree_takes_in_later_children_and_survives_the_tracker() {
        // The second ping is started after the tracking began.
        let shell = spawn(
            "cmd",
            &["/c", "ping -n 2 127.0.0.1 >nul & ping -n 30 127.0.0.1 >nul"],
        );
        let root = shell.0.id();
        let t = track(root, &[]).unwrap();
        assert_eq!(
            t.limit_flags() & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            0,
            "never kill on close"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let members = loop {
            let m = t.members().unwrap();
            if m.len() >= 2 || Instant::now() > deadline {
                break m;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(members.contains(&root) && members.len() >= 2, "{members:?}");
        let u = t.usage().unwrap();
        assert!(u.cpu_ms < 60_000, "{u:?}");
        drop(t);
        std::thread::sleep(Duration::from_millis(300));
        let mut shell = shell;
        assert!(
            shell.0.try_wait().unwrap().is_none(),
            "dropping the tracker must not end the tree"
        );
        assert!(
            processes()
                .unwrap()
                .iter()
                .any(|p| members.contains(&p.pid) && p.pid != root),
            "the child runs on too"
        );
    }

    #[test]
    fn priority_of_an_own_process_goes_up_and_back() {
        let p = spawn("ping", &["-n", "30", "127.0.0.1"]);
        let pid = p.0.id();
        assert_eq!(process_priority(pid).unwrap(), Some(ProcessPriority::Normal));
        set_process_priority(pid, ProcessPriority::AboveNormal).unwrap();
        assert_eq!(process_priority(pid).unwrap(), Some(ProcessPriority::AboveNormal));
        set_process_priority(pid, ProcessPriority::Normal).unwrap();
        assert_eq!(process_priority(pid).unwrap(), Some(ProcessPriority::Normal));
    }
}

#[cfg(test)]
mod exit_identity_tests {
    use super::*;
    #[test]
    fn a_retained_child_handle_does_not_make_an_exited_process_live() {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new(
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("cmd.exe"),
        )
        .args(["/d", "/c", "exit", "0"])
        .creation_flags(0x08000000)
        .spawn()
        .unwrap();
        child.wait().unwrap();
        assert_eq!(process_info(child.id()).unwrap_err().code, ErrorCode::NotFound);
    }
}
