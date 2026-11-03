//! OS-neutral, fixed egress firewall operations. No shell or arbitrary-command protocol.
use proto::ApiError;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExitRoute {
    /// Safe default until the user chooses a VPN interface or local proxy.
    DenyAll,
    Vpn {
        interface: String,
    },
    LocalProxy {
        endpoint: SocketAddr,
    },
}
impl Default for ExitRoute {
    fn default() -> Self {
        Self::DenyAll
    }
}
impl ExitRoute {
    pub fn validate(&self) -> Result<(), ApiError> {
        match self {
            Self::Vpn { interface } => {
                let s = interface.trim_matches(['{', '}']);
                if s.len() != 36
                    || !s.chars().enumerate().all(|(i, c)| {
                        if [8, 13, 18, 23].contains(&i) {
                            c == '-'
                        } else {
                            c.is_ascii_hexdigit()
                        }
                    })
                {
                    return Err(ApiError::invalid_args(
                        "VPN interface must be a stable interface UUID",
                    ));
                }
            }
            Self::LocalProxy { endpoint } if !endpoint.ip().is_loopback() || endpoint.port() == 0 => {
                return Err(ApiError::invalid_args(
                    "proxy firewall endpoint must be literal loopback with a nonzero port",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitFirewallPolicy {
    pub program: String,
    pub route: ExitRoute,
    /// Temporary fail-closed lock; clearing it retains the route restriction.
    pub blocked: bool,
}
impl ExitFirewallPolicy {
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::modules::exit_guard::config::path_key(&self.program).map_err(ApiError::invalid_args)?;
        if !self.program.to_ascii_lowercase().ends_with(".exe") {
            return Err(ApiError::invalid_args(
                "firewall target must be a full executable path",
            ));
        }
        self.route.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExitAdminRequest {
    Get { program: String },
    Set { policy: ExitFirewallPolicy },
    Remove { program: String },
    Block { program: String, blocked: bool },
}

/// Model of the native block rules, also used by fake tests to reason about packet paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    ConnectV4,
    ConnectV6,
    AcceptV4,
    AcceptV6,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    All,
    OtherInterface(String),
    /// ALE reauthorization can lack this direction's interface. Refuse the
    /// unknown value rather than letting a negative comparison fail to match.
    UnknownInterface,
    OtherAddress(std::net::IpAddr),
    OtherPort(u16),
    NonTcp,
    /// A fail-closed lock also blocks the selected loopback proxy, while preserving other local services.
    ProxyEndpoint(SocketAddr),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRule {
    pub layer: Layer,
    pub constraint: Constraint,
}

/// There are no allow rules: this policy cannot override another firewall's denial.
pub fn plan(policy: &ExitFirewallPolicy) -> Result<Vec<BlockRule>, ApiError> {
    policy.validate()?;
    let mut out = Vec::new();
    for layer in [
        Layer::ConnectV4,
        Layer::ConnectV6,
        Layer::AcceptV4,
        Layer::AcceptV6,
    ] {
        let mut push = |constraint| out.push(BlockRule { layer, constraint });
        match &policy.route {
            _ if policy.blocked => push(Constraint::All),
            ExitRoute::DenyAll => push(Constraint::All),
            ExitRoute::LocalProxy { endpoint } => {
                if matches!(layer, Layer::ConnectV4 | Layer::AcceptV4) == endpoint.is_ipv4() {
                    push(Constraint::OtherAddress(endpoint.ip()));
                    push(Constraint::OtherPort(endpoint.port()));
                    push(Constraint::NonTcp);
                } else {
                    push(Constraint::All);
                }
            }
            ExitRoute::Vpn { interface } => {
                push(Constraint::OtherInterface(interface.clone()));
                push(Constraint::UnknownInterface);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(route: ExitRoute) -> ExitFirewallPolicy {
        ExitFirewallPolicy {
            program: r"D:\apps\worker.exe".into(),
            route,
            blocked: false,
        }
    }
    #[test]
    fn block_lock_covers_both_ip_families_and_inbound_flows() {
        let mut p = policy(ExitRoute::LocalProxy {
            endpoint: "127.0.0.1:7890".parse().unwrap(),
        });
        p.blocked = true;
        let r = plan(&p).unwrap();
        assert_eq!(r.len(), 4);
        for layer in [
            Layer::ConnectV4,
            Layer::ConnectV6,
            Layer::AcceptV4,
            Layer::AcceptV6,
        ] {
            assert!(r.contains(&BlockRule {
                layer,
                constraint: Constraint::All
            }));
        }
    }
    #[test]
    fn proxy_plan_permits_only_the_configured_endpoint() {
        let r = plan(&policy(ExitRoute::LocalProxy {
            endpoint: "127.0.0.1:7890".parse().unwrap(),
        }))
        .unwrap();
        assert_eq!(r.len(), 8);
        for layer in [Layer::ConnectV4, Layer::AcceptV4] {
            assert!(r.contains(&BlockRule {
                layer,
                constraint: Constraint::OtherPort(7890)
            }));
            assert!(r.contains(&BlockRule {
                layer,
                constraint: Constraint::OtherAddress("127.0.0.1".parse().unwrap())
            }));
            assert!(r.contains(&BlockRule {
                layer,
                constraint: Constraint::NonTcp
            }));
        }
        for layer in [Layer::ConnectV6, Layer::AcceptV6] {
            assert!(r.contains(&BlockRule {
                layer,
                constraint: Constraint::All
            }));
        }
    }
    #[test]
    fn vpn_plan_covers_unknown_and_new_adapters_without_enumerating_them() {
        let id = "11111111-2222-3333-4444-555555555555";
        let r = plan(&policy(ExitRoute::Vpn { interface: id.into() })).unwrap();
        assert_eq!(r.len(), 8);
        for layer in [
            Layer::ConnectV4,
            Layer::ConnectV6,
            Layer::AcceptV4,
            Layer::AcceptV6,
        ] {
            assert!(r.contains(&BlockRule {
                layer,
                constraint: Constraint::OtherInterface(id.into())
            }));
            assert!(r.contains(&BlockRule {
                layer,
                constraint: Constraint::UnknownInterface
            }));
        }
    }
    #[test]
    fn invalid_targets_and_remote_proxy_are_rejected() {
        assert!(
            policy(ExitRoute::LocalProxy {
                endpoint: "8.8.8.8:80".parse().unwrap()
            })
            .validate()
            .is_err()
        );
        assert!(
            policy(ExitRoute::Vpn {
                interface: "Wi-Fi".into()
            })
            .validate()
            .is_err()
        );
    }
}
