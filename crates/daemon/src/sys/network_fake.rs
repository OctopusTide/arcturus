//! Scriptable network fake. No socket, DNS, desktop or native networking API is used.
use super::{DnsAssessment, Network, NetworkChange, NetworkSink, ProxyRoute};
use crate::sys::Guard;
use proto::ApiError;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct FakeNetwork {
    pub profiles: Mutex<Vec<super::WifiProfile>>,
    pub links: Mutex<Vec<super::LinkReading>>,
    pub connections: Mutex<Vec<(String, String)>>,
    pub connect_errors: Mutex<BTreeMap<String, ApiError>>,
    pub metered: Mutex<Vec<(String, String, bool)>>,
    pub before_get: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    pub guarded_requests: Mutex<Vec<super::super::exit_firewall::ExitRoute>>,
    pub interfaces: Mutex<Vec<super::NetworkInterface>>,
    pub requests: Mutex<Vec<(String, ProxyRoute)>>,
    pub replies: Mutex<BTreeMap<String, Result<String, ApiError>>>,
    pub dns: Mutex<DnsAssessment>,
    sinks: Arc<Mutex<Vec<NetworkSink>>>,
}
struct Subscription {
    sink: NetworkSink,
    sinks: Arc<Mutex<Vec<NetworkSink>>>,
}
impl Guard for Subscription {}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.sinks.lock().unwrap().retain(|s| !Arc::ptr_eq(s, &self.sink));
    }
}
impl FakeNetwork {
    pub fn reply(&self, url: &str, body: &str) {
        self.replies.lock().unwrap().insert(url.into(), Ok(body.into()));
    }
    pub fn emit(&self, change: NetworkChange) {
        let sinks = self.sinks.lock().unwrap().clone();
        for sink in sinks {
            sink(change);
        }
    }
    pub fn subscriptions(&self) -> usize {
        self.sinks.lock().unwrap().len()
    }
}
impl Network for FakeNetwork {
    fn disconnect(&self, interface: &str) -> Result<(), ApiError> {
        for p in self.profiles.lock().unwrap().iter_mut().filter(|p| p.interface == interface) {
            p.connected = false;
        }
        Ok(())
    }
    fn profiles(&self) -> Result<Vec<super::WifiProfile>, ApiError> {
        Ok(self.profiles.lock().unwrap().clone())
    }
    fn link_readings(&self) -> Result<Vec<super::LinkReading>, ApiError> {
        Ok(self.links.lock().unwrap().clone())
    }
    fn connect_profile(&self, interface: &str, profile: &str) -> Result<(), ApiError> {
        self.connections.lock().unwrap().push((interface.into(), profile.into()));
        if let Some(e) = self.connect_errors.lock().unwrap().get(profile).cloned() {
            return Err(e);
        }
        let mut profiles = self.profiles.lock().unwrap();
        if !profiles.iter().any(|p| p.interface == interface && p.name == profile) {
            return Err(ApiError::invalid_args("unknown profile"));
        }
        for p in profiles.iter_mut().filter(|p| p.interface == interface) {
            p.connected = p.name == profile;
        }
        Ok(())
    }
    fn set_profile_metered(&self, interface: &str, profile: &str, metered: bool) -> Result<(), ApiError> {
        self.metered.lock().unwrap().push((interface.into(), profile.into(), metered));
        Ok(())
    }
    fn get_for_route(
        &self,
        url: &str,
        proxy: &ProxyRoute,
        route: &super::super::exit_firewall::ExitRoute,
    ) -> Result<String, ApiError> {
        self.guarded_requests.lock().unwrap().push(route.clone());
        self.get(url, proxy)
    }
    fn interfaces(&self) -> Result<Vec<super::NetworkInterface>, ApiError> {
        Ok(self.interfaces.lock().unwrap().clone())
    }
    fn watch(&self, sink: NetworkSink) -> Result<Box<dyn Guard>, ApiError> {
        self.sinks.lock().unwrap().push(sink.clone());
        Ok(Box::new(Subscription { sink, sinks: self.sinks.clone() }))
    }
    fn get(&self, url: &str, route: &ProxyRoute) -> Result<String, ApiError> {
        let hook = self.before_get.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
        self.requests.lock().unwrap().push((url.into(), route.clone()));
        self.replies
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(ApiError::internal("scripted network failure")))
    }
    fn dns(&self) -> Result<DnsAssessment, ApiError> {
        Ok(self.dns.lock().unwrap().clone())
    }
}
