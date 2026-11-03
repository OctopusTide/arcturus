//! Finding installed agents and their versions. Running `<agent> --version` is the only
//! thing here that starts a program; it changes nothing.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::{Adapter, Env};

#[derive(Debug, Clone, Serialize)]
pub struct Installation {
    pub agent: &'static str,
    pub path: PathBuf,
    /// The version the program reports.
    pub version: Option<String>,
    /// Inside the adapter's tested range.
    pub recognised: bool,
    /// Why there is no recognised version.
    pub problem: Option<String>,
}

/// Executables of `adapter` on `PATH` and in its usual install locations (deduplicated, in
/// that order). Does not run them.
pub fn candidates(adapter: &dyn Adapter, env: &Env) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let exts: Vec<String> = if cfg!(windows) {
        let pathext = env.var("PATHEXT").unwrap_or(".EXE");
        pathext.split(';').filter(|e| !e.is_empty()).map(|e| e.to_lowercase()).collect()
    } else {
        vec![String::new()]
    };
    for dir in env.path_dirs() {
        for exe in adapter.info().executables {
            let p = dir.join(exe);
            let is_exe_name = std::path::Path::new(exe).extension().is_some();
            if is_exe_name && p.is_file() {
                out.push(p);
            } else if !is_exe_name {
                for ext in &exts {
                    let q = dir.join(format!("{exe}{ext}"));
                    if q.is_file() && !ext.is_empty() && ext != ".exe" {
                        // .cmd / .ps1 shims (npm installs) are listed too.
                        out.push(q);
                    }
                }
            }
        }
    }
    out.extend(adapter.install_locations(env));
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|p| seen.insert(p.to_string_lossy().to_lowercase()));
    out
}

/// Runs each candidate's version command (at most `timeout` each).
pub fn installations(adapter: &dyn Adapter, env: &Env, timeout: Duration) -> Vec<Installation> {
    candidates(adapter, env)
        .into_iter()
        .map(|path| {
            let info = adapter.info();
            match run_version(&path, adapter.version_args(), timeout) {
                Ok(out) => match adapter.parse_version_output(&out) {
                    Some(v) => {
                        let recognised = info.tested.contains(&v);
                        let problem = (!recognised)
                            .then(|| format!("version {v} is not recognised (tested {} to {})", info.tested.min, info.tested.max));
                        Installation { agent: info.id, path, version: Some(v.raw), recognised, problem }
                    }
                    None => Installation {
                        agent: info.id,
                        path,
                        version: None,
                        recognised: false,
                        problem: Some(format!("version output not recognised: {}", out.trim().chars().take(80).collect::<String>())),
                    },
                },
                Err(e) => Installation { agent: info.id, path, version: None, recognised: false, problem: Some(e) },
            }
        })
        .collect()
}

fn run_version(path: &std::path::Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let is_script = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false);
    let mut cmd = if is_script {
        let mut c = Command::new("cmd");
        c.arg("/d").arg("/c").arg(path);
        c
    } else {
        Command::new(path)
    };
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", path.display()))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} did not answer within {} s", path.display(), timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        use std::io::Read;
        let _ = s.read_to_string(&mut out);
    }
    Ok(out)
}
