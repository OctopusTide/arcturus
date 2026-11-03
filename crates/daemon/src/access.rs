//! Who may call what. Each command declares whether agents may call it
//! ([`crate::module::CommandSpec::agent`]); the framework classifies every caller and checks
//! before a request reaches a module. Modules never do this check themselves.
//!
//! Classification uses the caller's executable as reported by the OS: our own page and tray
//! executables, installed next to the daemon, are `Trusted`; everything else (CLI, scripts,
//! agents) is `Agent`. Under one OS account this stops well-behaved agents only (see
//! ARCHITECTURE.md, 权限边界).

use std::path::{Path, PathBuf};

use proto::names;

use crate::module::CallerKind;

#[derive(Debug, Clone, Default)]
pub struct TrustPolicy {
    /// Lower-cased full paths of trusted executables.
    trusted: Vec<String>,
}

fn normalize(path: &str) -> String {
    path.to_lowercase()
}

impl TrustPolicy {
    /// Trusts nobody: every caller is an agent.
    pub fn none() -> Self {
        TrustPolicy::default()
    }

    /// Trusts exactly these executables.
    pub fn trusting<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> Self {
        TrustPolicy { trusted: paths.into_iter().map(|p| normalize(&p.as_ref().to_string_lossy())).collect() }
    }

    /// Trusts the page and tray executables in the same folder as the running daemon.
    pub fn beside_daemon() -> Self {
        let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) else {
            return TrustPolicy::none();
        };
        let exe = |name: &str| -> PathBuf { dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)) };
        TrustPolicy::trusting([exe(names::UI_BIN), exe(names::TRAY_BIN)])
    }

    pub fn classify(&self, exe: Option<&str>) -> CallerKind {
        match exe {
            Some(e) if self.trusted.contains(&normalize(e)) => CallerKind::Trusted,
            _ => CallerKind::Agent,
        }
    }
}
