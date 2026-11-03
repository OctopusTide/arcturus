//! Reusable read-only networking boundary, shared by connectivity and egress policies.

use std::sync::Arc;
use proto::ApiError;
use serde::{Deserialize, Serialize};
use super::{Guard, unsupported};
#[path = "network_fake.rs"]
pub mod fake;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkChange { Interface, Address, Route, Proxy }

pub type NetworkSink = Arc<dyn Fn(NetworkChange) + Send + Sync>;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct NetworkInterface {pub id:String,pub name:String,pub up:bool,pub tunnel:bool}

/// DNS route assessment, not a claim about application DoH or remote proxy DNS.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsAssessment {
    pub state: String,
    pub details: Vec<String>,
}

/// Each request explicitly selects the same OS/VPN + proxy route as the application.
/// Local control APIs always use Direct, ignore proxies and reject redirects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "url", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProxyRoute { #[default] System, Direct, Explicit(String) }

pub trait Network: Send + Sync {
    /// Saved profiles in OS order, without keys or credentials.
    fn profiles(&self)->Result<Vec<WifiProfile>,ApiError>{Err(unsupported("saved_wifi_profiles"))}
    fn connect_profile(&self,_interface:&str,_profile:&str)->Result<(),ApiError>{Err(unsupported("connect_saved_wifi"))}
    fn disconnect(&self,_interface:&str)->Result<(),ApiError>{Err(unsupported("disconnect_wifi"))}
    fn link_readings(&self)->Result<Vec<LinkReading>,ApiError>{Err(unsupported("network_link_readings"))}
    /// Marks a saved profile metered; no credential or profile XML is accepted.
    fn set_profile_metered(&self,_interface:&str,_profile:&str,_metered:bool)->Result<(),ApiError>{Err(unsupported("set_profile_metered"))}
    fn probe(&self,url:&str,route:&ProxyRoute)->Probe {
        probe_result(self.get(url,route))
    }
    /// A VPN check must verify the actual connection's selected interface, not guess from its country.
    fn get_for_route(&self,url:&str,proxy:&ProxyRoute,route:&super::exit_firewall::ExitRoute)->Result<String,ApiError>{
        match route{super::exit_firewall::ExitRoute::Vpn{..}=>Err(unsupported("verify VPN probe route")),_=>self.get(url,proxy)}
    }
    fn interfaces(&self)->Result<Vec<NetworkInterface>,ApiError>{Err(unsupported("network_interfaces"))}
    fn watch(&self, _sink: NetworkSink) -> Result<Box<dyn Guard>, ApiError> {
        Err(unsupported("watch_network"))
    }
    /// HTTPS public requests; HTTP is allowed only for literal loopback control APIs.
    /// Bounded time/body, checked HTTP status, no redirect or direct-on-proxy-error fallback.
    fn get(&self, _url: &str, _route: &ProxyRoute) -> Result<String, ApiError> {
        Err(unsupported("network_get"))
    }
    fn dns(&self) -> Result<DnsAssessment, ApiError> {
        Err(unsupported("dns_routes"))
    }
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct WifiProfile {pub interface:String,pub name:String,pub connected:bool,pub available:bool,pub hotspot_hint:bool}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct LinkReading {pub interface:String,pub up:bool,pub received_bytes:u64,pub sent_bytes:u64,pub metric:u32,pub automatic_metric:bool}
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Probe {pub reachable:bool,pub captive:bool,pub body:Option<String>,pub error:Option<String>}
pub fn probe_result(result:Result<String,ApiError>)->Probe {
    match result {Ok(body)=>Probe {reachable:true,captive:false,body:Some(body),error:None},Err(e)=>{
        let status=e.details.as_ref().and_then(|d|d["http_status"].as_u64());
        Probe {reachable:status.is_some_and(|s|matches!(s,401|403|404|405|429)),captive:status.is_some_and(|s|(300..400).contains(&s)),body:None,error:Some(e.message)}
    }}
}

pub struct UnsupportedNetwork;
impl Network for UnsupportedNetwork {}
pub static UNSUPPORTED: UnsupportedNetwork = UnsupportedNetwork;
