//! Start at logon, start again after a crash. A Task Scheduler task for the current user
//! runs the launcher at logon and every five minutes after that; a second copy is not started
//! while one runs (`IgnoreNew`), so the repetition only matters if the launcher itself died.
//! The launcher starts the daemon and starts it again when it stops unexpectedly.
//!
//! If the task cannot be created (refused for lack of rights), `install` falls back to the
//! per-user Run key: the launcher still starts at logon and still restarts the daemon, but
//! nothing restarts the launcher itself. Both are changes to this machine and only happen when
//! the user runs `away-guard-daemon autostart install`; tests only build the XML.

use std::path::Path;
use std::process::Command;

use proto::names;

use crate::windows::ffi;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// One task per user, so two accounts on the machine do not overwrite each other.
pub fn task_name(sid: &str) -> String {
    format!("{}-{sid}", names::DAEMON_BIN)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The task definition (Task Scheduler schema 1.2).
pub fn task_xml(launcher: &Path, sid: &str) -> String {
    let command = escape(&launcher.display().to_string());
    let dir = escape(&launcher.parent().map(|p| p.display().to_string()).unwrap_or_default());
    let sid = escape(sid);
    let name = escape(&task_name(&sid));
    let display = names::DISPLAY_NAME;
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{display} ({product}): starts the background process at logon and again if it stops.</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
      <Repetition>
        <Interval>PT5M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{command}"</Command>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        product = names::PRODUCT,
    )
}

/// How autostart ended up being set up.
#[derive(Debug, PartialEq)]
pub enum Installed {
    Task(String),
    /// The task was refused (the reason); the Run key is used instead.
    RunKey(String),
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    let text = format!("{}{}", decode(&out.stdout), decode(&out.stderr)).trim().to_owned();
    if out.status.success() { Ok(text) } else { Err(text) }
}

/// schtasks and reg print in the console code page; good enough for messages.
fn decode(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn quoted(launcher: &Path) -> String {
    format!("\"{}\"", launcher.display())
}

/// Registers the task for the current user (replacing an older one), or the Run key if the
/// task is refused.
pub fn install(launcher: &Path) -> Result<Installed, String> {
    let sid = ffi::current_user_sid().map_err(|e| format!("cannot read the user SID: {e}"))?;
    let name = task_name(&sid);
    // schtasks reads the file as UTF-16 with a byte-order mark.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(task_xml(launcher, &sid).encode_utf16().flat_map(u16::to_le_bytes));
    let file = std::env::temp_dir().join(format!("{}-task-{}.xml", names::PRODUCT, std::process::id()));
    std::fs::write(&file, bytes).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    let created = run(Command::new("schtasks").args(["/Create", "/F", "/TN", &name, "/XML"]).arg(&file));
    let _ = std::fs::remove_file(&file);
    match created {
        Ok(_) => {
            // Only one way of starting at logon.
            let _ = remove_run_key();
            Ok(Installed::Task(name))
        }
        Err(refused) => {
            run(Command::new("reg").args(["add", RUN_KEY, "/v", names::PRODUCT, "/t", "REG_SZ", "/f", "/d"]).arg(quoted(launcher)))
                .map_err(|e| format!("the scheduled task was refused ({refused}) and so was the Run key ({e})"))?;
            Ok(Installed::RunKey(refused))
        }
    }
}

fn remove_run_key() -> Result<String, String> {
    run(Command::new("reg").args(["delete", RUN_KEY, "/v", names::PRODUCT, "/f"]))
}

/// Removes the task and the Run key; says which of them were there.
pub fn remove() -> Result<Vec<String>, String> {
    let sid = ffi::current_user_sid().map_err(|e| format!("cannot read the user SID: {e}"))?;
    let name = task_name(&sid);
    let mut removed = Vec::new();
    if run(Command::new("schtasks").args(["/Delete", "/F", "/TN", &name])).is_ok() {
        removed.push(format!("scheduled task {name}"));
    }
    if remove_run_key().is_ok() {
        removed.push(format!(r"Run key value {RUN_KEY}\{}", names::PRODUCT));
    }
    Ok(removed)
}

/// What is set up now, for people to read.
pub fn show() -> Result<String, String> {
    let sid = ffi::current_user_sid().map_err(|e| format!("cannot read the user SID: {e}"))?;
    let name = task_name(&sid);
    let task = run(Command::new("schtasks").args(["/Query", "/TN", &name, "/V", "/FO", "LIST"]))
        .unwrap_or_else(|_| format!("scheduled task {name}: not installed"));
    let key = run(Command::new("reg").args(["query", RUN_KEY, "/v", names::PRODUCT]))
        .unwrap_or_else(|_| format!(r"Run key value {RUN_KEY}\{}: not set", names::PRODUCT));
    Ok(format!("{task}\n\n{key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_runs_the_launcher_for_this_user_only_and_never_stops_it() {
        let x = task_xml(Path::new(r"C:\Program Files\A&B\away-guard-launcher.exe"), "S-1-5-21-1-2-3-1001");
        assert!(x.starts_with(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        for part in [
            "<URI>\\away-guard-daemon-S-1-5-21-1-2-3-1001</URI>",
            "<LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>S-1-5-21-1-2-3-1001</UserId>",
            "<Interval>PT5M</Interval>",
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<Priority>4</Priority>",
            "<Count>999</Count>",
            "<Command>\"C:\\Program Files\\A&amp;B\\away-guard-launcher.exe\"</Command>",
            "<WorkingDirectory>C:\\Program Files\\A&amp;B</WorkingDirectory>",
        ] {
            assert!(x.contains(part), "{part} missing from\n{x}");
        }
        // Well formed enough: every opening tag of the main blocks is closed.
        for tag in ["Task", "Triggers", "Principals", "Settings", "Actions", "Exec"] {
            let opened = x.matches(&format!("<{tag}>")).count() + x.matches(&format!("<{tag} ")).count();
            assert_eq!((opened, x.matches(&format!("</{tag}>")).count()), (1, 1), "{tag}");
        }
    }

    #[test]
    fn one_task_per_user() {
        assert_ne!(task_name("S-1-5-21-1"), task_name("S-1-5-21-2"));
        assert!(task_name("S-1").starts_with(names::DAEMON_BIN));
    }
}
