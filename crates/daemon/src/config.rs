//! The configuration file (`config.json` in the data folder). Today it only switches modules
//! on and off; unknown keys are ignored so later versions can add more.
//!
//! ```json
//! { "modules": { "scope": { "enabled": true }, "network": { "enabled": false } } }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub modules: BTreeMap<String, ModuleConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModuleConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl Config {
    /// Reads the file. A missing file means defaults; a broken file is an error, so a typo
    /// never silently re-enables a module the user switched off.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn module_enabled(&self, module: &str) -> Option<bool> {
        self.modules.get(module).and_then(|m| m.enabled)
    }
}
