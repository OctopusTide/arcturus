//! Short-lived, same-user launcher inheriting the failed agent's environment.
//! Credentials never leave this process's inherited environment: no snapshots, files or IPC fields.
//! The agent's start options stay here too: it reads them from the agent's own command line while
//! the agent still waits for the hook, and they may include a system prompt or a settings path.
use super::{ffi, process};
use daemon::sys::{AgentResume, ProcessInfo};
use daemon::transport::{Listener, Transport};
use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
};
const LIMIT: usize = 65536;
#[derive(Serialize, Deserialize)]
struct Config {
    agent: String,
    executable: String,
    session: String,
    daemon_endpoint: String,
    origin_pid: u32,
    transcript: Option<String>,
}
fn error(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(e.to_string())
}
fn endpoint(p: &ProcessInfo) -> String {
    format!(r"\\.\pipe\away-guard-resume-{}-{}", p.pid, p.started_at)
}
pub(crate) fn session_id() -> Result<String, ApiError> {
    let mut id = windows_sys::core::GUID::default();
    let code = unsafe { windows_sys::Win32::System::Rpc::UuidCreate(&mut id) };
    if code != 0 && code != 1824 {
        return Err(error(format!("UUID: {code}")));
    }
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        id.data1,
        id.data2,
        id.data3,
        id.data4[0],
        id.data4[1],
        id.data4[2],
        id.data4[3],
        id.data4[4],
        id.data4[5],
        id.data4[6],
        id.data4[7]
    ))
}
/// One argument as the C runtime reads it back.
fn quote_argument(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        out.push_str(&"\\".repeat(if c == '"' { slashes * 2 + 1 } else { slashes }));
        slashes = 0;
        out.push(c);
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}
/// `Command` lets a child inherit every inheritable handle, which here includes the pipes the
/// agent reads this hook's output from and whatever was inherited from further up. The broker
/// outlives the hook by up to two hours, so it must hold none of them.
///
/// It must also leave the agent's jobs. Claude Code's runtime keeps a job that ends its members
/// when the agent exits and lets their children out silently, but under task protection the hook
/// is in a job nested inside that one, where a child only gets out by asking. Staying in there
/// would also keep the protection going after the agent is gone. A job that refuses leaves the
/// broker where it is: if it dies with the agent, the queue says so instead of continuing.
fn spawn_without_handles(exe: &Path, args: &str) -> Result<u32, ApiError> {
    let application = ffi::to_wide(exe);
    let line = ffi::to_wide(format!("{} {args}", quote_argument(&exe.to_string_lossy())));
    // SAFETY: plain data, all zero is valid.
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    // SAFETY: as above.
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut create = |flags| {
        let mut line = line.clone();
        // SAFETY: both strings are NUL-terminated and `line` is writable; null keeps this
        // process's environment and directory; bInheritHandles is FALSE.
        unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                0,
                flags,
                null(),
                null(),
                &startup,
                &mut info,
            )
        }
    };
    if create(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB) == 0 && create(CREATE_NO_WINDOW) == 0 {
        return Err(ffi::os_error("CreateProcessW"));
    }
    // SAFETY: owned handles from the successful call, closed once; the broker keeps running.
    unsafe {
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(info.dwProcessId)
}
/// The agent this hook runs under: the nearest ancestor that is its program, same user.
fn origin_of_this_hook(adapter: &dyn agents::Adapter) -> Result<ProcessInfo, ApiError> {
    let list = process::processes()?;
    let mut at = std::process::id();
    let mut origin = None;
    for _ in 0..32 {
        let Some(p) = list.iter().find(|p| p.pid == at) else {
            break;
        };
        at = p.parent_pid;
        if let Some(p) = list.iter().find(|p| p.pid == at) {
            if adapter
                .info()
                .executables
                .iter()
                .any(|name| p.name.eq_ignore_ascii_case(name))
            {
                origin = Some(process::process_info(at)?);
                break;
            }
        }
    }
    origin
        .filter(|p| p.own_user == Some(true))
        .ok_or_else(|| error("cannot verify original agent"))
}
/// Whether the agent this hook runs under is a session a person sits at. None: cannot tell.
pub fn agent_runs_in_a_window(agent: &str) -> Option<bool> {
    let adapter = agents::get(agent)?;
    let origin = origin_of_this_hook(adapter).ok()?;
    let command_line = process::process_command_line(origin.pid).ok()?;
    Some(adapter.runs_in_a_window(&command_line))
}
pub fn create_resume_broker(
    agent: &str,
    session: &str,
    daemon_endpoint: &str,
    transcript: Option<&str>,
) -> Result<ProcessInfo, ApiError> {
    let adapter = agents::get(agent).ok_or_else(|| error("unknown adapter"))?;
    let origin = origin_of_this_hook(adapter)?;
    let config = Config {
        agent: agent.into(),
        origin_pid: origin.pid,
        executable: origin.exe.ok_or_else(|| error("agent image missing"))?,
        session: session.into(),
        daemon_endpoint: daemon_endpoint.into(),
        transcript: transcript.map(str::to_owned),
    };
    let broker = spawn_without_handles(
        &std::env::current_exe().map_err(error)?,
        &format!(
            "--resume-broker {}",
            quote_argument(&serde_json::to_string(&config).map_err(error)?)
        ),
    )?;
    let identity = process::process_info(broker)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ffi::pipe_exists(&endpoint(&identity)) {
        if Instant::now() >= deadline {
            return Err(error("continuation broker did not start"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(identity)
}
pub fn run_resume_broker(config: &str) -> Result<(), ApiError> {
    let config: Config = serde_json::from_str(config).map_err(error)?;
    let me = process::process_info(std::process::id())?;
    let adapter = agents::get(&config.agent).ok_or_else(|| error("unknown adapter"))?;
    // Read now: the hook waits for this broker's pipe, so the agent has not gone on yet. Without
    // it the continuation could not be set up like the original, so there is no broker at all.
    let origin = process::process_info(config.origin_pid)?;
    if origin.own_user != Some(true) || origin.exe.as_deref() != Some(config.executable.as_str()) {
        return Err(error("original agent changed before its broker started"));
    }
    let started_with = adapter.run_settings(&process::process_command_line(origin.pid)?);
    let expected = std::env::current_exe()
        .map_err(error)?
        .with_file_name(format!("{}.exe", proto::names::DAEMON_BIN));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(error)?;
    rt.block_on(async {
        let mut listener = super::PipeTransport
            .bind(&endpoint(&me))
            .map_err(|e| error(format!("{e:?}")))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120 * 60 + 30);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            let (mut pipe, peer) = match tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
            {
                Ok(result) => result.map_err(error)?,
                Err(_) if !ffi::pipe_exists(&config.daemon_endpoint) => return Ok(()),
                Err(_) => continue,
            };
            let allowed = peer
                .pid
                .and_then(|pid| process::process_info(pid).ok())
                .is_some_and(|p| {
                    p.own_user == Some(true)
                        && p.exe
                            .is_some_and(|path| path.eq_ignore_ascii_case(&expected.to_string_lossy()))
                });
            if !allowed {
                continue;
            }
            let operation = async {
                let n = pipe.read_u32_le().await? as usize;
                if n > LIMIT {
                    return Err(io::Error::other("oversized launch request"));
                }
                let mut data = vec![0; n];
                pipe.read_exact(&mut data).await?;
                let request: AgentResume = serde_json::from_slice(&data).map_err(io::Error::other)?;
                if request.agent != config.agent
                    || request.executable != config.executable
                    || request.session_id != config.session
                {
                    return Err(io::Error::other(
                        "broker is restricted to the original agent and conversation",
                    ));
                }
                let mut settings = started_with.clone();
                if request.permission_mode.is_some() {
                    settings.permission_mode.clone_from(&request.permission_mode);
                }
                if let Some(transcript) = &config.transcript {
                    adapter.refine_from_transcript(&mut settings, Path::new(transcript));
                }
                let result = super::resume::launch_inherited(&request, &settings);
                let data = serde_json::to_vec(&result).map_err(io::Error::other)?;
                pipe.write_u32_le(data.len() as u32).await?;
                pipe.write_all(&data).await?;
                pipe.flush().await?;
                Ok::<_, io::Error>(())
            };
            tokio::time::timeout(Duration::from_secs(10), operation)
                .await
                .map_err(error)?
                .map_err(error)?;
            return Ok(());
        }
    })
}
pub(crate) fn launch(request: &AgentResume, broker: &ProcessInfo) -> Result<ProcessInfo, ApiError> {
    let live = process::process_info(broker.pid)?;
    if live != *broker {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "continuation broker identity changed",
        ));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(error)?;
    rt.block_on(async {
        let operation = async {
            let mut pipe = tokio::net::windows::named_pipe::ClientOptions::new()
                .open(endpoint(broker))
                .map_err(error)?;
            if ffi::pipe_server_pid(pipe.as_raw_handle()) != Some(broker.pid) {
                return Err(error("broker pipe owner changed"));
            }
            let data = serde_json::to_vec(request).map_err(error)?;
            pipe.write_u32_le(data.len() as u32).await.map_err(error)?;
            pipe.write_all(&data).await.map_err(error)?;
            let n = pipe.read_u32_le().await.map_err(error)? as usize;
            if n > LIMIT {
                return Err(error("oversized broker reply"));
            }
            let mut data = vec![0; n];
            pipe.read_exact(&mut data).await.map_err(error)?;
            serde_json::from_slice::<Result<ProcessInfo, ApiError>>(&data).map_err(error)?
        };
        tokio::time::timeout(Duration::from_secs(10), operation)
            .await
            .map_err(error)?
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_quoted_argument_reads_back_exactly() {
        let wide = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        for arg in [
            "plain",
            "with space",
            r#"{"path":"C:\\dir with space\\","q":"a\"b"}"#,
            r"ends with backslash\",
            r#"both\"#,
            "中文 提示",
        ] {
            let line = format!("{} {}", quote_argument(r"C:\a b\x.exe"), quote_argument(arg));
            let argv = process::split_command_line(&wide(&line)).unwrap();
            assert_eq!(argv, [r"C:\a b\x.exe", arg]);
        }
    }
    #[test]
    fn this_process_reads_its_own_command_line() {
        let argv = process::process_command_line(std::process::id()).unwrap();
        assert_eq!(argv.len(), std::env::args().count());
        assert!(argv[1..].iter().zip(std::env::args().skip(1)).all(|(a, b)| *a == b));
    }

    const IN_JOBS: &str = "AWAY_GUARD_TEST_STARTS_FROM_INSIDE_JOBS";

    /// Not a test by itself: the process the next test puts in jobs, standing in for a hook.
    #[test]
    #[ignore = "run by a_broker_outlives_the_jobs_of_the_hook_that_started_it"]
    fn start_a_process_from_inside_the_jobs() {
        use std::io::{BufRead, Write};
        if std::env::var_os(IN_JOBS).is_none() {
            return;
        }
        let mut told = String::new();
        std::io::stdin().lock().read_line(&mut told).unwrap();
        let ping = std::env::var("SystemRoot").unwrap() + r"\System32\ping.exe";
        let pid = spawn_without_handles(Path::new(&ping), "-n 60 127.0.0.1").unwrap();
        let mut out = std::io::stdout();
        writeln!(out, "started={pid}").unwrap();
        out.flush().unwrap();
        // Waits for the job to end it, like a hook whose agent is about to exit.
        told.clear();
        let _ = std::io::stdin().lock().read_line(&mut told);
    }

    /// A hook under a protected agent, whose runtime keeps its own job that ends its members when
    /// the agent exits and lets their children out silently. Protection nests a job inside that
    /// one, and a child there only gets out by asking.
    #[test]
    fn a_broker_outlives_the_jobs_of_the_hook_that_started_it() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::windows::io::AsRawHandle;
        use std::process::{Command, Stdio};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        let mut hook = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "windows::resume_broker::tests::start_a_process_from_inside_the_jobs",
            ])
            .env(IN_JOBS, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // SAFETY: anonymous job, the struct matches the information class, all handles are live.
        let runtime = unsafe {
            let job = CreateJobObjectW(null(), null());
            assert!(!job.is_null());
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK
                | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
                | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            assert_ne!(
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ),
                0
            );
            assert_ne!(AssignProcessToJobObject(job, hook.as_raw_handle().cast()), 0);
            job
        };
        let protection = process::track_tree(hook.id(), &[]).unwrap();
        let mut stdin = hook.stdin.take().unwrap();
        writeln!(stdin, "go").unwrap();
        let mut stdout = BufReader::new(hook.stdout.take().unwrap());
        let mut line = String::new();
        let broker: u32 = loop {
            line.clear();
            assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "the hook ended without starting anything");
            if let Some(pid) = line.trim().strip_prefix("started=") {
                break pid.parse().unwrap();
            }
        };
        let identity = process::process_info(broker).unwrap();
        // The agent exits: the runtime's last handle to its job goes, and the hook with it.
        // SAFETY: owned handle, closed once.
        unsafe { CloseHandle(runtime) };
        hook.wait().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let after = process::process_info(broker);
        let in_protection = protection.members().unwrap();
        let _ = Command::new("taskkill").args(["/F", "/PID", &broker.to_string()]).output();
        assert_eq!(after.ok().as_ref(), Some(&identity), "the broker died with the agent");
        assert!(in_protection.is_empty(), "it would keep the protection alive: {in_protection:?}");
    }
}
