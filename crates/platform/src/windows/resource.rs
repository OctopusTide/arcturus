//! Process actions are scoped to a creation-checked handle. Thread receipts make thaw
//! idempotent even if the daemon dies between writing its journal and suspending a thread.
use super::ffi::{os_error, to_wide};
use super::{process, sensors::Counter};
use daemon::arbitration::ProgramRule;
use daemon::sys::{ThreadState, VideoMemory, access_denied, unsupported};
use proto::{ApiError, ErrorCode};
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, FILETIME, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Cryptography::Catalog::*;
use windows_sys::Win32::Security::Cryptography::{
    CERT_NAME_ATTR_TYPE, CertGetNameStringW, szOID_ORGANIZATION_NAME,
};
use windows_sys::Win32::Security::WinTrust::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessIdOfThread, GetProcessTimes, GetThreadTimes, OpenProcess,
    OpenThread, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    ResumeThread, SuspendThread, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
    TerminateProcess,
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}
fn missing() -> ApiError {
    ApiError::new(
        ErrorCode::NotFound,
        "process or thread exited or its id was reused",
    )
}
fn process_handle(pid: u32, at: i64, rights: u32) -> Result<Handle, ApiError> {
    // SAFETY: plain calls; every returned handle is owned, all output storage is valid.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | rights, 0, pid) };
    if h.is_null() {
        return Err(if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            access_denied("resource process action")
        } else {
            missing()
        });
    }
    let h = Handle(h);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    if unsafe { GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(os_error("GetProcessTimes"));
    }
    let mut exit_code = 0;
    if (ticks(created) / 10_000) as i64 - 11_644_473_600_000 != at
        || unsafe { GetExitCodeProcess(h.0, &mut exit_code) } == 0
        || exit_code != 259
    {
        return Err(missing());
    }
    Ok(h)
}
fn thread_handle(pid: u32, t: &ThreadState) -> Result<Handle, ApiError> {
    let h = unsafe {
        OpenThread(
            THREAD_QUERY_LIMITED_INFORMATION | THREAD_SUSPEND_RESUME,
            0,
            t.tid,
        )
    };
    if h.is_null() {
        return Err(if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            access_denied("thread suspension action")
        } else {
            missing()
        });
    }
    let h = Handle(h);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    if unsafe { GetProcessIdOfThread(h.0) } != pid
        || unsafe { GetThreadTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0
        || ticks(created) != t.started_at
    {
        return Err(missing());
    }
    Ok(h)
}

// Windows exposes the read-only suspend count through NtQueryInformationThread, class 35.
// If unavailable on a future Windows version, refuse freezing instead of losing recovery.
windows_link::link!("ntdll.dll" "system" fn NtQueryInformationThread(thread: HANDLE, class: i32, buffer: *mut core::ffi::c_void, len: u32, returned: *mut u32) -> i32);
fn count(h: HANDLE) -> Result<u32, ApiError> {
    let mut value = 0u32;
    let status =
        unsafe { NtQueryInformationThread(h, 35, (&mut value as *mut u32).cast(), 4, null_mut()) };
    if status < 0 {
        Err(unsupported("thread suspend count (safe recovery required)"))
    } else {
        Ok(value)
    }
}
fn thread_ids(pid: u32) -> Result<Vec<u32>, ApiError> {
    let h = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if h == INVALID_HANDLE_VALUE {
        return Err(os_error("thread snapshot"));
    }
    let h = Handle(h);
    let mut t = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut more = unsafe { Thread32First(h.0, &mut t) } != 0;
    let mut ids = Vec::new();
    while more {
        if t.th32OwnerProcessID == pid {
            ids.push(t.th32ThreadID);
        }
        more = unsafe { Thread32Next(h.0, &mut t) } != 0;
    }
    Ok(ids)
}
pub fn suspension_state(pid: u32, at: i64) -> Result<Vec<ThreadState>, ApiError> {
    let _process = process_handle(pid, at, 0)?;
    thread_ids(pid)?
        .into_iter()
        .map(|tid| {
            let h = unsafe {
                OpenThread(
                    THREAD_QUERY_LIMITED_INFORMATION | THREAD_SUSPEND_RESUME,
                    0,
                    tid,
                )
            };
            if h.is_null() {
                return Err(access_denied("read suspension state"));
            }
            let h = Handle(h);
            let (mut created, mut exited, mut kernel, mut user) = Default::default();
            if unsafe { GetThreadTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) }
                == 0
            {
                return Err(os_error("GetThreadTimes"));
            }
            Ok(ThreadState {
                tid,
                started_at: ticks(created),
                suspend_count: count(h.0)?,
            })
        })
        .collect()
}
pub fn thaw_process(pid: u32, at: i64, threads: &[ThreadState]) -> Result<(), ApiError> {
    let _process = match process_handle(pid, at, 0) {
        Ok(h) => h,
        Err(e) if e.code == ErrorCode::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut error = None;
    for t in threads {
        let h = match thread_handle(pid, t) {
            Ok(h) => h,
            Err(e) if e.code == ErrorCode::NotFound => continue,
            Err(e) => {
                error = Some(e);
                continue;
            }
        };
        match count(h.0) {
            Ok(n) if n == t.suspend_count.saturating_add(1) => {
                if unsafe { ResumeThread(h.0) } == u32::MAX {
                    error = Some(os_error("ResumeThread"));
                }
            }
            Ok(n) if n <= t.suspend_count => {}
            Ok(_) => {
                error = Some(ApiError::internal(
                    "another tool also suspended this thread; retry thaw after its suspension ends",
                ))
            }
            Err(e) => error = Some(e),
        }
    }
    error.map_or(Ok(()), Err)
}
pub fn freeze_process(pid: u32, at: i64, threads: &[ThreadState]) -> Result<(), ApiError> {
    let h = process_handle(pid, at, PROCESS_SET_QUOTA)?;
    if threads.is_empty() {
        return Err(missing());
    }
    let result = (|| {
        for t in threads {
            let thread = thread_handle(pid, t)?;
            if count(thread.0)? != t.suspend_count {
                return Err(ApiError::internal("thread suspension changed; retry later"));
            }
            let before = unsafe { SuspendThread(thread.0) };
            if before == u32::MAX {
                return Err(os_error("SuspendThread"));
            }
            if before != t.suspend_count {
                return Err(ApiError::internal(
                    "concurrent thread suspension; rolled back",
                ));
            }
        }
        let planned: BTreeSet<_> = threads.iter().map(|t| t.tid).collect();
        if thread_ids(pid)?.iter().any(|t| !planned.contains(t)) {
            return Err(ApiError::internal(
                "new threads appeared during freeze; rolled back",
            ));
        }
        // Trimming pages is best effort; private commit remains allocated.
        unsafe {
            EmptyWorkingSet(h.0);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = thaw_process(pid, at, threads);
    }
    result
}
pub fn terminate_process(pid: u32, at: i64) -> Result<(), ApiError> {
    let h = process_handle(pid, at, PROCESS_TERMINATE)?;
    if unsafe { TerminateProcess(h.0, 1) } == 0 {
        return Err(if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            access_denied("terminate_process")
        } else {
            os_error("TerminateProcess")
        });
    }
    Ok(())
}

pub(super) fn set_priority_checked(
    pid: u32,
    at: i64,
    priority: daemon::sys::ProcessPriority,
) -> Result<(), ApiError> {
    use windows_sys::Win32::System::Threading::*;
    let h = process_handle(pid, at, PROCESS_SET_INFORMATION)?;
    let class = match priority {
        daemon::sys::ProcessPriority::Idle => IDLE_PRIORITY_CLASS,
        daemon::sys::ProcessPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        daemon::sys::ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
        daemon::sys::ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        daemon::sys::ProcessPriority::High => HIGH_PRIORITY_CLASS,
    };
    if unsafe { SetPriorityClass(h.0, class) } == 0 {
        return Err(os_error("SetPriorityClass"));
    }
    Ok(())
}

type SignatureCache = BTreeMap<String, (u64, Option<std::time::SystemTime>, Option<String>)>;
static SIGNERS: OnceLock<Mutex<SignatureCache>> = OnceLock::new();
pub fn file_signer(path: &str) -> Result<Option<String>, ApiError> {
    let meta =
        std::fs::metadata(path).map_err(|e| ApiError::internal(format!("signature file: {e}")))?;
    let key = path.to_lowercase();
    let mut cache = SIGNERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some((len, modified, signer)) = cache.get(&key) {
        if *len == meta.len() && *modified == meta.modified().ok() {
            return Ok(signer.clone());
        }
    }
    let wide = to_wide(path);
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_ptr(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
        ..Default::default()
    };
    let signer = verify_data(&mut data).or_else(|| catalog_signer(path));
    if cache.len() > 2048 {
        cache.clear();
    }
    cache.insert(key, (meta.len(), meta.modified().ok(), signer.clone()));
    Ok(signer)
}

fn verify_data(data: &mut WINTRUST_DATA) -> Option<String> {
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let result = unsafe { WinVerifyTrustEx(null_mut(), &mut action, data) };
    let signer = if result == 0 {
        // SAFETY: provider pointers belong to the live verified state until CLOSE below.
        unsafe {
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            let signer = if provider.is_null() {
                null_mut()
            } else {
                WTHelperGetProvSignerFromChain(provider, 0, 0, 0)
            };
            let cert = if signer.is_null() {
                null_mut()
            } else {
                WTHelperGetProvCertFromChain(signer, 0)
            };
            if cert.is_null() || (*cert).pCert.is_null() {
                None
            } else {
                let len = CertGetNameStringW(
                    (*cert).pCert,
                    CERT_NAME_ATTR_TYPE,
                    0,
                    szOID_ORGANIZATION_NAME.cast(),
                    null_mut(),
                    0,
                );
                if len <= 1 {
                    None
                } else {
                    let mut buf = vec![0u16; len as usize];
                    CertGetNameStringW(
                        (*cert).pCert,
                        CERT_NAME_ATTR_TYPE,
                        0,
                        szOID_ORGANIZATION_NAME.cast(),
                        buf.as_mut_ptr(),
                        len,
                    );
                    Some(String::from_utf16_lossy(&buf[..len as usize - 1]))
                }
            }
        }
    } else {
        None
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrustEx(null_mut(), &mut action, data);
    }
    signer
}

fn catalog_signer(path: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let wide = to_wide(path);
    // Catalogs may use either algorithm. Every context is released on every branch.
    for algorithm in ["SHA256", "SHA1"] {
        let mut admin = 0isize;
        let algorithm = to_wide(algorithm);
        if unsafe {
            CryptCATAdminAcquireContext2(&mut admin, null(), algorithm.as_ptr(), null(), 0)
        } == 0
        {
            continue;
        }
        struct Admin(isize);
        impl Drop for Admin {
            fn drop(&mut self) {
                unsafe {
                    CryptCATAdminReleaseContext(self.0, 0);
                }
            }
        }
        let admin = Admin(admin);
        let mut len = 0;
        unsafe {
            CryptCATAdminCalcHashFromFileHandle2(
                admin.0,
                file.as_raw_handle(),
                &mut len,
                null_mut(),
                0,
            );
        }
        if len == 0 || len > 1024 {
            continue;
        }
        let mut hash = vec![0u8; len as usize];
        if unsafe {
            CryptCATAdminCalcHashFromFileHandle2(
                admin.0,
                file.as_raw_handle(),
                &mut len,
                hash.as_mut_ptr(),
                0,
            )
        } == 0
        {
            continue;
        }
        let tag = to_wide(hash.iter().map(|b| format!("{b:02X}")).collect::<String>());
        let mut catalog =
            unsafe { CryptCATAdminEnumCatalogFromHash(admin.0, hash.as_ptr(), len, 0, null_mut()) };
        while catalog != 0 {
            let mut info = CATALOG_INFO {
                cbStruct: size_of::<CATALOG_INFO>() as u32,
                ..Default::default()
            };
            let mut signer = None;
            if unsafe { CryptCATCatalogInfoFromContext(catalog, &mut info, 0) } != 0 {
                let mut cat = WINTRUST_CATALOG_INFO {
                    cbStruct: size_of::<WINTRUST_CATALOG_INFO>() as u32,
                    pcwszCatalogFilePath: info.wszCatalogFile.as_ptr(),
                    pcwszMemberTag: tag.as_ptr(),
                    pcwszMemberFilePath: wide.as_ptr(),
                    hMemberFile: file.as_raw_handle(),
                    pbCalculatedFileHash: hash.as_mut_ptr(),
                    cbCalculatedFileHash: len,
                    hCatAdmin: admin.0,
                    ..Default::default()
                };
                let mut data = WINTRUST_DATA {
                    cbStruct: size_of::<WINTRUST_DATA>() as u32,
                    dwUIChoice: WTD_UI_NONE,
                    fdwRevocationChecks: WTD_REVOKE_NONE,
                    dwUnionChoice: WTD_CHOICE_CATALOG,
                    Anonymous: WINTRUST_DATA_0 { pCatalog: &mut cat },
                    dwStateAction: WTD_STATEACTION_VERIFY,
                    dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
                    ..Default::default()
                };
                signer = verify_data(&mut data);
            }
            if signer.is_some() {
                unsafe {
                    CryptCATAdminReleaseCatalogContext(admin.0, catalog, 0);
                }
                return signer;
            }
            // Enum consumes the previous context when continuing the enumeration.
            catalog = unsafe {
                CryptCATAdminEnumCatalogFromHash(admin.0, hash.as_ptr(), len, 0, &mut catalog)
            };
        }
    }
    None
}

pub fn program_rules() -> Result<Vec<ProgramRule>, ApiError> {
    let mut rules = BTreeMap::<String, ProgramRule>::new();
    let windows = std::env::var("SystemRoot")
        .unwrap_or_else(|_| r"C:\Windows".into())
        .to_lowercase();
    let me = std::env::current_exe().map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(dir) = me.parent() {
        for name in [
            proto::names::DAEMON_BIN,
            proto::names::CLI_BIN,
            proto::names::UI_BIN,
            proto::names::TRAY_BIN,
            proto::names::LAUNCHER_BIN,
            proto::names::ADMIN_BIN,
            proto::names::RELAY_BIN,
            proto::names::HOOK_BIN,
        ] {
            let path = dir
                .join(format!("{name}.exe"))
                .to_string_lossy()
                .into_owned();
            rules.insert(
                path.to_lowercase(),
                ProgramRule {
                    path,
                    signer: None,
                    source: "builtin".into(),
                },
            );
        }
    }
    for p in process::processes()? {
        let Some(path) = super::ffi::process_image_path(p.pid) else {
            continue;
        };
        let lower = path.to_lowercase();
        let name = lower.rsplit('\\').next().unwrap_or("");
        let in_windows = lower.starts_with(&format!("{windows}\\"));
        let security = matches!(
            name,
            "msmpeng.exe"
                | "nissrv.exe"
                | "securityhealthservice.exe"
                | "securityhealthsystray.exe"
                | "avp.exe"
                | "avastsvc.exe"
                | "avgsvc.exe"
                | "mbamservice.exe"
                | "ekrn.exe"
                | "bdservicehost.exe"
                | "360tray.exe"
                | "huorong.exe"
                | "hipsdaemon.exe"
        );
        let default = matches!(
            name,
            "claude.exe"
                | "codex.exe"
                | "windowsterminal.exe"
                | "wt.exe"
                | "powershell.exe"
                | "pwsh.exe"
                | "cmd.exe"
                // Windows Terminal's console host: its parent is svchost, so no agent's tree
                // reaches it, yet the programs in that terminal stop with it.
                | "openconsole.exe"
                | "code.exe"
                | "winword.exe"
                | "excel.exe"
                | "powerpnt.exe"
                | "onenote.exe"
                | "notepad.exe"
        );
        let signer = if in_windows || security || default {
            file_signer(&path).ok().flatten()
        } else {
            None
        };
        let protected_path = (in_windows || security)
            && super::admin::service_path_is_secure(std::path::Path::new(&path));
        let system = in_windows
            && !default
            && protected_path
            && signer
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("Microsoft Corporation"));
        let security =
            security && protected_path && trusted_security_signer(name, signer.as_deref());
        if system || security || default {
            rules.entry(lower).or_insert_with(|| ProgramRule {
                signer,
                path,
                source: if system || security {
                    "builtin"
                } else {
                    "default"
                }
                .into(),
            });
        }
    }
    Ok(rules.into_values().collect())
}

static VIDEO_PROCESS: Mutex<Option<Counter>> = Mutex::new(None);
static VIDEO_TOTAL: Mutex<Option<Counter>> = Mutex::new(None);
pub fn video_memory() -> Result<VideoMemory, ApiError> {
    let mut total = VIDEO_TOTAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut each = VIDEO_PROCESS.lock().unwrap_or_else(|p| p.into_inner());
    if total.is_none() {
        *total = Some(Counter::open(&[r"\GPU Adapter Memory(*)\Dedicated Usage"])?.0);
    }
    if each.is_none() {
        *each = Some(Counter::open(&[r"\GPU Process Memory(*)\Dedicated Usage"])?.0);
    }
    let used = total
        .as_ref()
        .unwrap()
        .read()?
        .iter()
        .map(|(_, v)| v.max(0.0) as u64)
        .sum::<u64>()
        / (1024 * 1024);
    let mut per_pid_mb = BTreeMap::<u32, u64>::new();
    for (name, value) in each.as_ref().unwrap().read()? {
        if let Some(pid) = name
            .strip_prefix("pid_")
            .and_then(|s| s.split('_').next())
            .and_then(|s| s.parse().ok())
        {
            *per_pid_mb.entry(pid).or_default() += value.max(0.0) as u64;
        }
    }
    for v in per_pid_mb.values_mut() {
        *v /= 1024 * 1024;
    }
    Ok(VideoMemory {
        used_mb: used,
        per_pid_mb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    struct Own(Child);
    impl Drop for Own {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn only_an_own_child_is_frozen_resumed_and_terminated_with_creation_checks() {
        let child = Command::new("ping.exe")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        let child = Own(child);
        let pid = child.0.id();
        let info = process::process_info(pid).unwrap();
        assert_eq!(info.own_user, Some(true));
        assert!(freeze_process(pid, info.started_at - 1, &[]).is_err());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut original = suspension_state(pid, info.started_at).unwrap();
        assert!(!original.is_empty());
        // Recovery before any change must be harmless.
        thaw_process(pid, info.started_at, &original).unwrap();
        let mut result = freeze_process(pid, info.started_at, &original);
        for _ in 0..10 {
            if result.is_ok() {
                break;
            }
            assert!(
                result.as_ref().unwrap_err().message.contains("thread"),
                "{result:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
            original = suspension_state(pid, info.started_at).unwrap();
            result = freeze_process(pid, info.started_at, &original);
        }
        result.unwrap();
        let frozen = suspension_state(pid, info.started_at).unwrap();
        for t in &original {
            assert_eq!(
                frozen
                    .iter()
                    .find(|f| f.tid == t.tid)
                    .unwrap()
                    .suspend_count,
                t.suspend_count + 1
            );
        }
        thaw_process(pid, info.started_at, &original).unwrap();
        thaw_process(pid, info.started_at, &original).unwrap();
        for t in suspension_state(pid, info.started_at).unwrap() {
            if let Some(o) = original.iter().find(|o| o.tid == t.tid) {
                assert_eq!(t.suspend_count, o.suspend_count);
            }
        }
        assert!(terminate_process(pid, info.started_at - 1).is_err());
        terminate_process(pid, info.started_at).unwrap();
    }
    #[test]
    fn executable_signatures_and_rules_use_real_full_paths() {
        let root = std::env::var("SystemRoot").unwrap();
        let path = format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe");
        assert_eq!(
            file_signer(&path).unwrap().as_deref(),
            Some("Microsoft Corporation")
        );
        let rules = program_rules().unwrap();
        assert!(rules.iter().any(|r| r.source == "builtin"));
        assert!(rules.iter().all(|r| r.path.contains('\\')));
        // Availability is reported explicitly on devices with no GPU counters.
        if let Ok(v) = video_memory() {
            assert!(v.used_mb < 1_000_000);
        }
    }
}

fn trusted_security_signer(name: &str, signer: Option<&str>) -> bool {
    let Some(signer) = signer else { return false };
    let signer = signer.to_ascii_lowercase();
    let vendors: &[&str] = match name {
        "msmpeng.exe"
        | "nissrv.exe"
        | "securityhealthservice.exe"
        | "securityhealthsystray.exe" => &["microsoft corporation"],
        "avp.exe" => &["kaspersky"],
        "avastsvc.exe" | "avgsvc.exe" => &["avast", "avg technologies", "gen digital"],
        "mbamservice.exe" => &["malwarebytes"],
        "ekrn.exe" => &["eset"],
        "bdservicehost.exe" => &["bitdefender"],
        "360tray.exe" => &["qihoo", "360 technology", "360 software"],
        "huorong.exe" | "hipsdaemon.exe" => &["huorong"],
        _ => &[],
    };
    vendors.iter().any(|vendor| signer.contains(vendor))
}
#[cfg(test)]
mod rule_identity_tests {
    use super::*;
    #[test]
    fn security_basename_alone_is_not_trusted() {
        assert!(!trusted_security_signer("avp.exe", None));
        assert!(!trusted_security_signer(
            "avp.exe",
            Some("Unrelated Software LLC")
        ));
        assert!(trusted_security_signer("avp.exe", Some("Kaspersky Lab")));
        assert!(!trusted_security_signer(
            "360tray.exe",
            Some("Microsoft Corporation")
        ));
    }
}
