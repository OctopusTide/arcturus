//! Claude Code (the CLI, also what the Claude desktop app's Code tab runs).
//!
//! - Executable: `claude.exe` (native build; the desktop app bundles one under
//!   `%LOCALAPPDATA%\Claude-Data\<package>\claude-code\<version>\...`), or an npm `claude.cmd`.
//!   The desktop app itself is also `Claude.exe`.
//! - Config folder: `%CLAUDE_CONFIG_DIR%`, else `~\.claude`.
//! - Transcripts: `<config>\projects\<folder slug>\<session id>.jsonl`; subagents write
//!   `<session id>\subagents\[workflows\<id>\]agent-<id>.jsonl`, which belong to that session.
//! - Version: `claude --version` prints `2.1.309 (Claude Code)`; every transcript record has
//!   `version`.
//! - Hooks: yes (see `hooks.rs`); resume: `claude --resume <id> -p <prompt>`. A resume takes the
//!   conversation from the transcript but not how it was started: without the options again it
//!   runs in the default permission mode on the default model (measured on 2.1.309).

mod hooks;
mod transcript;

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::env::{Env, glob};
use crate::hooks::{ApiFailure, FailureClass, Hooks, looks_like_network};
use crate::model::{Encoding, ReadOptions, ReadOutcome, Record, TranscriptFile};
use crate::version::{Tested, Version};
use crate::{Adapter, AdapterInfo, HookSupport, RunSettings, stat};

pub const ID: &str = "claude-code";

/// Versions whose transcripts were read on this machine (2026-11-01).
pub const TESTED: Tested = Tested {
    min: "2.1.293",
    max: "2.1.309",
};

static INFO: AdapterInfo = AdapterInfo {
    id: ID,
    display_name: "Claude Code",
    executables: &["claude.exe", "claude"],
    host_apps: &["claude.exe"],
    tested: TESTED,
    transcripts: "<CLAUDE_CONFIG_DIR or ~/.claude>/projects/<slug>/<session>.jsonl (+ <session>/subagents/**/agent-*.jsonl), JSON lines",
    hooks: HookSupport {
        available: true,
        reports_api_errors: true,
        can_continue: true,
        notes: "StopFailure (instead of Stop) reports API errors but cannot continue the turn; Stop can continue it \
                (at most 8 times in a row). Network failures arrive as server_error/unknown and are told apart by the \
                error text.",
    },
};

pub struct ClaudeCode;

pub static ADAPTER: ClaudeCode = ClaudeCode;

/// The config folder.
pub(crate) fn root(env: &Env) -> PathBuf {
    env.dir_or_home("CLAUDE_CONFIG_DIR", ".claude")
}

/// Claude Code's error codes (`error` in transcripts and `StopFailure`) in unified terms.
pub(crate) fn classify(code: &str, status: Option<u16>, text: Option<&str>) -> ApiFailure {
    let class = |class: FailureClass| ApiFailure {
        class,
        network: Some(false),
        code: code.to_owned(),
        http_status: status,
    };
    match code {
        "rate_limit" => class(FailureClass::RateLimit),
        "overloaded" => class(FailureClass::Overloaded),
        "authentication_failed" | "oauth_org_not_allowed" | "account_on_hold" | "cloud_credential_error" => {
            class(FailureClass::Auth)
        }
        "billing_error" => class(FailureClass::Billing),
        "invalid_request" | "model_not_found" => class(FailureClass::BadRequest),
        "max_output_tokens" => class(FailureClass::OutputLimit),
        _ => {
            if text.is_some_and(looks_like_network) {
                ApiFailure {
                    class: FailureClass::Network,
                    network: Some(true),
                    code: code.to_owned(),
                    http_status: status,
                }
            } else if status.is_some_and(|s| (500..600).contains(&s)) {
                class(FailureClass::Server)
            } else {
                let c = if code == "server_error" {
                    FailureClass::Server
                } else {
                    FailureClass::Unknown
                };
                ApiFailure {
                    class: c,
                    network: None,
                    code: code.to_owned(),
                    http_status: status,
                }
            }
        }
    }
}

impl Adapter for ClaudeCode {
    fn info(&self) -> &'static AdapterInfo {
        &INFO
    }

    fn transcripts(&self, env: &Env) -> std::io::Result<Vec<TranscriptFile>> {
        let projects = root(env).join("projects");
        let mut out = Vec::new();
        let Ok(dirs) = std::fs::read_dir(&projects) else {
            return Ok(out);
        };
        for project in dirs.flatten() {
            if !project.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for e in std::fs::read_dir(project.path())?.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_file() {
                    if let Some(id) = name.strip_suffix(".jsonl") {
                        push(&mut out, path, id.to_owned(), None);
                    }
                } else if ft.is_dir() {
                    subagents(&path.join("subagents"), &name, 0, &mut out);
                }
            }
        }
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
        let mut out = Vec::new();
        out.extend(glob(
            &env.local_app_data(),
            &["Claude-Data", "*", "claude-code", "*", "*", "claude.exe"],
        ));
        out.extend(glob(
            &env.app_data(),
            &["Claude", "claude-code", "*", "claude.exe"],
        ));
        out.extend(glob(
            &env.app_data(),
            &["Claude", "claude-code", "*", "*", "claude.exe"],
        ));
        out.extend(glob(&env.home, &[".local", "bin", "claude.exe"]));
        out.extend(glob(&env.app_data(), &["npm", "claude.cmd"]));
        out
    }

    fn parse_version_output(&self, output: &str) -> Option<Version> {
        // "2.1.309 (Claude Code)"
        let line = output.lines().find(|l| l.contains("Claude Code"))?;
        Version::parse(line.split_whitespace().next()?)
    }

    fn hooks(&self) -> Option<&dyn Hooks> {
        Some(&hooks::ClaudeHooks)
    }

    fn fork_args(
        &self,
        session_id: &str,
        new_session: &str,
        prompt: &str,
        settings: &RunSettings,
    ) -> Option<Vec<String>> {
        let mut args = vec![
            "--resume".into(),
            session_id.into(),
            "--fork-session".into(),
            "--session-id".into(),
            new_session.into(),
        ];
        args.extend(start_options(settings));
        args.extend(["-p".into(), prompt.into()]);
        Some(args)
    }
    fn resume_args(&self, session_id: &str, prompt: &str, settings: &RunSettings) -> Option<Vec<String>> {
        let mut args = vec!["--resume".into(), session_id.into()];
        args.extend(start_options(settings));
        args.extend(["-p".into(), prompt.into()]);
        Some(args)
    }

    fn run_settings(&self, command_line: &[String]) -> RunSettings {
        run_settings(command_line)
    }

    fn refine_from_transcript(&self, settings: &mut RunSettings, transcript: &Path) {
        let (first, last) = models_in_transcript(transcript);
        let Some(last) = last else { return };
        let changed = match &settings.model {
            Some(given) => !same_model(given, &last),
            // Started without --model: only a change since the start needs the option. Passing
            // what the transcript says otherwise would override "opus[1m]" in the settings file.
            None => first.as_deref() != Some(last.as_str()),
        };
        if changed {
            settings.model = Some(last);
        }
    }

    fn runs_unattended(&self, permission_mode: &str) -> bool {
        matches!(permission_mode, "bypassPermissions" | "dontAsk")
    }

    fn runs_in_a_window(&self, command_line: &[String]) -> bool {
        !command_line
            .iter()
            .skip(1)
            .take_while(|a| *a != "--")
            .any(|a| a == "-p" || a == "--print")
    }

    /// Measured on 2.1.309: `/exit`, Ctrl+C and Ctrl+D say `prompt_input_exit`, `/clear` says
    /// `clear`. Closing the window says `other`, as does the end of a `-p` run, so only the window
    /// tells them apart. A kill or crash says nothing. `logout` and `resume` are in its hook
    /// documentation, not measured. Anything else is not taken for a person's choice.
    fn ended_by_the_user(&self, reason: &str, in_a_window: bool) -> bool {
        match reason {
            "prompt_input_exit" | "clear" | "logout" | "resume" => true,
            "other" => in_a_window,
            _ => false,
        }
    }
}

/// `--permission-mode` values of 2.1.309, plus `default`, which its hooks report and it accepts.
const PERMISSION_MODES: &[&str] = &["acceptEdits", "auto", "bypassPermissions", "default", "dontAsk", "manual", "plan"];

/// Start options a resume does not take from the conversation, in the order they are passed.
fn start_options(settings: &RunSettings) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(mode) = settings.permission_mode.as_deref().filter(|m| PERMISSION_MODES.contains(m)) {
        out.extend(["--permission-mode".to_owned(), mode.to_owned()]);
    }
    if let Some(model) = &settings.model {
        out.extend(["--model".to_owned(), model.clone()]);
    }
    out.extend(settings.options.iter().cloned());
    out
}

/// Options taking one value, carried over as given (help of 2.1.309). Anything that identifies
/// the run itself or only shapes its output is left out, and so is the prompt.
const ONE_VALUE: &[&str] = &[
    "--agent",
    "--agents",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--autocompact",
    "--effort",
    "--fallback-model",
    "--max-budget-usd",
    "--max-turns",
    "--permission-prompt-tool",
    "--plugin-dir",
    "--plugin-url",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-file",
];
/// Options taking values up to the next option.
const MANY_VALUES: &[&str] = &[
    "--add-dir",
    "--allowed-tools",
    "--allowedTools",
    "--betas",
    "--disallowed-tools",
    "--disallowedTools",
    "--mcp-config",
    "--tools",
];
const NO_VALUE: &[&str] = &[
    "--chrome",
    "--disable-slash-commands",
    "--no-chrome",
    "--restricted",
    "--safe-mode",
    "--strict-mcp-config",
];

fn run_settings(command_line: &[String]) -> RunSettings {
    let mut out = RunSettings::default();
    let mut args = command_line.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v)),
            _ => (arg.as_str(), None),
        };
        match name {
            "--dangerously-skip-permissions" => out.permission_mode = Some("bypassPermissions".into()),
            "--permission-mode" | "--model" => {
                let value = inline.map(str::to_owned).or_else(|| args.next().cloned());
                if name == "--model" {
                    out.model = value;
                } else {
                    out.permission_mode = value;
                }
            }
            _ if NO_VALUE.contains(&name) => out.options.push(arg.clone()),
            _ if ONE_VALUE.contains(&name) => {
                out.options.push(arg.clone());
                if inline.is_none() {
                    out.options.extend(args.next().cloned());
                }
            }
            _ if MANY_VALUES.contains(&name) => {
                out.options.push(arg.clone());
                if inline.is_none() {
                    while let Some(value) = args.next_if(|v| !v.starts_with('-')) {
                        out.options.push(value.clone());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether the name given to `--model` (an alias, maybe with a suffix like `[1m]`) is the model
/// a transcript names.
fn same_model(given: &str, actual: &str) -> bool {
    let given = given.split('[').next().unwrap_or(given).to_ascii_lowercase();
    !given.is_empty() && actual.to_ascii_lowercase().contains(&given)
}

/// Each end of a transcript is read this far: they can be very large.
const TRANSCRIPT_END: u64 = 1 << 20;

/// The first and last model that really answered in this transcript, as far as both ends tell.
fn models_in_transcript(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return (None, None);
    };
    let size = file.metadata().map_or(0, |m| m.len());
    let mut chunk = |from: u64| -> String {
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(from)).is_err() || file.by_ref().take(TRANSCRIPT_END).read_to_end(&mut bytes).is_err() {
            return String::new();
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let head = chunk(0);
    let tail = chunk(size.saturating_sub(TRANSCRIPT_END));
    let model_of = |line: &str| -> Option<String> {
        if !line.contains("\"assistant\"") {
            return None;
        }
        let record: serde_json::Value = serde_json::from_str(line).ok()?;
        // API errors are written as assistant records from "<synthetic>".
        record["message"]["model"].as_str().filter(|m| !m.starts_with('<')).map(str::to_owned)
    };
    (head.lines().find_map(model_of), tail.lines().rev().find_map(model_of))
}

fn push(out: &mut Vec<TranscriptFile>, path: PathBuf, conversation_id: String, subagent: Option<String>) {
    let Some((size, modified_ms)) = stat(&path) else {
        return;
    };
    out.push(TranscriptFile {
        agent: ID,
        path,
        conversation_id,
        subagent,
        size,
        modified_ms,
        encoding: Encoding::Plain,
    });
}

/// `agent-<id>.jsonl` files under `<session>/subagents`, including `workflows/<run>/`.
fn subagents(dir: &Path, session: &str, depth: u32, out: &mut Vec<TranscriptFile>) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            subagents(&e.path(), session, depth + 1, out);
        } else if ft.is_file()
            && let Some(id) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".jsonl"))
        {
            push(out, e.path(), session.to_owned(), Some(id.to_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_output_and_error_classes() {
        assert_eq!(
            ADAPTER
                .parse_version_output("2.1.309 (Claude Code)\n")
                .unwrap()
                .raw,
            "2.1.309"
        );
        assert!(ADAPTER.parse_version_output("codex-cli 0.165.0").is_none());
        let net = classify(
            "server_error",
            None,
            Some("API Error: Can't reach the API server — check your connection"),
        );
        assert_eq!((net.class, net.network), (FailureClass::Network, Some(true)));
        let five = classify("server_error", Some(503), Some("API Error: 503"));
        assert_eq!((five.class, five.network), (FailureClass::Server, Some(false)));
        let vague = classify("unknown", None, Some("API Error: something odd"));
        assert_eq!((vague.class, vague.network), (FailureClass::Unknown, None));
        let rl = classify("rate_limit", Some(429), Some("You've hit your session limit"));
        assert_eq!((rl.class, rl.network), (FailureClass::RateLimit, Some(false)));
    }
}
