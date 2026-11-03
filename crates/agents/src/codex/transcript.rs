//! Codex rollouts: one JSON object per line, `{timestamp, [ordinal], type, payload}`.
//! Checked on this machine against cli_version 0.163.0-alpha.11.1 – 0.165.0-alpha.1.2
//! (2122 plain rollouts). The first line is `session_meta` (`id`, `cwd`, `cli_version`). Used:
//!
//! - `turn_context`: `cwd` for the turns that follow;
//! - `response_item`: `message` (`role`, `content[].text`), `function_call` /
//!   `custom_tool_call` (`name`, `call_id`, `arguments` / `input`) and their `*_output`;
//! - `event_msg` `item_completed`: `CommandExecution` (`command` = argv, `cwd`, `status`) and
//!   `FileChange` (`changes` = path → `{type: add|update|delete, move_path}`, `status`).
//!
//! Everything else (`token_count`, `reasoning`, `world_state`, ...) is counted and ignored.

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::TESTED;
use crate::lenient::{self, List, Obj};
use crate::lines::{Line, Lines, first_line};
use crate::model::{
    Encoding, FileOp, ReadOptions, ReadOutcome, Record, Role, Shell, Stop, TranscriptFile, parse_utc_ms,
};

#[derive(Deserialize, Default)]
#[serde(default)]
struct CxLine {
    #[serde(deserialize_with = "lenient::str")]
    timestamp: Option<String>,
    #[serde(rename = "type", deserialize_with = "lenient::str")]
    ty: Option<String>,
    payload: Obj<Payload>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Payload {
    #[serde(rename = "type", deserialize_with = "lenient::str")]
    ty: Option<String>,
    // session_meta / turn_context
    #[serde(deserialize_with = "lenient::str")]
    id: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    cwd: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    cli_version: Option<String>,
    // response_item
    #[serde(deserialize_with = "lenient::str")]
    role: Option<String>,
    content: List<Part>,
    #[serde(deserialize_with = "lenient::str")]
    name: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    call_id: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    arguments: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    input: Option<String>,
    // event_msg item_completed
    item: Obj<Item>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Part {
    #[serde(deserialize_with = "lenient::str")]
    text: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Item {
    #[serde(rename = "type", deserialize_with = "lenient::str")]
    ty: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    id: Option<String>,
    command: Option<Value>,
    #[serde(deserialize_with = "lenient::str")]
    cwd: Option<String>,
    #[serde(deserialize_with = "lenient::str")]
    status: Option<String>,
    changes: Option<Value>,
    aggregated_output: Option<IgnoredAny>,
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
    // The version is only in the header, so check it on every pass. The folder of the turn
    // in progress is not known when starting mid-file: a command without its own `cwd` then
    // gets none rather than a wrong one.
    let mut cwd: Option<String> = None;
    if from > 0 {
        let header = first_line(&file.path)?.unwrap_or_default();
        match header_of(&header) {
            Err(stop) => {
                out.stop = Some(stop);
                return Ok(out);
            }
            Ok((version, _)) => out.version = Some(version),
        }
    }
    let mut lines = Lines::open(&file.path, from)?;
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
                } else if start == 0 {
                    match header_of(bytes) {
                        Err(stop) => Some(stop),
                        Ok((version, header_cwd)) => {
                            let ts = serde_json::from_slice::<CxLine>(bytes).ok().and_then(|l| l.timestamp).as_deref().and_then(parse_utc_ms);
                            sink(Record::Session { ts, conversation_id: file.conversation_id.clone(), version: Some(version.clone()), cwd: header_cwd.clone() });
                            out.version = Some(version);
                            cwd = header_cwd;
                            None
                        }
                    }
                } else {
                    match serde_json::from_slice::<CxLine>(bytes) {
                        Err(_) => {
                            out.bad_lines += 1;
                            None
                        }
                        Ok(rec) => one(file, rec, start, opts, &mut out, &mut cwd, sink),
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

/// The header line: `session_meta` with a recognised `cli_version`. Returns the version and cwd.
fn header_of(bytes: &[u8]) -> Result<(String, Option<String>), Stop> {
    let rec: CxLine = serde_json::from_slice(bytes)
        .map_err(|e| Stop::UnknownFormat { at: 0, detail: format!("first line is not JSON ({})", lenient::reason(&e)) })?;
    let p = match (rec.ty.as_deref(), rec.payload.0) {
        (Some("session_meta"), Some(p)) => p,
        (ty, _) => return Err(Stop::UnknownFormat { at: 0, detail: format!("first record is {ty:?}, not session_meta") }),
    };
    let version = p.cli_version.ok_or_else(|| Stop::UnknownFormat { at: 0, detail: "session_meta without cli_version".to_owned() })?;
    TESTED.check(&version).map_err(|detail| Stop::UnknownVersion { version: version.clone(), at: 0, detail })?;
    Ok((version, p.cwd.map(|c| path_of(&c))))
}

fn one(
    file: &TranscriptFile,
    rec: CxLine,
    start: u64,
    opts: ReadOptions,
    out: &mut ReadOutcome,
    cwd: &mut Option<String>,
    sink: &mut dyn FnMut(Record),
) -> Option<Stop> {
    let ts = rec.timestamp.as_deref().and_then(parse_utc_ms);
    let ty = rec.ty.as_deref().unwrap_or("");
    let ignore = |out: &mut ReadOutcome, what: String| *out.ignored.entry(what).or_default() += 1;
    let Some(p) = rec.payload.0 else {
        ignore(out, if ty.is_empty() { "(no type)".to_owned() } else { ty.to_owned() });
        return None;
    };
    let pty = p.ty.clone().unwrap_or_default();
    match (ty, pty.as_str()) {
        ("session_meta", _) => {
            // A later header (resumed with another build) must also be recognised.
            let version = p.cli_version.clone().unwrap_or_default();
            if let Err(detail) = TESTED.check(&version) {
                return Some(Stop::UnknownVersion { version, at: start, detail });
            }
            out.version = Some(version.clone());
            *cwd = p.cwd.as_deref().map(path_of).or(cwd.take());
            sink(Record::Session { ts, conversation_id: file.conversation_id.clone(), version: Some(version), cwd: cwd.clone() });
        }
        ("turn_context", _) => {
            let new = p.cwd.as_deref().map(path_of);
            if new.is_some() && new != *cwd {
                *cwd = new;
                sink(Record::Session { ts, conversation_id: file.conversation_id.clone(), version: out.version.clone(), cwd: cwd.clone() });
            }
        }
        ("response_item", "message") => {
            let role = match p.role.as_deref() {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                Some(_) => {
                    ignore(out, format!("response_item/message/{}", p.role.unwrap_or_default()));
                    return None;
                }
                None => return Some(Stop::UnknownFormat { at: start, detail: "a message without role".to_owned() }),
            };
            let text: Vec<&str> = p.content.0.iter().filter_map(|c| c.text.as_deref()).collect();
            let text = text.join("\n");
            if !text.is_empty() {
                sink(Record::Message { ts, role, chars: text.chars().count(), text: if opts.text { text } else { String::new() } });
            }
        }
        ("response_item", "function_call" | "custom_tool_call") => {
            let (Some(call_id), Some(name)) = (p.call_id, p.name) else {
                return Some(Stop::UnknownFormat { at: start, detail: format!("a {pty} without call_id or name") });
            };
            let input = if !opts.text {
                Value::Null
            } else if let Some(args) = p.arguments {
                serde_json::from_str(&args).unwrap_or(Value::String(args))
            } else {
                p.input.map(Value::String).unwrap_or(Value::Null)
            };
            sink(Record::ToolCall { ts, call_id, name, input });
        }
        ("response_item", "function_call_output" | "custom_tool_call_output") => {
            let Some(call_id) = p.call_id else {
                return Some(Stop::UnknownFormat { at: start, detail: format!("a {pty} without call_id") });
            };
            sink(Record::ToolResult { ts, call_id, is_error: None });
        }
        ("event_msg", "item_completed") => {
            let Some(item) = p.item.0 else {
                return Some(Stop::UnknownFormat { at: start, detail: "item_completed without item".to_owned() });
            };
            return item_completed(item, ts, start, cwd.as_deref(), out, sink);
        }
        (t, pt) => ignore(out, if pt.is_empty() { t.to_owned() } else { format!("{t}/{pt}") }),
    }
    None
}

fn item_completed(
    item: Item,
    ts: Option<i64>,
    start: u64,
    turn_cwd: Option<&str>,
    out: &mut ReadOutcome,
    sink: &mut dyn FnMut(Record),
) -> Option<Stop> {
    let ty = item.ty.clone().unwrap_or_default();
    let call_id = item.id.clone().unwrap_or_default();
    match ty.as_str() {
        "CommandExecution" => {
            let Some((shell, text)) = item.command.as_ref().and_then(command_of) else {
                return Some(Stop::UnknownFormat { at: start, detail: "CommandExecution without command".to_owned() });
            };
            let cwd = item.cwd.as_deref().map(path_of).or_else(|| turn_cwd.map(str::to_owned));
            sink(Record::Command { ts, call_id, tool: ty, shell, text, cwd });
        }
        "FileChange" => {
            let Some(Value::Object(changes)) = item.changes else {
                return Some(Stop::UnknownFormat { at: start, detail: "FileChange without changes".to_owned() });
            };
            if item.status.as_deref() != Some("completed") {
                return None;
            }
            for (path, change) in changes {
                let moved = change.get("move_path").and_then(Value::as_str).map(str::to_owned);
                let (path, op) = match (change.get("type").and_then(Value::as_str), moved) {
                    (Some("add"), None) => (path, FileOp::Create),
                    (Some("update"), None) => (path, FileOp::Modify),
                    (Some("delete"), None) => (path, FileOp::Delete),
                    (Some("add" | "update"), Some(to)) => (to, FileOp::Move { from: path }),
                    (t, _) => {
                        return Some(Stop::UnknownFormat { at: start, detail: format!("FileChange of type {t:?}") });
                    }
                };
                sink(Record::FileWrite { ts, call_id: call_id.clone(), tool: ty.clone(), path, op });
            }
        }
        other => *out.ignored.entry(format!("item_completed/{other}")).or_default() += 1,
    }
    None
}

/// The shell and the text it runs, from Codex's argv (or a plain string).
fn command_of(v: &Value) -> Option<(Shell, String)> {
    if let Some(s) = v.as_str() {
        return Some((Shell::Direct, s.to_owned()));
    }
    let argv: Vec<&str> = v.as_array()?.iter().map(Value::as_str).collect::<Option<_>>()?;
    let program = crate::file_name(argv.first()?).to_lowercase();
    let program = program.strip_suffix(".exe").unwrap_or(&program);
    let after = |flags: &[&str]| {
        argv.iter().position(|a| flags.iter().any(|f| a.eq_ignore_ascii_case(f))).and_then(|i| argv.get(i + 1)).map(|s| (*s).to_owned())
    };
    let found = match program {
        "powershell" | "pwsh" => after(&["-Command", "-c"]).map(|t| (Shell::PowerShell, t)),
        "bash" | "sh" | "zsh" => after(&["-c", "-lc"]).map(|t| (Shell::Bash, t)),
        "cmd" => after(&["/c", "/k"]).map(|t| (Shell::Cmd, t)),
        _ => None,
    };
    Some(found.unwrap_or_else(|| (Shell::Direct, argv.join(" "))))
}

/// A path from Codex's `cwd`, which is either a path or a `file:///C:/...` URL.
pub(super) fn path_of(s: &str) -> String {
    let Some(rest) = s.strip_prefix("file://") else { return s.to_owned() };
    let rest = rest.strip_prefix('/').filter(|r| r.as_bytes().get(1) == Some(&b':')).unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if raw[i] == b'%'
            && i + 2 < raw.len()
            && let (Some(h), Some(l)) = (hex(raw[i + 1]), hex(raw[i + 2]))
        {
            bytes.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&bytes).into_owned();
    if decoded.as_bytes().get(1) == Some(&b':') { decoded.replace('/', "\\") } else { decoded }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_and_argv() {
        assert_eq!(path_of("file:///C:/Users/me/AppData/Local/Temp/a%20b"), r"C:\Users\me\AppData\Local\Temp\a b");
        assert_eq!(path_of(r"D:\work"), r"D:\work");
        assert_eq!(path_of("file:///home/x"), "/home/x");
        let ps = serde_json::json!([r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe", "-Command", "git clone x"]);
        assert_eq!(command_of(&ps), Some((Shell::PowerShell, "git clone x".to_owned())));
        let direct = serde_json::json!(["git", "status"]);
        assert_eq!(command_of(&direct), Some((Shell::Direct, "git status".to_owned())));
        assert_eq!(command_of(&serde_json::json!([1])), None);
    }
}
