//! Where things live on Windows. The single place for platform-convention locations.

use std::io;
use std::path::{Path, PathBuf};

use proto::names;

/// `\\.\pipe\away-guard-<user SID>`. The SID keeps two Windows users on one machine from
/// colliding; the pipe's DACL is what actually restricts access.
pub fn default_endpoint() -> io::Result<String> {
    Ok(format!(r"\\.\pipe\{}-{}", names::PRODUCT, super::ffi::current_user_sid()?))
}

/// `%LOCALAPPDATA%\away-guard`.
pub fn default_data_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(base).join(names::PRODUCT))
}

/// The configuration file sits next to the database on Windows.
pub fn config_file(data_dir: &Path) -> PathBuf {
    data_dir.join(names::CONFIG_FILE)
}
