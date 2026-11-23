//! Memory use, the way HWiNFO's System section shows it. No driver: GlobalMemoryStatusEx.

use crate::engine::{Batch, Source};
use crate::model::{DeviceInfo, DeviceKind, Group, SensorInfo};
use crate::system::SystemInfo;

const DEV: &str = "system";

pub fn system_device(sys: &SystemInfo) -> DeviceInfo {
    let name = if sys.computer.is_empty() { "System".to_owned() } else { sys.computer.clone() };
    DeviceInfo::new(DEV, DeviceKind::System, name).detail(sys.os.clone())
}

pub struct SystemUsage {
    device: DeviceInfo,
}

impl SystemUsage {
    pub fn new(sys: &SystemInfo) -> SystemUsage {
        SystemUsage { device: system_device(sys) }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Memory {
    pub load: f64,
    pub total_mb: f64,
    pub available_mb: f64,
    pub commit_limit_mb: f64,
    pub committed_mb: f64,
    pub swap_mb: Option<(f64, f64)>,
}

#[cfg(windows)]
pub fn memory() -> Option<Memory> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: dwLength set as the API requires; the struct is plain data.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    const MB: f64 = 1024.0 * 1024.0;
    Some(Memory {
        load: m.dwMemoryLoad as f64,
        total_mb: m.ullTotalPhys as f64 / MB,
        available_mb: m.ullAvailPhys as f64 / MB,
        commit_limit_mb: m.ullTotalPageFile as f64 / MB,
        committed_mb: m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile) as f64 / MB,
        swap_mb: None,
    })
}

#[cfg(target_os = "linux")]
pub fn memory() -> Option<Memory> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(target_os = "linux")]
fn parse_meminfo(text: &str) -> Option<Memory> {
    let fields: std::collections::HashMap<&str, u64> = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key, value.split_whitespace().next()?.parse().ok()?))
        })
        .collect();
    let mb = |key| fields.get(key).map(|&kib| kib as f64 / 1024.0);
    let total_mb = mb("MemTotal").filter(|&v| v > 0.0)?;
    let available_mb = mb("MemAvailable").or_else(|| mb("MemFree"))?.min(total_mb);
    let swap_mb =
        mb("SwapTotal").filter(|&v| v > 0.0).zip(mb("SwapFree")).map(|(total, free)| (total, free.min(total)));
    Some(Memory {
        load: (total_mb - available_mb) / total_mb * 100.0,
        total_mb,
        available_mb,
        commit_limit_mb: mb("CommitLimit").unwrap_or(f64::NAN),
        committed_mb: mb("Committed_AS").unwrap_or(f64::NAN),
        swap_mb,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn memory() -> Option<Memory> {
    None
}

impl Source for SystemUsage {
    fn name(&self) -> &str {
        "System"
    }

    fn poll(&mut self, out: &mut Batch) {
        out.device(self.device.clone());
        let source = if cfg!(target_os = "linux") { "/proc/meminfo" } else { "GlobalMemoryStatusEx" };
        let Some(m) = memory() else {
            out.status(format!("System: no memory readings from {source}."));
            return;
        };
        out.status(format!("System: memory use from {source}."));
        let s = |key: &str, label: &str, unit: &str, decimals: u8| {
            SensorInfo::new(DEV, key, Group::Memory, label, unit, decimals)
        };
        let used = m.total_mb - m.available_mb;
        out.put(&s("phys/used", "Physical Memory Used", "MB", 0), used);
        out.put(&s("phys/available", "Physical Memory Available", "MB", 0), m.available_mb);
        out.put(&SensorInfo::new(DEV, "phys/load", Group::Load, "Physical Memory Load", "%", 1), m.load);
        if m.commit_limit_mb > 0.0 && m.committed_mb.is_finite() {
            out.put(&s("virt/committed", "Virtual Memory Committed", "MB", 0), m.committed_mb);
            out.put(
                &s("virt/available", "Virtual Memory Available", "MB", 0),
                (m.commit_limit_mb - m.committed_mb).max(0.0),
            );
            out.put(
                &SensorInfo::new(DEV, "virt/load", Group::Load, "Virtual Memory Load", "%", 1),
                m.committed_mb / m.commit_limit_mb * 100.0,
            );
        }
        if let Some((total, free)) = m.swap_mb {
            out.put(&s("swap/used", "Swap Used", "MB", 0), total - free);
            out.put(&s("swap/free", "Swap Available", "MB", 0), free);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn this_machine_reports_its_memory() {
        let m = super::memory().unwrap();
        assert!(m.total_mb > 1024.0);
        assert!(m.available_mb <= m.total_mb);
    }
}
