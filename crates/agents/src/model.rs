//! The unified transcript format. Adapters turn each agent's own records into these; feature
//! code only ever sees these types.
//!
//! A conversation is one agent session. Files a subagent wrote belong to the conversation that
//! started it: a subagent's transcript is listed with its parent's `conversation_id` and its
//! own `subagent` id.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

/// One transcript file on disk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TranscriptFile {
    /// Adapter id, e.g. `claude-code`.
    pub agent: &'static str,
    pub path: PathBuf,
    /// The conversation it belongs to (for a subagent: the parent conversation).
    pub conversation_id: String,
    /// The subagent's own id when this file is a subagent's transcript.
    pub subagent: Option<String>,
    pub size: u64,
    /// Last write, Unix ms.
    pub modified_ms: Option<i64>,
    pub encoding: Encoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// Plain JSON lines: readable.
    Plain,
    /// Compressed by the agent (Codex keeps older rollouts as `.jsonl.zst`); not read in
    /// this version, only counted.
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// What a write did to a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FileOp {
    /// A new file.
    Create,
    /// Whole-file write over an existing file.
    Overwrite,
    /// In-place edit of an existing file.
    Modify,
    /// Written, but the agent does not say whether the file existed.
    Write,
    Delete,
    /// Renamed or moved here from `from`.
    Move { from: String },
}

/// How a command was run, so callers can split it without knowing the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    Bash,
    PowerShell,
    Cmd,
    /// No shell: `text` is the program and its arguments joined with spaces.
    Direct,
}

/// One unified record. `ts` is Unix ms when the agent recorded a time.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Record {
    /// Facts about the conversation: emitted for the first record that carries them and
    /// again whenever the working folder or version changes.
    Session {
        ts: Option<i64>,
        conversation_id: String,
        version: Option<String>,
        cwd: Option<String>,
    },
    /// A user or assistant message. `text` is empty unless [`ReadOptions::text`] is set.
    Message { ts: Option<i64>, role: Role, chars: usize, text: String },
    /// A tool call. `input` is `null` unless [`ReadOptions::text`] is set.
    ToolCall { ts: Option<i64>, call_id: String, name: String, input: Value },
    /// The result of a tool call; `is_error` when the agent says.
    ToolResult { ts: Option<i64>, call_id: String, is_error: Option<bool> },
    /// A file the conversation wrote, from a write the agent reports as done.
    FileWrite { ts: Option<i64>, call_id: String, tool: String, path: String, op: FileOp },
    /// A command the conversation ran. `cwd` is where it started.
    Command { ts: Option<i64>, call_id: String, tool: String, shell: Shell, text: String, cwd: Option<String> },
    /// A turn ended because the model API failed (rate limit, network, ...).
    TurnFailed { ts: Option<i64>, failure: crate::hooks::ApiFailure },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOptions {
    /// Fill message text and tool inputs. Off by default: callers that only need paths and
    /// commands should not hold conversation text.
    pub text: bool,
}

/// Why reading a file stopped before its end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum Stop {
    /// The agent version that wrote it is not one this adapter was tested with. `at` is the
    /// byte offset of the record.
    UnknownVersion { version: String, at: u64, detail: String },
    /// A record this adapter relies on does not look as expected.
    UnknownFormat { at: u64, detail: String },
    /// The file is compressed and this version does not decompress it.
    Compressed,
}

/// Result of reading (part of) a transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ReadOutcome {
    /// Byte offset to continue from next time: just after the last complete line read, or
    /// the start of the line where reading stopped.
    pub offset: u64,
    pub lines: u64,
    /// Lines that were not valid JSON or longer than the line limit; skipped.
    pub bad_lines: u64,
    /// Record types this adapter does not use, with counts (they are ignored).
    pub ignored: BTreeMap<String, u64>,
    pub stop: Option<Stop>,
    /// The agent version seen in this pass, if any.
    pub version: Option<String>,
}

/// Unix ms from an RFC 3339 UTC time like `2026-11-01T22:19:21.644Z` (also accepts a
/// `+00:00` suffix and no fraction). `None` for anything else.
pub fn parse_utc_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        ms = format!("{:0<3}", &digits[..digits.len().min(3)]).parse().ok()?;
        rest = &frac[digits.len()..];
    }
    if rest != "Z" && rest != "+00:00" {
        return None;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_times_parse() {
        assert_eq!(parse_utc_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc_ms("2026-11-01T22:19:21.644Z"), Some(1_793_571_561_644));
        assert_eq!(parse_utc_ms("2026-08-21T04:00:58.0128883Z"), Some(1_787_284_858_012));
        assert_eq!(parse_utc_ms("2026-11-01T22:19:21+00:00"), Some(1_793_571_561_000));
        assert_eq!(parse_utc_ms("2026-11-01 22:19:21Z"), None);
        assert_eq!(parse_utc_ms("2026-11-01T22:19:21-07:00"), None);
    }
}
