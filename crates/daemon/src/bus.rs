//! Event bus. Modules never call each other; they publish events (`network.down`,
//! `power.away_entered`, `task.protected`) and subscribe to topics. Each subscriber gets its
//! own delivery thread, so a slow or stuck module delays only its own events.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::module::Event;

/// Does `pattern` (`"a.b"`, `"a.*"` or `"*"`) match `topic`?
pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some("") => true,
        Some(prefix) if prefix.ends_with('.') => topic.starts_with(prefix),
        _ => pattern == topic,
    }
}

/// Is `pattern` a well-formed subscription?
pub fn valid_pattern(pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let body = pattern.strip_suffix(".*").unwrap_or(pattern);
    !body.is_empty() && body.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
}

struct Subscriber {
    patterns: &'static [&'static str],
    tx: Sender<Event>,
}

#[derive(Default)]
struct Pending {
    count: Mutex<u64>,
    idle: Condvar,
}

impl Pending {
    fn add(&self) {
        *self.count.lock().unwrap() += 1;
    }

    fn done(&self) {
        let mut n = self.count.lock().unwrap();
        *n -= 1;
        if *n == 0 {
            self.idle.notify_all();
        }
    }
}

#[derive(Default)]
pub struct Bus {
    subscribers: Mutex<Vec<Subscriber>>,
    pending: Arc<Pending>,
}

impl Bus {
    pub fn new() -> Arc<Bus> {
        Arc::new(Bus::default())
    }

    /// Starts a delivery thread that calls `deliver` for every event matching `patterns`.
    pub fn subscribe(&self, name: &str, patterns: &'static [&'static str], deliver: impl Fn(&Event) + Send + 'static) {
        let (tx, rx) = mpsc::channel::<Event>();
        let pending = self.pending.clone();
        let spawned = std::thread::Builder::new().name(format!("events-{name}")).spawn(move || {
            for event in rx {
                deliver(&event);
                pending.done();
            }
        });
        if spawned.is_ok() {
            self.subscribers.lock().unwrap().push(Subscriber { patterns, tx });
        }
    }

    /// Queues `event` for every matching subscriber and returns at once.
    pub fn publish(&self, event: Event) {
        for sub in self.subscribers.lock().unwrap().iter() {
            if sub.patterns.iter().any(|p| topic_matches(p, &event.topic)) {
                self.pending.add();
                if sub.tx.send(event.clone()).is_err() {
                    self.pending.done();
                }
            }
        }
    }

    /// Waits until every published event has been delivered. Returns false on timeout.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut n = self.pending.count.lock().unwrap();
        while *n > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            n = self.pending.idle.wait_timeout(n, left).unwrap().0;
        }
        true
    }

    /// Stops delivery: drops every queue so the delivery threads end.
    pub fn close(&self) {
        self.subscribers.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(topic: &str) -> Event {
        Event { topic: topic.into(), source: "t".into(), ts: 0, data: json!(null) }
    }

    #[test]
    fn patterns() {
        assert!(topic_matches("*", "network.down"));
        assert!(topic_matches("network.*", "network.down"));
        assert!(!topic_matches("network.*", "networkx.down"));
        assert!(topic_matches("network.down", "network.down"));
        assert!(!topic_matches("network.down", "network.up"));
        assert!(valid_pattern("power.*") && valid_pattern("a.b") && valid_pattern("*"));
        assert!(!valid_pattern("") && !valid_pattern("Power.x") && !valid_pattern("a..b") && !valid_pattern("a*"));
    }

    #[test]
    fn delivers_matching_events_in_order() {
        static NET: [&str; 1] = ["network.*"];
        let bus = Bus::new();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        bus.subscribe("t", &NET, move |e| sink.lock().unwrap().push(e.topic.clone()));
        for t in ["network.down", "power.away_entered", "network.up"] {
            bus.publish(event(t));
        }
        assert!(bus.wait_idle(Duration::from_secs(5)));
        assert_eq!(*got.lock().unwrap(), ["network.down", "network.up"]);
        bus.close();
    }
}
