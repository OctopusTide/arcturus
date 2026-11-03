//! The Windows transport and client over real named pipes (test-only pipe names, fake
//! system calls inside the daemon). Nothing here changes the system.

#![cfg(windows)]

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use daemon::transport::{Listener, Transport};
use daemon::{BackgroundDaemon, DaemonConfig, DaemonError, TrustPolicy};
use platform::PipeTransport;
use proto::client::Client;
use proto::{ClientInfo, ErrorCode, subcode};
use serde_json::{Value, json};

const UI_EXE: &str = r"C:\test\away-guard-ui.exe";
const WAIT: Duration = Duration::from_secs(5);

fn endpoint(tag: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(r"\\.\pipe\away-guard-test-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

fn info() -> ClientInfo {
    ClientInfo { name: "pipe-test".into(), version: None }
}

struct Running {
    _dir: tempfile::TempDir,
    endpoint: String,
    daemon: BackgroundDaemon,
}

/// A daemon on a test pipe. With `trust_self`, this test process looks like our page.
fn start(tag: &str, trust_self: bool) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let (sys, system, _) = daemon::sys::fake::sys();
    if trust_self {
        system.processes.lock().unwrap().insert(std::process::id(), UI_EXE.into());
    }
    let endpoint = endpoint(tag);
    let config = DaemonConfig {
        endpoint: endpoint.clone(),
        data_dir: dir.path().to_owned(),
        config_file: platform::paths::config_file(dir.path()),
        sys,
        sqlite: platform::Winsqlite3::load().unwrap(),
        trust: TrustPolicy::trusting([UI_EXE]),
    };
    let daemon = daemon::spawn(PipeTransport, config, daemon::modules::builtin()).unwrap();
    Running { _dir: dir, endpoint, daemon }
}

fn raw(endpoint: &str) -> (BufReader<std::fs::File>, std::fs::File) {
    let f = OpenOptions::new().read(true).write(true).open(endpoint).unwrap();
    (BufReader::new(f.try_clone().unwrap()), f)
}

fn exchange(r: &mut BufReader<std::fs::File>, w: &mut std::fs::File, line: &str) -> Value {
    w.write_all(line.as_bytes()).unwrap();
    w.write_all(b"\n").unwrap();
    let mut reply = String::new();
    r.read_line(&mut reply).unwrap();
    serde_json::from_str(&reply).unwrap()
}

#[test]
fn handshake_and_calls_over_a_real_pipe() {
    let d = start("hs", false);
    let mut c = platform::connect_client(&d.endpoint, info(), WAIT).unwrap();
    let h = c.hello();
    assert_eq!(h.protocol, 1);
    assert_eq!(h.daemon.pid, std::process::id());
    // The daemon learns the caller from the pipe (GetNamedPipeClientProcessId), not the client.
    assert_eq!(h.caller.pid, Some(std::process::id()));
    assert_eq!(h.caller.kind, "agent");
    assert!(h.command("protect").is_some());
    let resp = c.call("status", json!({})).unwrap();
    assert!(resp.ok);
    assert_eq!(resp.result.unwrap()["daemon"]["endpoint"], d.endpoint.as_str());
    let resp = c.call("free", json!({})).unwrap();
    assert!(resp.ok);
    d.daemon.stop().unwrap();
}

#[test]
fn page_caller_is_recognised_by_its_executable() {
    let d = start("trust", true);
    let c = platform::connect_client(&d.endpoint, info(), WAIT).unwrap();
    assert_eq!(c.hello().caller.kind, "trusted");
    assert!(c.hello().command("permission.set").unwrap().callable);
}

#[test]
fn pipe_is_restricted_to_the_current_user() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let d = start("dacl", false);
    let f = OpenOptions::new().read(true).write(true).open(&d.endpoint).unwrap();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut text: *mut u16 = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: valid handle and out-pointers; both buffers are freed with LocalFree.
    let sddl = unsafe {
        let rc = GetSecurityInfo(
            f.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        assert_eq!(rc, 0, "GetSecurityInfo");
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            &mut len,
        );
        assert_ne!(ok, 0);
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len as usize));
        LocalFree(text.cast());
        LocalFree(sd);
        s
    };
    let sddl = sddl.trim_end_matches('\0');
    let sid = platform::paths::default_endpoint().unwrap().rsplit_once("away-guard-").unwrap().1.to_owned();
    assert!(sid.starts_with("S-1-5-"), "{sid}");
    // Protected DACL, exactly one entry, an allow for this user.
    assert!(sddl.starts_with("D:P("), "{sddl}");
    assert_eq!(sddl.matches('(').count(), 1, "{sddl}");
    assert!(sddl.contains("(A;;") && sddl.ends_with(&format!(";;;{sid})")), "{sddl}");
}

#[test]
fn second_daemon_on_the_same_pipe_is_refused() {
    let d = start("twice", false);
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        endpoint: d.endpoint.clone(),
        data_dir: dir.path().to_owned(),
        config_file: platform::paths::config_file(dir.path()),
        sys: daemon::sys::fake::sys().0,
        sqlite: platform::Winsqlite3::load().unwrap(),
        trust: TrustPolicy::none(),
    };
    match daemon::spawn(PipeTransport, config, daemon::modules::builtin()) {
        Err(DaemonError::AlreadyRunning(_)) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("two daemons on one pipe"),
    }
    // The first one is unaffected.
    assert!(platform::connect_client(&d.endpoint, info(), WAIT).is_ok());
}

#[test]
fn nobody_listening_is_daemon_not_running() {
    let e = platform::connect_client(&endpoint("absent"), info(), WAIT).err().unwrap();
    assert_eq!(e.code, ErrorCode::DaemonNotRunning);
    assert_eq!(e.exit_code(), 3);
}

#[test]
fn raw_protocol_lines() {
    let d = start("raw", false);
    let (mut r, mut w) = raw(&d.endpoint);
    let v = exchange(&mut r, &mut w, r#"{"v":1,"id":"a","cmd":"status"}"#);
    assert_eq!(v["error"]["subcode"], subcode::HANDSHAKE_REQUIRED);
    let v = exchange(&mut r, &mut w, "this is not json");
    assert_eq!(v["error"]["code"], "BAD_REQUEST");
    let v = exchange(&mut r, &mut w, r#"{"v":1,"id":"h","cmd":"core.hello","args":{"protocols":[1]}}"#);
    assert_eq!(v["ok"], true);
    let v = exchange(&mut r, &mut w, r#"{"v":1,"id":"b","cmd":"frobnicate"}"#);
    assert_eq!((v["id"].as_str(), v["error"]["code"].as_str()), (Some("b"), Some("UNKNOWN_COMMAND")));

    // A line over the limit is refused and the connection closed.
    let (mut r, mut w) = raw(&d.endpoint);
    w.write_all(&vec![b'x'; proto::protocol::MAX_LINE_BYTES + 10]).unwrap();
    let mut reply = String::new();
    r.read_line(&mut reply).unwrap();
    let v: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(v["error"]["code"], "BAD_REQUEST");
    reply.clear();
    assert_eq!(r.read_line(&mut reply).unwrap_or(0), 0, "connection closed");
}

/// A stand-in daemon that answers the first request with `reply(request)`.
fn scripted_daemon(endpoint: &str, reply: fn(&Value) -> Value) -> std::thread::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let endpoint = endpoint.to_owned();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let mut listener = PipeTransport.bind(&endpoint).unwrap();
            ready_tx.send(()).unwrap();
            let (conn, _) = listener.accept().await.unwrap();
            let mut conn = tokio::io::BufReader::new(conn);
            let mut line = String::new();
            conn.read_line(&mut line).await.unwrap();
            let out = format!("{}\n", reply(&serde_json::from_str(&line).unwrap()));
            conn.get_mut().write_all(out.as_bytes()).await.unwrap();
            line.clear();
            let _ = conn.read_line(&mut line).await; // until the client hangs up
        });
    });
    ready_rx.recv().unwrap();
    t
}

#[test]
fn version_mismatch_in_either_direction_is_a_clear_error() {
    // A daemon from before the handshake existed.
    let ep = endpoint("old");
    let t = scripted_daemon(&ep, |req| {
        json!({"v": 1, "id": req["id"], "ok": false, "error": {"code": "UNKNOWN_COMMAND", "message": "unknown command"}})
    });
    let e = platform::connect_client(&ep, info(), WAIT).err().unwrap();
    assert_eq!((e.code, e.subcode.as_deref()), (ErrorCode::ProtocolVersion, Some(subcode::DAEMON_TOO_OLD)));
    assert_eq!(e.exit_code(), 5);
    t.join().unwrap();

    // A daemon that only speaks newer versions.
    let ep = endpoint("new");
    let t = scripted_daemon(&ep, |req| {
        json!({"v": 3, "id": req["id"], "ok": false, "error": {
            "code": "PROTOCOL_VERSION", "subcode": "core.CLIENT_TOO_OLD", "message": "daemon speaks 3..=4"}})
    });
    let e = platform::connect_client(&ep, info(), WAIT).err().unwrap();
    assert_eq!(e.subcode.as_deref(), Some(subcode::CLIENT_TOO_OLD));
    t.join().unwrap();

    // The real daemon, against a client offering only versions it does not speak.
    let d = start("ver", false);
    let stream = platform::connect(&d.endpoint, WAIT).unwrap();
    let e = Client::handshake_with(stream, info(), &[2]).err().unwrap();
    assert_eq!(e.subcode.as_deref(), Some(subcode::DAEMON_TOO_OLD));
    let stream = platform::connect(&d.endpoint, WAIT).unwrap();
    let e = Client::handshake_with(stream, info(), &[0]).err().unwrap();
    assert_eq!(e.subcode.as_deref(), Some(subcode::CLIENT_TOO_OLD));
}

#[test]
fn real_system_layer_reads_without_changing_anything() {
    use daemon::sys::{Elevated, System};
    let s = platform::WindowsSystem;
    let exe = std::env::current_exe().unwrap();
    assert_eq!(s.process_image_path(std::process::id()).map(std::path::PathBuf::from), Some(exe));
    assert!(s.uptime_ms().unwrap() > 0);
    // Today's administrator layer refuses everything and does nothing.
    let e = platform::UserModeElevated.terminate_process(u32::MAX).unwrap_err();
    assert_eq!((e.code, e.subcode.as_deref()), (ErrorCode::Denied, Some(subcode::NEEDS_ELEVATION)));
    assert_eq!(platform::UserModeElevated.backend(), "user");
}
