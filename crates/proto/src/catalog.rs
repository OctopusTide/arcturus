//! The handshake (`core.hello`) and the command catalog it returns. Clients build their
//! command lists from this catalog, so a module added to the daemon shows up in every client
//! without client changes.

use serde::{Deserialize, Serialize};

use crate::protocol::ClientInfo;

/// Which part of the product a module belongs to. New values may be added later; clients
/// that do not know a value see [`Section::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    /// 公共部分: the skeleton itself, page, tray.
    Common,
    /// 使用: keeping things running while the person is away (power, network, tasks, resources).
    Usage,
    /// 账号和存储: conversation scope, search, file ownership, cleanup.
    AccountsStorage,
    /// 保护: permission list, exit protection.
    Protection,
    #[serde(other)]
    Unknown,
}

impl Section {
    /// Display order for clients.
    pub const ORDER: [Section; 5] =
        [Section::Common, Section::Usage, Section::AccountsStorage, Section::Protection, Section::Unknown];

    pub const fn wire_name(self) -> &'static str {
        match self {
            Section::Common => "common",
            Section::Usage => "usage",
            Section::AccountsStorage => "accounts_storage",
            Section::Protection => "protection",
            Section::Unknown => "unknown",
        }
    }

    /// Chinese heading used in help output.
    pub const fn title(self) -> &'static str {
        match self {
            Section::Common => "公共部分",
            Section::Usage => "使用",
            Section::AccountsStorage => "账号和存储",
            Section::Protection => "保护",
            Section::Unknown => "其他",
        }
    }
}

/// Type of one command argument. Clients unaware of a newer kind see `Unknown` and may pass
/// the raw value as JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgKind {
    /// Signed 64-bit integer.
    Int,
    /// Unsigned 64-bit integer.
    Uint,
    Str,
    /// Flag; present means `true`.
    Bool,
    /// List of strings (repeatable flag).
    StrList,
    /// Any JSON value.
    Json,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArgInfo {
    pub name: String,
    pub kind: ArgKind,
    #[serde(default)]
    pub required: bool,
    /// Given by position on the command line instead of `--name`.
    #[serde(default)]
    pub positional: bool,
    #[serde(default)]
    pub help: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandInfo {
    /// Canonical wire name, `<module>.<verb>`.
    pub name: String,
    pub module: String,
    pub verb: String,
    #[serde(default)]
    pub section: Option<Section>,
    /// Optional top-level short name (`protect`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub args: Vec<ArgInfo>,
    #[serde(default)]
    pub read_only: bool,
    /// Agents may call it (otherwise page/tray only).
    #[serde(default)]
    pub agent: bool,
    /// The caller that did this handshake may call it.
    #[serde(default)]
    pub callable: bool,
    /// Not meant for command lines (for example `core.hello`).
    #[serde(default)]
    pub hidden: bool,
    /// The owning module is switched on.
    #[serde(default)]
    pub module_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub name: String,
    #[serde(default)]
    pub section: Option<Section>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub enabled: bool,
    /// `running`, `stopped`, `disabled` or `faulted`.
    #[serde(default)]
    pub health: String,
    /// Declared subcodes, `<module>.<NAME>`.
    #[serde(default)]
    pub error_codes: Vec<String>,
    /// Events the module publishes, `<module>.<name>`.
    #[serde(default)]
    pub events: Vec<String>,
}

/// `args` of `core.hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloArgs {
    /// Every protocol version the client speaks.
    pub protocols: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonHello {
    pub product: String,
    pub version: String,
    pub pid: u32,
    pub min_protocol: u32,
    pub max_protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallerInfo {
    /// `agent` or `trusted` (page/tray).
    pub kind: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub exe: Option<String>,
}

/// `result` of `core.hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    /// The version every later request on this connection must use.
    pub protocol: u32,
    pub daemon: DaemonHello,
    pub caller: CallerInfo,
    #[serde(default)]
    pub modules: Vec<ModuleInfo>,
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
}

impl HelloResult {
    /// Looks a command up by canonical or short name.
    pub fn command(&self, name: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|c| c.name == name || c.short.as_deref() == Some(name))
    }
}

/// Highest version in both the daemon's range and the client's list.
pub fn negotiate(daemon_min: u32, daemon_max: u32, client: &[u32]) -> Option<u32> {
    client.iter().copied().filter(|v| (daemon_min..=daemon_max).contains(v)).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_picks_the_highest_common_version() {
        assert_eq!(negotiate(1, 1, &[1]), Some(1));
        assert_eq!(negotiate(1, 3, &[1, 2]), Some(2));
        assert_eq!(negotiate(2, 3, &[1]), None);
        assert_eq!(negotiate(1, 1, &[2, 3]), None);
    }

    #[test]
    fn unknown_enum_values_from_newer_peers_are_tolerated() {
        let s: Section = serde_json::from_str("\"something_new\"").unwrap();
        assert_eq!(s, Section::Unknown);
        let k: ArgKind = serde_json::from_str("\"duration\"").unwrap();
        assert_eq!(k, ArgKind::Unknown);
        let c: CommandInfo = serde_json::from_str(r#"{"name":"a.b","module":"a","verb":"b","later":1}"#).unwrap();
        assert!(!c.agent && c.args.is_empty());
    }

    #[test]
    fn section_wire_names_match_serde() {
        for s in Section::ORDER {
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::Value::String(s.wire_name().into()));
        }
    }
}
