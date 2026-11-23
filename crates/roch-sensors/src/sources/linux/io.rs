use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::engine::{Batch, Source};
use crate::model::{DeviceInfo, DeviceKind, Group, SensorInfo};

use super::{paths, text};

const MIB: f64 = 1024.0 * 1024.0;

pub(super) struct Drive {
    pub(super) device: DeviceInfo,
    pub(super) controller: PathBuf,
    stat: PathBuf,
}

pub(super) fn drives() -> Vec<Drive> {
    paths("/sys/block")
        .into_iter()
        .filter_map(|path| {
            // Physical disks link to a device. Loop, zram and device-mapper have no such link.
            let controller = path.join("device").canonicalize().ok()?;
            let name = path.file_name()?.to_str()?;
            let model = text(controller.join("model")).unwrap_or_else(|| name.to_owned());
            Some(Drive {
                device: DeviceInfo::new(format!("drive/{name}"), DeviceKind::Drive, model).detail(name),
                controller,
                stat: path.join("stat"),
            })
        })
        .collect()
}

fn disk_stats(text: &str) -> Option<[u64; 3]> {
    let mut fields = text.split_whitespace();
    let read = fields.nth(2)?.parse().ok()?;
    let written = fields.nth(3)?.parse().ok()?;
    let busy_ms = fields.nth(2)?.parse().ok()?;
    Some([read, written, busy_ms])
}

fn rate(current: u64, previous: u64, seconds: f64) -> f64 {
    if seconds > 0.0 {
        current.checked_sub(previous).map_or(f64::NAN, |delta| delta as f64 / seconds)
    } else {
        f64::NAN
    }
}

#[derive(Default)]
pub(super) struct Drives {
    previous: HashMap<String, ([u64; 3], Instant)>,
}

impl Source for Drives {
    fn name(&self) -> &str {
        "Drives"
    }

    fn poll(&mut self, out: &mut Batch) {
        let drives = drives();
        out.status(format!("{} block device(s) under /sys/block.", drives.len()));
        let mut current = HashMap::new();
        for drive in drives {
            let Some(sample) = text(&drive.stat).as_deref().and_then(disk_stats) else { continue };
            let now = Instant::now();
            let values = self.previous.get(&drive.device.id).map_or([f64::NAN; 3], |&(prev, at)| {
                let time = now.duration_since(at).as_secs_f64();
                [
                    rate(sample[0], prev[0], time) * 512.0 / MIB,
                    rate(sample[1], prev[1], time) * 512.0 / MIB,
                    (rate(sample[2], prev[2], time) / 10.0).clamp(0.0, 100.0),
                ]
            });
            for ((key, group, label, unit), value) in [
                ("read", Group::Throughput, "Read Rate", "MB/s"),
                ("write", Group::Throughput, "Write Rate", "MB/s"),
                ("busy", Group::Load, "Disk Activity", "%"),
            ]
            .into_iter()
            .zip(values)
            {
                let sensor = SensorInfo::new(&drive.device.id, key, group, label, unit, 1)
                    .source(drive.stat.display().to_string());
                out.put(&sensor, value);
            }
            current.insert(drive.device.id.clone(), (sample, now));
            out.device(drive.device);
        }
        self.previous = current;
    }
}

fn network_stats(line: &str) -> Option<(&str, [u64; 2])> {
    let (name, columns) = line.rsplit_once(':')?;
    let name = name.trim();
    if name.is_empty() || name == "lo" {
        return None;
    }
    let mut fields = columns.split_whitespace();
    let receive = fields.next()?.parse().ok()?;
    let transmit = fields.nth(7)?.parse().ok()?;
    Some((name, [receive, transmit]))
}

pub(super) fn network_device(name: &str) -> DeviceInfo {
    DeviceInfo::new(format!("net/{name}"), DeviceKind::Network, name)
}

#[derive(Default)]
pub(super) struct Network {
    interfaces: HashMap<String, NetworkSample>,
}

struct NetworkSample {
    counters: [u64; 2],
    total: [u64; 2],
    time: Instant,
}

impl Source for Network {
    fn name(&self) -> &str {
        "Network"
    }

    fn poll(&mut self, out: &mut Batch) {
        let Some(contents) = text("/proc/net/dev") else {
            out.status("Network: /proc/net/dev is unavailable.");
            self.interfaces.clear();
            return;
        };
        let now = Instant::now();
        let mut current = HashMap::new();
        for (name, counters) in contents.lines().filter_map(network_stats) {
            let dev = network_device(name);
            let previous = self.interfaces.get(name);
            let total = std::array::from_fn(|i| {
                previous.map_or(0, |p| p.total[i].saturating_add(counters[i].saturating_sub(p.counters[i])))
            });
            let values: [f64; 2] = std::array::from_fn(|i| {
                previous.map_or(f64::NAN, |p| {
                    rate(counters[i], p.counters[i], now.duration_since(p.time).as_secs_f64()) / MIB
                })
            });
            for ((key, label), value) in [("rx", "Download Rate"), ("tx", "Upload Rate")].into_iter().zip(values) {
                out.put(
                    &SensorInfo::new(&dev.id, key, Group::Throughput, label, "MB/s", 2).source("/proc/net/dev"),
                    value,
                );
            }
            for ((key, label), value) in [("rx/total", "Downloaded"), ("tx/total", "Uploaded")].into_iter().zip(total) {
                out.put(
                    &SensorInfo::new(&dev.id, key, Group::Data, label, "MB", 1).source("/proc/net/dev"),
                    value as f64 / MIB,
                );
            }
            current.insert(name.to_owned(), NetworkSample { counters, total, time: now });
            out.device(dev);
        }
        out.status(format!("{} interface(s) in /proc/net/dev.", current.len()));
        self.interfaces = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_and_network_use_the_expected_kernel_fields() {
        assert_eq!(disk_stats("1 2 3 4 5 6 7 8 9 10 11"), Some([3, 7, 10]));
        assert_eq!(disk_stats("1 2"), None);
        assert_eq!(network_stats("enp1s0: 10 2 3 4 5 6 7 8 90 10"), Some(("enp1s0", [10, 90])));
        assert_eq!(network_stats("lo: 10 2 3 4 5 6 7 8 90 10"), None);
        assert_eq!(rate(30, 10, 2.0), 10.0);
        assert!(rate(5, 10, 2.0).is_nan());
    }
}
