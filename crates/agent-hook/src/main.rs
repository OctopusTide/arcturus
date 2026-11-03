//! `away-guard-hook`: what an agent's hook runs, and the tool that installs that hook.
//!
//! - `away-guard-hook --agent <id>`: hook mode. Reads the agent's hook input on stdin, turns it
//!   into the unified format with that agent's adapter (conversation text is dropped here, never
//!   sent), and hands it to the daemon (`agents.hook`). Never blocks or fails the agent: no
//!   daemon, a timeout or input it does not recognise all end with exit code 0 and nothing on
//!   stdout (a short reason on stderr). Prints the daemon's `reply` when there is one.
//! - `detect`, `status`: which agents are installed (and their versions); which of our hooks
//!   are in their config files. Read only.
//! - `install` / `uninstall --agent <id>`: show the change to the agent's config file; only
//!   with `--apply` is it written (a backup of the old file is kept next to it).

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use agents::hooks::{HookInstall, HookKind, HookPlan};
use agents::{Adapter, Env};
use clap::{Parser, Subcommand};
use proto::{ClientInfo, names};
use serde_json::{Value, json};

/// Hook input larger than this is not read further (it is never this big).
const MAX_INPUT: u64 = 4 << 20;
const DETECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What agents' hooks run; also installs and removes those hooks.
#[derive(Parser)]
#[command(name = names::HOOK_BIN, version)]
struct Args {
    #[arg(long, hide = true)]
    resume_broker: Option<String>,
    /// Hook mode: the hook input of this agent (claude-code, codex) is on stdin
    #[arg(long)]
    agent: Option<String>,
    /// Daemon endpoint (default: this user's pipe)
    #[arg(long, env = names::ENV_ENDPOINT)]
    endpoint: Option<String>,
    /// Hook mode: seconds to wait for the daemon before giving up silently
    #[arg(long, default_value_t = 3)]
    timeout: u64,
    /// Print one JSON object instead of text
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Find installed agents and their versions (runs `<agent> --version`)
    Detect,
    /// Our hooks in each agent's config file
    Status,
    /// Show the change that adds our hooks to an agent's config; write it with --apply
    Install {
        #[arg(long)]
        agent: String,
        /// Hook kinds: session_start, prompt_submit, turn_end, session_end (default: all the agent has)
        #[arg(long, value_delimiter = ',')]
        events: Vec<String>,
        /// Seconds the agent lets each hook run
        #[arg(long, default_value_t = 30)]
        hook_timeout: u32,
        /// Hook program to register (default: this executable)
        #[arg(long)]
        program: Option<PathBuf>,
        /// Write the change (without it, nothing is written)
        #[arg(long)]
        apply: bool,
    },
    /// Show the change that removes our hooks from an agent's config; write it with --apply
    Uninstall {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        apply: bool,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(config) = args.resume_broker {
        return if platform::run_resume_broker(&config).is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let json_out = args.json;
    let result = match (args.command, args.agent) {
        (None, Some(agent)) => {
            return hook_mode(&agent, args.endpoint, Duration::from_secs(args.timeout));
        }
        (None, None) => Err("give --agent <id> (hook mode) or a command; see --help".to_owned()),
        (Some(Cmd::Detect), _) => Ok(detect()),
        (Some(Cmd::Status), _) => status(),
        (
            Some(Cmd::Install {
                agent,
                events,
                hook_timeout,
                program,
                apply,
            }),
            _,
        ) => install(&agent, &events, hook_timeout, program, apply),
        (Some(Cmd::Uninstall { agent, apply }), _) => uninstall(&agent, apply),
    };
    match result {
        Ok(v) => {
            if json_out {
                println!("{v}");
            } else {
                print!("{}", text(&v));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json_out {
                println!("{}", json!({ "ok": false, "error": e }));
            } else {
                eprintln!("error: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

fn adapter(id: &str) -> Result<&'static dyn Adapter, String> {
    agents::get(id).ok_or_else(|| {
        let known: Vec<&str> = agents::all().iter().map(|a| a.info().id).collect();
        format!("no agent '{id}' (known: {})", known.join(", "))
    })
}

fn program_name() -> String {
    format!("{}{}", names::HOOK_BIN, std::env::consts::EXE_SUFFIX)
}

// --- hook mode ---------------------------------------------------------------------------

fn hook_mode(agent: &str, endpoint: Option<String>, timeout: Duration) -> ExitCode {
    let quiet = |why: String| {
        eprintln!("{}: {why}", names::HOOK_BIN);
        ExitCode::SUCCESS
    };
    let adapter = match adapter(agent) {
        Ok(a) => a,
        Err(e) => return quiet(e),
    };
    let Some(hooks) = adapter.hooks() else {
        return quiet(format!("{agent} has no hooks"));
    };
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().take(MAX_INPUT).read_to_end(&mut input) {
        return quiet(format!("cannot read the hook input: {e}"));
    }
    let event = match hooks.parse(&input) {
        Ok(e) => e,
        Err(e) => return quiet(e.to_string()),
    };
    let endpoint = match endpoint.map_or_else(platform::paths::default_endpoint, Ok) {
        Ok(e) => e,
        Err(e) => return quiet(format!("cannot work out the endpoint: {e}")),
    };
    let mut event = serde_json::to_value(event).unwrap();
    event["automatic_continuation"] =
        json!(std::env::var("AWAY_GUARD_CONTINUATION").ok().as_deref() == event["session_id"].as_str());
    if event["kind"] == "turn_end" && event["outcome"]["failure"]["network"] == true {
        // Read now: when the session ends, its hook has little time and the answer is the same.
        event["in_a_window"] = json!(platform::agent_runs_in_a_window(agent));
        if let Some(session) = event["session_id"].as_str() {
            match platform::create_resume_broker(agent, session, &endpoint, event["transcript_path"].as_str()) {
                Ok(broker) => event["resume_broker"] = json!(broker),
                Err(e) => event["resume_broker_error"] = json!(e.message),
            }
        }
    }
    let request = json!({ "agent": agent, "event": event });
    // Everything that talks to the daemon runs on a thread we stop waiting for after `timeout`.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let info = ClientInfo {
            name: names::HOOK_BIN.into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        };
        let out = platform::connect_client(&endpoint, info, timeout)
            .and_then(|mut c| c.call("agents.hook", request))
            .and_then(|r| r.into_result());
        let _ = tx.send(out);
    });
    match rx.recv_timeout(timeout + Duration::from_millis(500)) {
        Ok(Ok(result)) => match result.get("reply").filter(|r| r.is_object()) {
            Some(reply) => {
                if let Some(s) = reply.get("stdout").and_then(Value::as_str) {
                    println!("{s}");
                }
                let code = reply.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
                ExitCode::from(u8::try_from(code).unwrap_or(0))
            }
            None => ExitCode::SUCCESS,
        },
        Ok(Err(e)) => quiet(format!("daemon: {}", e.message)),
        Err(_) => quiet("the daemon did not answer in time".to_owned()),
    }
}

// --- detect / status ---------------------------------------------------------------------

fn detect() -> Value {
    let env = Env::from_process();
    let installs: Vec<_> = agents::all()
        .iter()
        .flat_map(|a| agents::detect::installations(*a, &env, DETECT_TIMEOUT))
        .collect();
    json!({ "installs": installs })
}

fn status() -> Result<Value, String> {
    let env = Env::from_process();
    let mut out = Vec::new();
    for a in agents::all() {
        let Some(h) = a.hooks() else { continue };
        let file = h.config_file(&env);
        match h.installed(&env, &program_name()) {
            Ok(list) => out.push(json!({ "agent": a.info().id, "config_file": file, "hooks": list })),
            Err(e) => out.push(json!({ "agent": a.info().id, "config_file": file, "error": e.to_string() })),
        }
    }
    Ok(json!({ "agents": out }))
}

// --- install / uninstall -----------------------------------------------------------------

fn install(
    id: &str,
    events: &[String],
    timeout: u32,
    program: Option<PathBuf>,
    apply: bool,
) -> Result<Value, String> {
    let a = adapter(id)?;
    let hooks = a.hooks().ok_or_else(|| format!("{id} has no hooks"))?;
    let env = Env::from_process();
    let version = installed_version(a, &env)?;
    let program = match program {
        Some(p) => p,
        None => std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?,
    };
    if !program.is_absolute() || !program.is_file() {
        return Err(format!(
            "{} is not an existing file given by its full path",
            program.display()
        ));
    }
    let mut kinds: Vec<HookKind> = Vec::new();
    if events.is_empty() {
        for (k, _) in hooks.events() {
            if !kinds.contains(k) {
                kinds.push(*k);
            }
        }
    } else {
        for e in events {
            kinds.push(HookKind::from_name(e).ok_or_else(|| format!("unknown hook kind '{e}'"))?);
        }
    }
    let req = HookInstall {
        program,
        kinds: kinds.into_iter().map(|k| (k, timeout)).collect(),
        agent_version: Some(version),
    };
    let plan = hooks.plan_install(&env, &req).map_err(|e| e.to_string())?;
    finish(plan, apply)
}

fn uninstall(id: &str, apply: bool) -> Result<Value, String> {
    let a = adapter(id)?;
    let hooks = a.hooks().ok_or_else(|| format!("{id} has no hooks"))?;
    let plan = hooks
        .plan_uninstall(&Env::from_process(), &program_name())
        .map_err(|e| e.to_string())?;
    finish(plan, apply)
}

/// The version of the agent's installs; refuses when none is found or any is not recognised
/// (they all share one config file).
fn installed_version(a: &dyn Adapter, env: &Env) -> Result<String, String> {
    let installs = agents::detect::installations(a, env, DETECT_TIMEOUT);
    if installs.is_empty() {
        return Err(format!(
            "{} is not installed (looked on PATH and in its usual folders)",
            a.info().display_name
        ));
    }
    if let Some(bad) = installs.iter().find(|i| !i.recognised) {
        return Err(format!(
            "{}: {} — not installing hooks for a version this tool was not tested with",
            bad.path.display(),
            bad.problem.clone().unwrap_or_default()
        ));
    }
    installs
        .iter()
        .filter_map(|i| i.version.clone())
        .max_by(|x, y| agents::Version::parse(x).cmp(&agents::Version::parse(y)))
        .ok_or_else(|| "no version found".to_owned())
}

fn finish(plan: HookPlan, apply: bool) -> Result<Value, String> {
    let mut v = json!({ "agent": plan.agent, "changes": plan.files, "notes": plan.notes, "applied": false });
    if plan.is_noop() {
        v["nothing_to_do"] = json!(true);
        return Ok(v);
    }
    if apply {
        let done = plan.apply().map_err(|e| e.to_string())?;
        v["applied"] = json!(true);
        v["written"] = json!(done.written);
        v["backups"] = json!(done.backups);
    }
    Ok(v)
}

// --- text output -------------------------------------------------------------------------

fn text(v: &Value) -> String {
    let mut s = String::new();
    if let Some(installs) = v["installs"].as_array() {
        if installs.is_empty() {
            s.push_str("no agent found\n");
        }
        for i in installs {
            let ok = if i["recognised"] == true {
                "ok"
            } else {
                "NOT RECOGNISED"
            };
            s.push_str(&format!(
                "{:<12} {:<20} {:<15} {}\n",
                i["agent"].as_str().unwrap_or(""),
                i["version"].as_str().unwrap_or("?"),
                ok,
                i["path"].as_str().unwrap_or("")
            ));
            if let Some(p) = i["problem"].as_str() {
                s.push_str(&format!("             {p}\n"));
            }
        }
    }
    if let Some(list) = v["agents"].as_array() {
        for a in list {
            s.push_str(&format!(
                "{} ({})\n",
                a["agent"].as_str().unwrap_or(""),
                a["config_file"].as_str().unwrap_or("")
            ));
            if let Some(e) = a["error"].as_str() {
                s.push_str(&format!("  ! {e}\n"));
            }
            let hooks = a["hooks"].as_array().cloned().unwrap_or_default();
            if hooks.is_empty() && a["error"].is_null() {
                s.push_str("  no hooks of ours\n");
            }
            for h in hooks {
                s.push_str(&format!(
                    "  {:<18} {}\n",
                    h["native_event"].as_str().unwrap_or(""),
                    h["command"].as_str().unwrap_or("")
                ));
            }
        }
    }
    if v.get("changes").is_some() {
        if v["nothing_to_do"] == true {
            s.push_str("nothing to change\n");
        }
        for f in v["changes"].as_array().into_iter().flatten() {
            s.push_str(&format!("{}:\n", f["path"].as_str().unwrap_or("")));
            for line in f["summary"].as_array().into_iter().flatten() {
                s.push_str(&format!("  {}\n", line.as_str().unwrap_or("")));
            }
        }
        for n in v["notes"].as_array().into_iter().flatten() {
            s.push_str(&format!("note: {}\n", n.as_str().unwrap_or("")));
        }
        if v["applied"] == true {
            for b in v["backups"].as_array().into_iter().flatten() {
                s.push_str(&format!("backup: {}\n", b.as_str().unwrap_or("")));
            }
            s.push_str("written.\n");
        } else if v["nothing_to_do"] != true {
            s.push_str("nothing written (dry run); add --apply to write it.\n");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn binary_name_matches_names_table() {
        assert_eq!(env!("CARGO_BIN_NAME"), proto::names::HOOK_BIN);
    }
}
