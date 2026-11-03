//! Persistent WFP filters. Called only by the authenticated administrator service.
//! Replacements are transactional, contain block rules only, and are scoped by app + user.
use super::ffi;
use daemon::sys::exit_firewall::{BlockRule, Constraint, ExitAdminRequest, ExitFirewallPolicy, Layer, plan};
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};
use std::ptr::{null, null_mut};
use windows_sys::{
    Win32::{
        Foundation::{FWP_E_ALREADY_EXISTS, FWP_E_FILTER_NOT_FOUND, HANDLE, LocalFree},
        NetworkManagement::{
            IpHelper::ConvertInterfaceGuidToLuid, Ndis::NET_LUID_LH, WindowsFilteringPlatform::*,
        },
        Security::{
            Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
            Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash},
            GetSecurityDescriptorLength,
        },
    },
    core::GUID,
};

fn check(code: u32, op: &str) -> Result<(), ApiError> {
    if code == 0 {
        Ok(())
    } else {
        Err(ApiError::internal(format!("{op}: WFP error 0x{code:08x}")))
    }
}
fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
struct Engine(HANDLE);
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.0);
        }
    }
}
struct Allocation(*mut std::ffi::c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe {
            FwpmFreeMemory0(&mut self.0);
        }
    }
}
fn engine() -> Result<Engine, ApiError> {
    let mut h = null_mut();
    check(
        unsafe { FwpmEngineOpen0(null(), 10, null(), null(), &mut h) },
        "open firewall engine",
    )?;
    Ok(Engine(h))
}
fn key(sid: &str, label: &str) -> Result<GUID, ApiError> {
    let data = format!("Arcturus.egress.v1\0{sid}\0{label}");
    let mut digest = [0u8; 32];
    let code = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            null(),
            0,
            data.as_ptr(),
            data.len() as u32,
            digest.as_mut_ptr(),
            32,
        )
    };
    if code < 0 {
        return Err(ApiError::internal("cannot derive firewall rule id"));
    }
    Ok(GUID::from_u128(u128::from_be_bytes(
        digest[..16].try_into().unwrap(),
    )))
}
fn filter_key(sid: &str, path: &str, index: usize) -> Result<GUID, ApiError> {
    key(
        sid,
        &format!(
            "app:{}:{index}",
            daemon::modules::exit_guard::config::path_key(path).map_err(ApiError::invalid_args)?
        ),
    )
}
fn guid(s: &str) -> Result<GUID, ApiError> {
    let n = u128::from_str_radix(&s.trim_matches(['{', '}']).replace('-', ""), 16)
        .map_err(|_| ApiError::invalid_args("invalid VPN interface UUID"))?;
    Ok(GUID::from_u128(n))
}

fn ensure_provider(e: &Engine, sid: &str) -> Result<(), ApiError> {
    let mut provider = key(sid, "provider")?;
    let name = ffi::to_wide("Arcturus egress protection");
    let mut p = FWPM_PROVIDER0::default();
    p.providerKey = provider;
    p.displayData.name = name.as_ptr() as *mut _;
    p.flags = FWPM_PROVIDER_FLAG_PERSISTENT;
    let code = unsafe { FwpmProviderAdd0(e.0, &p, null_mut()) };
    if code != FWP_E_ALREADY_EXISTS as u32 {
        check(code, "add firewall provider")?;
    }
    let mut s = FWPM_SUBLAYER0::default();
    s.subLayerKey = key(sid, "sublayer")?;
    s.displayData.name = name.as_ptr() as *mut _;
    s.flags = FWPM_SUBLAYER_FLAG_PERSISTENT;
    s.providerKey = &mut provider;
    s.weight = u16::MAX;
    let code = unsafe { FwpmSubLayerAdd0(e.0, &s, null_mut()) };
    if code != FWP_E_ALREADY_EXISTS as u32 {
        check(code, "add firewall sublayer")?;
    }
    Ok(())
}
fn transaction<T>(e: &Engine, f: impl FnOnce() -> Result<T, ApiError>) -> Result<T, ApiError> {
    check(
        unsafe { FwpmTransactionBegin0(e.0, 0) },
        "begin firewall transaction",
    )?;
    match f() {
        Ok(v) => match check(
            unsafe { FwpmTransactionCommit0(e.0) },
            "commit firewall transaction",
        ) {
            Ok(()) => Ok(v),
            Err(e2) => {
                unsafe {
                    FwpmTransactionAbort0(e.0);
                };
                Err(e2)
            }
        },
        Err(err) => {
            unsafe {
                FwpmTransactionAbort0(e.0);
            };
            Err(err)
        }
    }
}
fn get_on(e: &Engine, sid: &str, path: &str) -> Result<Option<ExitFirewallPolicy>, ApiError> {
    let id = filter_key(sid, path, 0)?;
    let mut p = null_mut();
    let code = unsafe { FwpmFilterGetByKey0(e.0, &id, &mut p) };
    if code == FWP_E_FILTER_NOT_FOUND as u32 {
        return Ok(None);
    }
    check(code, "read firewall policy")?;
    let _allocation = Allocation(p.cast());
    let f = unsafe { &*p };
    if f.providerKey.is_null() || !same_guid(unsafe { &*f.providerKey }, &key(sid, "provider")?) {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "firewall rule belongs to another provider",
        ));
    }
    if f.providerData.size == 0 || f.providerData.size > 16384 || f.providerData.data.is_null() {
        return Err(ApiError::internal("invalid firewall metadata"));
    }
    let policy: ExitFirewallPolicy = serde_json::from_slice(unsafe {
        std::slice::from_raw_parts(f.providerData.data, f.providerData.size as usize)
    })
    .map_err(|_| ApiError::internal("unreadable firewall metadata"))?;
    policy.validate()?;
    if daemon::modules::exit_guard::config::path_key(&policy.program).ok()
        != daemon::modules::exit_guard::config::path_key(path).ok()
    {
        return Err(ApiError::internal("firewall metadata path mismatch"));
    }
    // Completeness is checked by filters_current when reconciling. Older versions
    // can have a different rule count; metadata must remain readable to repair them.
    Ok(Some(policy))
}
fn delete_on(e: &Engine, sid: &str, path: &str) -> Result<(), ApiError> {
    for i in 0..8 {
        let id = filter_key(sid, path, i)?;
        let mut found = null_mut();
        let c = unsafe { FwpmFilterGetByKey0(e.0, &id, &mut found) };
        if c == FWP_E_FILTER_NOT_FOUND as u32 {
            continue;
        }
        check(c, "inspect firewall rule before removal")?;
        let memory = Allocation(found.cast());
        if unsafe {
            (*found).providerKey.is_null() || !same_guid(&*(*found).providerKey, &key(sid, "provider")?)
        } {
            return Err(ApiError::new(
                ErrorCode::Denied,
                "refusing to remove another provider's rule",
            ));
        }
        drop(memory);
        check(
            unsafe { FwpmFilterDeleteByKey0(e.0, &id) },
            "remove own firewall rule",
        )?;
    }
    Ok(())
}

struct Descriptor(*mut std::ffi::c_void);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn user_descriptor(sid: &str) -> Result<Descriptor, ApiError> {
    if !super::admin::valid_sid(sid) {
        return Err(ApiError::invalid_args("invalid policy owner SID"));
    }
    let mut sd = null_mut();
    let s = ffi::to_wide(format!("D:(A;;CC;;;{sid})"));
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
    } == 0
    {
        return Err(ffi::os_error("firewall user descriptor"));
    }
    Ok(Descriptor(sd))
}
fn add_rule(
    e: &Engine,
    sid: &str,
    p: &ExitFirewallPolicy,
    index: usize,
    rule: &BlockRule,
) -> Result<(), ApiError> {
    let mut app = null_mut();
    check(
        unsafe { FwpmGetAppIdFromFileName0(ffi::to_wide(&p.program).as_ptr(), &mut app) },
        "resolve executable firewall id",
    )?;
    let _app = Allocation(app.cast());
    let sd = user_descriptor(sid)?;
    let mut user = FWP_BYTE_BLOB {
        size: unsafe { GetSecurityDescriptorLength(sd.0) },
        data: sd.0.cast(),
    };
    let mut conditions = vec![
        FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_ALE_APP_ID,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_BYTE_BLOB_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 { byteBlob: app },
            },
        },
        FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_ALE_USER_ID,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 { sd: &mut user },
            },
        },
    ];
    if matches!(
        rule.constraint,
        Constraint::OtherInterface(_) | Constraint::UnknownInterface
    ) {
        conditions.push(FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_FLAGS,
            matchType: FWP_MATCH_FLAGS_NONE_SET,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT32,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    uint32: FWP_CONDITION_FLAG_IS_LOOPBACK,
                },
            },
        });
    }
    if let Constraint::ProxyEndpoint(endpoint) = &rule.constraint {
        conditions.push(FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT16,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    uint16: endpoint.port(),
                },
            },
        });
    }
    let mut luid = 0u64;
    let mut v6 = FWP_BYTE_ARRAY16::default();
    let extra = match &rule.constraint {
        Constraint::All => None,
        // Reauthorization can be triggered by a packet opposite to the ALE
        // flow direction; its corresponding interface field is then FWP_EMPTY.
        // https://learn.microsoft.com/en-us/windows/win32/fwp/ale-re-authorization
        Constraint::UnknownInterface => Some(FWPM_FILTER_CONDITION0 {
            fieldKey: if matches!(rule.layer, Layer::ConnectV4 | Layer::ConnectV6) {
                FWPM_CONDITION_IP_NEXTHOP_INTERFACE
            } else {
                FWPM_CONDITION_IP_ARRIVAL_INTERFACE
            },
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_EMPTY,
                ..Default::default()
            },
        }),
        Constraint::OtherInterface(id) => {
            let mut value = NET_LUID_LH::default();
            let code = unsafe { ConvertInterfaceGuidToLuid(&guid(id)?, &mut value) };
            if code == 0 {
                luid = unsafe { value.Value };
            }
            // A missing/deleted VPN becomes a deny-all filter, never permission to use Wi-Fi.
            if luid == 0 {
                None
            } else {
                Some(FWPM_FILTER_CONDITION0 {
                    fieldKey: if matches!(rule.layer, Layer::ConnectV4 | Layer::ConnectV6) {
                        FWPM_CONDITION_IP_NEXTHOP_INTERFACE
                    } else {
                        FWPM_CONDITION_IP_ARRIVAL_INTERFACE
                    },
                    matchType: FWP_MATCH_NOT_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_UINT64,
                        Anonymous: FWP_CONDITION_VALUE0_0 { uint64: &mut luid },
                    },
                })
            }
        }
        Constraint::OtherAddress(ip) => Some(match ip {
            std::net::IpAddr::V4(ip) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
                matchType: FWP_MATCH_NOT_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT32,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        uint32: u32::from_be_bytes(ip.octets()),
                    },
                },
            },
            std::net::IpAddr::V6(ip) => {
                v6.byteArray16 = ip.octets();
                FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
                    matchType: FWP_MATCH_NOT_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_BYTE_ARRAY16_TYPE,
                        Anonymous: FWP_CONDITION_VALUE0_0 { byteArray16: &mut v6 },
                    },
                }
            }
        }),
        Constraint::ProxyEndpoint(endpoint) => Some(match endpoint.ip() {
            std::net::IpAddr::V4(ip) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT32,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        uint32: u32::from_be_bytes(ip.octets()),
                    },
                },
            },
            std::net::IpAddr::V6(ip) => {
                v6.byteArray16 = ip.octets();
                FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
                    matchType: FWP_MATCH_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_BYTE_ARRAY16_TYPE,
                        Anonymous: FWP_CONDITION_VALUE0_0 { byteArray16: &mut v6 },
                    },
                }
            }
        }),
        Constraint::OtherPort(port) => Some(FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
            matchType: FWP_MATCH_NOT_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT16,
                Anonymous: FWP_CONDITION_VALUE0_0 { uint16: *port },
            },
        }),
        Constraint::NonTcp => Some(FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_PROTOCOL,
            matchType: FWP_MATCH_NOT_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT8,
                Anonymous: FWP_CONDITION_VALUE0_0 { uint8: 6 },
            },
        }),
    };
    if let Some(c) = extra {
        conditions.push(c);
    }
    let mut provider = key(sid, "provider")?;
    let mut metadata = serde_json::to_vec(p).map_err(|e| ApiError::internal(e.to_string()))?;
    let name = ffi::to_wide(format!("Arcturus egress: {}", p.program));
    let mut f = FWPM_FILTER0::default();
    f.filterKey = filter_key(sid, &p.program, index)?;
    f.displayData.name = name.as_ptr() as *mut _;
    f.flags = FWPM_FILTER_FLAG_PERSISTENT;
    f.providerKey = &mut provider;
    f.providerData = FWP_BYTE_BLOB {
        size: metadata.len() as u32,
        data: metadata.as_mut_ptr(),
    };
    f.subLayerKey = key(sid, "sublayer")?;
    f.layerKey = match rule.layer {
        Layer::ConnectV4 => FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        Layer::ConnectV6 => FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        Layer::AcceptV4 => FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
        Layer::AcceptV6 => FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
    };
    f.weight.r#type = FWP_UINT8;
    f.weight.Anonymous.uint8 = 15;
    f.action.r#type = FWP_ACTION_BLOCK;
    f.numFilterConditions = conditions.len() as u32;
    f.filterCondition = conditions.as_mut_ptr();
    check(
        unsafe { FwpmFilterAdd0(e.0, &f, null_mut(), null_mut()) },
        "add persistent app/user block rule",
    )
}
fn set_on(e: &Engine, sid: &str, p: &ExitFirewallPolicy) -> Result<(), ApiError> {
    let rules = plan(p)?;
    if get_on(e, sid, &p.program)?.as_ref() == Some(p) && filters_current(e, sid, p)? {
        return Ok(());
    }
    ensure_provider(e, sid)?;
    delete_on(e, sid, &p.program)?;
    for (i, r) in rules.iter().enumerate() {
        add_rule(e, sid, p, i, r)?;
    }
    Ok(())
}

/// Service dispatch after transport authentication. It accepts exactly four fixed operations.
pub fn dispatch(sid: &str, request: ExitAdminRequest) -> Result<Value, ApiError> {
    let path = match &request {
        ExitAdminRequest::Set { policy } => {
            policy.validate()?;
            &policy.program
        }
        ExitAdminRequest::Get { program }
        | ExitAdminRequest::Remove { program }
        | ExitAdminRequest::Block { program, .. } => program,
    };
    let normalized = daemon::modules::exit_guard::config::path_key(path).map_err(ApiError::invalid_args)?;
    if normalized.rsplit('\\').next().is_some_and(|n| {
        [
            proto::names::DAEMON_BIN,
            proto::names::ADMIN_BIN,
            proto::names::UI_BIN,
            proto::names::TRAY_BIN,
        ]
        .iter()
        .any(|name| n == format!("{name}.exe"))
    }) {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "cannot firewall the guard's own executables",
        ));
    }
    if matches!(
        request,
        ExitAdminRequest::Set { .. } | ExitAdminRequest::Block { .. }
    ) {
        validate_local_program(&normalized)?;
    }
    let e = engine()?;
    match request {
        ExitAdminRequest::Get { .. } => Ok(json!(get_on(&e, sid, &normalized)?)),
        ExitAdminRequest::Set { mut policy } => {
            policy.program = normalized;
            transaction(&e, || set_on(&e, sid, &policy))?;
            Ok(json!(true))
        }
        ExitAdminRequest::Remove { .. } => {
            transaction(&e, || delete_on(&e, sid, &normalized))?;
            Ok(json!(true))
        }
        ExitAdminRequest::Block { blocked, .. } => {
            transaction(&e, || {
                let mut p = get_on(&e, sid, &normalized)?
                    .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no egress firewall policy"))?;
                p.blocked = blocked;
                set_on(&e, sid, &p)
            })?;
            Ok(json!(true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rule_ids_are_stable_and_user_and_path_scoped() {
        let a = key("S-1-5-21-1-2-3-4", "a").unwrap();
        assert!(same_guid(&a, &key("S-1-5-21-1-2-3-4", "a").unwrap()));
        assert!(!same_guid(&a, &key("S-1-5-21-1-2-3-5", "a").unwrap()));
        assert!(!same_guid(&a, &key("S-1-5-21-1-2-3-4", "b").unwrap()));
    }
}

/// Explicit service uninstall only. Persistent guards survive ordinary service/daemon stops.
pub fn remove_all(sid: &str) -> Result<(), ApiError> {
    let e = engine()?;
    let mut provider = key(sid, "provider")?;
    let mut found = null_mut();
    let code = unsafe { FwpmProviderGetByKey0(e.0, &provider, &mut found) };
    if code == windows_sys::Win32::Foundation::FWP_E_PROVIDER_NOT_FOUND as u32 {
        return Ok(());
    }
    check(code, "find own firewall provider")?;
    let _memory = Allocation(found.cast());
    let mut template = FWPM_FILTER_ENUM_TEMPLATE0::default();
    template.providerKey = &mut provider;
    template.enumType = FWP_FILTER_ENUM_FULLY_CONTAINED;
    template.actionMask = u32::MAX;
    let mut handle = null_mut();
    check(
        unsafe { FwpmFilterCreateEnumHandle0(e.0, &template, &mut handle) },
        "enumerate own firewall rules",
    )?;
    let mut ids = Vec::new();
    let result = (|| {
        loop {
            let mut entries = null_mut();
            let mut count = 0;
            check(
                unsafe { FwpmFilterEnum0(e.0, handle, 256, &mut entries, &mut count) },
                "read own firewall rules",
            )?;
            let _memory = Allocation(entries.cast());
            if count == 0 {
                break;
            }
            for f in unsafe { std::slice::from_raw_parts(entries, count as usize) } {
                ids.push(unsafe { (**f).filterId });
            }
        }
        Ok::<(), ApiError>(())
    })();
    unsafe {
        FwpmFilterDestroyEnumHandle0(e.0, handle);
    };
    result?;
    transaction(&e, || {
        for id in ids {
            check(
                unsafe { FwpmFilterDeleteById0(e.0, id) },
                "remove own firewall rule",
            )?;
        }
        check(
            unsafe { FwpmSubLayerDeleteByKey0(e.0, &key(sid, "sublayer")?) },
            "remove own firewall sublayer",
        )?;
        check(
            unsafe { FwpmProviderDeleteByKey0(e.0, &provider) },
            "remove own firewall provider",
        )
    })
}

fn validate_local_program(path: &str) -> Result<(), ApiError> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, GetDriveTypeW, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    };
    daemon::modules::exit_guard::config::path_key(path).map_err(ApiError::invalid_args)?;
    let root = &path[..3];
    if !matches!(unsafe { GetDriveTypeW(ffi::to_wide(root).as_ptr()) }, 2 | 3 | 6) {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "firewall targets must be on a local volume",
        ));
    }
    let mut part = std::path::PathBuf::from(root);
    for component in path[3..].split('\\') {
        part.push(component);
        let attributes = unsafe { GetFileAttributesW(ffi::to_wide(&part).as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ApiError::new(
                ErrorCode::Denied,
                "firewall target must exist without reparse points",
            ));
        }
    }
    Ok(())
}

fn filters_current(e: &Engine, sid: &str, p: &ExitFirewallPolicy) -> Result<bool, ApiError> {
    for (index, rule) in plan(p)?.iter().enumerate() {
        let mut found = null_mut();
        let code = unsafe { FwpmFilterGetByKey0(e.0, &filter_key(sid, &p.program, index)?, &mut found) };
        if code == FWP_E_FILTER_NOT_FOUND as u32 {
            return Ok(false);
        }
        check(code, "inspect current filter")?;
        let _memory = Allocation(found.cast());
        let f = unsafe { &*found };
        let conditions = if f.numFilterConditions == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(f.filterCondition, f.numFilterConditions as usize) }
        };
        let excludes_loopback = conditions.iter().any(|c| {
            same_guid(&c.fieldKey, &FWPM_CONDITION_FLAGS)
                && c.matchType == FWP_MATCH_FLAGS_NONE_SET
                && c.conditionValue.r#type == FWP_UINT32
                && unsafe { c.conditionValue.Anonymous.uint32 } == FWP_CONDITION_FLAG_IS_LOOPBACK
        });
        if excludes_loopback
            != matches!(
                rule.constraint,
                Constraint::OtherInterface(_) | Constraint::UnknownInterface
            )
        {
            return Ok(false);
        }
        if let Constraint::OtherInterface(id) = &rule.constraint {
            let mut luid = NET_LUID_LH::default();
            if unsafe { ConvertInterfaceGuidToLuid(&guid(id)?, &mut luid) } != 0 {
                return Ok(false);
            }
            let expected = unsafe { luid.Value };
            if !conditions.iter().any(|c| {
                c.matchType == FWP_MATCH_NOT_EQUAL
                    && c.conditionValue.r#type == FWP_UINT64
                    && unsafe {
                        !c.conditionValue.Anonymous.uint64.is_null()
                            && *c.conditionValue.Anonymous.uint64 == expected
                    }
            }) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
