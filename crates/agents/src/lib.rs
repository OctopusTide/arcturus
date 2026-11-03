//! Agent adapters. One adapter per agent *program* (not per model: a model used inside Claude
//! Code is Claude Code). Each adapter says:
//!
//! - which executables are the agent ([`AdapterInfo::executables`]);
//! - where its transcripts are and how to read them into the unified format in [`model`];
//! - whether it has hooks, how to install/remove ours, and how to read what a hook receives
//!   ([`hooks`]);
//! - how to tell its version, and which versions it was tested with.
//!
//! When a version or a record is not one the adapter knows, it stops and says so
//! ([`model::Stop`], [`hooks::HookError`]) instead of guessing.
//!
//! Adding an agent: a directory `src/<name>/` with `pub static ADAPTER`, plus one line in
//! `register_adapters!` below. Feature code uses [`all`] / [`get`] and the unified types only.

pub mod detect;
pub mod env;
pub mod hooks;
mod lenient;
mod lines;
pub mod model;
pub mod ojson;
pub mod version;

use std::path::Path;

use serde::Serialize;

pub use env::Env;
pub use hooks::{HookEvent, HookKind, Hooks};
pub use model::{ReadOptions, ReadOutcome, Record, Stop, TranscriptFile};
pub use version::{Tested, Version};

/// Static facts about an adapter.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AdapterInfo {
    /// Stable id, e.g. `claude-code`. Stored in databases: never rename.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Executable file names of the agent's command-line program, lower case, as process
    /// lists show them (`claude.exe`). For task protection and default allow lists.
    pub executables: &'static [&'static str],
    /// Desktop apps that host the agent (lower-case file names); also never to be closed.
    pub host_apps: &'static [&'static str],
    /// Versions this adapter was checked against.
    #[serde(skip)]
    pub tested: Tested,
    /// One line on where transcripts live and their format.
    pub transcripts: &'static str,
    pub hooks: HookSupport,
}

/// What the agent's hooks can do for us.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct HookSupport {
    pub available: bool,
    /// A turn that ended on a model-API error is reported to a hook, with the reason.
    pub reports_api_errors: bool,
    /// A hook at the end of a turn can make the agent continue.
    pub can_continue: bool,
    pub notes: &'static str,
}

pub trait Adapter: Sync {
    fn info(&self) -> &'static AdapterInfo;

    /// Transcript files, newest data not required to be complete (files being written are
    /// fine: readers only take whole lines). An absent folder is an empty list.
    fn transcripts(&self, env: &Env) -> std::io::Result<Vec<TranscriptFile>>;

    /// Reads `file` from byte `from`, handing each record to `sink`. Stops early (see
    /// [`ReadOutcome::stop`]) at a version or record it does not recognise.
    fn read(
        &self,
        file: &TranscriptFile,
        from: u64,
        opts: ReadOptions,
        sink: &mut dyn FnMut(Record),
    ) -> std::io::Result<ReadOutcome>;

    /// Places the agent's executable is usually installed, besides `PATH`.
    fn install_locations(&self, env: &Env) -> Vec<std::path::PathBuf>;

    /// Arguments that make the executable print its version.
    fn version_args(&self) -> &'static [&'static str] {
        &["--version"]
    }

    /// The version in the output of [`Adapter::version_args`], if it looks like this agent's.
    fn parse_version_output(&self, output: &str) -> Option<Version>;

    fn hooks(&self) -> Option<&dyn Hooks>;

    /// Arguments (after the executable) that continue conversation `session_id` with
    /// `prompt` without a window, if the agent can, set up as in `settings`.
    fn resume_args(&self, session_id: &str, prompt: &str, settings: &RunSettings) -> Option<Vec<String>>;
    fn fork_args(
        &self,
        _session_id: &str,
        _new_session: &str,
        _prompt: &str,
        _settings: &RunSettings,
    ) -> Option<Vec<String>> {
        None
    }

    /// What a running agent's command line (program name first) says about its setup. Only
    /// known start options are kept: a prompt on the command line stays out.
    fn run_settings(&self, _command_line: &[String]) -> RunSettings {
        RunSettings::default()
    }

    /// Corrects `settings.model` from the conversation's transcript: the user may have changed
    /// model after the agent started. Reads the model name only.
    fn refine_from_transcript(&self, _settings: &mut RunSettings, _transcript: &std::path::Path) {}

    /// Whether a run in this permission mode has nothing waiting for someone to approve it.
    fn runs_unattended(&self, _permission_mode: &str) -> bool {
        false
    }

    /// Whether a running agent's command line (program name first) is a session a person sits
    /// at, rather than a run that ends by itself.
    fn runs_in_a_window(&self, _command_line: &[String]) -> bool {
        false
    }

    /// Whether a session that ended for `reason`, as its hook says, was ended by a person.
    fn ended_by_the_user(&self, _reason: &str, _in_a_window: bool) -> bool {
        false
    }
}

/// How a run was set up, so that continuing it behaves the same.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSettings {
    /// As the agent's hooks report it, e.g. `acceptEdits`.
    pub permission_mode: Option<String>,
    pub model: Option<String>,
    /// Other start options, as they were given.
    pub options: Vec<String>,
}

macro_rules! register_adapters {
    ($($name:ident),* $(,)?) => {
        $(pub mod $name;)*

        static ALL: &[&dyn Adapter] = &[$(&$name::ADAPTER),*];

        /// Every adapter, in a fixed order.
        pub fn all() -> &'static [&'static dyn Adapter] {
            ALL
        }
    };
}

register_adapters! {
    claude_code,
    codex,
}

/// The adapter with this id.
pub fn get(id: &str) -> Option<&'static dyn Adapter> {
    all().iter().copied().find(|a| a.info().id == id)
}

/// The adapter whose executables include this file name (case-insensitive), e.g. for a
/// process found in a process tree.
pub fn by_executable(file_name: &str) -> Option<&'static dyn Adapter> {
    let name = file_name.to_lowercase();
    all()
        .iter()
        .copied()
        .find(|a| a.info().executables.contains(&name.as_str()))
}

/// The file name part of a path given with `\` or `/`.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Size and last write of a file as a transcript entry, or `None` when it is gone.
pub(crate) fn stat(path: &Path) -> Option<(u64, Option<i64>)> {
    let m = std::fs::metadata(path).ok()?;
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    Some((m.len(), modified))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique_and_lookups_work() {
        let ids: Vec<&str> = all().iter().map(|a| a.info().id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
        for a in all() {
            let i = a.info();
            assert!(
                i.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{}",
                i.id
            );
            assert!(get(i.id).is_some());
            for exe in i.executables {
                assert_eq!(*exe, exe.to_lowercase());
                assert_eq!(
                    by_executable(&exe.to_uppercase()).map(|x| x.info().id),
                    Some(i.id)
                );
            }
            assert!(Version::parse(i.tested.min).is_some() && Version::parse(i.tested.max).is_some());
            assert_eq!(a.hooks().is_some(), i.hooks.available);
        }
        assert_eq!(file_name(r"C:\a\b\claude.exe"), "claude.exe");
        assert_eq!(file_name("/usr/bin/codex"), "codex");
    }
}

pub fn safe_resume_env(key: &str, value: &str) -> bool {
    if value.len() > 8192 || value.chars().any(char::is_control) {
        return false;
    }
    match key {
        "ANTHROPIC_BASE_URL" | "OPENAI_BASE_URL" | "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" => {
            (value.starts_with("https://") || value.starts_with("http://"))
                && !value.contains(['@', '?', '#', '\\'])
                && value
                    .split("://")
                    .nth(1)
                    .is_some_and(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
        }
        "NO_PROXY" => value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".,:-*[] ".contains(c)),
        _ => false,
    }
}
