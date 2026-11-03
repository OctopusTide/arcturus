//! Claude Code transcripts: one JSON object per line. Checked on this machine against
//! 2.1.293–2.1.309 (3203 files). The records used:
//!
//! - `user` / `assistant`: `sessionId`, `timestamp`, `cwd` (the shell's folder at that moment),
//!   `version`, `message.content` (a string, or blocks `text` / `tool_use` / `tool_result`);
//!   a `user` record answering a tool also has `toolUseResult` (for `Write`:
//!   `{type: create|update, filePath}`, for `Edit`: `{filePath, oldString, ...}`).
//! - an `assistant` record with `isApiErrorMessage: true` is a turn that ended on an API
//!   error: `error` (`rate_limit`, `server_error`, ...), `apiErrorStatus`, text `API Error: ...`.
//!
//! Every other record type (`api-request`, `api-request-blob`, `attachment`, `system`,
//! `custom-title`, ...) is counted and ignored. `api-request-blob` repeats tool calls already
//! in `assistant` records, so it must never be read for them.

use std::collections::HashMap;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::{TESTED, classify};
use crate::lenient::{self, List, Obj};
use crate::lines::{Line, Lines};
use crate::model::{
    Encoding, FileOp, ReadOptions, ReadOutcome, Record, Role, Shell, Stop, TranscriptFile, parse_utc_ms,
};

pub(super) const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct CcLine {
    #[serde(rename = "type", deserialize_with = "lenient::str")]
    ty: Option<String>,
    #[serde(rename = "sessionId", deserialize_with = "lenient::str")]
    session_id: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    timestamp: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    cwd: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    version: Option<String>,
    message: Obj<Msg>,
    #[serde(rename = "toolUseResult")]
    tool_use_result: Obj<ResultShape>,
    #[serde(rename = "isApiErrorMessage", deserialize_with = "lenient::bool")]
    api_error_message: Option<bool>,
    #[serde(deserialize_with = "lenient::str")]
    error: Option<String>,
    #[serde(rename = "apiErrorStatus", deserialize_with = "lenient::int")]
    api_error_status: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Msg {
    #[serde(deserialize_with = "lenient::str")]
    role: Option<String>,
    content: Content,
}

#[derive(Default)]
enum Content {
    #[default]
    Missing,
    Text(String),
    Blocks(Vec<Block>),
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // A string, or a list of blocks; anything else counts as missing. Decoded in one pass
        // (no buffering), so large tool results are skipped without being copied.
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Content;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("text or a list of blocks")
            }
            fn visit_str<E>(self, v: &str) -> Result<Content, E> {
                Ok(Content::Text(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<Content, E> {
                Ok(Content::Text(v))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Content, A::Error> {
                let list: List<Block> = Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
                Ok(Content::Blocks(list.0))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Content, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Content::Missing)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Content, E> {
                Ok(Content::Missing)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Content, E> {
                Ok(Content::Missing)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Content, E> {
                Ok(Content::Missing)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Content, E> {
                Ok(Content::Missing)
            }
            fn visit_unit<E>(self) -> Result<Content, E> {
                Ok(Content::Missing)
            }
        }
        d.deserialize_any(V)
    }
}

/// The parts of `toolUseResult` that say which file a write touched. Everything else in it
/// (file contents, patches) is skipped unread.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ResultShape {
    #[serde(rename = "type", deserialize_with = "lenient::str")]
    kind: Option<String>,
    #[serde(rename = "filePath", deserialize_with = "lenient::str")]
    file_path: Option<String>,
    #[serde(rename = "oldString")]
    old_string: Option<IgnoredAny>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Block {
    #[serde(rename = "type", deserialize_with = "lenient::str")]
    ty: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    text: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    id: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    name: Option<String>,
    input: Option<Value>,
    #[serde(deserialize_with = "lenient::str")]
    tool_use_id: Option<String>,
    #[serde(deserialize_with = "lenient::bool")]
    is_error: Option<bool>,
}

/// What a pending tool call will write, by call id.
struct PendingWrite {
    tool: String,
    path: Option<String>,
}

pub(super) fn read(
    file: &TranscriptFile,
    from: u64,
    opts: ReadOptions,
    sink: &mut dyn FnMut(Record),
) -> std::io::Result<ReadOutcome> {
    let mut out = ReadOutcome { offset: from, ..Default::default() };
    if file.encoding != Encoding::Plain {
        out.stop = Some(Stop::Compressed);
        return Ok(out);
    }
    let mut lines = Lines::open(&file.path, from)?;
    let mut pending: HashMap<String, PendingWrite> = HashMap::new();
    let mut last_session: Option<(Option<String>, Option<String>)> = None;
    loop {
        let stop = match lines.next_line()? {
            None | Some(Line::Unfinished) => break,
            Some(Line::TooLong) => {
                out.lines += 1;
                out.bad_lines += 1;
                None
            }
            Some(Line::Complete { start, bytes }) => {
                out.lines += 1;
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    None
                } else {
                    match serde_json::from_slice::<CcLine>(bytes) {
                        Err(_) => {
                            out.bad_lines += 1;
                            None
                        }
                        Ok(rec) => one(file, rec, start, opts, &mut out, &mut pending, &mut last_session, sink),
                    }
                }
            }
        };
        if let Some(stop) = stop {
            out.stop = Some(stop);
            return Ok(out);
        }
        out.offset = lines.offset();
    }
    Ok(out)
}

/// Handles one decoded line; `Some(stop)` ends the pass at this line.
#[allow(clippy::too_many_arguments)]
fn one(
    file: &TranscriptFile,
    rec: CcLine,
    start: u64,
    opts: ReadOptions,
    out: &mut ReadOutcome,
    pending: &mut HashMap<String, PendingWrite>,
    last_session: &mut Option<(Option<String>, Option<String>)>,
    sink: &mut dyn FnMut(Record),
) -> Option<Stop> {
    if let Some(v) = &rec.version {
        if let Err(detail) = TESTED.check(v) {
            return Some(Stop::UnknownVersion { version: v.clone(), at: start, detail });
        }
        out.version = Some(v.clone());
    }
    let ty = rec.ty.as_deref().unwrap_or("");
    if ty != "user" && ty != "assistant" {
        *out.ignored.entry(if ty.is_empty() { "(no type)".to_owned() } else { ty.to_owned() }).or_default() += 1;
        return None;
    }
    let (Some(_), Some(version), Some(msg)) = (&rec.session_id, &rec.version, rec.message.0) else {
        return Some(Stop::UnknownFormat {
            at: start,
            detail: format!("a '{ty}' record without sessionId, version or message object"),
        });
    };
    let ts = rec.timestamp.as_deref().and_then(parse_utc_ms);
    let session = (Some(version.clone()), rec.cwd.clone());
    if last_session.as_ref() != Some(&session) {
        sink(Record::Session {
            ts,
            conversation_id: file.conversation_id.clone(),
            version: session.0.clone(),
            cwd: session.1.clone(),
        });
        *last_session = Some(session);
    }
    let role = if ty == "user" { Role::User } else { Role::Assistant };
    if msg.role.as_deref().is_some_and(|r| r != ty) {
        return Some(Stop::UnknownFormat { at: start, detail: format!("a '{ty}' record with message.role {:?}", msg.role) });
    }

    if role == Role::Assistant && rec.api_error_message == Some(true) {
        let text = match &msg.content {
            Content::Text(t) => Some(t.clone()),
            Content::Blocks(b) => b.iter().find_map(|x| x.text.clone()),
            Content::Missing => None,
        };
        let status = rec.api_error_status.and_then(|s| u16::try_from(s).ok());
        let failure = classify(rec.error.as_deref().unwrap_or("unknown"), status, text.as_deref());
        sink(Record::TurnFailed { ts, failure });
        return None;
    }

    let blocks = match msg.content {
        Content::Missing => {
            return Some(Stop::UnknownFormat { at: start, detail: format!("a '{ty}' record whose content is neither text nor blocks") });
        }
        Content::Text(t) => {
            message(sink, ts, role, &t, opts);
            return None;
        }
        Content::Blocks(b) => b,
    };
    let texts: Vec<&str> = blocks.iter().filter(|b| b.ty.as_deref() == Some("text")).filter_map(|b| b.text.as_deref()).collect();
    if !texts.is_empty() {
        message(sink, ts, role, &texts.join("\n"), opts);
    }
    let results = blocks.iter().filter(|b| b.ty.as_deref() == Some("tool_result")).count();
    for b in &blocks {
        match (role, b.ty.as_deref()) {
            (Role::Assistant, Some("tool_use")) => {
                let (Some(id), Some(name)) = (&b.id, &b.name) else {
                    return Some(Stop::UnknownFormat { at: start, detail: "a tool_use block without id or name".to_owned() });
                };
                let input = b.input.as_ref();
                let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str).map(str::to_owned);
                sink(Record::ToolCall {
                    ts,
                    call_id: id.clone(),
                    name: name.clone(),
                    input: if opts.text { input.cloned().unwrap_or(Value::Null) } else { Value::Null },
                });
                let shell = match name.as_str() {
                    "Bash" => Some(Shell::Bash),
                    "PowerShell" => Some(Shell::PowerShell),
                    _ => None,
                };
                if let (Some(shell), Some(text)) = (shell, field("command")) {
                    sink(Record::Command { ts, call_id: id.clone(), tool: name.clone(), shell, text, cwd: rec.cwd.clone() });
                }
                if WRITE_TOOLS.contains(&name.as_str()) {
                    let path = field("file_path").or_else(|| field("notebook_path"));
                    pending.insert(id.clone(), PendingWrite { tool: name.clone(), path });
                }
            }
            (Role::User, Some("tool_result")) => {
                let Some(id) = &b.tool_use_id else {
                    return Some(Stop::UnknownFormat { at: start, detail: "a tool_result block without tool_use_id".to_owned() });
                };
                sink(Record::ToolResult { ts, call_id: id.clone(), is_error: b.is_error });
                let call = pending.remove(id);
                if b.is_error == Some(true) {
                    continue;
                }
                // toolUseResult describes the record's only result; with several it is ambiguous.
                let shape = if results == 1 { rec.tool_use_result.0.as_ref() } else { None };
                if let Some((tool, path, op)) = file_write(call, shape) {
                    sink(Record::FileWrite { ts, call_id: id.clone(), tool, path, op });
                }
            }
            _ => {}
        }
    }
    None
}

fn message(sink: &mut dyn FnMut(Record), ts: Option<i64>, role: Role, text: &str, opts: ReadOptions) {
    if text.is_empty() {
        return;
    }
    sink(Record::Message { ts, role, chars: text.chars().count(), text: if opts.text { text.to_owned() } else { String::new() } });
}

/// The file a finished tool call wrote: from the call itself when it was read in this pass,
/// else from the result's own description (for calls read in an earlier pass).
fn file_write(call: Option<PendingWrite>, shape: Option<&ResultShape>) -> Option<(String, String, FileOp)> {
    let kind = shape.and_then(|s| s.kind.as_deref());
    let shape_path = shape.and_then(|s| s.file_path.clone());
    let write_op = |kind: Option<&str>| match kind {
        Some("create") => FileOp::Create,
        Some("update") => FileOp::Overwrite,
        _ => FileOp::Write,
    };
    match call {
        Some(c) => {
            let op = if c.tool == "Write" { write_op(kind) } else { FileOp::Modify };
            let path = shape_path.or(c.path)?;
            Some((c.tool, path, op))
        }
        None => {
            let path = shape_path?;
            let is_edit = shape.is_some_and(|s| s.old_string.is_some());
            match kind {
                Some("create" | "update") => Some(("Write".to_owned(), path, write_op(kind))),
                _ if is_edit => Some(("Edit".to_owned(), path, FileOp::Modify)),
                _ => None,
            }
        }
    }
}
