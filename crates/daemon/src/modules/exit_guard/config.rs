use crate::sys::exit_firewall::ExitRoute;
use crate::sys::network::ProxyRoute;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::net::IpAddr;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Region {
    /// ISO 3166-1 alpha-2; None means no country has been selected yet.
    pub country: Option<String>,
    pub ip: Option<IpAddr>,
    pub node: Option<String>,
}
impl Region {
    pub fn configured(&self) -> bool {
        self.country.is_some() || self.ip.is_some() || self.node.is_some()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    Block,
    Freeze,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub path: String,
    #[serde(default)]
    pub region: Option<Region>,
    #[serde(default)]
    pub action: Action,
    /// Override when this application uses a different proxy from the system.
    #[serde(default)]
    pub proxy: Option<ProxyRoute>,
    #[serde(default)]
    pub firewall_route: Option<ExitRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Control {
    pub url: String,
    pub node_pointer: String,
    pub mode_pointer: String,
    pub poll_seconds: u32,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            url: String::new(),
            node_pointer: "/now".into(),
            mode_pointer: "/mode".into(),
            poll_seconds: 3,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub programs: Vec<Program>,
    pub region: Region,
    pub proxy: ProxyRoute,
    /// None disables periodic public requests.
    pub interval_seconds: Option<u32>,
    pub control: Option<Control>,
    pub firewall_route: Option<ExitRoute>,
}

/// Protection settings fail closed: malformed stored data must not erase the active list.
pub fn load(ctx: &crate::ModuleCtx) -> Result<Settings, proto::ApiError> {
    let Some(stored) = ctx.settings().get("settings")? else {
        return Ok(Settings::default());
    };
    // Stored values are complete settings, not a UI merge patch. In particular,
    // programs:null must be rejected rather than deleting the key and defaulting
    // to an empty list, which would remove persistent protection on recovery.
    let settings: Settings = serde_json::from_value(stored).map_err(|e| {
        proto::ApiError::invalid_args(format!(
            "stored egress settings are invalid; existing firewall locks retained: {e}"
        ))
    })?;
    validate(&settings).map_err(proto::ApiError::invalid_args)?;
    Ok(settings)
}

pub fn path_key(path: &str) -> Result<String, String> {
    let p = path.replace('/', "\\");
    let b = p.as_bytes();
    let drive = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    if !drive
        || p.starts_with("\\\\?\\")
        || p.starts_with("\\\\.\\")
        || p.ends_with('\\')
        || p.contains(['*', '?', '\0', '"'])
        || p.chars().any(char::is_control)
        || p.split('\\')
            .any(|s| s == "." || s == ".." || s.ends_with(' ') || s.ends_with('.'))
        || p[if drive { 2 } else { 0 }..].contains(':')
    {
        return Err("program must be a full local drive path without UNC, wildcards, device paths or relative segments".into());
    }
    Ok(p.to_lowercase())
}

pub fn local_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        let Some((host, suffix)) = v6.split_once(']') else {
            return false;
        };
        if !suffix.is_empty() && !valid_port(suffix.strip_prefix(':').unwrap_or("")) {
            return false;
        }
        host
    } else {
        let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
        if authority.contains(':') && !valid_port(port) {
            return false;
        }
        host
    };
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        && !url.contains(['@', '#', '\\'])
        && !url.chars().any(char::is_control)
}
fn valid_port(p: &str) -> bool {
    p.parse::<u16>().is_ok_and(|p| p != 0)
}

pub fn validate(s: &Settings) -> Result<(), String> {
    if s.programs.len() > 256 {
        return Err("at most 256 protected programs".into());
    }
    let mut keys = BTreeSet::new();
    for p in &s.programs {
        if !keys.insert(path_key(&p.path)?) {
            return Err("duplicate full program path".into());
        }
        if let Some(r) = &p.region {
            validate_region(r)?;
        }
        if let Some(proxy) = &p.proxy {
            validate_proxy(proxy)?;
        }
        if let Some(route) = &p.firewall_route {
            route.validate().map_err(|e| e.message)?;
        }
    }
    validate_proxy(&s.proxy)?;
    validate_region(&s.region)?;
    if let Some(route) = &s.firewall_route {
        route.validate().map_err(|e| e.message)?;
    }
    if s.interval_seconds
        .is_some_and(|n| !(10..=86400).contains(&n))
    {
        return Err("interval_seconds must be null or 10..86400".into());
    }
    if let Some(c) = &s.control {
        if !local_url(&c.url) {
            return Err("control.url must use a literal loopback HTTP(S) address".into());
        }
        if !(1..=3600).contains(&c.poll_seconds) {
            return Err("control.poll_seconds must be 1..3600".into());
        }
        for p in [&c.node_pointer, &c.mode_pointer] {
            if !p.starts_with('/') || p.len() > 256 {
                return Err("control fields must be JSON pointers beginning with /".into());
            }
        }
    }
    Ok(())
}
fn validate_proxy(p: &ProxyRoute) -> Result<(), String> {
    if let ProxyRoute::Explicit(p) = p {
        let p = p.strip_prefix("http://").unwrap_or(p);
        if p.is_empty()
            || !p.contains(':')
            || p.contains(['/', '@', '\\', ';'])
            || p.chars().any(char::is_whitespace)
        {
            return Err("explicit proxy must be an HTTP proxy host:port; SOCKS and credentials are unsupported".into());
        }
    }
    Ok(())
}
fn validate_region(r: &Region) -> Result<(), String> {
    if r.country
        .as_ref()
        .is_some_and(|c| c.len() != 2 || !c.bytes().all(|b| b.is_ascii_alphabetic()))
    {
        return Err("country must be a two-letter country code".into());
    }
    if r.node
        .as_ref()
        .is_some_and(|n| n.trim().is_empty() || n.len() > 512)
    {
        return Err("node must contain 1..512 characters".into());
    }
    Ok(())
}

/// Tagged proxy routes are replaced when their kind changes; deep-merging enum payloads
/// would retain an old URL when switching back to System or Direct.
pub fn patch(base: &Settings, patch: &serde_json::Value) -> Result<Settings, proto::ApiError> {
    if !patch.is_object() {
        return Err(proto::ApiError::invalid_args("settings must be an object"));
    }
    let mut merged = crate::modules::settings::merge(serde_json::json!(base), patch.clone());
    if let Some(proxy) = patch.get("proxy").filter(|p| p.get("kind").is_some()) {
        merged["proxy"] = proxy.clone();
    }
    if let Some(route) = patch
        .get("firewall_route")
        .filter(|p| p.get("kind").is_some())
    {
        merged["firewall_route"] = route.clone();
    }
    let next: Settings = serde_json::from_value(merged)
        .map_err(|e| proto::ApiError::invalid_args(format!("settings: {e}")))?;
    validate(&next).map_err(proto::ApiError::invalid_args)?;
    Ok(next)
}
