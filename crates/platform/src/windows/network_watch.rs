//! Read-only network notifications and WinHTTP requests. No adapter or firewall writes.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::ptr::{null, null_mut};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use daemon::sys::{
    Guard,
    network::{DnsAssessment, Network, NetworkChange, NetworkSink, ProxyRoute},
};
use proto::ApiError;
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE},
    NetworkManagement::IpHelper::*,
    Networking::WinSock::AF_UNSPEC,
    System::{
        Registry::*,
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

pub struct WindowsNetwork;
pub static NETWORK: WindowsNetwork = WindowsNetwork;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn failure(op: &str) -> ApiError {
    ApiError::internal(format!("{op}: Windows error {}", unsafe { GetLastError() }))
}

struct Watch {
    handles: Vec<usize>,
    // Stable address retained until CancelMibChangeNotify2 has waited out callbacks.
    sink: Box<NetworkSink>,
    stop: mpsc::Sender<()>,
    proxy: Option<JoinHandle<()>>,
}
impl Guard for Watch {}
impl Drop for Watch {
    fn drop(&mut self) {
        for h in &self.handles {
            unsafe {
                CancelMibChangeNotify2(*h as HANDLE);
            }
        }
        let _ = self.stop.send(());
        if let Some(t) = self.proxy.take() {
            let _ = t.join();
        }
        // Explicitly keep the callback context alive through cancellation above.
        let _ = &self.sink;
    }
}
unsafe extern "system" fn interface_cb(
    ctx: *const c_void,
    _: *const MIB_IPINTERFACE_ROW,
    _: MIB_NOTIFICATION_TYPE,
) {
    unsafe {
        (&*(ctx as *const NetworkSink))(NetworkChange::Interface);
    }
}
unsafe extern "system" fn address_cb(
    ctx: *const c_void,
    _: *const MIB_UNICASTIPADDRESS_ROW,
    _: MIB_NOTIFICATION_TYPE,
) {
    unsafe {
        (&*(ctx as *const NetworkSink))(NetworkChange::Address);
    }
}
unsafe extern "system" fn route_cb(
    ctx: *const c_void,
    row: *const MIB_IPFORWARD_ROW2,
    _: MIB_NOTIFICATION_TYPE,
) {
    // Include more-specific routes too: split tunnels may affect a probe or DNS route.
    if !row.is_null() {
        unsafe {
            (&*(ctx as *const NetworkSink))(NetworkChange::Route);
        }
    }
}

impl Network for WindowsNetwork {
    fn disconnect(&self, interface: &str) -> Result<(), ApiError> {
        super::wlan::disconnect(interface)
    }
    fn profiles(&self) -> Result<Vec<daemon::sys::network::WifiProfile>, ApiError> {
        super::wlan::profiles()
    }
    fn connect_profile(&self, interface: &str, profile: &str) -> Result<(), ApiError> {
        super::wlan::connect(interface, profile)
    }
    fn link_readings(&self) -> Result<Vec<daemon::sys::network::LinkReading>, ApiError> {
        super::wlan::readings()
    }
    fn set_profile_metered(&self, interface: &str, profile: &str, metered: bool) -> Result<(), ApiError> {
        super::wlan::set_metered(interface, profile, metered)
    }
    fn probe(&self, url: &str, route: &ProxyRoute) -> daemon::sys::network::Probe {
        daemon::sys::network::probe_result(get_with_timeout(url, route, 3000))
    }
    fn get_for_route(
        &self,
        url: &str,
        proxy: &ProxyRoute,
        route: &daemon::sys::exit_firewall::ExitRoute,
    ) -> Result<String, ApiError> {
        use daemon::sys::exit_firewall::ExitRoute;
        match route {
            ExitRoute::Vpn { interface } => get_checked(url, &ProxyRoute::Direct, Some(interface), 3000),
            ExitRoute::LocalProxy { endpoint } => get_checked(
                url,
                &ProxyRoute::Explicit(format!("http://{endpoint}")),
                None,
                3000,
            ),
            ExitRoute::DenyAll => get(url, proxy),
        }
    }
    fn interfaces(&self) -> Result<Vec<daemon::sys::network::NetworkInterface>, ApiError> {
        let mut table = null_mut();
        let code = unsafe { GetIfTable2(&mut table) };
        if code != 0 {
            return Err(ApiError::internal(format!("read network interfaces: {code}")));
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
        let count = unsafe { (*table.0).NumEntries as usize };
        if count > 4096 {
            return Err(ApiError::internal("too many network interfaces"));
        }
        let rows = unsafe { std::slice::from_raw_parts((*table.0).Table.as_ptr(), count) };
        Ok(rows
            .iter()
            .map(|r| {
                let g = &r.InterfaceGuid;
                let d = g.data4;
                let id = format!(
                    "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
                );
                let name = String::from_utf16_lossy(
                    &r.Alias[..r.Alias.iter().position(|v| *v == 0).unwrap_or(r.Alias.len())],
                );
                let desc = String::from_utf16_lossy(&r.Description).to_ascii_lowercase();
                daemon::sys::network::NetworkInterface {
                    id,
                    name,
                    up: r.OperStatus == 1,
                    tunnel: r.Type == 23
                        || r.Type == 131
                        || ["vpn", "wintun", "wireguard", "tap"]
                            .iter()
                            .any(|v| desc.contains(v)),
                }
            })
            .collect())
    }
    fn watch(&self, sink: NetworkSink) -> Result<Box<dyn Guard>, ApiError> {
        let sink = Box::new(sink);
        let ctx = (&*sink as *const NetworkSink).cast();
        let mut handles = Vec::new();
        let mut h = null_mut();
        let first = unsafe { NotifyIpInterfaceChange(AF_UNSPEC, Some(interface_cb), ctx, false, &mut h) };
        if first != 0 {
            return Err(ApiError::internal(format!("NotifyIpInterfaceChange: {first}")));
        }
        handles.push(h as usize);
        let second = unsafe { NotifyUnicastIpAddressChange(AF_UNSPEC, Some(address_cb), ctx, false, &mut h) };
        if second == 0 {
            handles.push(h as usize);
        }
        let third = unsafe { NotifyRouteChange2(AF_UNSPEC, Some(route_cb), ctx, false, &mut h) };
        if third == 0 {
            handles.push(h as usize);
        }
        if second != 0 || third != 0 {
            for h in handles {
                unsafe {
                    CancelMibChangeNotify2(h as HANDLE);
                }
            }
            return Err(ApiError::internal(format!(
                "network subscription failed: {second}/{third}"
            )));
        }
        let (stop, rx) = mpsc::channel();
        let callback = (*sink).clone();
        // Arm notifications before returning, so an immediate registry change is not lost.
        let (ready_tx, ready_rx) = mpsc::channel();
        let proxy = std::thread::spawn(move || proxy_watch(callback, rx, ready_tx));
        let watch = Watch {
            handles,
            sink,
            stop,
            proxy: Some(proxy),
        };
        ready_rx
            .recv()
            .map_err(|_| ApiError::internal("proxy watcher stopped"))??;
        Ok(Box::new(watch))
    }
    fn get(&self, url: &str, route: &ProxyRoute) -> Result<String, ApiError> {
        get(url, route)
    }
    fn dns(&self) -> Result<DnsAssessment, ApiError> {
        dns()
    }
}

fn proxy_watch(sink: NetworkSink, stop: mpsc::Receiver<()>, ready: mpsc::Sender<Result<(), ApiError>>) {
    // Only HKCU, read/notify rights, no registry writes. Watch subtree for connection profiles.
    let path = wide(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
    let mut key = null_mut();
    let code = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_NOTIFY, &mut key) };
    if code != 0 {
        let _ = ready.send(Err(ApiError::internal(format!("proxy registry: {code}"))));
        return;
    }
    let event = unsafe { CreateEventW(null(), 0, 0, null()) };
    if event.is_null() {
        unsafe {
            RegCloseKey(key);
        }
        let _ = ready.send(Err(failure("proxy event")));
        return;
    }
    let mut initial = Some(ready);
    loop {
        let code = unsafe {
            RegNotifyChangeKeyValue(
                key,
                1,
                REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME,
                event,
                1,
            )
        };
        if let Some(tx) = initial.take() {
            let _ = tx.send(if code == 0 {
                Ok(())
            } else {
                Err(ApiError::internal(format!("proxy notify: {code}")))
            });
        }
        if code != 0 {
            break;
        }
        loop {
            if stop.try_recv().is_ok() {
                unsafe {
                    RegCloseKey(key);
                    CloseHandle(event);
                }
                return;
            }
            if unsafe { WaitForSingleObject(event, 200) } == 0 {
                sink(NetworkChange::Proxy);
                break;
            }
        }
    }
    unsafe {
        RegCloseKey(key);
        CloseHandle(event);
    }
}

// WinHTTP is part of Windows. These declarations avoid adding a platform dependency to daemon.
type H = *mut c_void;
#[repr(C)]
#[derive(Default)]
struct ProxyInfo {
    access: u32,
    proxy: *mut u16,
    bypass: *mut u16,
}
#[repr(C)]
#[derive(Default)]
struct IeProxy {
    auto: i32,
    pac: *mut u16,
    proxy: *mut u16,
    bypass: *mut u16,
}
#[repr(C)]
struct AutoProxy {
    flags: u32,
    detect: u32,
    pac: *const u16,
    reserved: *mut c_void,
    reserved2: u32,
    login: i32,
}
windows_link::link!("winhttp.dll" "system" fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> H);
windows_link::link!("winhttp.dll" "system" fn WinHttpCloseHandle(h: H) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpSetTimeouts(h: H, resolve: i32, connect: i32, send: i32, receive: i32) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpGetIEProxyConfigForCurrentUser(config: *mut IeProxy) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpGetProxyForUrl(h: H, url: *const u16, options: *const AutoProxy, proxy: *mut ProxyInfo) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpConnect(h: H, server: *const u16, port: u16, reserved: u32) -> H);
windows_link::link!("winhttp.dll" "system" fn WinHttpOpenRequest(h: H, verb: *const u16, object: *const u16, version: *const u16, referer: *const u16, accept: *const *const u16, flags: u32) -> H);
windows_link::link!("winhttp.dll" "system" fn WinHttpSetOption(h: H, option: u32, buffer: *const c_void, length: u32) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpSendRequest(h: H, headers: *const u16, length: u32, body: *const c_void, body_len: u32, total: u32, context: usize) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpReceiveResponse(h: H, reserved: *mut c_void) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpQueryHeaders(h: H, level: u32, name: *const u16, buffer: *mut c_void, length: *mut u32, index: *mut u32) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpReadData(h: H, buffer: *mut c_void, count: u32, read: *mut u32) -> i32);
windows_link::link!("winhttp.dll" "system" fn WinHttpQueryOption(h:H,option:u32,buffer:*mut c_void,length:*mut u32)->i32);
windows_link::link!("kernel32.dll" "system" fn GlobalFree(h: H) -> H);
struct HttpHandle(H);
impl Drop for HttpHandle {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0);
        }
    }
}
fn checked(h: H, op: &str) -> Result<HttpHandle, ApiError> {
    if h.is_null() {
        Err(failure(op))
    } else {
        Ok(HttpHandle(h))
    }
}
impl Drop for IeProxy {
    fn drop(&mut self) {
        unsafe {
            for p in [self.pac, self.proxy, self.bypass] {
                if !p.is_null() {
                    GlobalFree(p.cast());
                }
            }
        }
    }
}
impl Drop for ProxyInfo {
    fn drop(&mut self) {
        unsafe {
            for p in [self.proxy, self.bypass] {
                if !p.is_null() {
                    GlobalFree(p.cast());
                }
            }
        }
    }
}

struct Url {
    host: String,
    port: u16,
    path: String,
    secure: bool,
}
fn parse_url(url: &str) -> Result<Url, ApiError> {
    let (secure, rest) = if let Some(x) = url.strip_prefix("https://") {
        (true, x)
    } else if let Some(x) = url.strip_prefix("http://") {
        (false, x)
    } else {
        return Err(ApiError::invalid_args("HTTP(S) URL required"));
    };
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
    if authority.contains(['@', '?', '#', '\\']) || url.chars().any(char::is_control) {
        return Err(ApiError::invalid_args("invalid URL"));
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, suffix) = v6
            .split_once(']')
            .ok_or_else(|| ApiError::invalid_args("invalid IPv6 URL"))?;
        (
            host.to_owned(),
            if suffix.is_empty() {
                None
            } else {
                Some(
                    suffix
                        .strip_prefix(':')
                        .ok_or_else(|| ApiError::invalid_args("invalid port"))?,
                )
            },
        )
    } else {
        authority
            .split_once(':')
            .map_or((authority.to_owned(), None), |(h, p)| (h.to_owned(), Some(p)))
    };
    let port = port
        .map(str::parse::<u16>)
        .transpose()
        .map_err(|_| ApiError::invalid_args("invalid port"))?
        .unwrap_or(if secure { 443 } else { 80 });
    let local = host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let ncsi = url == "http://www.msftconnecttest.com/connecttest.txt";
    if host.is_empty() || port == 0 || (!secure && !local && !ncsi) {
        return Err(ApiError::invalid_args(
            "plain HTTP is allowed only for literal loopback or the fixed NCSI connectivity probe",
        ));
    }
    Ok(Url {
        host,
        port,
        path,
        secure,
    })
}

fn get(url: &str, route: &ProxyRoute) -> Result<String, ApiError> {
    get_with_timeout(url, route, 3000)
}
fn get_with_timeout(url: &str, route: &ProxyRoute, timeout: i32) -> Result<String, ApiError> {
    get_checked(url, route, None, timeout)
}
fn get_checked(
    url: &str,
    route: &ProxyRoute,
    expected_interface: Option<&str>,
    timeout: i32,
) -> Result<String, ApiError> {
    let u = parse_url(url)?;
    let agent = wide("Arcturus/egress-check");
    let url_w = wide(url);
    let mut ie = IeProxy::default();
    let mut explicit = None;
    let mut use_system = false;
    match route {
        ProxyRoute::Explicit(p) => explicit = Some(proxy_name(p)?),
        ProxyRoute::System => {
            if let Some(p) = std::env::var("HTTPS_PROXY")
                .ok()
                .filter(|p| !p.is_empty())
                .or_else(|| std::env::var("https_proxy").ok().filter(|p| !p.is_empty()))
            {
                explicit = Some(proxy_name(&p)?);
            } else {
                if unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut ie) } == 0 {
                    return Err(failure("read system proxy"));
                }
                use_system = true;
            }
        }
        ProxyRoute::Direct => {}
    }
    let proxy = explicit.as_ref().map(|p| wide(p));
    let static_proxy = proxy.as_ref().map_or(ie.proxy as *const u16, |p| p.as_ptr());
    let access = if !static_proxy.is_null() { 3 } else { 1 }; // named / no proxy
    let session = checked(
        unsafe {
            WinHttpOpen(
                agent.as_ptr(),
                access,
                static_proxy,
                if use_system { ie.bypass } else { null() },
                0,
            )
        },
        "WinHttpOpen",
    )?;
    if unsafe { WinHttpSetTimeouts(session.0, timeout, timeout, timeout, timeout) } == 0 {
        return Err(failure("HTTP timeouts"));
    }
    let server = wide(&u.host);
    let connection = checked(
        unsafe { WinHttpConnect(session.0, server.as_ptr(), u.port, 0) },
        "WinHttpConnect",
    )?;
    let path = wide(&u.path);
    let request = checked(
        unsafe {
            WinHttpOpenRequest(
                connection.0,
                wide("GET").as_ptr(),
                path.as_ptr(),
                null(),
                null(),
                null(),
                if u.secure { 0x00800000 } else { 0 },
            )
        },
        "WinHttpOpenRequest",
    )?;
    if use_system && (ie.auto != 0 || !ie.pac.is_null()) {
        let options = AutoProxy {
            flags: (if ie.auto != 0 { 1 } else { 0 }) | (if !ie.pac.is_null() { 2 } else { 0 }),
            detect: 3,
            pac: ie.pac,
            reserved: null_mut(),
            reserved2: 0,
            login: 0,
        };
        let mut info = ProxyInfo::default();
        if unsafe { WinHttpGetProxyForUrl(session.0, url_w.as_ptr(), &options, &mut info) } == 0 {
            return Err(failure("resolve PAC proxy (no direct fallback)"));
        }
        if unsafe {
            WinHttpSetOption(
                request.0,
                38,
                (&info as *const ProxyInfo).cast(),
                std::mem::size_of::<ProxyInfo>() as u32,
            )
        } == 0
        {
            return Err(failure("set PAC proxy"));
        }
    }
    // Disable redirects and cookies; a local status API cannot redirect to another host.
    let disabled: u32 = 2 | 1;
    if unsafe { WinHttpSetOption(request.0, 63, (&disabled as *const u32).cast(), 4) } == 0 {
        return Err(failure("disable redirects"));
    }
    if unsafe { WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) } == 0
        || unsafe { WinHttpReceiveResponse(request.0, null_mut()) } == 0
    {
        return Err(failure("HTTP request"));
    }
    let mut status: u32 = 0;
    let mut size = 4;
    if unsafe {
        WinHttpQueryHeaders(
            request.0,
            19 | 0x20000000,
            null(),
            (&mut status as *mut u32).cast(),
            &mut size,
            null_mut(),
        )
    } == 0
    {
        return Err(failure("HTTP status"));
    }
    if status != 200 && status != 204 {
        return Err(ApiError::internal(format!("HTTP status {status}"))
            .with_details(serde_json::json!({"http_status":status})));
    }
    if let Some(interface) = expected_interface {
        use windows_sys::Win32::{
            NetworkManagement::Ndis::NET_LUID_LH,
            Networking::WinSock::{SOCKADDR_INET, SOCKADDR_STORAGE},
        };
        #[repr(C)]
        struct ConnectionInfo {
            size: u32,
            local: SOCKADDR_STORAGE,
            remote: SOCKADDR_STORAGE,
        }
        let mut info: ConnectionInfo = unsafe { std::mem::zeroed() };
        info.size = std::mem::size_of::<ConnectionInfo>() as u32;
        let mut size = info.size;
        if unsafe {
            WinHttpQueryOption(
                request.0,
                93,
                (&mut info as *mut ConnectionInfo).cast(),
                &mut size,
            )
        } == 0
        {
            return Err(failure("verify actual VPN probe connection"));
        }
        let id = u128::from_str_radix(&interface.trim_matches(['{', '}']).replace('-', ""), 16)
            .map_err(|_| ApiError::invalid_args("invalid probe interface UUID"))?;
        let mut expected = NET_LUID_LH::default();
        if unsafe { ConvertInterfaceGuidToLuid(&windows_sys::core::GUID::from_u128(id), &mut expected) } != 0
        {
            return Err(ApiError::internal("selected VPN interface is unavailable"));
        }
        // Verify the connection's actual source belongs to the selected interface too.
        // A route changing after the response must not make an old direct response look safe.
        let mut addresses = null_mut();
        let code = unsafe { GetUnicastIpAddressTable(info.local.ss_family, &mut addresses) };
        if code != 0 {
            return Err(ApiError::internal("cannot verify the probe source interface"));
        }
        struct Addresses(*mut MIB_UNICASTIPADDRESS_TABLE);
        impl Drop for Addresses {
            fn drop(&mut self) {
                unsafe {
                    FreeMibTable(self.0.cast());
                }
            }
        }
        let addresses = Addresses(addresses);
        let rows = unsafe {
            std::slice::from_raw_parts((*addresses.0).Table.as_ptr(), (*addresses.0).NumEntries as usize)
        };
        let local = unsafe { &*(&info.local as *const SOCKADDR_STORAGE).cast::<SOCKADDR_INET>() };
        let source_matches = rows.iter().any(|r| unsafe {
            r.InterfaceLuid.Value == expected.Value
                && r.Address.si_family == local.si_family
                && match local.si_family {
                    2 => r.Address.Ipv4.sin_addr.S_un.S_addr == local.Ipv4.sin_addr.S_un.S_addr,
                    23 => r.Address.Ipv6.sin6_addr.u.Byte == local.Ipv6.sin6_addr.u.Byte,
                    _ => false,
                }
        });
        if !source_matches {
            return Err(ApiError::internal(
                "probe source address is outside the selected VPN interface",
            ));
        }
        let mut best = MIB_IPFORWARD_ROW2::default();
        let mut source = SOCKADDR_INET::default();
        let code = unsafe {
            GetBestRoute2(
                null(),
                0,
                (&info.local as *const SOCKADDR_STORAGE).cast(),
                (&info.remote as *const SOCKADDR_STORAGE).cast(),
                0,
                &mut best,
                &mut source,
            )
        };
        if code != 0 || unsafe { best.InterfaceLuid.Value != expected.Value } {
            return Err(ApiError::internal(
                "probe did not use the selected VPN route; egress cannot be verified",
            ));
        }
    }
    let mut body = Vec::new();
    let started = std::time::Instant::now();
    loop {
        let mut buf = [0u8; 4096];
        let mut n = 0;
        if unsafe { WinHttpReadData(request.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n) } == 0 {
            return Err(failure("HTTP body"));
        }
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n as usize]);
        if body.len() > 65536 || started.elapsed() > Duration::from_secs(8) {
            return Err(ApiError::internal("HTTP response exceeds limits"));
        }
    }
    String::from_utf8(body).map_err(|_| ApiError::internal("HTTP body is not UTF-8"))
}

fn proxy_name(p: &str) -> Result<String, ApiError> {
    let p = p.strip_prefix("http://").unwrap_or(p).trim_end_matches('/');
    if p.is_empty() || p.contains(['/', '@', '\\', ';', ' ', '\r', '\n']) {
        return Err(ApiError::invalid_args(
            "an HTTP proxy host:port is required; SOCKS and credentials are not supported",
        ));
    }
    Ok(p.to_owned())
}

fn powershell() -> std::process::Command {
    let mut c = std::process::Command::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    c.args([
        "-NoProfile",
        "-NonInteractive",
        "-WindowStyle",
        "Hidden",
        "-Command",
    ]);
    c.creation_flags(0x08000000);
    c
}

fn dns() -> Result<DnsAssessment, ApiError> {
    // This inspects configured resolvers and their selected routes, without issuing DNS.
    // VPN identification is necessarily heuristic; local stubs / DoH remain unknown.
    const SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $rows=@(); $adapters=Get-NetAdapter -IncludeHidden; foreach($entry in (Get-DnsClientServerAddress)) { foreach($server in $entry.ServerAddresses) { if($server -eq '127.0.0.1' -or $server -eq '::1') { $rows+=@{server=$server;kind='unknown'}; continue }; try { $route=Find-NetRoute -RemoteIPAddress $server; $idx=($route | Select-Object -First 1).InterfaceIndex; $a=$adapters | Where-Object InterfaceIndex -eq $idx | Select-Object -First 1; $vpn=($a.InterfaceDescription -match 'VPN|WireGuard|Wintun|TAP|TUN|PPP') -or ($a.ifType -eq 23); $rows+=@{server=$server;kind=$(if($vpn){'vpn'}else{'bypass'});interface=$idx} } catch { $rows+=@{server=$server;kind='unknown'} } } }; ConvertTo-Json -Compress -InputObject $rows"#;
    let mut child = powershell()
        .arg(SCRIPT)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ApiError::internal(format!("DNS routes: {e}")))?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ApiError::internal("DNS route inspection timed out"));
            }
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| ApiError::internal(format!("DNS routes: {e}")))?;
    if !out.status.success() {
        return Err(ApiError::internal("cannot inspect DNS routes"));
    }
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&out.stdout).map_err(|e| ApiError::internal(format!("DNS routes: {e}")))?;
    let state = if rows.iter().any(|r| r["kind"] == "bypass") {
        "bypass"
    } else if !rows.is_empty() && rows.iter().all(|r| r["kind"] == "vpn") {
        "vpn_route"
    } else {
        "unknown"
    };
    Ok(DnsAssessment {
        state: state.into(),
        details: rows.iter().map(ToString::to_string).collect(),
    })
}

pub fn notify_user(title: &str, message: &str) -> Result<(), ApiError> {
    // Pass text as numeric UTF-16, never as executable PowerShell source.
    let chars = |s: &str| {
        s.encode_utf16()
            .take(220)
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; $n=New-Object System.Windows.Forms.NotifyIcon; $n.Icon=[System.Drawing.SystemIcons]::Warning; $n.Visible=$true; $n.BalloonTipTitle=-join [char[]]({}); $n.BalloonTipText=-join [char[]]({}); $n.ShowBalloonTip(10000); Start-Sleep -Seconds 12; $n.Dispose()",
        chars(title),
        chars(message)
    );
    powershell()
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| ApiError::internal(format!("desktop notification: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_validation_is_local_and_does_not_send_traffic() {
        assert!(parse_url("https://www.cloudflare.com/cdn-cgi/trace").is_ok());
        assert!(parse_url("http://127.0.0.1:9090/proxies/GLOBAL").is_ok());
        assert!(parse_url("http://[::1]:9090/configs").is_ok());
        for url in [
            "http://example.com/a",
            "http://localhost:90/",
            "https://user@host/",
            "file:///a",
        ] {
            assert!(parse_url(url).is_err(), "{url}");
        }
        assert!(proxy_name("socks5://127.0.0.1:90").is_err());
    }
}
