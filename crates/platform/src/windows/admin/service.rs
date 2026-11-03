use super::super::ffi;
use super::transport::Config;
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SERVICE_DOES_NOT_EXIST, GetLastError, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, INHERIT_ONLY_ACE,
    OWNER_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
};
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

struct ServiceHandle(SC_HANDLE);
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
fn error(op: &str) -> String {
    format!("{op}: {}", std::io::Error::last_os_error())
}
fn sid_string(sid: *mut c_void) -> Result<String, String> {
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(error("read security principal"));
    }
    let result = unsafe { ffi::from_wide_ptr(text) };
    unsafe {
        LocalFree(text.cast());
    }
    Ok(result)
}
/// Refuse user-writable owners/ACLs and reparse points throughout the install path.
/// Path authentication is not safe if a caller can replace a parent directory.
pub fn verify_secure_chain(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.to_string_lossy().starts_with(r"\\") {
        return Err("service directory must be an absolute local path".into());
    }
    for part in path.ancestors() {
        if !part.exists() {
            continue;
        }
        let attributes = unsafe { GetFileAttributesW(ffi::to_wide(part).as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "reparse/unreadable service ancestor: {}",
                part.display()
            ));
        }
        let mut owner = null_mut();
        let mut acl: *mut ACL = null_mut();
        let mut descriptor = null_mut();
        let mut name = ffi::to_wide(part);
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_mut_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        if code != 0 {
            return Err(format!(
                "cannot inspect service ACL {}: {code}",
                part.display()
            ));
        }
        struct Descriptor(*mut c_void);
        impl Drop for Descriptor {
            fn drop(&mut self) {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
        let _descriptor = Descriptor(descriptor);
        let privileged = |sid: &str| {
            sid == "S-1-5-18"
                || sid == "S-1-5-32-544"
                || sid == "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
        };
        if owner.is_null() || !privileged(&sid_string(owner)?) || acl.is_null() {
            return Err(format!(
                "service ancestor must be owned by Administrators/SYSTEM with a DACL: {}",
                part.display()
            ));
        }
        for index in 0..unsafe { (*acl).AceCount } {
            let mut ace = null_mut();
            if unsafe { GetAce(acl, index as u32, &mut ace) } == 0 {
                return Err(error("read service ACE"));
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
                if [5, 9, 11].contains(&header.AceType) {
                    return Err("object/callback allow ACE on service path is not supported".into());
                }
                continue;
            }
            let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let principal = sid_string((&ace.SidStart as *const u32).cast_mut().cast())?;
            if privileged(&principal) {
                continue;
            }
            // Generic all/write, write owner/DACL, delete-child, and target file content writes.
            let mut dangerous = 0x10000000u32 | 0x40000000 | 0x00040000 | 0x00080000 | 0x00000040;
            if part.parent().is_some() {
                dangerous |= 0x00010000;
            }
            if part.is_file() || part == path {
                dangerous |= 0x00000002 | 0x00000004;
            }
            if ace.Mask & dangerous != 0 {
                return Err(format!(
                    "unprivileged principal can replace service path {}; choose a protected local directory",
                    part.display()
                ));
            }
        }
    }
    Ok(())
}
fn create_protected(path: &Path, sid: &str) -> Result<(), String> {
    if path.exists() {
        return verify_secure_chain(path);
    }
    let parent = path.parent().ok_or("deployment path has no parent")?;
    create_protected(parent, sid)?;
    let sddl = ffi::to_wide(format!(
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;{sid})"
    ));
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(error("build deployment ACL"));
    }
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(
            ffi::to_wide(path).as_ptr(),
            &attributes,
        )
    };
    let failure = if ok == 0 {
        Some(error(
            "create protected directory; concurrent creation refused",
        ))
    } else {
        None
    };
    unsafe {
        LocalFree(descriptor);
    }
    if let Some(e) = failure {
        return Err(e);
    }
    verify_secure_chain(path)
}

fn registered_image(service: &ServiceHandle) -> Result<std::path::PathBuf, String> {
    let mut needed = 0;
    unsafe {
        QueryServiceConfigW(service.0, null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 65536 {
        return Err("invalid service registration size".into());
    }
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    let config = storage.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    if unsafe { QueryServiceConfigW(service.0, config, needed, &mut needed) } == 0 {
        return Err(error("read registered service image"));
    }
    let config = unsafe { &*config };
    if !unsafe { ffi::from_wide_ptr(config.lpServiceStartName) }.eq_ignore_ascii_case("LocalSystem")
        || config.dwServiceType != SERVICE_WIN32_OWN_PROCESS
    {
        return Err("administrator service must be an own-process LocalSystem service".into());
    }
    let command = unsafe { ffi::from_wide_ptr(config.lpBinaryPathName) };
    let exe = command
        .strip_prefix('"')
        .and_then(|s| s.split_once('"').map(|(exe, _)| exe))
        .ok_or("unquoted registered service image")?;
    let path = std::path::PathBuf::from(exe);
    verify_secure_chain(&path)?;
    Ok(path)
}

pub(super) fn registered_directory(sid: &str) -> Result<std::path::PathBuf, String> {
    let service = open(sid, SERVICE_QUERY_CONFIG)?;
    let image = registered_image(&service)?;
    if image
        .file_name()
        .is_none_or(|n| !n.eq_ignore_ascii_case(&format!("{}.exe", proto::names::ADMIN_BIN)))
    {
        return Err("unexpected registered administrator executable".into());
    }
    image
        .parent()
        .map(Path::to_path_buf)
        .ok_or("registered image has no directory".into())
}

fn scm(access: u32) -> Result<ServiceHandle, String> {
    let h = unsafe { OpenSCManagerW(null(), null(), access) };
    if h.is_null() {
        return Err(if unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
            "open an elevated terminal (one UAC authorization) to install or remove the administrator service".into()
        } else {
            error("open service manager")
        });
    }
    Ok(ServiceHandle(h))
}
fn open(sid: &str, access: u32) -> Result<ServiceHandle, String> {
    let manager = scm(SC_MANAGER_CONNECT)?;
    let h = unsafe {
        OpenServiceW(
            manager.0,
            ffi::to_wide(super::service_name(sid)).as_ptr(),
            access,
        )
    };
    if h.is_null() {
        return Err(error("open administrator service"));
    }
    Ok(ServiceHandle(h))
}

/// SCM's protected registration is the authority when a normal user cannot query a
/// LocalSystem process token. The pipe PID must be the registered own-process service.
pub(super) fn verify_service_process(sid: &str, pid: u32, expected: &Path) -> Result<(), String> {
    let service = open(sid, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)?;
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(error("query authenticated service PID"));
    }
    if status.dwProcessId != pid
        || status.dwCurrentState != SERVICE_RUNNING
        || status.dwServiceType & SERVICE_WIN32_OWN_PROCESS == 0
    {
        return Err("pipe PID is not the registered administrator service".into());
    }
    unsafe {
        QueryServiceConfigW(service.0, null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 65536 {
        return Err("invalid service registration size".into());
    }
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    let config = storage.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    if unsafe { QueryServiceConfigW(service.0, config, needed, &mut needed) } == 0 {
        return Err(error("query authenticated service configuration"));
    }
    let config = unsafe { &*config };
    let account = unsafe { ffi::from_wide_ptr(config.lpServiceStartName) };
    if !account.eq_ignore_ascii_case("LocalSystem") {
        return Err("administrator service is not registered as LocalSystem".into());
    }
    let command = unsafe { ffi::from_wide_ptr(config.lpBinaryPathName) };
    let exe = command
        .strip_prefix('"')
        .and_then(|v| v.split_once('"').map(|(exe, _)| exe))
        .ok_or("unquoted administrator service image")?;
    let registered = std::fs::canonicalize(exe).map_err(|e| e.to_string())?;
    let expected = std::fs::canonicalize(expected).map_err(|e| e.to_string())?;
    if !registered
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy())
    {
        return Err("registered service executable differs from installed image".into());
    }
    Ok(())
}

/// Copies into a new protected deployment directory. Never changes an ancestor's ACL.
pub fn install(directory: &Path) -> Result<String, String> {
    let sid = ffi::current_user_sid().map_err(|e| e.to_string())?;
    if !super::valid_sid(&sid) {
        return Err("install from the intended desktop user account".into());
    }
    let manager = scm(SC_MANAGER_CREATE_SERVICE)?;
    let existing = directory
        .ancestors()
        .find(|p| p.exists())
        .ok_or("deployment volume does not exist")?;
    verify_secure_chain(existing)?;
    let registered = unsafe {
        OpenServiceW(
            manager.0,
            ffi::to_wide(super::service_name(&sid)).as_ptr(),
            SERVICE_QUERY_STATUS,
        )
    };
    if !registered.is_null() {
        drop(ServiceHandle(registered));
        return Err("remove the registered service before reinstalling".into());
    }
    if unsafe { GetLastError() } != ERROR_SERVICE_DOES_NOT_EXIST {
        return Err(error("check existing administrator service"));
    }
    if directory.exists() {
        verify_secure_chain(directory)?;
        let path = directory.join("service.json");
        if path.exists() {
            verify_secure_chain(&path)?;
            let saved: Config =
                serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if saved.user_sid != sid || saved.directory != directory {
                return Err("retained deployment belongs to another user/directory".into());
            }
        }
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let source = me.parent().ok_or("no binary directory")?;
    let required = [
        proto::names::ADMIN_BIN,
        proto::names::DAEMON_BIN,
        proto::names::LAUNCHER_BIN,
        proto::names::CLI_BIN,
    ];
    for name in required {
        if !source.join(format!("{name}.exe")).is_file() {
            return Err(format!("build {name}.exe next to the installer first"));
        }
    }
    create_protected(directory, &sid)?;
    verify_secure_chain(directory)?;
    for name in required
        .into_iter()
        .chain([proto::names::HOOK_BIN, proto::names::RELAY_BIN])
    {
        let source = source.join(format!("{name}.exe"));
        if !source.exists() {
            continue;
        }
        // A protected directory and create_new avoid following a destination link.
        let mut input = std::fs::File::open(source).map_err(|e| e.to_string())?;
        let temporary = directory.join(format!("{name}.installing"));
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
        drop(output);
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        if unsafe {
            MoveFileExW(
                ffi::to_wide(&temporary).as_ptr(),
                ffi::to_wide(directory.join(format!("{name}.exe"))).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(error("replace protected service binary"));
        }
    }
    let config = Config {
        user_sid: sid.clone(),
        directory: directory.into(),
    };
    let config_path = directory.join("service.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let command = format!(
        "\"{}\" run --config \"{}\"",
        config.service().display(),
        config_path.display()
    );
    let name = ffi::to_wide(super::service_name(&sid));
    let display = ffi::to_wide(format!("Arcturus Administrator ({sid})"));
    let h = unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_START | SERVICE_QUERY_STATUS | SERVICE_CHANGE_CONFIG | DELETE,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            ffi::to_wide(command).as_ptr(),
            null(),
            null_mut(),
            null(),
            null(),
            null(),
        )
    };
    if h.is_null() {
        return Err(error(
            "create administrator service; protected deployment retained",
        ));
    }
    let service = ServiceHandle(h);
    let mut actions = [
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 1000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 1000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 5000,
        },
    ];
    let mut failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86400,
        lpRebootMsg: null_mut(),
        lpCommand: null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&mut failure as *mut SERVICE_FAILURE_ACTIONSW).cast(),
        )
    } == 0
    {
        return Err(error("configure administrator service recovery"));
    }
    let mut flags = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: 1,
    };
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            (&mut flags as *mut SERVICE_FAILURE_ACTIONS_FLAG).cast(),
        )
    } == 0
    {
        return Err(error("configure recovery after service errors"));
    }
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        return Err(error("service installed but could not start"));
    }
    Ok(format!(
        "installed {}\nRun {} from this protected directory; clients discover it through the Service Control Manager.",
        super::service_name(&sid),
        config.daemon().display()
    ))
}
pub fn remove() -> Result<String, String> {
    let sid = ffi::current_user_sid().map_err(|e| e.to_string())?;
    let service = open(&sid, SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE)?;
    let mut status = SERVICE_STATUS::default();
    unsafe {
        ControlService(service.0, SERVICE_CONTROL_STOP, &mut status);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if unsafe { QueryServiceStatus(service.0, &mut status) } == 0 {
            return Err(error("query stopping service"));
        }
        if status.dwCurrentState == SERVICE_STOPPED {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err("service has not stopped/restored yet; removal cancelled".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if status.dwWin32ExitCode != 0 || status.dwServiceSpecificExitCode != 0 {
        return Err(format!(
            "service stopped with an error ({}/{}); removal cancelled so recovery can be retried",
            status.dwWin32ExitCode, status.dwServiceSpecificExitCode
        ));
    }
    super::super::exit_firewall::remove_all(&sid).map_err(|e| e.message)?;
    if unsafe { DeleteService(service.0) } == 0 {
        return Err(error("remove administrator service"));
    }
    Ok("service removed; protected binaries and audit/recovery files retained".into())
}
pub fn show() -> Result<String, String> {
    let sid = ffi::current_user_sid().map_err(|e| e.to_string())?;
    let service = open(&sid, SERVICE_QUERY_STATUS)?;
    let mut status = SERVICE_STATUS::default();
    if unsafe { QueryServiceStatus(service.0, &mut status) } == 0 {
        return Err(error("query administrator service"));
    }
    Ok(format!(
        "{} state={} (4=running, 1=stopped)",
        super::service_name(&sid),
        status.dwCurrentState
    ))
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static HANDLE: AtomicUsize = AtomicUsize::new(0);
fn report(state: u32, error: u32) {
    let handle = HANDLE.load(Ordering::SeqCst) as SERVICE_STATUS_HANDLE;
    if handle.is_null() {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: error,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING {
            1
        } else {
            0
        },
        dwWaitHint: 20_000,
    };
    unsafe {
        SetServiceStatus(handle, &status);
    }
}
unsafe extern "system" fn control(
    code: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    if code == SERVICE_CONTROL_STOP || code == SERVICE_CONTROL_SHUTDOWN {
        report(SERVICE_STOP_PENDING, 0);
        if let Some(stop) = STOP.get() {
            stop.store(true, Ordering::SeqCst);
        }
    }
    0
}
unsafe extern "system" fn main(_count: u32, _args: *mut *mut u16) {
    let config = CONFIG.get().unwrap();
    let name = ffi::to_wide(super::service_name(&config.user_sid));
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), null()) };
    if h.is_null() {
        return;
    }
    HANDLE.store(h as usize, Ordering::SeqCst);
    report(SERVICE_START_PENDING, 0);
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
        .and_then(|rt| {
            rt.block_on(super::transport::serve(
                config.clone(),
                STOP.get().unwrap().clone(),
                || report(SERVICE_RUNNING, 0),
            ))
        });
    if let Err(e) = &result {
        eprintln!("administrator service: {e}");
    }
    report(SERVICE_STOPPED, if result.is_ok() { 0 } else { 1 });
}
pub fn run(config: &Path) -> Result<(), String> {
    verify_secure_chain(config)?;
    let config: Config = serde_json::from_slice(&std::fs::read(config).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    config.validate()?;
    verify_secure_chain(&config.directory)?;
    if ffi::current_user_sid().map_err(|e| e.to_string())? != "S-1-5-18" {
        return Err("run is SCM-only and requires LocalSystem; use install instead".into());
    }
    let mut name = ffi::to_wide(super::service_name(&config.user_sid));
    CONFIG.set(config).map_err(|_| "already initialized")?;
    let _ = STOP.set(Arc::new(AtomicBool::new(false)));
    let entries = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(entries.as_ptr()) } == 0 {
        return Err(error("start service dispatcher"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn user_writable_deployment_is_rejected_without_changing_any_acl() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_secure_chain(dir.path()).is_err());
    }
    #[test]
    fn identity_is_per_user_and_private_endpoint_is_local() {
        assert!(super::super::valid_sid("S-1-5-21-1-2-3-1001"));
        assert!(!super::super::valid_sid("S-1-5-18"));
        assert!(!super::super::valid_sid("S-1-5-21-1;calc"));
        assert!(super::super::endpoint("S-1-5-21-1-2-3-1001").starts_with(r"\\.\pipe\"));
    }
}
