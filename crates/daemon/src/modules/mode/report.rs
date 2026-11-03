//! The away report: collected from other modules' events while away, finished on the way back.
//!
//! Event contract (fields read; anything else is ignored):
//! - `network.down`, `network.up` (`down_ms`): how often and how long the network was down
//! - `resource.frozen`, `resource.killed`: kept as they are, one entry per program
//! - `power.reading` (`temp_c`): highest temperature
//! - `resume.continued`: how many times an agent was told to continue
//! - `task.unprotected`: kept as it is (who ended which protection, and why)

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::module::Event;

/// Report in progress. Persisted (when something significant changes) so a daemon restart
/// during an absence does not lose it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub started_at: i64,
    pub preset: String,
    pub entered_by: String,
    pub network_downs: u32,
    pub network_down_ms: i64,
    pub down_since: Option<i64>,
    pub frozen: Vec<Value>,
    pub killed: Vec<Value>,
    pub max_temp_c: Option<f64>,
    pub agent_continues: u32,
    pub agent_forks: Vec<Value>,
    pub agent_notices: Vec<Value>,
    pub unprotected: Vec<Value>,
    pub exit_guard: Vec<Value>,
}

impl Draft {
    pub fn new(started_at: i64, preset: &str, entered_by: &str) -> Draft {
        Draft {
            started_at,
            preset: preset.into(),
            entered_by: entered_by.into(),
            ..Default::default()
        }
    }

    /// Takes in one event. Returns true when the draft changed enough to be worth saving.
    pub fn absorb(&mut self, e: &Event) -> bool {
        match e.topic.as_str() {
            "exit_guard.violation" | "exit_guard.acted" => {
                upsert_exit(&mut self.exit_guard, &e.data);
                true
            }
            "network.down" => {
                self.network_downs += 1;
                self.down_since.get_or_insert(e.ts);
                true
            }
            "network.up" => {
                let since = self.down_since.take();
                let ms = e.data["down_ms"]
                    .as_i64()
                    .or_else(|| since.map(|s| e.ts - s))
                    .unwrap_or(0);
                self.network_down_ms += ms.max(0);
                true
            }
            "resource.frozen" => {
                self.frozen.push(e.data.clone());
                true
            }
            "resource.killed" => {
                self.killed.push(e.data.clone());
                true
            }
            "power.reading" => match e.data["temp_c"].as_f64() {
                // Saved only when the maximum rises by a whole degree.
                Some(t) if self.max_temp_c.is_none_or(|m| t > m) => {
                    let significant = self.max_temp_c.is_none_or(|m| t.floor() > m.floor());
                    self.max_temp_c = Some(t);
                    significant
                }
                _ => false,
            },
            "resume.forked" => {
                self.agent_forks.push(e.data.clone());
                true
            }
            "resume.notice" | "resume.expired" => {
                self.agent_notices.push(e.data.clone());
                true
            }
            "resume.continued" => {
                self.agent_continues += 1;
                true
            }
            "task.unprotected" => {
                self.unprotected.push(e.data.clone());
                true
            }
            _ => false,
        }
    }

    /// The finished report.
    pub fn finish(&self, ended_at: i64, left_by: &str) -> Value {
        let open_down = self.down_since.map_or(0, |s| (ended_at - s).max(0));
        json!({
            "started_at": self.started_at,
            "ended_at": ended_at,
            "away_ms": (ended_at - self.started_at).max(0),
            "preset": self.preset,
            "entered_by": self.entered_by,
            "left_by": left_by,
            "network": { "downs": self.network_downs, "down_ms": self.network_down_ms + open_down },
            "frozen": self.frozen,
            "killed": self.killed,
            "max_temp_c": self.max_temp_c,
            "agent_continues": self.agent_continues,
            "agent_forks": self.agent_forks,
            "agent_notices": self.agent_notices,
            "unprotected": self.unprotected,
            "exit_guard": self.exit_guard,
        })
    }
}

pub fn upsert_exit(items: &mut Vec<Value>, item: &Value) {
    let id = item.get("incident_id");
    if let Some(previous) = items
        .iter_mut()
        .find(|v| id.is_some() && v.get("incident_id") == id)
    {
        *previous = item.clone();
    } else {
        items.push(item.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(topic: &str, ts: i64, data: Value) -> Event {
        Event {
            topic: topic.into(),
            source: topic.split('.').next().unwrap().into(),
            ts,
            data,
        }
    }

    #[test]
    fn collects_the_listed_events() {
        let mut d = Draft::new(1_000, "away", "cli");
        assert!(d.absorb(&ev("network.down", 2_000, json!({}))));
        assert!(d.absorb(&ev("network.up", 5_000, json!({}))));
        d.absorb(&ev("network.down", 6_000, json!({})));
        d.absorb(&ev("network.up", 9_000, json!({"down_ms": 2_500})));
        assert!(d.absorb(&ev("power.reading", 0, json!({"temp_c": 50.2}))));
        assert!(
            !d.absorb(&ev("power.reading", 0, json!({"temp_c": 50.7}))),
            "same degree"
        );
        assert!(!d.absorb(&ev("power.reading", 0, json!({"temp_c": 40.0}))));
        d.absorb(&ev("resume.continued", 0, json!({})));
        d.absorb(&ev(
            "resume.forked",
            0,
            json!({"original_session_id":"old","new_session_id":"new"}),
        ));
        d.absorb(&ev(
            "resume.notice",
            0,
            json!({"reason":"continuation_attempt_limit"}),
        ));
        d.absorb(&ev("resume.expired", 0, json!({"reason":"network_wait_timeout"})));
        d.absorb(&ev("task.unprotected", 0, json!({"task_id": 3, "by": "agent"})));
        d.absorb(&ev("resource.killed", 0, json!({"program": "x.exe"})));
        assert!(!d.absorb(&ev("power.lid", 0, json!({}))));
        d.absorb(&ev("network.down", 10_000, json!({})));

        let r = d.finish(11_000, "unlock");
        assert_eq!(r["away_ms"], 10_000);
        assert_eq!(
            r["network"],
            json!({"downs": 3, "down_ms": 3_000 + 2_500 + 1_000})
        );
        assert_eq!(r["max_temp_c"], 50.7);
        assert_eq!(r["agent_continues"], 1);
        assert_eq!(r["agent_forks"][0]["new_session_id"], "new");
        assert_eq!(r["agent_notices"].as_array().unwrap().len(), 2);
        assert_eq!(r["unprotected"][0]["task_id"], 3);
        assert_eq!(r["killed"][0]["program"], "x.exe");
        assert_eq!(
            (r["entered_by"].as_str(), r["left_by"].as_str()),
            (Some("cli"), Some("unlock"))
        );
    }
}
