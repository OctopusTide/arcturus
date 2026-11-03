//! Power on Windows: battery, power plan settings (powrprof), the power mode slider,
//! keep-awake requests (`powercfg /requests` shows them), lock, screen off, sleep and
//! hibernate. Plan settings are written to the user's active plan, which needs no
//! administrator rights.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use daemon::sys::{Guard, PowerCapabilities, PowerMode, PowerSetting, PowerStatus, PowerValue, Suspend, unsupported};
use proto::ApiError;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID, LocalFree};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Power::{
    GetPwrCapabilities, GetSystemPowerStatus, PowerClearRequest, PowerCreateRequest, PowerGetActiveScheme,
    PowerReadACValueIndex, PowerReadDCValueIndex, PowerRequestDisplayRequired, PowerRequestSystemRequired,
    PowerSetActiveScheme, PowerSetRequest, PowerWriteACValueIndex, PowerWriteDCValueIndex, SYSTEM_POWER_CAPABILITIES,
    SYSTEM_POWER_STATUS, SetSuspendState,
};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_64KEY, RegCloseKey, RegOpenKeyExW};
use windows_sys::Win32::System::Shutdown::{
    EWX_POWEROFF, EWX_SHUTDOWN, ExitWindowsEx, LockWorkStation, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_POWER,
    SHTDN_REASON_MINOR_ENVIRONMENT,
};
use windows_sys::Win32::System::SystemServices::POWER_REQUEST_CONTEXT_VERSION;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, PostMessageW, SC_MONITORPOWER, WM_SYSCOMMAND};
use windows_sys::core::GUID;

use super::ffi::{os_error, to_wide};

// Not in windows-sys: the power mode slider ("overlay scheme"), powersetting.h.
windows_link::link!("powrprof.dll" "system" fn PowerGetEffectiveOverlayScheme(overlay: *mut GUID) -> u32);
windows_link::link!("powrprof.dll" "system" fn PowerSetActiveOverlayScheme(overlay: *const GUID) -> u32);

pub fn guid_u128(g: &GUID) -> u128 {
    (u128::from(g.data1) << 96)
        | (u128::from(g.data2) << 80)
        | (u128::from(g.data3) << 64)
        | u128::from(u64::from_be_bytes(g.data4))
}

pub fn guid_string(g: &GUID) -> String {
    let v = guid_u128(g);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        v >> 96,
        (v >> 80) & 0xffff,
        (v >> 64) & 0xffff,
        (v >> 48) & 0xffff,
        v & 0xffff_ffff_ffff
    )
}

pub fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s.trim_matches(['{', '}']).chars().filter(|c| *c != '-').collect();
    (hex.len() == 32).then(|| u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)).flatten()
}

/// (subgroup, setting) of each plan setting.
fn setting_ids(s: PowerSetting) -> (GUID, GUID) {
    let g = GUID::from_u128;
    match s {
        // SUB_BUTTONS / LIDACTION
        PowerSetting::LidAction => (g(0x4f971e89_eebd_4455_a8de_9e59040e7347), g(0x5ca83367_6e45_459f_a27b_476b1d01c936)),
        // SUB_PROCESSOR / PROCTHROTTLEMAX
        PowerSetting::CpuMaxPercent => (g(0x54533251_82be_4824_96c1_47b60b740d00), g(0xbc5038f7_23e0_4960_96da_33abaf5935ec)),
        // SUB_VIDEO / display brightness
        PowerSetting::DisplayBrightness => {
            (g(0x7516b95f_f776_4464_8c53_06167f40cc99), g(0xaded5e82_b909_4619_9949_f5d71dac0bcb))
        }
        // SUB_ENERGYSAVER / ESBATTTHRESHOLD
        PowerSetting::SaverThreshold => (g(0xde830923_a562_41af_a086_e3a2c6bad2da), g(0xe69653ca_cf7f_4f05_aa73_cb833fa90ad4)),
        // USB settings / USB selective suspend
        PowerSetting::UsbSuspend => (g(0x2a737441_1930_4402_8d77_b2bebba308a3), g(0x48e6b7a6_50f5_4782_a5d4_53bb8f07e226)),
        // Wireless adapter settings / power saving mode
        PowerSetting::WifiPowerSaving => {
            (g(0x19cbb8fa_5279_450e_9fac_8a3d5fedd0c1), g(0x12bbebe6_58d6_4636_95bb_3217ef867c1a))
        }
    }
}

const OVERLAYS: [(&str, u128); 3] = [
    (PowerMode::EFFICIENCY, 0x961cc777_2547_4f9d_8174_7d86181b8a7a),
    (PowerMode::BALANCED, 0),
    (PowerMode::PERFORMANCE, 0xded574b5_45a0_4f42_8737_46345c09c238),
];

fn win32(what: &str, code: u32) -> Result<(), ApiError> {
    match code {
        0 => Ok(()),
        // ERROR_FILE_NOT_FOUND: the plan has no such setting on this machine.
        2 => Err(unsupported(what)),
        c => Err(ApiError::internal(format!("{what} failed: Windows error {c}"))),
    }
}

pub fn power_status() -> Result<PowerStatus, ApiError> {
    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: valid out-pointer.
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return Err(os_error("GetSystemPowerStatus"));
    }
    let no_battery = s.BatteryFlag == 128 || s.BatteryFlag == 255;
    Ok(PowerStatus {
        on_ac: match s.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        battery_present: !no_battery,
        battery_percent: (!no_battery && s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent),
        charging: s.BatteryFlag != 255 && s.BatteryFlag & 8 != 0,
        os_remaining_s: (s.BatteryLifeTime != u32::MAX).then_some(u64::from(s.BatteryLifeTime)),
        saver_on: s.SystemStatusFlag & 1 != 0,
    })
}

pub fn capabilities() -> Result<PowerCapabilities, ApiError> {
    // SAFETY: plain data, zeroed is valid.
    let mut c: SYSTEM_POWER_CAPABILITIES = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer.
    if !unsafe { GetPwrCapabilities(&mut c) } {
        return Err(os_error("GetPwrCapabilities"));
    }
    Ok(PowerCapabilities {
        sleep: c.SystemS3 || c.AoAc,
        hibernate: c.SystemS4 && c.HiberFilePresent,
        modern_standby: c.AoAc,
        lid: c.LidPresent,
    })
}

fn active_plan() -> Result<GUID, ApiError> {
    let mut p: *mut GUID = null_mut();
    // SAFETY: valid out-pointer; the GUID is LocalAlloc'd and freed below.
    win32("PowerGetActiveScheme", unsafe { PowerGetActiveScheme(null_mut::<c_void>() as HKEY, &mut p) })?;
    // SAFETY: success means `p` points to a GUID.
    let g = unsafe { *p };
    // SAFETY: freeing the buffer the call allocated, once.
    unsafe { LocalFree(p.cast()) };
    Ok(g)
}

pub fn read_setting(s: PowerSetting) -> Result<PowerValue, ApiError> {
    let plan = active_plan()?;
    let (sub, id) = setting_ids(s);
    let (mut ac, mut dc) = (0u32, 0u32);
    let root = null_mut::<c_void>() as HKEY;
    // SAFETY: GUID pointers refer to locals; out-pointers are valid.
    win32(s.name(), unsafe { PowerReadACValueIndex(root, &plan, &sub, &id, &mut ac) })?;
    // SAFETY: as above.
    win32(s.name(), unsafe { PowerReadDCValueIndex(root, &plan, &sub, &id, &mut dc) })?;
    Ok(PowerValue { plan: guid_string(&plan), ac, dc })
}

pub fn write_setting(s: PowerSetting, v: &PowerValue) -> Result<(), ApiError> {
    let plan = parse_guid(&v.plan).ok_or_else(|| ApiError::internal(format!("bad power plan id '{}'", v.plan)))?;
    let (sub, id) = setting_ids(s);
    let root = null_mut::<c_void>() as HKEY;
    // SAFETY: GUID pointers refer to locals.
    win32(s.name(), unsafe { PowerWriteACValueIndex(root, &plan, &sub, &id, v.ac) })?;
    // SAFETY: as above.
    win32(s.name(), unsafe { PowerWriteDCValueIndex(root, &plan, &sub, &id, v.dc) })?;
    // Values written to the active plan only take effect when it is applied again.
    if guid_u128(&active_plan()?) == guid_u128(&plan) {
        // SAFETY: as above.
        win32("PowerSetActiveScheme", unsafe { PowerSetActiveScheme(root, &plan) })?;
    }
    Ok(())
}

pub fn power_mode() -> Result<String, ApiError> {
    let mut g = GUID::default();
    // SAFETY: valid out-pointer.
    win32("power_mode", unsafe { PowerGetEffectiveOverlayScheme(&mut g) })?;
    let v = guid_u128(&g);
    Ok(OVERLAYS.iter().find(|(_, id)| *id == v).map_or_else(|| guid_string(&g), |(n, _)| (*n).to_owned()))
}

pub fn set_power_mode(mode: &str) -> Result<(), ApiError> {
    let g = OVERLAYS
        .iter()
        .find(|(n, _)| *n == mode)
        .map(|(_, id)| GUID::from_u128(*id))
        .or_else(|| parse_guid(mode))
        .ok_or_else(|| ApiError::invalid_args(format!("unknown power mode '{mode}'")))?;
    // SAFETY: GUID pointer refers to a local.
    win32("set_power_mode", unsafe { PowerSetActiveOverlayScheme(&g) })
}

/// A live `PowerCreateRequest`; dropping it clears the requests and closes the handle.
struct AwakeRequest {
    handle: HANDLE,
    display: bool,
    _reason: Vec<u16>,
}

// SAFETY: the handle is a kernel object handle, usable from any thread.
unsafe impl Send for AwakeRequest {}

impl Guard for AwakeRequest {}

impl Drop for AwakeRequest {
    fn drop(&mut self) {
        // SAFETY: the handle came from PowerCreateRequest and is closed once.
        unsafe {
            PowerClearRequest(self.handle, PowerRequestSystemRequired);
            if self.display {
                PowerClearRequest(self.handle, PowerRequestDisplayRequired);
            }
            CloseHandle(self.handle);
        }
    }
}

pub fn keep_awake(reason: &str, display: bool) -> Result<Box<dyn Guard>, ApiError> {
    let mut text = to_wide(reason);
    let ctx = REASON_CONTEXT {
        Version: POWER_REQUEST_CONTEXT_VERSION,
        Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        Reason: REASON_CONTEXT_0 { SimpleReasonString: text.as_mut_ptr() },
    };
    // SAFETY: `ctx` and the string it points to outlive the call.
    let handle = unsafe { PowerCreateRequest(&ctx) };
    if handle.is_null() || handle as isize == -1 {
        return Err(os_error("PowerCreateRequest"));
    }
    let request = AwakeRequest { handle, display, _reason: text };
    // SAFETY: live request handle.
    if unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } == 0 {
        return Err(os_error("PowerSetRequest"));
    }
    // SAFETY: as above.
    if display && unsafe { PowerSetRequest(handle, PowerRequestDisplayRequired) } == 0 {
        return Err(os_error("PowerSetRequest(display)"));
    }
    Ok(Box::new(request))
}

pub fn lock_screen() -> Result<(), ApiError> {
    // SAFETY: no arguments.
    if unsafe { LockWorkStation() } == 0 {
        return Err(os_error("LockWorkStation"));
    }
    Ok(())
}

pub fn display_off() -> Result<(), ApiError> {
    // Posted, not sent: a hung window elsewhere cannot block us. 2 = power off.
    // SAFETY: plain message post.
    if unsafe { PostMessageW(HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER as usize, 2) } == 0 {
        return Err(os_error("PostMessageW(SC_MONITORPOWER)"));
    }
    Ok(())
}

/// Sleep, hibernate and shutdown need SeShutdownPrivilege enabled in our token. Normal
/// users hold it; it is just off by default.
fn enable_shutdown_privilege() -> Result<(), ApiError> {
    let mut token: HANDLE = null_mut();
    // SAFETY: pseudo handle and valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) } == 0 {
        return Err(os_error("OpenProcessToken"));
    }
    let mut luid = LUID::default();
    // SAFETY: constant name, valid out-pointer.
    let looked_up = unsafe { LookupPrivilegeValueW(null(), SE_SHUTDOWN_NAME, &mut luid) } != 0;
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
    };
    // SAFETY: live token, valid struct.
    let adjusted = looked_up && unsafe { AdjustTokenPrivileges(token, 0, &tp, 0, null_mut(), null_mut()) } != 0;
    let err = (!adjusted).then(|| os_error("AdjustTokenPrivileges(SeShutdownPrivilege)"));
    // SAFETY: closing the token once.
    unsafe { CloseHandle(token) };
    err.map_or(Ok(()), Err)
}

pub fn suspend(how: Suspend) -> Result<(), ApiError> {
    enable_shutdown_privilege()?;
    let ok = match how {
        // SAFETY: plain calls; they return after the machine wakes up.
        Suspend::Sleep => unsafe { SetSuspendState(false, false, false) },
        Suspend::Hibernate => unsafe { SetSuspendState(true, false, false) },
        Suspend::ShutDown => unsafe {
            ExitWindowsEx(
                EWX_SHUTDOWN | EWX_POWEROFF,
                SHTDN_REASON_MAJOR_POWER | SHTDN_REASON_MINOR_ENVIRONMENT | SHTDN_REASON_FLAG_PLANNED,
            ) != 0
        },
    };
    if !ok {
        return Err(os_error(how.name()));
    }
    Ok(())
}

/// Windows Update or component servicing wait for a restart. Reading these keys needs no
/// administrator rights.
pub fn restart_pending() -> Result<bool, ApiError> {
    const KEYS: [&str; 2] = [
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired",
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending",
    ];
    for path in KEYS {
        let wide = to_wide(path);
        let mut key: HKEY = null_mut();
        // SAFETY: valid string and out-pointer.
        let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide.as_ptr(), 0, KEY_QUERY_VALUE | KEY_WOW64_64KEY, &mut key) };
        if rc == 0 {
            // SAFETY: closing the key just opened.
            unsafe { RegCloseKey(key) };
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_round_trip() {
        let id = "5ca83367-6e45-459f-a27b-476b1d01c936";
        assert_eq!(guid_string(&parse_guid(id).unwrap()), id);
        assert_eq!(guid_string(&parse_guid("{5CA83367-6E45-459F-A27B-476B1D01C936}").unwrap()), id);
        assert!(parse_guid("nope").is_none());
    }

    /// Read-only: every setting the module may change can be read on this machine.
    #[test]
    fn plan_settings_are_readable() {
        for s in PowerSetting::ALL {
            match read_setting(s) {
                Ok(v) => assert!(parse_guid(&v.plan).is_some(), "{s:?}: {v:?}"),
                Err(e) => assert_eq!(e.subcode.as_deref(), Some(proto::subcode::UNSUPPORTED), "{s:?}: {e}"),
            }
        }
        assert_eq!(read_setting(PowerSetting::LidAction).unwrap().plan.len(), 36);
    }
}
