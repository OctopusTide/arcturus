//! Shared full-path identity and cannot-close decisions.
use super::{ProcessInfo, System};
use crate::arbitration::ProgramRule;
use proto::ApiError;

/// Normalize full Windows paths without turning a basename into a wildcard.
pub fn normalize(path: &str) -> Result<String, ApiError> {
    let path = path.replace('/', "\\");
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    let drive = path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().get(2) == Some(&b'\\')
        && path.as_bytes()[0].is_ascii_alphabetic();
    let unc = path.starts_with(r"\\")
        && !path.starts_with(r"\\.\")
        && path.split('\\').filter(|s| !s.is_empty()).count() >= 3;
    if (!drive && !unc)
        || path.contains('\0')
        || path.contains(['*', '?'])
        || path
            .split('\\')
            .any(|p| p == ".." || p == "." || p.ends_with(' ') || p.ends_with('.'))
        || path[if drive { 2 } else { 0 }..].contains(':')
        || ![".exe", ".scr"]
            .iter()
            .any(|ext| path.to_ascii_lowercase().ends_with(ext))
    {
        return Err(ApiError::invalid_args(
            "program must be a full executable path without wildcards or traversal",
        ));
    }
    Ok(path.to_lowercase())
}

/// Unknown identity/ownership and mismatched signatures always fail closed.
pub fn blocked(sys: &dyn System, info: &ProcessInfo, rules: &[ProgramRule]) -> Option<String> {
    if info.pid <= 4 || info.pid == std::process::id() {
        return Some("system_or_daemon".into());
    }
    if info.own_user != Some(true) {
        return Some("other_or_unknown_user".into());
    }
    let path = match info.exe.as_deref().and_then(|p| normalize(p).ok()) {
        Some(p) => p,
        None => return Some("unknown_executable".into()),
    };
    if let Some(rule) = rules.iter().find(|r| r.path == path) {
        if let Some(expected) = &rule.signer {
            match sys.file_signer(info.exe.as_deref().unwrap()) {
                Ok(Some(actual)) if actual.eq_ignore_ascii_case(expected) => {}
                _ => return Some("signature_unverified".into()),
            }
        }
        return Some(rule.source.clone());
    }
    None
}
