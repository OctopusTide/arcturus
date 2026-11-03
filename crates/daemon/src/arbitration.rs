//! Shared, synchronous safety facts. Events notify observers; this lock serializes
//! process actions with mode, allowlist and task changes so event delivery cannot race them.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgramRule {
    pub path: String,
    pub signer: Option<String>,
    pub source: String,
}

#[derive(Default)]
pub struct Arbitration {
    /// Non-secret connection context from the hook, retained only in RAM.
    pub resume_environments: BTreeMap<(String, String), (crate::sys::ProcessInfo, BTreeMap<String, String>)>,
    pub resume_brokers: BTreeMap<(String, String), crate::sys::ProcessInfo>,
    pub permission_ready: bool,
    pub tasks_ready: bool,
    pub automatic: bool,
    pub hotspot: bool,
    pub network_online: Option<bool>,
    pub network_outage: Option<i64>,
    pub rules: Vec<ProgramRule>,
    pub excluded_defaults: BTreeSet<String>,
    pub tasks: Vec<serde_json::Value>,
    pub protected: BTreeSet<u32>,
    /// pid -> creation time, across modules; prevents double suspension.
    pub frozen: BTreeMap<u32, i64>,
    /// Resource-owned suspensions must be thawed before restoring away power settings.
    pub resource_frozen: BTreeMap<u32, i64>,
}
