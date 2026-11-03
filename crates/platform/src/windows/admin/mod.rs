//! Narrow, authenticated administrator helper. Service installation is an explicit action.
mod client;
mod operations;
mod service;
mod transport;
pub use client::ServiceElevated;
use proto::names;
pub use service::{install, remove, run, show};
use std::path::PathBuf;
pub fn install_dir() -> Result<PathBuf, String> {
    Err("choose an administrator-owned local deployment directory with --directory; no machine-specific default is assumed".into())
}
pub fn endpoint(sid: &str) -> String {
    format!(r"\\.\pipe\{}-admin-{sid}", names::PRODUCT)
}
pub fn service_name(sid: &str) -> String {
    format!("{}-admin-{sid}", names::PRODUCT)
}
pub fn valid_sid(sid: &str) -> bool {
    sid.starts_with("S-1-5-21-")
        && sid.len() < 184
        && sid[9..].chars().all(|c| c.is_ascii_digit() || c == '-')
}

pub(super) fn service_path_is_secure(path: &std::path::Path) -> bool {
    service::verify_secure_chain(path).is_ok()
}
