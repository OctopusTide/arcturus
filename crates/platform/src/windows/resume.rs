use daemon::sys::{AgentResume, ProcessInfo};
use proto::{ApiError, ErrorCode};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
pub fn launch(request: &AgentResume) -> Result<ProcessInfo, ApiError> {
    if let Some(broker) = &request.broker {
        return super::resume_broker::launch(request, broker);
    }
    let settings = agents::RunSettings {
        permission_mode: request.permission_mode.clone(),
        ..Default::default()
    };
    launch_scoped(request, false, &settings)
}
pub(crate) fn launch_inherited(
    request: &AgentResume,
    settings: &agents::RunSettings,
) -> Result<ProcessInfo, ApiError> {
    launch_scoped(request, true, settings)
}
fn launch_scoped(
    request: &AgentResume,
    inherited: bool,
    settings: &agents::RunSettings,
) -> Result<ProcessInfo, ApiError> {
    let adapter =
        agents::get(&request.agent).ok_or_else(|| ApiError::invalid_args("unknown agent adapter"))?;
    let path = Path::new(&request.executable);
    if !path.is_absolute()
        || !path.is_file()
        || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
        || !adapter
            .info()
            .executables
            .iter()
            .any(|n| n.eq_ignore_ascii_case(agents::file_name(&request.executable)))
    {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "resume requires the recognised native agent executable, not a shell or shim",
        ));
    }
    if request.session_id.is_empty()
        || request.session_id.len() > 128
        || request.session_id.starts_with('-')
        || !request
            .session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(ApiError::invalid_args("invalid session id"));
    }
    let empty = std::collections::BTreeMap::new();
    let environment = request
        .environment
        .as_ref()
        .or(inherited.then_some(&empty))
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::Denied,
                "original connection context unavailable; resume manually",
            )
        })?;
    if environment
        .iter()
        .any(|(key, value)| !agents::safe_resume_env(key, value))
    {
        return Err(ApiError::invalid_args("unsafe connection context"));
    }
    let mut version_command = Command::new(path);
    for key in [
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "OPENAI_API_KEY",
        "CLAUDE_CONFIG_DIR",
        "NODE_OPTIONS",
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
    ] {
        if !inherited {
            version_command
                .env_remove(key)
                .env_remove(key.to_ascii_lowercase());
        }
    }
    version_command.envs(environment);
    let mut version = version_command
        .args(adapter.version_args())
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if version
            .try_wait()
            .map_err(|e| ApiError::internal(e.to_string()))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = version.kill();
            let _ = version.wait();
            return Err(ApiError::internal("agent version check timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = version
        .wait_with_output()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let recognised = output.status.success()
        && adapter
            .parse_version_output(&String::from_utf8_lossy(&output.stdout))
            .is_some_and(|v| adapter.info().tested.contains(&v));
    if !recognised {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "agent version is not recognised; automatic continuation was not started",
        ));
    }
    let args = match &request.fork_session_id {
        Some(new_session) => adapter.fork_args(&request.session_id, new_session, &request.prompt, settings),
        None => adapter.resume_args(&request.session_id, &request.prompt, settings),
    }
    .ok_or_else(|| {
        ApiError::new(
            ErrorCode::NotImplemented,
            "adapter cannot reopen this conversation",
        )
    })?;
    let mut command = Command::new(path);
    for key in [
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "OPENAI_API_KEY",
        "CLAUDE_CONFIG_DIR",
        "NODE_OPTIONS",
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
    ] {
        if !inherited {
            command.env_remove(key).env_remove(key.to_ascii_lowercase());
        }
    }
    command.envs(environment);
    command.env(
        "AWAY_GUARD_CONTINUATION",
        request.fork_session_id.as_ref().unwrap_or(&request.session_id),
    );
    command
        .args(args)
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = &request.cwd {
        let path = Path::new(cwd);
        if !path.is_absolute() || !path.is_dir() {
            return Err(ApiError::invalid_args(
                "resume working directory no longer exists",
            ));
        }
        command.current_dir(path);
    }
    let child = command.spawn().map_err(|e| ApiError::internal(e.to_string()))?;
    // Dropping Child does not kill the conversation if our daemon restarts.
    super::process::process_info(child.id())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_executables_and_shells_are_not_resume_targets() {
        let r = AgentResume {
            agent: "claude-code".into(),
            executable: r"C:\Windows\System32\cmd.exe".into(),
            session_id: "session".into(),
            cwd: None,
            prompt: "continue".into(),
            environment: None,
            fork_session_id: None,
            permission_mode: None,
            broker: None,
        };
        assert_eq!(launch(&r).unwrap_err().code, ErrorCode::Denied);
    }
}
