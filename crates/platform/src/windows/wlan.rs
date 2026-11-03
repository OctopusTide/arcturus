//! Saved WLAN profiles and interface counters. Passwords and profile XML are never read.
use super::ffi;
use daemon::sys::network::{LinkReading, WifiProfile};
use proto::ApiError;
use std::os::windows::process::CommandExt;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::IpHelper::*;
use windows_sys::Win32::NetworkManagement::WiFi::*;
use windows_sys::Win32::Networking::WinSock::AF_INET;
struct Client(HANDLE);
impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.0, null());
        }
    }
}
struct Buffer<T>(*mut T);
impl<T> Drop for Buffer<T> {
    fn drop(&mut self) {
        unsafe {
            WlanFreeMemory(self.0.cast());
        }
    }
}
fn client() -> Result<Client, ApiError> {
    let mut version = 0;
    let mut handle = null_mut();
    let code = unsafe { WlanOpenHandle(2, null(), &mut version, &mut handle) };
    if code != 0 {
        return Err(ApiError::internal(format!("WLAN service unavailable: {code}")));
    }
    Ok(Client(handle))
}
fn string(wide: &[u16]) -> String {
    String::from_utf16_lossy(&wide[..wide.iter().position(|c| *c == 0).unwrap_or(wide.len())])
}
fn guid_text(g: &windows_sys::core::GUID) -> String {
    let d = g.data4;
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}
fn guid(id: &str) -> Result<windows_sys::core::GUID, ApiError> {
    proto::admin::Operation::InterfaceMetric { interface: id.into(), metric: 1 }.validate(0)?;
    let text = id.trim_matches(['{', '}']).replace('-', "");
    let bytes = (0..16)
        .map(|i| {
            u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| ApiError::invalid_args("bad interface id"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(windows_sys::core::GUID {
        data1: u32::from_be_bytes(bytes[..4].try_into().unwrap()),
        data2: u16::from_be_bytes(bytes[4..6].try_into().unwrap()),
        data3: u16::from_be_bytes(bytes[6..8].try_into().unwrap()),
        data4: bytes[8..].try_into().unwrap(),
    })
}
pub fn profiles() -> Result<Vec<WifiProfile>, ApiError> {
    let client = client()?;
    let mut interfaces = null_mut();
    let code = unsafe { WlanEnumInterfaces(client.0, null(), &mut interfaces) };
    if code != 0 {
        return Err(ApiError::internal(format!("list Wi-Fi adapters: {code}")));
    }
    let interfaces = Buffer(interfaces);
    let count = unsafe { (*interfaces.0).dwNumberOfItems } as usize;
    if count > 128 {
        return Err(ApiError::internal("too many WLAN adapters"));
    }
    let entries = unsafe { std::slice::from_raw_parts((*interfaces.0).InterfaceInfo.as_ptr(), count) };
    let mut out = Vec::new();
    for interface in entries {
        let mut profiles = null_mut();
        let code = unsafe { WlanGetProfileList(client.0, &interface.InterfaceGuid, null(), &mut profiles) };
        if code != 0 {
            return Err(ApiError::internal(format!("list saved Wi-Fi profiles: {code}")));
        }
        let profiles = Buffer(profiles);
        let count = unsafe { (*profiles.0).dwNumberOfItems } as usize;
        if count > 4096 {
            return Err(ApiError::internal("too many saved WLAN profiles"));
        }
        let profiles = unsafe { std::slice::from_raw_parts((*profiles.0).ProfileInfo.as_ptr(), count) };
        let mut available = null_mut();
        let scan =
            unsafe { WlanGetAvailableNetworkList(client.0, &interface.InterfaceGuid, 0, null(), &mut available) };
        let available = if scan == 0 { Some(Buffer(available)) } else { None };
        let networks = available.as_ref().map(|a| unsafe {
            std::slice::from_raw_parts((*a.0).Network.as_ptr(), ((*a.0).dwNumberOfItems as usize).min(4096))
        });
        let mut connected = null_mut();
        let mut size = 0;
        let mut kind = 0;
        let current = unsafe {
            WlanQueryInterface(
                client.0,
                &interface.InterfaceGuid,
                wlan_intf_opcode_current_connection,
                null(),
                &mut size,
                &mut connected,
                &mut kind,
            )
        };
        let current = if current == 0 && size as usize >= std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
            let buffer = Buffer(connected);
            Some(unsafe { string(&(*(buffer.0.cast::<WLAN_CONNECTION_ATTRIBUTES>())).strProfileName) })
        } else {
            None
        };
        for p in profiles {
            let name = string(&p.strProfileName);
            let lower = name.to_lowercase();
            out.push(WifiProfile {
                interface: guid_text(&interface.InterfaceGuid),
                connected: current.as_ref() == Some(&name),
                available: networks.is_none_or(|ns| {
                    ns.iter().any(|n| string(&n.strProfileName) == name && n.bNetworkConnectable != 0)
                }),
                hotspot_hint: ["iphone", "hotspot", "android", "手机", "个人热点"].iter().any(|s| lower.contains(s)),
                name,
            });
        }
    }
    Ok(out)
}
pub fn connect(interface: &str, profile: &str) -> Result<(), ApiError> {
    let interface_guid = guid(interface)?;
    if profile.is_empty() || profile.encode_utf16().count() > 256 || profile.contains('\0') {
        return Err(ApiError::invalid_args("invalid saved profile name"));
    }
    if !profiles()?.iter().any(|p| p.interface.eq_ignore_ascii_case(interface) && p.name == profile) {
        return Err(ApiError::invalid_args("only a saved profile on the named interface can be connected"));
    }
    let client = client()?;
    let name = ffi::to_wide(profile);
    let params = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: name.as_ptr(),
        dot11BssType: dot11_BSS_type_any,
        ..Default::default()
    };
    let code = unsafe { WlanConnect(client.0, &interface_guid, &params, null()) };
    if code != 0 {
        return Err(ApiError::internal(format!("connect saved Wi-Fi: {code}")));
    }
    Ok(())
}
pub fn disconnect(interface: &str) -> Result<(), ApiError> {
    let id = guid(interface)?;
    let client = client()?;
    let code = unsafe { WlanDisconnect(client.0, &id, null()) };
    if code != 0 {
        return Err(ApiError::internal(format!("disconnect Wi-Fi: {code}")));
    }
    Ok(())
}
pub fn readings() -> Result<Vec<LinkReading>, ApiError> {
    let mut table = null_mut();
    let code = unsafe { GetIfTable2(&mut table) };
    if code != 0 {
        return Err(ApiError::internal(format!("interface counters: {code}")));
    }
    struct Table(*mut MIB_IF_TABLE2);
    impl Drop for Table {
        fn drop(&mut self) {
            unsafe {
                FreeMibTable(self.0.cast());
            }
        }
    }
    let table = Table(table);
    let rows = unsafe { std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize) };
    let mut result = Vec::new();
    for row in rows {
        let mut ip = unsafe { std::mem::zeroed::<MIB_IPINTERFACE_ROW>() };
        unsafe {
            InitializeIpInterfaceEntry(&mut ip);
        }
        ip.Family = AF_INET;
        ip.InterfaceLuid = row.InterfaceLuid;
        if unsafe { GetIpInterfaceEntry(&mut ip) } != 0 {
            continue;
        }
        result.push(LinkReading {
            interface: guid_text(&row.InterfaceGuid),
            up: row.OperStatus == 1,
            received_bytes: row.InOctets,
            sent_bytes: row.OutOctets,
            metric: ip.Metric,
            automatic_metric: ip.UseAutomaticMetric,
        });
    }
    Ok(result)
}
pub fn set_metered(interface: &str, profile: &str, metered: bool) -> Result<(), ApiError> {
    if !profiles()?.iter().any(|p| p.interface == interface && p.name == profile) {
        return Err(ApiError::invalid_args("unknown saved Wi-Fi profile"));
    }
    let alias = super::network_watch::NETWORK
        .interfaces()?
        .into_iter()
        .find(|i| i.id == interface)
        .ok_or_else(|| ApiError::invalid_args("unknown Wi-Fi interface"))?
        .name;
    let path = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\Windows".into())
        .join("System32/netsh.exe");
    let mut child = std::process::Command::new(path)
        .args([
            "wlan",
            "set",
            "profileparameter",
            &format!("name={profile}"),
            &format!("interface={alias}"),
            if metered { "cost=fixed" } else { "cost=default" },
        ])
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| ApiError::internal(e.to_string()))? {
            return if status.success() {
                Ok(())
            } else {
                Err(daemon::sys::needs_elevation("set saved hotspot as metered"))
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ApiError::internal("mark metered timed out"));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
use daemon::sys::network::Network;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_network_targets_are_rejected_before_any_change() {
        assert!(connect("bad", "x").is_err());
        assert!(guid("bad").is_err());
        assert!(guid("a8776d53-0e5b-4023-b425-71dbd176cf2d").is_ok());
    }
}
