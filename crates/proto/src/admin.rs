//! Private daemon-to-service protocol. There is deliberately no shell/command/file API.
use crate::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThreadReceipt {
    pub tid: u32,
    pub started_at: u64,
    pub suspend_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    SuspensionState {
        pid: u32,
        started_at: i64,
    },
    Freeze {
        pid: u32,
        started_at: i64,
        threads: Vec<ThreadReceipt>,
    },
    Thaw {
        pid: u32,
        started_at: i64,
        threads: Vec<ThreadReceipt>,
    },
    Ping {},
    RestoreInterfaceMetric {
        interface: String,
    },
    InterfaceMetric {
        interface: String,
        metric: u32,
    },
    Terminate {
        pid: u32,
        started_at: i64,
    },
    Priority {
        pid: u32,
        started_at: i64,
        level: String,
    },
    UpdatePause {},
    SetUpdatePause {
        until: Option<i64>,
    },
    AdapterPowerSaving {},
    SetAdapterPowerSaving {
        state: BTreeMap<String, bool>,
    },
    PowerRequests {},
    /// Payload is decoded and strictly validated as the fixed egress request enum in the service.
    ExitFirewall {
        request: serde_json::Value,
    },
}
impl Operation {
    pub fn validate(&self, now: i64) -> Result<(), ApiError> {
        let invalid = |m| Err(ApiError::invalid_args(m));
        match self {
            Self::RestoreInterfaceMetric { interface } => Self::InterfaceMetric {
                interface: interface.clone(),
                metric: 1,
            }
            .validate(now)?,
            Self::ExitFirewall { request } => {
                if !request.is_object() {
                    return invalid("egress request must be an object");
                }
            }
            Self::InterfaceMetric { interface, metric } => {
                let id = interface.trim_matches(['{', '}']);
                if id.len() != 36
                    || !id.chars().enumerate().all(|(i, c)| {
                        if [8, 13, 18, 23].contains(&i) {
                            c == '-'
                        } else {
                            c.is_ascii_hexdigit()
                        }
                    })
                    || !(1..=9999).contains(metric)
                {
                    return invalid("interface must be a GUID; metric must be 1..=9999");
                }
            }
            Self::Terminate { pid, started_at }
            | Self::Priority { pid, started_at, .. }
            | Self::SuspensionState { pid, started_at }
            | Self::Freeze { pid, started_at, .. }
            | Self::Thaw { pid, started_at, .. } => {
                if *pid <= 4 || *started_at <= 0 {
                    return invalid("target needs a process id and creation time");
                }
                if let Self::Freeze { threads, .. } | Self::Thaw { threads, .. } = self {
                    if threads.is_empty()
                        || threads.len() > 2048
                        || threads
                            .iter()
                            .any(|t| t.tid == 0 || t.started_at == 0 || t.suspend_count > 127)
                    {
                        return invalid("invalid suspension receipt");
                    }
                }
                if let Self::Priority { level, .. } = self {
                    if !["idle", "below_normal", "normal", "above_normal", "high"].contains(&level.as_str()) {
                        return invalid("invalid priority; real-time is forbidden");
                    }
                }
            }
            Self::SetUpdatePause { until: Some(until) } => {
                if *until < 0 || *until > now.saturating_add(35 * 86_400_000) {
                    return invalid("update pause must end within 35 days");
                }
            }
            Self::SetAdapterPowerSaving { state } => {
                if state.len() > 128
                    || state
                        .keys()
                        .any(|k| k.is_empty() || k.len() > 1024 || k.contains(['\0', '\r', '\n']))
                {
                    return invalid("invalid adapter state");
                }
            }
            _ => {}
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    pub operation: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub v: u32,
    pub id: u64,
    #[serde(
        default,
        deserialize_with = "present_result",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<serde_json::Value>,
    pub error: Option<ApiError>,
}
fn present_result<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
}
impl Reply {
    pub fn new(id: u64, result: Result<serde_json::Value, ApiError>) -> Self {
        match result {
            Ok(value) => Self {
                v: VERSION,
                id,
                result: Some(value),
                error: None,
            },
            Err(error) => Self {
                v: VERSION,
                id,
                result: None,
                error: Some(error),
            },
        }
    }
    pub fn checked(self, id: u64) -> Result<serde_json::Value, ApiError> {
        if self.v != VERSION || self.id != id || self.result.is_some() == self.error.is_some() {
            return Err(ApiError::new(
                ErrorCode::IpcError,
                "invalid administrator service reply",
            ));
        }
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.result.unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_arbitrary_operations_or_unknown_arguments() {
        for text in [
            r#"{"op":"shell","command":"whoami"}"#,
            r#"{"op":"ping","command":"whoami"}"#,
            r#"{"op":"terminate","pid":9,"started_at":5,"path":"x"}"#,
        ] {
            assert!(serde_json::from_str::<Operation>(text).is_err());
        }
        assert!(
            Operation::Priority {
                pid: 9,
                started_at: 5,
                level: "realtime".into()
            }
            .validate(10)
            .is_err()
        );
        assert!(
            Operation::InterfaceMetric {
                interface: "x; calc".into(),
                metric: 1
            }
            .validate(10)
            .is_err()
        );
        assert!(
            Operation::SetUpdatePause {
                until: Some(i64::MAX)
            }
            .validate(10)
            .is_err()
        );
    }
    #[test]
    fn replies_correlate_and_have_exactly_one_outcome() {
        assert!(Reply::new(1, Ok(serde_json::json!(true))).checked(2).is_err());
        assert!(
            Reply {
                v: 1,
                id: 1,
                result: Some(serde_json::Value::Null),
                error: Some(ApiError::invalid_args("x"))
            }
            .checked(1)
            .is_err()
        );
        assert_eq!(
            Reply::new(1, Ok(serde_json::json!(true))).checked(1).unwrap(),
            serde_json::json!(true)
        );
    }
    #[test]
    fn every_operation_and_its_success_or_error_survive_the_wire() {
        use serde_json::{Value, json};
        let threads = vec![ThreadReceipt {
            tid: 8,
            started_at: 10,
            suspend_count: 0,
        }];
        let ops = vec![
            (Operation::Ping {}, json!({"backend":"service"})),
            (
                Operation::SuspensionState {
                    pid: 8,
                    started_at: 10,
                },
                json!(threads),
            ),
            (
                Operation::Freeze {
                    pid: 8,
                    started_at: 10,
                    threads: threads.clone(),
                },
                Value::Null,
            ),
            (
                Operation::Thaw {
                    pid: 8,
                    started_at: 10,
                    threads,
                },
                Value::Null,
            ),
            (
                Operation::Terminate {
                    pid: 8,
                    started_at: 10,
                },
                Value::Null,
            ),
            (
                Operation::Priority {
                    pid: 8,
                    started_at: 10,
                    level: "normal".into(),
                },
                Value::Null,
            ),
            (
                Operation::RestoreInterfaceMetric {
                    interface: "11111111-2222-3333-4444-555555555555".into(),
                },
                Value::Null,
            ),
            (
                Operation::InterfaceMetric {
                    interface: "11111111-2222-3333-4444-555555555555".into(),
                    metric: 10,
                },
                Value::Null,
            ),
            (Operation::UpdatePause {}, Value::Null),
            (Operation::SetUpdatePause { until: None }, Value::Null),
            (Operation::AdapterPowerSaving {}, json!({})),
            (
                Operation::SetAdapterPowerSaving {
                    state: BTreeMap::new(),
                },
                Value::Null,
            ),
            (Operation::PowerRequests {}, json!([])),
            (
                Operation::ExitFirewall {
                    request: json!({"op":"get","program":r"D:\apps\agent.exe"}),
                },
                Value::Null,
            ),
            (
                Operation::ExitFirewall {
                    request: json!({"op":"set","policy":{"program":r"D:\apps\agent.exe","route":{"kind":"deny_all"},"blocked":true}}),
                },
                Value::Null,
            ),
            (
                Operation::ExitFirewall {
                    request: json!({"op":"block","program":r"D:\apps\agent.exe","blocked":true}),
                },
                Value::Null,
            ),
            (
                Operation::ExitFirewall {
                    request: json!({"op":"remove","program":r"D:\apps\agent.exe"}),
                },
                Value::Null,
            ),
        ];
        for (index, (operation, result)) in ops.into_iter().enumerate() {
            let id = index as u64;
            let request: Request = serde_json::from_slice(
                &serde_json::to_vec(&Request {
                    v: VERSION,
                    id,
                    operation: operation.clone(),
                })
                .unwrap(),
            )
            .unwrap();
            assert_eq!(request.operation, operation);
            request.operation.validate(10).unwrap();
            let reply: Reply =
                serde_json::from_slice(&serde_json::to_vec(&Reply::new(id, Ok(result.clone()))).unwrap())
                    .unwrap();
            assert_eq!(reply.checked(id).unwrap(), result);
            let reply: Reply = serde_json::from_slice(
                &serde_json::to_vec(&Reply::new(id, Err(ApiError::invalid_args("rejected")))).unwrap(),
            )
            .unwrap();
            assert_eq!(reply.checked(id).unwrap_err().message, "rejected");
        }
        let absent: Reply = serde_json::from_str(r#"{"v":1,"id":1,"error":null}"#).unwrap();
        assert!(absent.checked(1).is_err());
        let no_rule: Reply = serde_json::from_str(r#"{"v":1,"id":1,"result":null,"error":null}"#).unwrap();
        assert!(
            serde_json::from_value::<Option<Value>>(no_rule.checked(1).unwrap())
                .unwrap()
                .is_none()
        );
    }
}
