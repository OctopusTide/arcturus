//! Agent hooks in one shape: what a hook receives (turned into [`HookEvent`]), and how to add
//! or remove our hook in the agent's own config file ([`HookPlan`]).
//!
//! Installing never writes on its own: an adapter only *plans* the edit (old and new file
//! text). [`HookPlan::apply`] writes it, and only a command the user runs on purpose calls that.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::env::Env;
use crate::ojson::OValue;

/// What happened, in agent-neutral terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// A conversation started or was resumed.
    SessionStart,
    /// The user sent a message.
    PromptSubmit,
    /// The agent finished a turn (normally, or because the model API failed: see
    /// [`HookEvent::outcome`]).
    TurnEnd,
    /// The conversation ended (closed, cleared, exited).
    SessionEnd,
}

impl HookKind {
    pub const ALL: [HookKind; 4] = [HookKind::SessionStart, HookKind::PromptSubmit, HookKind::TurnEnd, HookKind::SessionEnd];

    pub fn name(self) -> &'static str {
        match self {
            HookKind::SessionStart => "session_start",
            HookKind::PromptSubmit => "prompt_submit",
            HookKind::TurnEnd => "turn_end",
            HookKind::SessionEnd => "session_end",
        }
    }

    pub fn from_name(name: &str) -> Option<HookKind> {
        HookKind::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// Coarse kind of model-API failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// No connection, connection dropped, timed out, TLS/proxy failure.
    Network,
    RateLimit,
    Overloaded,
    Auth,
    Billing,
    /// The API answered with a server error (5xx).
    Server,
    /// The request was refused as invalid (includes safety refusals).
    BadRequest,
    /// The answer hit the output limit.
    OutputLimit,
    Unknown,
}

/// Why a turn ended on an API error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiFailure {
    pub class: FailureClass,
    /// `Some(true)` when the failure is a network/connection problem, `Some(false)` when it is
    /// clearly something else, `None` when the agent does not say enough to tell.
    pub network: Option<bool>,
    /// The agent's own error code, e.g. `server_error`.
    pub code: String,
    pub http_status: Option<u16>,
}

/// How a turn ended, from a [`HookKind::TurnEnd`] hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TurnOutcome {
    Completed,
    ApiError { failure: ApiFailure },
    /// The agent's hook does not say why the turn ended.
    Unknown,
}

/// One hook call in the unified format. Holds no conversation text: the prompt and the last
/// assistant message the agent passes in are read only to classify errors, then dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookEvent {
    pub agent: &'static str,
    pub kind: HookKind,
    /// The agent's own event name, e.g. `StopFailure`.
    pub native_event: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    /// Only for `TurnEnd`.
    pub outcome: Option<TurnOutcome>,
    /// `TurnEnd` only: the agent is already continuing because a hook asked it to.
    pub continuing: Option<bool>,
    /// `SessionStart`: startup, resume, clear, ...; `SessionEnd`: why it ended. As the agent says.
    pub detail: Option<String>,
    /// The conversation's permission mode, when this hook reports it. As the agent says.
    pub permission_mode: Option<String>,
}

/// What the hook program prints and exits with, to steer the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookReply {
    pub stdout: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "error", content = "detail")]
pub enum HookError {
    /// The config file or hook input does not look as this adapter expects.
    UnknownFormat(String),
    /// The installed agent version is not one this adapter was tested with.
    UnknownVersion(String),
    /// The hook input names an event this adapter does not know.
    UnknownEvent(String),
    /// The agent cannot hook this.
    Unsupported(String),
    /// The file changed after the plan was made.
    Conflict(String),
    Io(String),
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookError::UnknownFormat(m) => write!(f, "format not recognised: {m}"),
            HookError::UnknownVersion(m) => write!(f, "{m}"),
            HookError::UnknownEvent(m) => write!(f, "hook event not recognised: {m}"),
            HookError::Unsupported(m) => write!(f, "not supported: {m}"),
            HookError::Conflict(m) => write!(f, "changed since planned: {m}"),
            HookError::Io(m) => write!(f, "{m}"),
        }
    }
}

/// What to install: our hook program and, per kind, the timeout in seconds.
#[derive(Debug, Clone)]
pub struct HookInstall {
    /// Absolute path of the hook program (it is passed `--agent <id>`).
    pub program: PathBuf,
    pub kinds: Vec<(HookKind, u32)>,
    /// The installed agent's version ([`crate::detect`]). Planning refuses a version the
    /// adapter was not tested with, and an unknown one.
    pub agent_version: Option<String>,
}

/// A hook of ours found in an agent's config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledHook {
    pub native_event: String,
    pub kind: Option<HookKind>,
    pub command: String,
    pub timeout_s: Option<u64>,
}

/// One file an install or uninstall would write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedFile {
    pub path: PathBuf,
    /// Current text, `None` when the file does not exist yet.
    #[serde(skip)]
    pub before: Option<String>,
    #[serde(skip)]
    pub after: String,
    /// What changes, one line each (no file contents: config files can hold keys).
    pub summary: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookPlan {
    pub agent: &'static str,
    pub files: Vec<PlannedFile>,
    /// Steps the user has to do in the agent itself, and caveats.
    pub notes: Vec<String>,
}

/// What [`HookPlan::apply`] wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Applied {
    pub written: Vec<PathBuf>,
    /// Copies of the previous files, next to them.
    pub backups: Vec<PathBuf>,
}

impl HookPlan {
    pub fn is_noop(&self) -> bool {
        self.files.is_empty()
    }

    /// Writes the planned files. Refuses (and writes nothing) if any file changed since the
    /// plan was made. Each previous file is copied to `<name>.away-guard-<unix secs>.bak`
    /// first; the new text goes to a temporary file that is then renamed over the old one.
    pub fn apply(&self) -> Result<Applied, HookError> {
        for f in &self.files {
            let now = read_optional(&f.path)?;
            if now != f.before {
                return Err(HookError::Conflict(format!("{} changed after the plan was made; plan again", f.path.display())));
            }
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let mut out = Applied { written: Vec::new(), backups: Vec::new() };
        for f in &self.files {
            let io = |e: std::io::Error| HookError::Io(format!("{}: {e}", f.path.display()));
            if let Some(dir) = f.path.parent() {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
            if let Some(old) = &f.before {
                let mut backup = with_suffix(&f.path, &format!(".away-guard-{stamp}.bak"));
                for n in 1.. {
                    if !backup.exists() {
                        break;
                    }
                    backup = with_suffix(&f.path, &format!(".away-guard-{stamp}-{n}.bak"));
                }
                std::fs::write(&backup, old).map_err(io)?;
                out.backups.push(backup);
            }
            let tmp = with_suffix(&f.path, ".away-guard-tmp");
            std::fs::write(&tmp, &f.after).map_err(io)?;
            std::fs::rename(&tmp, &f.path).map_err(io)?;
            out.written.push(f.path.clone());
        }
        Ok(out)
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// File text, `None` when it does not exist.
pub fn read_optional(path: &Path) -> Result<Option<String>, HookError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(HookError::Io(format!("{}: {e}", path.display()))),
    }
}

/// Per-agent hook knowledge. Every adapter with hooks implements it.
pub trait Hooks: Sync {
    /// Unified kinds this agent can hook, each with the agent's own event name(s).
    fn events(&self) -> &'static [(HookKind, &'static str)];

    /// The config file our hooks go into.
    fn config_file(&self, env: &Env) -> PathBuf;

    /// Our hooks found in that file (`program_name`: our hook program's file name).
    fn installed(&self, env: &Env, program_name: &str) -> Result<Vec<InstalledHook>, HookError>;

    /// Plans adding (or updating) our hooks; leaves everything else in the file as it is.
    fn plan_install(&self, env: &Env, req: &HookInstall) -> Result<HookPlan, HookError>;

    /// Plans removing every hook of ours (and nothing else).
    fn plan_uninstall(&self, env: &Env, program_name: &str) -> Result<HookPlan, HookError>;

    /// Turns the JSON the agent passes on stdin into a [`HookEvent`].
    fn parse(&self, input: &[u8]) -> Result<HookEvent, HookError>;

    /// What a `TurnEnd` hook prints to make the agent continue with `prompt`, if the agent
    /// supports that.
    fn continue_reply(&self, prompt: &str) -> Option<HookReply>;
}

/// Hook groups shared by agents that copied Claude Code's layout:
/// `{"hooks": {"<Event>": [{"matcher": ..., "hooks": [<handler>, ...]}, ...]}}`.
pub(crate) mod groups {
    use super::*;

    /// Every handler in `doc` with its event name, for handlers where `ours` is true.
    pub fn find(doc: &OValue, ours: &dyn Fn(&OValue) -> bool) -> Result<Vec<(String, OValue)>, HookError> {
        let mut out = Vec::new();
        let Some(hooks) = doc.get("hooks") else { return Ok(out) };
        let OValue::Object(events) = hooks else { return Err(shape("\"hooks\" is not an object")) };
        for (event, groups) in events {
            let groups = groups.as_array().ok_or_else(|| shape(&format!("hooks.{event} is not a list")))?;
            for g in groups {
                let handlers = g
                    .get("hooks")
                    .and_then(OValue::as_array)
                    .ok_or_else(|| shape(&format!("a hooks.{event} entry has no \"hooks\" list")))?;
                for h in handlers.iter().filter(|h| ours(h)) {
                    out.push((event.clone(), h.clone()));
                }
            }
        }
        Ok(out)
    }

    /// Removes our handlers, then empty groups, empty event lists and an empty `hooks`.
    /// Returns one summary line per removed handler.
    pub fn remove(doc: &mut OValue, ours: &dyn Fn(&OValue) -> bool, describe: &dyn Fn(&OValue) -> String) -> Vec<String> {
        let mut summary = Vec::new();
        let Some(OValue::Object(events)) = doc.get_mut("hooks") else { return summary };
        for (event, groups) in events.iter_mut() {
            let Some(groups) = groups.as_array_mut() else { continue };
            for g in groups.iter_mut() {
                if let Some(handlers) = g.get_mut("hooks").and_then(OValue::as_array_mut) {
                    handlers.retain(|h| {
                        let mine = ours(h);
                        if mine {
                            summary.push(format!("remove {event}: {}", describe(h)));
                        }
                        !mine
                    });
                }
            }
            groups.retain(|g| g.get("hooks").and_then(OValue::as_array).is_none_or(|h| !h.is_empty()));
        }
        events.retain(|(_, groups)| groups.as_array().is_none_or(|g| !g.is_empty()));
        if events.is_empty() {
            doc.remove("hooks");
        }
        summary
    }

    /// Adds one group per `(event, handler)` at the end of that event's list.
    pub fn add(doc: &mut OValue, wanted: Vec<(&str, OValue)>, describe: &dyn Fn(&OValue) -> String) -> Vec<String> {
        let mut summary = Vec::new();
        if wanted.is_empty() {
            return summary;
        }
        if doc.get("hooks").is_none() {
            doc.set("hooks", OValue::Object(Vec::new()));
        }
        let Some(hooks) = doc.get_mut("hooks") else { return summary };
        for (event, handler) in wanted {
            summary.push(format!("add {event}: {}", describe(&handler)));
            let group = OValue::Object(vec![("hooks".to_owned(), OValue::Array(vec![handler]))]);
            match hooks.get_mut(event).and_then(OValue::as_array_mut) {
                Some(list) => list.push(group),
                None => hooks.set(event, OValue::Array(vec![group])),
            }
        }
        summary
    }

    fn shape(what: &str) -> HookError {
        HookError::UnknownFormat(what.to_owned())
    }

    /// The config as an object; an absent or empty file is an empty object.
    pub fn load(text: Option<&str>, path: &Path) -> Result<OValue, HookError> {
        match text.map(str::trim) {
            None | Some("") => Ok(OValue::Object(Vec::new())),
            Some(t) => {
                let v = OValue::parse(t)
                    .map_err(|e| HookError::UnknownFormat(format!("{} is not valid JSON ({e})", path.display())))?;
                if !v.is_object() {
                    return Err(HookError::UnknownFormat(format!("{} is not a JSON object", path.display())));
                }
                Ok(v)
            }
        }
    }
}

/// Phrases model clients use for connection-level failures (checked case-insensitively).
const NETWORK_PHRASES: &[&str] = &[
    "unable to connect",
    "can't reach",
    "cannot reach",
    "connection lost",
    "connection error",
    "connection refused",
    "connection reset",
    "stopped arriving",
    "ended before any complete data",
    "timed out",
    "timeout",
    "econnreset",
    "econnrefused",
    "enotfound",
    "etimedout",
    "eai_again",
    "fetch failed",
    "socket hang up",
    "network",
    "certificate",
    "proxy",
];

/// Whether an error text reads like a network/connection failure.
pub fn looks_like_network(text: &str) -> bool {
    let t = text.to_lowercase();
    NETWORK_PHRASES.iter().any(|p| t.contains(p))
}

/// The first three-digit HTTP status (100–599) at the start of `text` or after `status`.
pub fn http_status_in(text: &str) -> Option<u16> {
    let t = text.trim_start();
    let lead: String = t.chars().take_while(char::is_ascii_digit).collect();
    if lead.len() == 3 {
        return lead.parse().ok().filter(|s| (100..600).contains(s));
    }
    let lower = t.to_lowercase();
    let at = lower.find("status")?;
    let digits: String =
        lower[at + 6..].trim_start_matches([' ', ':', '=']).chars().take_while(char::is_ascii_digit).collect();
    if digits.len() == 3 { digits.parse().ok().filter(|s| (100..600).contains(s)) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_phrases_and_status() {
        assert!(looks_like_network("API Error: Unable to connect to API: Self-signed certificate"));
        assert!(looks_like_network("API Error: Can't reach the API server — check your connection"));
        assert!(looks_like_network("API Error: The response stopped arriving."));
        assert!(!looks_like_network("You've hit your session limit"));
        assert_eq!(http_status_in("429 Too Many Requests"), Some(429));
        assert_eq!(http_status_in("upstream status: 503"), Some(503));
        assert_eq!(http_status_in("Connection lost mid-response"), None);
        assert_eq!(http_status_in("12345"), None);
    }

    #[test]
    fn groups_add_find_remove_keep_other_hooks() {
        let text = r#"{"theme":"dark","hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"theirs.exe"}]}]}}"#;
        let mut doc = groups::load(Some(text), Path::new("s.json")).unwrap();
        let ours = |h: &OValue| h.get("command").and_then(OValue::as_str) == Some("ours.exe");
        let desc = |h: &OValue| h.get("command").and_then(OValue::as_str).unwrap_or("").to_owned();
        let h = OValue::Object(vec![("type".into(), OValue::str("command")), ("command".into(), OValue::str("ours.exe"))]);
        groups::add(&mut doc, vec![("Stop", h.clone()), ("SessionStart", h)], &desc);
        assert_eq!(groups::find(&doc, &ours).unwrap().len(), 2);
        let removed = groups::remove(&mut doc, &ours, &desc);
        assert_eq!(removed.len(), 2);
        assert_eq!(doc, groups::load(Some(text), Path::new("s.json")).unwrap(), "only our hooks went away");
        assert!(groups::load(Some("[1]"), Path::new("s.json")).is_err());
        let bad = groups::load(Some(r#"{"hooks":{"Stop":{}}}"#), Path::new("s.json")).unwrap();
        assert!(matches!(groups::find(&bad, &ours), Err(HookError::UnknownFormat(_))));
    }
}
