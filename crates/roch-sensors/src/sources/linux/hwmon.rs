//! Discover the channels exported by Linux hwmon and preserve their kernel labels.

use std::path::{Path, PathBuf};

use crate::engine::{Batch, Source};
use crate::model::{DeviceInfo, DeviceKind, Group, SensorInfo};
use crate::sources::Options;
use crate::system::{SystemInfo, cpu_device};

use super::{io, number, paths, text};

pub(super) struct Hwmon {
    chips: Vec<Chip>,
}

struct Chip {
    device: DeviceInfo,
    runtime_status: Option<PathBuf>,
    readings: Vec<Reading>,
}

struct Reading {
    info: SensorInfo,
    path: PathBuf,
    channel: Channel,
    seen: bool,
}

#[derive(Clone, Copy)]
struct Channel {
    group: Group,
    name: &'static str,
    unit: &'static str,
    scale: f64,
    decimals: u8,
}

fn channel(file: &str) -> Option<(Channel, &str, &str)> {
    let (base, suffix) = file.split_once('_').unwrap_or((file, ""));
    let (kind, index) = base.split_at(base.find(|c: char| c.is_ascii_digit())?);
    if !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (group, name, unit, scale, decimals) = match (kind, suffix) {
        ("temp", "input") => (Group::Temperatures, "Temperature", "°C", 0.001, 1),
        ("in", "input") => (Group::Voltages, "Voltage", "V", 0.001, 3),
        ("curr", "input") => (Group::Current, "Current", "A", 0.001, 2),
        ("power", "input" | "average") => (Group::Power, "Power", "W", 0.000_001, 1),
        ("fan", "input") => (Group::Fans, "Fan", "RPM", 1.0, 0),
        ("pwm", "") => (Group::FanDuty, "Fan duty", "%", 100.0 / 255.0, 1),
        ("freq", "input") => (Group::Clocks, "Clock", "MHz", 0.000_001, 0),
        _ => return None,
    };
    Some((Channel { group, name, unit, scale, decimals }, base, index))
}

impl Channel {
    fn decode(self, raw: i64) -> Option<f64> {
        let value = raw as f64 * self.scale;
        let valid = match self.group {
            Group::Temperatures => (-100.0..=200.0).contains(&value),
            Group::Fans => (0.0..=100_000.0).contains(&value),
            Group::FanDuty => (0..=255).contains(&raw),
            _ => true,
        };
        valid.then_some(value)
    }
}

fn settings_device(
    name: &str,
    path: &Path,
    id: &str,
    sys: &SystemInfo,
    options: Options,
    drives: &[io::Drive],
) -> Option<DeviceInfo> {
    match name {
        "amdgpu" => None, // The AMDGPU telemetry source already owns these channels.
        "coretemp" | "k10temp" | "zenpower" | "cpu_thermal" | "peci_cputemp" | "fam15h_power" => {
            options.hardware.then(|| cpu_device(sys))
        }
        "spd5118" | "jc42" => options.hardware.then(|| {
            let bus_address = path.file_name().and_then(|name| name.to_str());
            let label = bus_address.map_or_else(|| "DIMM sensor".into(), |address| format!("DIMM sensor ({address})"));
            DeviceInfo::new(format!("dimm/{id}"), DeviceKind::Dimm, label).detail(name)
        }),
        "nvme" | "drivetemp" => options.drives.then(|| {
            drives
                .iter()
                .find(|d| d.controller == path)
                .map(|drive| drive.device.clone())
                .unwrap_or_else(|| DeviceInfo::new(format!("drive/{id}"), DeviceKind::Drive, name))
        }),
        _ if path.join("drm").is_dir() => options.gpu.then(|| {
            let pci = path.file_name().and_then(|name| name.to_str()).unwrap_or(name);
            DeviceInfo::new(format!("gpu/{id}"), DeviceKind::Gpu, format!("{name} ({pci})")).detail("Linux hwmon")
        }),
        _ => {
            // PHY sensors can sit below a controller's MDIO bus instead of on the controller.
            // `/sys/devices/virtual` is no controller: its `net` holds every virtual interface
            // (docker0, bridges), and the ACPI thermal zone sits below it.
            if let Some(net) = path
                .ancestors()
                .take_while(|parent| parent.starts_with("/sys/devices"))
                .filter(|parent| *parent != Path::new("/sys/devices/virtual"))
                .map(|parent| parent.join("net"))
                .find(|net| net.is_dir())
            {
                return options.network.then(|| {
                    let interfaces = paths(net);
                    if let [interface] = interfaces.as_slice()
                        && let Some(interface) = interface.file_name().and_then(|name| name.to_str())
                    {
                        return io::network_device(interface);
                    }
                    DeviceInfo::new(format!("net/{id}"), DeviceKind::Network, name)
                });
            }
            options
                .hardware
                .then(|| DeviceInfo::new(format!("board/{id}"), DeviceKind::Board, sys.board_name()).detail(name))
        }
    }
}

impl Hwmon {
    pub(super) fn detect(sys: &SystemInfo, options: Options, out: &mut Batch) -> Option<Self> {
        let drives = if options.drives { io::drives() } else { Vec::new() };
        let chips: Vec<_> = paths("/sys/class/hwmon")
            .into_iter()
            .filter_map(|entry| {
                let name = text(entry.join("name"))?;
                let path = entry.join("device").canonicalize().or_else(|_| entry.canonicalize()).ok()?;
                let stable_path = path.strip_prefix("/sys/devices").unwrap_or(&path);
                let id = format!("{name}/{}", stable_path.display());
                let device = settings_device(&name, &path, &id, sys, options, &drives)?;
                let runtime_status = matches!(device.kind, DeviceKind::Drive | DeviceKind::Gpu)
                    .then(|| path.join("power/runtime_status"));
                let mut readings: Vec<_> = paths(&entry)
                    .into_iter()
                    .filter_map(|path| {
                        let file = path.file_name()?.to_str()?;
                        let (channel, base, index) = channel(file)?;
                        let mut label = text(entry.join(format!("{base}_label")))
                            .unwrap_or_else(|| format!("{} {index}", channel.name));
                        if file.ends_with("_average") {
                            label.push_str(" Average");
                        }
                        let info = SensorInfo::new(
                            &device.id,
                            format!("hwmon/{id}/{file}"),
                            channel.group,
                            label,
                            channel.unit,
                            channel.decimals,
                        )
                        .source(format!("{name} · {file}"));
                        Some((index.parse::<u32>().ok()?, Reading { info, path, channel, seen: false }))
                    })
                    .collect();
                readings.sort_by_key(|(index, reading)| (reading.channel.group, *index));
                let readings: Vec<_> = readings.into_iter().map(|(_, reading)| reading).collect();
                (!readings.is_empty()).then_some(Chip { device, runtime_status, readings })
            })
            .collect();
        if chips.is_empty() {
            out.status("Kernel sensors: no matching channels under /sys/class/hwmon.");
            return None;
        }
        Some(Self { chips })
    }
}

impl Source for Hwmon {
    fn name(&self) -> &str {
        "Kernel sensors"
    }

    fn poll(&mut self, out: &mut Batch) {
        out.status(format!("{} hwmon device(s) with matching channels.", self.chips.len()));
        for chip in &mut self.chips {
            let asleep = chip
                .runtime_status
                .as_ref()
                .and_then(text)
                .is_some_and(|s| matches!(s.as_str(), "suspended" | "suspending" | "resuming"));
            let mut show_device = false;
            for reading in &mut chip.readings {
                let value =
                    if asleep { None } else { number(&reading.path).and_then(|raw| reading.channel.decode(raw)) };
                reading.seen |= value.is_some();
                if reading.seen {
                    out.put(&reading.info, value.unwrap_or(f64::NAN));
                    show_device = true;
                }
            }
            if show_device {
                out.guest_device(chip.device.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hwmon_channels_use_the_kernel_units_and_skip_controls() {
        assert_eq!(channel("temp7_input").and_then(|(c, _, _)| c.decode(45_250)), Some(45.25));
        assert_eq!(channel("in0_input").and_then(|(c, _, _)| c.decode(1200)), Some(1.2));
        assert_eq!(channel("power2_average").and_then(|(c, _, _)| c.decode(3_500_000)), Some(3.5));
        assert_eq!(channel("pwm3").and_then(|(c, _, _)| c.decode(255)), Some(100.0));
        assert!(channel("pwm3_enable").is_none());
        assert!(channel("temp7_crit").is_none());
    }
}
