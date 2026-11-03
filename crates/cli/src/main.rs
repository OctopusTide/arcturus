//! Command-line client. It has no command list of its own: the handshake returns the
//! daemon's catalog and the subcommands are built from it, so a command declared by a new
//! module shows up here without CLI changes. Commands the daemon marks as not callable for
//! this caller (page-only ones, such as editing the permission list) are not offered.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use proto::catalog::{ArgInfo, CommandInfo, HelloResult};
use proto::{ApiError, ArgKind, ClientInfo, ErrorCode, Response, Section, commands, names, subcode};
use serde_json::{Map, Value, json};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Argument ids taken by the global options; the daemon refuses module arguments with
/// these names, and the CLI skips them if a daemon sends them anyway.
const RESERVED_ARGS: [&str; 5] = ["json", "endpoint", "timeout", "help", "version"];

/// What has to be known before the daemon is reached. The full parse happens later, once
/// the catalog is known, and accepts the same options anywhere on the line.
#[derive(Debug, Default, PartialEq)]
struct Globals {
    json: bool,
    endpoint: Option<String>,
    timeout: Option<String>,
    help: bool,
    version: bool,
    /// The first words that are not options (command, or module and verb).
    words: Vec<String>,
}

fn prescan(args: &mut Vec<OsString>) -> Globals {
    let mut g = Globals::default();
    let mut first_word = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].to_string_lossy().into_owned();
        match a.as_str() {
            "--" => break,
            "--json" => g.json = true,
            "-h" | "--help" => g.help = true,
            "-V" | "--version" => g.version = true,
            "--endpoint" | "--timeout" => {
                let v = args.get(i + 1).map(|v| v.to_string_lossy().into_owned());
                if a == "--endpoint" {
                    g.endpoint = v;
                } else {
                    g.timeout = v;
                }
                i += 1;
            }
            _ => {
                if let Some(v) = a.strip_prefix("--endpoint=") {
                    g.endpoint = Some(v.to_owned());
                } else if let Some(v) = a.strip_prefix("--timeout=") {
                    g.timeout = Some(v.to_owned());
                } else if !a.starts_with('-') && g.words.len() < 2 {
                    first_word.get_or_insert(i);
                    g.words.push(a);
                }
            }
        }
        i += 1;
    }
    // `away-guard help [command]` means `away-guard [command] --help`.
    if let Some(p) = first_word
        && args[p] == "help"
    {
        args.remove(p);
        args.push("--help".into());
        g.words.remove(0);
        g.help = true;
    }
    g
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn print_json(resp: &Response) {
    println!("{}", serde_json::to_string(resp).expect("responses always serialize"));
}

/// Reports a failure that happened before or instead of a daemon reply.
fn fail(err: ApiError, json: bool) -> ExitCode {
    let code = err.exit_code();
    if json {
        print_json(&Response::failure(None, err));
    } else {
        eprintln!("error: {err}");
    }
    exit(code)
}

/// Runs `f` on a helper thread so a hung daemon cannot hang the CLI.
fn with_timeout<T: Send + 'static>(
    timeout: Duration,
    what: &str,
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        Err(ApiError::new(ErrorCode::IpcError, format!("no {what} from the daemon within {}s", timeout.as_secs()))
            .with_details(json!({ "reason": "timeout" })))
    })
}

fn base_command() -> Command {
    // Subcommands are hidden from clap's own listing, so the usage line is spelled out.
    Command::new(names::CLI_BIN)
        .version(env!("CARGO_PKG_VERSION"))
        .about(format!("Talk to the {} daemon. Its commands come from the daemon itself.", names::PRODUCT))
        .override_usage(format!("{} [OPTIONS] <COMMAND> [ARGS]", names::CLI_BIN))
        .arg(
            Arg::new("json")
                .long("json")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Print exactly one JSON object (the protocol response) on stdout"),
        )
        .arg(
            Arg::new("endpoint")
                .long("endpoint")
                .global(true)
                .value_name("NAME")
                .help(format!("Daemon endpoint (default: this user's; or set {})", names::ENV_ENDPOINT)),
        )
        .arg(
            Arg::new("timeout")
                .long("timeout")
                .global(true)
                .value_name("SECS")
                .value_parser(clap::value_parser!(u64))
                .help(format!("Seconds to wait for the daemon [default: {DEFAULT_TIMEOUT_SECS}]")),
        )
}

/// Help when there is no catalog to show.
fn offline_help(reason: &ApiError) -> String {
    let mut help = base_command();
    format!(
        "{}\nThe command list comes from the daemon, which could not be reached:\n  {reason}\n\
         Start {} and run `{} --help` again.\nCommon commands: {}\n",
        help.render_help(),
        names::DAEMON_BIN,
        names::CLI_BIN,
        commands::COMMON_SHORT_NAMES.join(", ")
    )
}

/// Commands this caller may run from a command line.
fn offered(hello: &HelloResult) -> impl Iterator<Item = &CommandInfo> {
    hello.commands.iter().filter(|c| c.callable && !c.hidden)
}

fn usable_args(cmd: &CommandInfo) -> impl Iterator<Item = &ArgInfo> {
    cmd.args.iter().filter(|a| !RESERVED_ARGS.contains(&a.name.as_str()))
}

fn arg_for(a: &ArgInfo) -> Arg {
    let arg = Arg::new(a.name.clone()).help(a.help.clone());
    let long = a.name.replace('_', "-");
    if a.kind == ArgKind::Bool {
        return arg.long(long).action(ArgAction::SetTrue);
    }
    let arg = if a.positional { arg } else { arg.long(long) };
    let arg = arg.value_name(a.name.to_uppercase()).required(a.required);
    match a.kind {
        ArgKind::StrList if a.positional => arg.action(ArgAction::Append).num_args(1..),
        ArgKind::StrList => arg.action(ArgAction::Append),
        ArgKind::Int => arg.value_parser(clap::value_parser!(i64)),
        ArgKind::Uint => arg.value_parser(clap::value_parser!(u64)),
        _ => arg,
    }
}

fn subcommand_for(name: &str, cmd: &CommandInfo) -> Command {
    let mut about = cmd.summary.clone();
    if !cmd.module_enabled {
        about.push_str(" [module off]");
    }
    Command::new(name.to_owned()).about(about).args(usable_args(cmd).map(arg_for))
}

/// The full command line grammar for this daemon's catalog. Top level: one subcommand per
/// short name, plus one per module with its verbs below it (`away-guard task protect`).
fn full_command(hello: &HelloResult) -> Command {
    let mut root = base_command()
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true)
        .after_help(grouped_listing(hello));
    let modules: BTreeSet<&str> = offered(hello).map(|c| c.module.as_str()).collect();
    let mut taken: BTreeSet<&str> = modules.clone();
    // Listed in `after_help` grouped by section, so clap's own flat list is hidden.
    for c in offered(hello) {
        if let Some(short) = &c.short
            && taken.insert(short.as_str())
        {
            root = root.subcommand(subcommand_for(short, c).hide(true));
        }
    }
    for module in modules {
        let summary = hello.modules.iter().find(|m| m.name == module).map(|m| m.summary.clone()).unwrap_or_default();
        let mut m = Command::new(module.to_owned())
            .about(summary)
            .subcommand_required(true)
            .disable_help_subcommand(true)
            .hide(true);
        for c in offered(hello).filter(|c| c.module == module) {
            m = m.subcommand(subcommand_for(&c.verb, c));
        }
        root = root.subcommand(m);
    }
    root
}

/// The command list for `--help`, grouped by the section each module belongs to.
fn grouped_listing(hello: &HelloResult) -> String {
    let rows: Vec<(Section, String, String)> = offered(hello)
        .map(|c| {
            let canonical = format!("{} {}", c.module, c.verb);
            let left = match &c.short {
                Some(s) => format!("{s}, {canonical}"),
                None => canonical,
            };
            let mut right = c.summary.clone();
            if !c.module_enabled {
                right.push_str(" [module off]");
            }
            (c.section.unwrap_or(Section::Unknown), left, right)
        })
        .collect();
    let width = rows.iter().map(|(_, l, _)| l.chars().count()).max().unwrap_or(0);
    let mut out = String::from("Commands, by section:\n");
    for section in Section::ORDER {
        let mut in_section = rows.iter().filter(|(s, _, _)| *s == section).peekable();
        if in_section.peek().is_none() {
            continue;
        }
        out.push_str(&format!("\n{} ({}):\n", section.title(), section.wire_name()));
        for (_, left, right) in in_section {
            out.push_str(&format!("  {left:<width$}  {right}\n"));
        }
    }
    out.push_str(&format!("\nRun `{} <command> --help` for a command's options.", names::CLI_BIN));
    out
}

/// The catalog entry and wire arguments for a parsed command line.
fn request_from<'a>(hello: &'a HelloResult, m: &ArgMatches) -> Option<(&'a CommandInfo, Value)> {
    let (name, sub) = m.subcommand()?;
    let (cmd, matches) = match offered(hello).find(|c| c.short.as_deref() == Some(name)) {
        Some(c) if !offered(hello).any(|o| o.module == name) => (c, sub),
        _ => {
            let (verb, vm) = sub.subcommand()?;
            (offered(hello).find(|c| c.module == name && c.verb == verb)?, vm)
        }
    };
    Some((cmd, collect_args(cmd, matches)))
}

/// Arguments the user gave, as the daemon expects them. Absent ones are left out.
fn collect_args(cmd: &CommandInfo, m: &ArgMatches) -> Value {
    let mut out = Map::new();
    for a in usable_args(cmd) {
        let id = a.name.as_str();
        let v = match a.kind {
            ArgKind::Bool => m.get_flag(id).then_some(Value::Bool(true)),
            ArgKind::StrList => m.get_many::<String>(id).map(|vs| json!(vs.collect::<Vec<_>>())),
            ArgKind::Int => m.get_one::<i64>(id).map(|v| json!(v)),
            ArgKind::Uint => m.get_one::<u64>(id).map(|v| json!(v)),
            ArgKind::Str => m.get_one::<String>(id).map(|v| json!(v)),
            // Kinds this build does not know: pass JSON through, anything else as a string.
            ArgKind::Json | ArgKind::Unknown => {
                m.get_one::<String>(id).map(|s| serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())))
            }
        };
        if let Some(v) = v {
            out.insert(a.name.clone(), v);
        }
    }
    Value::Object(out)
}

/// A typed command the catalog knows but this caller may not run (page-only).
fn not_callable<'a>(hello: &'a HelloResult, words: &[String]) -> Option<&'a CommandInfo> {
    let first = words.first()?;
    let found = hello.command(first).or_else(|| hello.command(&format!("{first}.{}", words.get(1)?)))?;
    (!found.callable).then_some(found)
}

/// clap rejected the command line. Unknown subcommands map to `UNKNOWN_COMMAND`, every
/// other parse error to `USAGE`.
fn parse_failure(e: clap::Error, g: &Globals, hello: &HelloResult) -> ExitCode {
    use clap::error::ErrorKind;
    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        let _ = e.print();
        return ExitCode::SUCCESS;
    }
    let code = match e.kind() {
        ErrorKind::InvalidSubcommand => {
            if let Some(c) = not_callable(hello, &g.words) {
                let err = ApiError::new(ErrorCode::Denied, format!("'{}' can only be used from the page", c.name))
                    .with_subcode(subcode::AGENT_NOT_ALLOWED)
                    .with_module(c.module.clone());
                return fail(err, g.json);
            }
            ErrorCode::UnknownCommand
        }
        _ => ErrorCode::Usage,
    };
    if g.json {
        let message = if e.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
            format!("no command given; run `{} --help` for the list", names::CLI_BIN)
        } else {
            let text = e.render().to_string();
            text.lines().next().unwrap_or_default().trim_start_matches("error: ").to_owned()
        };
        print_json(&Response::failure(None, ApiError::new(code, message)));
    } else {
        let _ = e.print();
    }
    exit(code.exit_code())
}

fn fmt_uptime(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60)
}

fn section_of(v: &Value) -> Section {
    serde_json::from_value(v.clone()).unwrap_or(Section::Unknown)
}

fn print_status(v: &Value) {
    let d = &v["daemon"];
    println!(
        "{} daemon {} (protocol {}..{}), pid {}, up {}",
        d["product"].as_str().unwrap_or("?"),
        d["version"].as_str().unwrap_or("?"),
        d["protocol"]["min"],
        d["protocol"]["max"],
        d["pid"],
        fmt_uptime(d["uptime_ms"].as_i64().unwrap_or(0))
    );
    println!("endpoint: {}", d["endpoint"].as_str().unwrap_or("?"));
    println!("data:     {}", d["data_dir"].as_str().unwrap_or("?"));
    println!("sqlite:   {}", d["sqlite"].as_str().unwrap_or("?"));
    if let Some(e) = d["config_error"].as_str() {
        println!("CONFIG ERROR, no module started (fix the file and restart the daemon): {e}");
    }
    for w in d["config_warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        println!("config warning: {w}");
    }
    let modules = v["modules"].as_array().map(Vec::as_slice).unwrap_or_default();
    for section in Section::ORDER {
        let in_section: Vec<&Value> = modules.iter().filter(|m| section_of(&m["section"]) == section).collect();
        if in_section.is_empty() {
            continue;
        }
        println!("{}:", section.title());
        for m in in_section {
            let on = if m["enabled"].as_bool() == Some(true) { "on" } else { "off" };
            let mut health = m["health"].as_str().unwrap_or("?").to_owned();
            if let Some(fault) = m["fault"].as_str() {
                health = format!("{health}: {fault}");
            } else if m["state"]["implemented"].as_bool() == Some(false) {
                health.push_str(" (stub)");
            }
            let cmds: Vec<&str> = m["commands"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let cmds = if cmds.is_empty() { "-".to_owned() } else { cmds.join(", ") };
            println!("  {:<11} {on:<4} {health:<16} {cmds}", m["name"].as_str().unwrap_or("?"));
            // Any module may add a one-line summary and notices (what did not take effect, and why).
            if let Some(s) = m["state"]["summary"].as_str() {
                println!("      {s}");
            }
            for n in m["state"]["notices"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                println!("      ! {n}");
            }
        }
    }
    let j = &v["journal"];
    let r = &j["last_recovery"];
    println!(
        "recovery journal: {} open (last start: {} restored, {} adopted, {} failed, {} orphaned)",
        j["open"], r["restored"], r["adopted"], r["failed"], r["orphaned"]
    );
}

fn print_log(v: &Value) {
    let entries = v["entries"].as_array().map(Vec::as_slice).unwrap_or_default();
    if entries.is_empty() {
        println!("(no entries)");
    }
    for e in entries {
        let ts = platform::format_local_time(e["ts"].as_i64().unwrap_or(0));
        let outcome = if e["ok"].as_bool() == Some(true) {
            "ok".to_owned()
        } else {
            match (e["error_code"].as_str(), e["subcode"].as_str()) {
                (Some(c), Some(s)) => format!("{c} {s}"),
                (c, _) => c.unwrap_or("?").to_owned(),
            }
        };
        let kind = e["caller_kind"].as_str().unwrap_or("?");
        let pid = e["client_pid"].as_i64().map_or("?".to_owned(), |p| p.to_string());
        let exe = e["client_exe"].as_str().unwrap_or("?");
        let name = e["client_name"].as_str().map(|n| format!(" ({n})")).unwrap_or_default();
        println!("{ts}  {:<16} {outcome:<20} {kind} pid {pid} {exe}{name}", e["cmd"].as_str().unwrap_or("?"));
    }
}

fn print_human(cmd: &str, resp: &Response) {
    match (&resp.result, &resp.error) {
        (Some(result), _) if resp.ok => match cmd {
            commands::STATUS => print_status(result),
            commands::LOG => print_log(result),
            _ => println!("{}", serde_json::to_string_pretty(result).unwrap_or_default()),
        },
        (_, Some(e)) => eprintln!("error: {e}"),
        _ if resp.ok => println!("ok"),
        _ => eprintln!("error: the daemon sent a failed response without details"),
    }
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let g = prescan(&mut args);
    if g.version {
        println!("{} {}", names::CLI_BIN, env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let timeout = match g.timeout.as_deref().map(str::parse::<u64>) {
        None => Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Some(Ok(secs)) => Duration::from_secs(secs.max(1)),
        Some(Err(_)) => return fail(ApiError::new(ErrorCode::Usage, "--timeout takes a whole number of seconds"), g.json),
    };
    let endpoint = g.endpoint.clone().or_else(|| std::env::var(names::ENV_ENDPOINT).ok().filter(|s| !s.is_empty()));
    let endpoint = match endpoint.map_or_else(platform::paths::default_endpoint, Ok) {
        Ok(e) => e,
        Err(e) => return fail(ApiError::new(ErrorCode::CliFailure, format!("cannot work out the endpoint: {e}")), g.json),
    };

    let info = ClientInfo { name: names::CLI_BIN.into(), version: Some(env!("CARGO_PKG_VERSION").into()) };
    let client = with_timeout(timeout, "handshake", move || platform::connect_client(&endpoint, info, timeout));
    let mut client = match client {
        Ok(c) => c,
        Err(e) if g.help => {
            print!("{}", offline_help(&e));
            return ExitCode::SUCCESS;
        }
        Err(e) => return fail(e, g.json),
    };

    let hello = client.hello().clone();
    let argv = std::iter::once(OsString::from(names::CLI_BIN)).chain(args);
    let matches = match full_command(&hello).try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) => return parse_failure(e, &g, &hello),
    };
    let Some((cmd, args)) = request_from(&hello, &matches) else {
        return fail(ApiError::new(ErrorCode::CliFailure, "parsed a command that is not in the catalog"), g.json);
    };
    let name = cmd.name.clone();
    let wire_name = name.clone();
    let resp = match with_timeout(timeout, "reply", move || client.call(&wire_name, args)) {
        Ok(resp) => resp,
        Err(e) => Response::failure(None, e),
    };
    if g.json {
        print_json(&resp);
    } else {
        print_human(&name, &resp);
    }
    exit(resp.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::catalog::{CallerInfo, DaemonHello, ModuleInfo};

    fn arg(name: &str, kind: ArgKind, required: bool, positional: bool) -> ArgInfo {
        ArgInfo { name: name.into(), kind, required, positional, help: String::new() }
    }

    fn cmd(module: &str, verb: &str, short: Option<&str>, section: Section, args: Vec<ArgInfo>) -> CommandInfo {
        CommandInfo {
            name: format!("{module}.{verb}"),
            module: module.into(),
            verb: verb.into(),
            section: Some(section),
            short: short.map(Into::into),
            summary: format!("{module} {verb} summary"),
            args,
            read_only: false,
            agent: true,
            callable: true,
            hidden: false,
            module_enabled: true,
        }
    }

    fn module(name: &str, section: Section) -> ModuleInfo {
        ModuleInfo {
            name: name.into(),
            section: Some(section),
            summary: format!("{name} module"),
            enabled: true,
            health: "running".into(),
            error_codes: vec![],
            events: vec![],
        }
    }

    fn hello() -> HelloResult {
        let mut set = cmd("permission", "set", None, Section::Protection, vec![arg("program", ArgKind::Str, true, false)]);
        set.callable = false;
        set.agent = false;
        let mut hidden = cmd("core", "hello", None, Section::Common, vec![]);
        hidden.hidden = true;
        let mut off = cmd("example", "echo", None, Section::Common, vec![arg("text", ArgKind::Str, true, true)]);
        off.module_enabled = false;
        HelloResult {
            protocol: 1,
            daemon: DaemonHello { product: "p".into(), version: "0".into(), pid: 1, min_protocol: 1, max_protocol: 1 },
            caller: CallerInfo { kind: "agent".into(), pid: None, exe: None },
            modules: vec![module("task", Section::Usage), module("permission", Section::Protection)],
            commands: vec![
                hidden,
                cmd("core", "status", Some("status"), Section::Common, vec![]),
                cmd(
                    "task",
                    "protect",
                    Some("protect"),
                    Section::Usage,
                    vec![
                        arg("pid", ArgKind::Uint, false, false),
                        arg("ttl_minutes", ArgKind::Uint, false, false),
                        arg("deps", ArgKind::StrList, false, false),
                        arg("dry", ArgKind::Bool, false, false),
                        arg("extra", ArgKind::Json, false, false),
                        arg("json", ArgKind::Bool, false, false),
                    ],
                ),
                cmd("task", "renew", Some("renew"), Section::Usage, vec![arg("task_id", ArgKind::Int, true, true)]),
                cmd("permission", "list", None, Section::Protection, vec![]),
                set,
                off,
                cmd("future", "thing", None, Section::Unknown, vec![arg("when", ArgKind::Unknown, false, false)]),
            ],
        }
    }

    fn parse(line: &[&str]) -> Result<(String, Value), clap::Error> {
        let h = hello();
        let m = full_command(&h).try_get_matches_from(std::iter::once("x").chain(line.iter().copied()))?;
        let (c, a) = request_from(&h, &m).expect("parsed command is in the catalog");
        Ok((c.name.clone(), a))
    }

    #[test]
    fn binary_name_matches_names_table() {
        assert_eq!(env!("CARGO_BIN_NAME"), names::CLI_BIN);
    }

    #[test]
    fn built_grammar_is_valid() {
        full_command(&hello()).debug_assert();
        base_command().debug_assert();
    }

    #[test]
    fn short_and_module_forms_reach_the_same_command() {
        assert_eq!(parse(&["renew", "7"]).unwrap(), ("task.renew".into(), json!({"task_id": 7})));
        assert_eq!(parse(&["task", "renew", "7"]).unwrap(), ("task.renew".into(), json!({"task_id": 7})));
        assert_eq!(parse(&["status", "--json"]).unwrap(), ("core.status".into(), json!({})));
    }

    #[test]
    fn arguments_follow_their_declared_kinds() {
        let (_, a) = parse(&[
            "protect", "--pid", "42", "--ttl-minutes", "5", "--deps", "wsl", "--deps", "docker", "--dry", "--extra",
            "{\"a\":1}",
        ])
        .unwrap();
        assert_eq!(a, json!({"pid": 42, "ttl_minutes": 5, "deps": ["wsl", "docker"], "dry": true, "extra": {"a": 1}}));
        let (_, a) = parse(&["future", "thing", "--when", "tomorrow"]).unwrap();
        assert_eq!(a, json!({"when": "tomorrow"}));
        assert_eq!(parse(&["protect", "--pid", "abc"]).unwrap_err().kind(), clap::error::ErrorKind::ValueValidation);
        assert_eq!(parse(&["renew"]).unwrap_err().kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn reserved_names_stay_global() {
        // The daemon's `json` arg is skipped; `--json` keeps meaning "JSON output".
        let (_, a) = parse(&["protect", "--json"]).unwrap();
        assert_eq!(a, json!({}));
    }

    #[test]
    fn page_only_and_hidden_commands_are_not_offered() {
        let h = hello();
        let e = parse(&["permission", "set", "--program", "x"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert_eq!(not_callable(&h, &["permission".into(), "set".into()]).unwrap().name, "permission.set");
        assert!(not_callable(&h, &["frobnicate".into()]).is_none());
        assert_eq!(parse(&["core", "hello"]).unwrap_err().kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert_eq!(parse(&["frobnicate"]).unwrap_err().kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn help_lists_commands_grouped_by_section_in_order() {
        let text = grouped_listing(&hello());
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} missing from:\n{text}"));
        assert!(at("公共部分") < at("使用") && at("使用") < at("保护") && at("保护") < at("其他"));
        assert!(at("status, core status") < at("使用"));
        assert!(at("protect, task protect") > at("使用"));
        assert!(at("permission list") > at("保护"));
        assert!(text.contains("example echo") && text.contains("[module off]"));
        assert!(!text.contains("permission set") && !text.contains("core hello"));
        assert!(!text.contains("账号和存储"), "empty sections are left out");
    }

    #[test]
    fn prescan_finds_globals_anywhere() {
        let mut a = ["status", "--json", "--endpoint", "e1", "--timeout=5"].map(OsString::from).to_vec();
        let g = prescan(&mut a);
        assert!(g.json && !g.help);
        assert_eq!((g.endpoint.as_deref(), g.timeout.as_deref()), (Some("e1"), Some("5")));
        assert_eq!(g.words, ["status"]);

        let mut a = ["help", "task", "protect"].map(OsString::from).to_vec();
        let g = prescan(&mut a);
        assert!(g.help);
        assert_eq!(a, ["task", "protect", "--help"].map(OsString::from));
        assert_eq!(g.words, ["task"]);
    }
}
