//! Read-only readings: ACPI thermal zones (through the performance counters, the same data
//! as WMI `Win32_PerfFormattedData_Counters_ThermalZoneInformation`), memory, and the OS's
//! own connectivity verdict.

use std::ptr::{null, null_mut};
use std::sync::Mutex;

use daemon::sys::{Connectivity, ConnectivityHint, MemoryStatus, Temperature};
use proto::ApiError;
use windows_sys::Win32::NetworkManagement::IpHelper::GetNetworkConnectivityHint;
use windows_sys::Win32::Networking::WinSock::{
    NL_NETWORK_CONNECTIVITY_HINT, NetworkConnectivityCostHintFixed, NetworkConnectivityCostHintVariable,
    NetworkConnectivityLevelHintConstrainedInternetAccess, NetworkConnectivityLevelHintInternetAccess,
    NetworkConnectivityLevelHintLocalAccess, NetworkConnectivityLevelHintNone,
};
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use super::ffi::{from_wide_ptr, os_error, to_wide};

/// One open PDH query with one wildcard counter, kept for the life of the process.
pub(super) struct Counter {
    query: usize,
    counter: usize,
}

impl Counter {
    /// Opens the first of `paths` (English counter paths) that exists; also says which.
    pub(super) fn open(paths: &[&str]) -> Result<(Counter, usize), ApiError> {
        let mut query = null_mut();
        // SAFETY: valid out-pointer; no data source = live data.
        let rc = unsafe { PdhOpenQueryW(null(), 0, &mut query) };
        if rc != 0 {
            return Err(ApiError::internal(format!("PdhOpenQuery failed: {rc:#x}")));
        }
        for (i, path) in paths.iter().enumerate() {
            let wide = to_wide(path);
            let mut counter = null_mut();
            // SAFETY: live query, valid string and out-pointer.
            if unsafe { PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) } == 0 {
                return Ok((Counter { query: query as usize, counter: counter as usize }, i));
            }
        }
        Err(daemon::sys::unsupported(&format!("performance counter {}", paths.join(" / "))))
    }

    /// Takes a reading and returns every instance's value (instances without valid data
    /// are left out). Rate counters report the average since the previous reading.
    pub(super) fn read(&self) -> Result<Vec<(String, f64)>, ApiError> {
        // SAFETY: live query handle.
        let rc = unsafe { PdhCollectQueryData(self.query as _) };
        if rc != 0 {
            return Err(ApiError::internal(format!("PdhCollectQueryData failed: {rc:#x}")));
        }
        let (mut size, mut count) = (0u32, 0u32);
        // SAFETY: size probe with a null buffer.
        let rc = unsafe { PdhGetFormattedCounterArrayW(self.counter as _, PDH_FMT_DOUBLE, &mut size, &mut count, null_mut()) };
        if rc != PDH_MORE_DATA {
            return if rc == 0 { Ok(Vec::new()) } else { Err(ApiError::internal(format!("PdhGetFormattedCounterArray: {rc:#x}"))) };
        }
        // u64 storage keeps the items (which hold pointers and doubles) aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        // SAFETY: `buf` holds `size` bytes.
        let rc = unsafe { PdhGetFormattedCounterArrayW(self.counter as _, PDH_FMT_DOUBLE, &mut size, &mut count, items) };
        if rc != 0 {
            // A rate counter's first reading has nothing to compare with.
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for i in 0..count as usize {
            // SAFETY: the call filled `count` items; names point into `buf`.
            let item = unsafe { &*items.add(i) };
            if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA {
                continue;
            }
            // SAFETY: PDH_FMT_DOUBLE was asked for; the name is NUL-terminated inside `buf`.
            out.push(unsafe { (from_wide_ptr(item.szName), item.FmtValue.Anonymous.doubleValue) });
        }
        Ok(out)
    }
}

/// The thermal counter and its raw value per kelvin.
static THERMAL: Mutex<Option<(Counter, f64)>> = Mutex::new(None);

pub fn temperatures() -> Result<Vec<Temperature>, ApiError> {
    let mut slot = THERMAL.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        // Tenths of a kelvin where available, whole kelvins otherwise.
        let (c, which) = Counter::open(&[
            r"\Thermal Zone Information(*)\High Precision Temperature",
            r"\Thermal Zone Information(*)\Temperature",
        ])?;
        *slot = Some((c, if which == 0 { 10.0 } else { 1.0 }));
    }
    let (c, scale) = slot.as_ref().expect("just set");
    Ok(c.read()?
        .into_iter()
        .map(|(zone, raw)| Temperature { zone, celsius: ((raw / scale - 273.15) * 10.0).round() / 10.0 })
        .filter(|t| (-40.0..=150.0).contains(&t.celsius))
        .collect())
}

pub fn memory() -> Result<MemoryStatus, ApiError> {
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: dwLength set, valid pointer.
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return Err(os_error("GlobalMemoryStatusEx"));
    }
    let mb = |b: u64| b / (1024 * 1024);
    Ok(MemoryStatus {
        total_mb: mb(m.ullTotalPhys),
        available_mb: mb(m.ullAvailPhys),
        commit_limit_mb: mb(m.ullTotalPageFile),
        commit_used_mb: mb(m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile)),
    })
}

#[allow(non_upper_case_globals)]
pub fn connectivity() -> Result<ConnectivityHint, ApiError> {
    let mut h = NL_NETWORK_CONNECTIVITY_HINT::default();
    // SAFETY: valid out-pointer.
    let rc = unsafe { GetNetworkConnectivityHint(&mut h) };
    if rc != 0 {
        return Err(ApiError::internal(format!("GetNetworkConnectivityHint failed: {rc}")));
    }
    let level = match h.ConnectivityLevel {
        NetworkConnectivityLevelHintNone => Connectivity::None,
        NetworkConnectivityLevelHintLocalAccess => Connectivity::LocalOnly,
        NetworkConnectivityLevelHintConstrainedInternetAccess => Connectivity::Constrained,
        NetworkConnectivityLevelHintInternetAccess => Connectivity::Internet,
        _ => Connectivity::Unknown,
    };
    let metered = matches!(h.ConnectivityCost, NetworkConnectivityCostHintFixed | NetworkConnectivityCostHintVariable)
        || h.OverDataLimit
        || h.Roaming;
    Ok(ConnectivityHint { level, metered })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_are_plausible() {
        let m = memory().unwrap();
        assert!(m.total_mb > 1024 && m.available_mb <= m.total_mb, "{m:?}");
        assert!(m.commit_limit_mb >= m.total_mb / 2, "{m:?}");
        connectivity().unwrap();
        // Desktops may have no thermal zone; when there is one, it reads sensibly.
        if let Ok(t) = temperatures() {
            for z in &t {
                assert!((-40.0..=150.0).contains(&z.celsius), "{z:?}");
            }
            // A second read reuses the open query.
            temperatures().unwrap();
        }
    }
}
