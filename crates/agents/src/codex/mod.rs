//! Codex (OpenAI's coding agent: the `codex` CLI, also what the Codex desktop app runs).
//!
//! - Executable: `codex.exe` (the desktop app keeps it in `%LOCALAPPDATA%\OpenAI\Codex\bin\<hash>\`;
//!   the app window itself is `ChatGPT.exe` from the `OpenAI.Codex` package), or an npm `codex.cmd`.
//! - Home: `%CODEX_HOME%`, else `~\.codex`.
//! - Transcripts ("rollouts"): `<home>\sessions\YYYY\MM\DD\rollout-<time>-<id>.jsonl`; after
//!   about a week Codex compresses them to `.jsonl.zst` (not read in this version). Archived
//!   ones are in `<home>\archived_sessions\`.
//! - Version: `codex --version` prints `codex-cli 0.165.0-alpha.1.2`; the rollout header
//!   (`session_meta`) has `cli_version`.
//! - Hooks: yes, `hooks.json` (see `hooks.rs`), but no failure event; resume:
//!   `codex exec resume <id> <prompt>`.

mod hooks;
mod transcript;

use std::path::{Path, PathBuf};

use crate::env::{Env, glob, wildcard};
use crate::hooks::Hooks;
use crate::model::{Encoding, ReadOptions, ReadOutcome, Record, TranscriptFile};
use crate::version::{Tested, Version};
use crate::{Adapter, AdapterInfo, HookSupport, RunSettings, stat};

pub const ID: &str = "codex";

/// Versions whose rollouts were read on this machine (2026-11-01).
pub const TESTED: Tested = Tested { min: "0.163.0-alpha.11.1", max: "0.165.0-alpha.1.2" };

static INFO: AdapterInfo = AdapterInfo {
    id: ID,
    display_name: "Codex",
    executables: &["codex.exe", "codex"],
    host_apps: &["chatgpt.exe"],
    tested: TESTED,
    transcripts: "<CODEX_HOME or ~/.codex>/sessions/YYYY/MM/DD/rollout-*.jsonl (older ones .jsonl.zst) and archived_sessions/, JSON lines",
    hooks: HookSupport {
        available: true,
        reports_api_errors: false,
        can_continue: true,
        notes: "Stop can continue the turn ({\"decision\":\"block\"}); there is no failure event and Stop may not run when \
                a turn fails. New hooks must be trusted in Codex (/hooks). Not yet tried on this machine.",
    },
};

pub struct Codex;

pub static ADAPTER: Codex = Codex;

pub(crate) fn root(env: &Env) -> PathBuf {
    env.dir_or_home("CODEX_HOME", ".codex")
}

impl Adapter for Codex {
    fn info(&self) -> &'static AdapterInfo {
        &INFO
    }

    fn transcripts(&self, env: &Env) -> std::io::Result<Vec<TranscriptFile>> {
        let home = root(env);
        let mut out = Vec::new();
        for day in glob(&home, &["sessions", "*", "*", "*"]) {
            rollouts(&day, &mut out);
        }
        rollouts(&home.join("archived_sessions"), &mut out);
        Ok(out)
    }

    fn read(
        &self,
        file: &TranscriptFile,
        from: u64,
        opts: ReadOptions,
        sink: &mut dyn FnMut(Record),
    ) -> std::io::Result<ReadOutcome> {
        transcript::read(file, from, opts, sink)
    }

    fn install_locations(&self, env: &Env) -> Vec<PathBuf> {
        let mut out = glob(&env.local_app_data(), &["OpenAI", "Codex", "bin", "*", "codex.exe"]);
        out.extend(glob(&env.app_data(), &["npm", "codex.cmd"]));
        out
    }

    fn parse_version_output(&self, output: &str) -> Option<Version> {
        // "codex-cli 0.165.0-alpha.1.2"
        let line = output.lines().find(|l| l.trim_start().starts_with("codex"))?;
        Version::parse(line.split_whitespace().nth(1)?)
    }

    fn hooks(&self) -> Option<&dyn Hooks> {
        Some(&hooks::CodexHooks)
    }

    // Not reached yet: without reliable API-error hooks Codex conversations are only notified
    // about, so nothing has measured which of its options a resume keeps.
    fn resume_args(&self, session_id: &str, prompt: &str, _settings: &RunSettings) -> Option<Vec<String>> {
        Some(vec!["exec".into(), "resume".into(), session_id.into(), prompt.into()])
    }
}

/// `rollout-*.jsonl` / `rollout-*.jsonl.zst` directly in `dir`.
fn rollouts(dir: &Path, out: &mut Vec<TranscriptFile>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let (stem, encoding) = if let Some(s) = name.strip_suffix(".jsonl") {
            (s, Encoding::Plain)
        } else if let Some(s) = name.strip_suffix(".jsonl.zst") {
            (s, Encoding::Zstd)
        } else {
            continue;
        };
        if !wildcard("rollout-*", stem) {
            continue;
        }
        let path = e.path();
        let Some((size, modified_ms)) = stat(&path) else { continue };
        out.push(TranscriptFile {
            agent: ID,
            conversation_id: thread_id(stem).to_owned(),
            subagent: None,
            path,
            size,
            modified_ms,
            encoding,
        });
    }
}

/// The thread id at the end of `rollout-<time>-<uuid>`, or the whole stem.
fn thread_id(stem: &str) -> &str {
    if stem.len() >= 36 {
        let tail = &stem[stem.len() - 36..];
        let ok = tail.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() });
        if ok {
            return tail;
        }
    }
    stem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_versions() {
        assert_eq!(
            thread_id("rollout-2026-10-30T17-07-28-01a18f48-65c4-7a83-ada9-5fb70f69a32e"),
            "01a18f48-65c4-7a83-ada9-5fb70f69a32e"
        );
        assert_eq!(thread_id("rollout-x"), "rollout-x");
        assert_eq!(ADAPTER.parse_version_output("codex-cli 0.165.0-alpha.1.2\n").unwrap().raw, "0.165.0-alpha.1.2");
        assert!(ADAPTER.parse_version_output("2.1.309 (Claude Code)").is_none());
    }
}
