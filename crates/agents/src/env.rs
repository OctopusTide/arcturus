//! Where to look: the home folder and the environment variables agents honour. Tests build
//! their own `Env` pointing at a temporary folder; nothing here reads the real machine except
//! [`Env::from_process`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Env {
    /// The user's home folder (`%USERPROFILE%` on Windows, `$HOME` elsewhere).
    pub home: PathBuf,
    /// Only the variables adapters care about (see [`RELEVANT`]); unset ones are absent.
    pub vars: BTreeMap<String, String>,
}

/// Variables an adapter may read. Adding an adapter that honours another one: add it here.
pub const RELEVANT: &[&str] =
    &["CLAUDE_CONFIG_DIR", "CODEX_HOME", "LOCALAPPDATA", "APPDATA", "PATH", "PATHEXT", "HOME", "USERPROFILE"];

impl Env {
    /// The running process's home folder and relevant variables.
    pub fn from_process() -> Env {
        let vars: BTreeMap<String, String> = RELEVANT
            .iter()
            .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| ((*k).to_owned(), v)))
            .collect();
        let home = vars
            .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .or_else(|| vars.get("HOME"))
            .map(PathBuf::from)
            .unwrap_or_default();
        Env { home, vars }
    }

    /// An environment rooted at `home` with no variables set.
    pub fn with_home(home: impl Into<PathBuf>) -> Env {
        Env { home: home.into(), vars: BTreeMap::new() }
    }

    pub fn set(mut self, key: &str, value: impl Into<String>) -> Env {
        self.vars.insert(key.to_owned(), value.into());
        self
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// `var` as a path, or `home` joined with `fallback`.
    pub fn dir_or_home(&self, var: &str, fallback: &str) -> PathBuf {
        match self.var(var) {
            Some(v) => PathBuf::from(v),
            None => self.home.join(fallback),
        }
    }

    /// `%LOCALAPPDATA%`, or `<home>\AppData\Local` when it is not set.
    pub fn local_app_data(&self) -> PathBuf {
        self.var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| self.home.join("AppData").join("Local"))
    }

    /// `%APPDATA%`, or `<home>\AppData\Roaming`.
    pub fn app_data(&self) -> PathBuf {
        self.var("APPDATA").map(PathBuf::from).unwrap_or_else(|| self.home.join("AppData").join("Roaming"))
    }

    /// Folders on `PATH`, in order.
    pub fn path_dirs(&self) -> Vec<PathBuf> {
        self.var("PATH").map(|p| std::env::split_paths(p).collect()).unwrap_or_default()
    }
}

/// Expands `*` path segments (one level each, no `**`) under `base`; results sorted.
pub fn glob(base: &Path, pattern: &[&str]) -> Vec<PathBuf> {
    let mut current = vec![base.to_path_buf()];
    for seg in pattern {
        let mut next = Vec::new();
        for dir in &current {
            if seg.contains('*') {
                let Ok(rd) = std::fs::read_dir(dir) else { continue };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if wildcard(seg, &name) {
                        next.push(e.path());
                    }
                }
            } else {
                let p = dir.join(seg);
                if p.exists() {
                    next.push(p);
                }
            }
        }
        current = next;
    }
    current.sort();
    current
}

/// `*` matches any run of characters; case-insensitive (file names on Windows are).
pub fn wildcard(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.to_lowercase(), name.to_lowercase());
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == n;
    }
    let mut rest = n.as_str();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matches_segments() {
        assert!(wildcard("rollout-*.jsonl", "rollout-2026-10-30T17-07-28-x.jsonl"));
        assert!(!wildcard("rollout-*.jsonl", "rollout-2026.jsonl.zst"));
        assert!(wildcard("*", "anything"));
        assert!(wildcard("Claude_*", "claude_pzs8"));
        assert!(wildcard("a*b*c", "aXXbYYc") && !wildcard("a*b*c", "aXXcYYb"));
    }

    #[test]
    fn glob_walks_star_segments() {
        let dir = tempfile::tempdir().unwrap();
        for p in ["x/1.0/a/tool.exe", "x/2.0/b/tool.exe", "x/2.0/b/other.exe"] {
            let p = dir.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"").unwrap();
        }
        let found = glob(dir.path(), &["x", "*", "*", "tool.exe"]);
        assert_eq!(found.len(), 2);
        assert!(found[1].ends_with("x/2.0/b/tool.exe") || found[1].ends_with(r"x\2.0\b\tool.exe"));
    }
}
