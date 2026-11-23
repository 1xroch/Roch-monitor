use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::engine::{Batch, Source};
use crate::model::{DeviceInfo, Group, SensorInfo};
use crate::system::{SystemInfo, cpu_device};

use super::{number, paths, text};

const CPU: &str = "cpu";
const SYS_CPU: &str = "/sys/devices/system/cpu";

#[derive(Clone, Copy)]
struct Ticks {
    total: u64,
    busy: u64,
}

fn cpu_ticks(contents: &str) -> BTreeMap<Option<u32>, Ticks> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let cpu = fields.next()?.strip_prefix("cpu")?;
            let id = if cpu.is_empty() { None } else { Some(cpu.parse().ok()?) };
            // guest and guest_nice are already included in user and nice.
            let counters: Vec<u64> = fields.take(8).map(str::parse).collect::<Result<_, _>>().ok()?;
            if counters.len() < 4 {
                return None;
            }
            let total: u64 = counters.iter().sum();
            let idle = counters[3] + counters.get(4).copied().unwrap_or(0);
            Some((id, Ticks { total, busy: total - idle }))
        })
        .collect()
}

fn utilization(previous: Ticks, sample: Ticks) -> Option<f64> {
    let elapsed = sample.total.checked_sub(previous.total)?;
    let busy = sample.busy.checked_sub(previous.busy)?;
    (elapsed > 0).then(|| (100.0 * busy as f64 / elapsed as f64).min(100.0))
}

pub(super) struct Cpu {
    device: DeviceInfo,
    labels: BTreeMap<u32, String>,
    previous: BTreeMap<Option<u32>, Ticks>,
}

impl Cpu {
    pub(super) fn new(sys: &SystemInfo) -> Self {
        let labels = sys
            .topology
            .logical
            .iter()
            .filter_map(|cpu| {
                let label = sys.topology.label(cpu.group, cpu.number)?;
                Some((u32::from(cpu.group) * 64 + u32::from(cpu.number), label))
            })
            .collect();
        Self { device: cpu_device(sys), labels, previous: BTreeMap::new() }
    }
}

impl Source for Cpu {
    fn name(&self) -> &str {
        "CPU counters"
    }

    fn poll(&mut self, out: &mut Batch) {
        let Some(contents) = text("/proc/stat") else {
            self.previous.clear();
            out.status("CPU counters: /proc/stat is unavailable.");
            return;
        };
        let current = cpu_ticks(&contents);
        out.device(self.device.clone());
        out.status("CPU usage from /proc/stat and clocks from cpufreq.");
        let load = SensorInfo::new(CPU, "load/total", Group::Load, "Total CPU Usage", "%", 1).source("/proc/stat");
        let clock =
            SensorInfo::new(CPU, "clock/average", Group::Clocks, "Average CPU Clock", "MHz", 0).source("cpufreq");
        out.declare(load.clone());
        let mut clocks = Vec::new();
        for (&id, &ticks) in &current {
            let value = self.previous.get(&id).and_then(|&previous| utilization(previous, ticks)).unwrap_or(f64::NAN);
            let Some(id) = id else {
                out.value(load.key.clone(), value);
                continue;
            };
            let label = self.labels.get(&id).cloned().unwrap_or_else(|| format!("CPU {id}"));
            let thread_load = SensorInfo::new(CPU, format!("load/cpu{id}"), Group::Load, &label, "%", 1)
                .parent(&load.key)
                .source("/proc/stat");
            out.put(&thread_load, value);
            let frequency = format!("{SYS_CPU}/cpu{id}/cpufreq/scaling_cur_freq");
            if let Some(khz) = number::<u64>(&frequency).filter(|&v| v > 0) {
                let mhz = khz as f64 / 1000.0;
                let sensor = SensorInfo::new(CPU, format!("clock/cpu{id}"), Group::Clocks, label, "MHz", 0)
                    .parent(&clock.key)
                    .source(frequency);
                clocks.push((sensor, mhz));
            }
        }
        if !clocks.is_empty() {
            out.put(&clock, clocks.iter().map(|(_, mhz)| mhz).sum::<f64>() / clocks.len() as f64);
            for (sensor, mhz) in clocks {
                out.put(&sensor, mhz);
            }
        }
        self.previous = current;
    }
}

/// Package power from the kernel energy counter when the current user can read it.
pub(super) struct Power {
    packages: Vec<Package>,
}

struct Package {
    name: String,
    energy: PathBuf,
    range: u64,
    previous: Option<(Instant, u64)>,
}

impl Power {
    pub(super) fn detect(out: &mut Batch) -> Option<Self> {
        // Multiple RAPL control types can point to the same package. Use the native Intel driver
        // first, then its MMIO counterpart. Ignore subzones, whose energy overlaps with packages.
        let roots = paths("/sys/class/powercap");
        for prefix in ["intel-rapl:", "intel-rapl-mmio:", "amd-rapl:"] {
            let packages: Vec<_> = roots
                .iter()
                .filter_map(|path| {
                    let index = path.file_name()?.to_str()?.strip_prefix(prefix)?;
                    index.parse::<u32>().ok()?;
                    let name = text(path.join("name"))?;
                    let range = number(path.join("max_energy_range_uj"))?;
                    let energy = path.join("energy_uj");
                    let reading = number(&energy)?;
                    Some(Package { name, energy, range, previous: Some((Instant::now(), reading)) })
                })
                .collect();
            if !packages.is_empty() {
                return Some(Self { packages });
            }
        }
        out.status("CPU power: no readable package energy counters under /sys/class/powercap.");
        None
    }
}

impl Source for Power {
    fn name(&self) -> &str {
        "CPU power"
    }

    fn poll(&mut self, out: &mut Batch) {
        out.status("CPU package power from the kernel powercap energy counters.");
        for package in &mut self.packages {
            let sample = number::<u64>(&package.energy).map(|v| (Instant::now(), v));
            let value = sample
                .zip(package.previous)
                .and_then(|((now, current), (before, previous))| {
                    let micros = current
                        .checked_sub(previous)
                        .or_else(|| package.range.checked_sub(previous)?.checked_add(current))?;
                    let seconds = now.duration_since(before).as_secs_f64();
                    (seconds > 0.0).then(|| micros as f64 / 1e6 / seconds)
                })
                .unwrap_or(f64::NAN);
            let sensor = SensorInfo::new(
                CPU,
                format!("power/{}", package.name),
                Group::Power,
                format!("CPU {} Power", package.name),
                "W",
                1,
            )
            .source(package.energy.display().to_string());
            out.put(&sensor, value);
            package.previous = sample;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_load_handles_guest_time_and_counter_resets() {
        let before = cpu_ticks("cpu 10 0 10 80 0 0 0 0 3 0\ncpu3 2 1 1 6\nintr 200");
        let after = cpu_ticks("cpu 15 0 15 90 0 0 0 0 5 0\ncpu3 0 0 0 0");
        assert_eq!(utilization(before[&None], after[&None]), Some(50.0));
        assert_eq!(utilization(before[&Some(3)], after[&Some(3)]), None);
        assert_eq!(before.len(), 2);
    }
}
