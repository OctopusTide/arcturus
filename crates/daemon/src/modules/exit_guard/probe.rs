use super::config::Region;
use crate::sys::network::{Network, ProxyRoute};
use serde::Serialize;
use serde_json::Value;
use std::net::IpAddr;

pub const SERVICES: [&str; 3] = [
    "https://www.cloudflare.com/cdn-cgi/trace",
    "https://ipapi.co/json/",
    "https://ipwho.is/",
];
#[derive(Debug, Clone, Serialize)]
pub struct Exit {
    pub ip: IpAddr,
    pub country: String,
    pub service: String,
}

pub fn lookup(net: &dyn Network, route: &ProxyRoute) -> Result<Exit, Vec<String>> {
    lookup_guarded(net, route, &crate::sys::exit_firewall::ExitRoute::DenyAll)
}
pub fn lookup_guarded(
    net: &dyn Network,
    route: &ProxyRoute,
    firewall: &crate::sys::exit_firewall::ExitRoute,
) -> Result<Exit, Vec<String>> {
    let mut errors = Vec::new();
    for (index, url) in SERVICES.iter().enumerate() {
        match net
            .get_for_route(url, route, firewall)
            .map_err(|e| e.message)
            .and_then(|body| parse(&body, index))
        {
            Ok(mut exit) => {
                exit.service = (*url).into();
                return Ok(exit);
            }
            Err(e) => errors.push(format!("{url}: {e}")),
        }
    }
    Err(errors)
}
fn parse(body: &str, index: usize) -> Result<Exit, String> {
    let (ip, country) = if index == 0 {
        let field = |key: &str| {
            body.lines().find_map(|s| {
                s.split_once('=')
                    .filter(|(k, _)| *k == key)
                    .map(|(_, v)| v.trim().to_owned())
            })
        };
        (field("ip"), field("loc")) // loc is the client country; colo is the data center.
    } else {
        let v: Value = serde_json::from_str(body).map_err(|_| "invalid JSON".to_owned())?;
        if v["error"] == true || v["success"] == false {
            return Err("service refused lookup".into());
        }
        (
            v["ip"].as_str().map(str::to_owned),
            v["country_code"].as_str().map(str::to_owned),
        )
    };
    let ip = ip
        .ok_or("missing IP")?
        .parse::<IpAddr>()
        .map_err(|_| "invalid IP")?;
    let country = country.ok_or("missing country")?.to_ascii_uppercase();
    if country.len() != 2 || !country.bytes().all(|c| c.is_ascii_uppercase()) || country == "XX" {
        return Err("invalid country".into());
    }
    if ip.is_loopback() || ip.is_unspecified() {
        return Err("invalid public IP".into());
    }
    Ok(Exit {
        ip,
        country,
        service: String::new(),
    })
}
pub fn matches(region: &Region, exit: Option<&Exit>, node: Option<&str>) -> bool {
    let Some(exit) = exit else {
        return false;
    };
    region.configured()
        && region
            .country
            .as_ref()
            .is_none_or(|c| c.eq_ignore_ascii_case(&exit.country))
        && region.ip.is_none_or(|ip| ip == exit.ip)
        && region
            .node
            .as_deref()
            .is_none_or(|expected| node == Some(expected))
}
