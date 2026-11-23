//! Linux sources use kernel-published readings from procfs and sysfs.

pub mod board_voltages;
mod cpu;
mod hwmon;
mod io;

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::engine::Factory;
use crate::system::SystemInfo;

use super::{Options, boxed, gpu, system_usage};

pub fn factories(sys: &SystemInfo, options: Options) -> Vec<(String, Factory)> {
    let mut sources: Vec<(String, Factory)> = Vec::new();
    let mut add = |name: &str, factory: Factory| sources.push((name.into(), factory));
    let system = system_usage::SystemUsage::new(sys);
    add("System", Box::new(move |_| boxed(Some(system))));
    let cpu = cpu::Cpu::new(sys);
    add("CPU counters", Box::new(move |_| boxed(Some(cpu))));
    if options.hardware || options.drives || options.gpu || options.network {
        let sys = sys.clone();
        add("Kernel sensors", Box::new(move |batch| boxed(hwmon::Hwmon::detect(&sys, options, batch))));
    }
    if options.hardware {
        add("CPU power", Box::new(|batch| boxed(cpu::Power::detect(batch))));
    }
    if options.gpu {
        add("Graphics", Box::new(|batch| boxed(gpu::Gpus::detect(batch))));
    }
    if options.drives {
        add("Drives", Box::new(|_| boxed(Some(io::Drives::default()))));
    }
    if options.network {
        add("Network", Box::new(|_| boxed(Some(io::Network::default()))));
    }
    if options.hardware && options.board_voltages && board_voltages::supported(sys) {
        sources.push(board_voltages::factory(sys));
    }
    sources
}

fn text(path: impl AsRef<Path>) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn number<T: FromStr>(path: impl AsRef<Path>) -> Option<T> {
    text(path)?.parse().ok()
}

fn paths(dir: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path()).collect();
    entries.sort_unstable();
    entries
}
