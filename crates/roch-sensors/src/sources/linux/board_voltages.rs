//! Optional ITE voltage readings for the Z890 Tachyon ICE, using Roch Viewer's confirmed rail map.
//! The privileged subprocess publishes fixed-size samples. It accepts no register requests.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::Duration;

use crate::engine::{Batch, Factory, Source};
use crate::model::{DeviceInfo, DeviceKind, Group, SensorInfo};
use crate::system::SystemInfo;

use super::{paths, text};

pub const HELPER_ARG: &str = "--board-voltage-helper";
pub const SOURCE_NAME: &str = "Board voltages";
const BOARD: &str = "Z890 AORUS TACHYON ICE";
const VIEWER_ISA_LOCK: &str = ".rochviewer-Global_Access_ISABUS.HTP.Method.lock";
const FRAME_SIZE: usize = RAILS.len() + 1;

const PRIMARY: Chip = Chip { id: 0x8696, port: 0x2e, base: 0x0a40, name: "IT8696E", step: 0.012 };
const SECONDARY: Chip = Chip { id: 0x8695, port: 0x4e, base: 0x0b10, name: "IT87952E", step: 0.0109 };

struct Chip {
    id: u16,
    port: u16,
    base: u16,
    name: &'static str,
    step: f64,
}

struct Rail {
    key: &'static str,
    name: &'static str,
    chip: &'static Chip,
    register: u8,
    /// The board's resistor divider ahead of the input, 1.0 for none.
    divider: f64,
    range: (f64, f64),
    /// Where the register and divider come from, for the reading's source line.
    basis: &'static str,
}

const fn rail(key: &'static str, name: &'static str, chip: &'static Chip, register: u8, range: (f64, f64)) -> Rail {
    Rail { key, name, chip, register, divider: 1.0, range, basis: "Roch Viewer ITE map" }
}

/// A supply rail. The IT8696E's 0x21-0x23 and 0x27/0x28/0x2F inputs and dividers are Gigabyte's
/// usual ITE wiring and match the X870 TACHYON's map for this same chip. The ATX tolerance band
/// keeps a rail hidden if this board wires that input differently.
const fn supply(key: &'static str, name: &'static str, register: u8, divider: f64, range: (f64, f64)) -> Rail {
    Rail { key, name, chip: &PRIMARY, register, divider, range, basis: "Gigabyte ITE supply wiring" }
}

const RAILS: [Rail; 11] = [
    rail("vcore", "Vcore", &PRIMARY, 0x20, (0.4, 1.8)),
    rail("sa", "VCCSA (System Agent)", &PRIMARY, 0x26, (0.6, 1.7)),
    rail("vnnaon", "CPU VNNAON", &PRIMARY, 0x25, (0.4, 1.4)),
    rail("vdd2", "VDD2 (IMC VDD)", &SECONDARY, 0x21, (0.9, 1.6)),
    rail("vccio", "VCCIO", &SECONDARY, 0x24, (0.8, 1.8)),
    supply("12v", "+12V", 0x22, 6.0, (10.8, 13.2)),
    supply("5v", "+5V", 0x23, 2.5, (4.5, 5.5)),
    supply("3v3", "+3.3V", 0x21, 1.65, (3.0, 3.6)),
    supply("3vsb", "3VSB", 0x27, 2.0, (3.0, 3.6)),
    supply("vbat", "VBAT (CMOS battery)", 0x28, 2.0, (2.0, 3.6)),
    supply("avcc3", "AVCC3", 0x2F, 2.0, (3.0, 3.6)),
];

pub fn supported(sys: &SystemInfo) -> bool {
    sys.smbios.board_product.eq_ignore_ascii_case(BOARD)
}

pub fn kernel_owns_superio() -> bool {
    paths("/sys/class/hwmon")
        .iter()
        .any(|path| text(path.join("name")).is_some_and(|name| name.starts_with("it8") || name.starts_with("nct6")))
}

pub fn factory(sys: &SystemInfo) -> (String, Factory) {
    let sys = sys.clone();
    (SOURCE_NAME.to_owned(), Box::new(move |batch| super::boxed(BoardVoltages::start(&sys, batch))))
}

fn lock_path(uid: u32) -> PathBuf {
    Path::new("/run/user").join(uid.to_string()).join(VIEWER_ISA_LOCK)
}

struct BoardVoltages {
    device: DeviceInfo,
    child: Option<Child>,
    output: ChildStdout,
    frame: [u8; FRAME_SIZE],
    filled: usize,
    seen: [bool; RAILS.len()],
    status: &'static str,
}

impl BoardVoltages {
    fn start(sys: &SystemInfo, out: &mut Batch) -> Option<Self> {
        if kernel_owns_superio() {
            out.status("A kernel driver owns the Super I/O. See the kernel sensor readings.");
            return None;
        }
        match Self::launch(sys) {
            Ok(source) => Some(source),
            Err(error) => {
                out.status(format!("Could not start the board voltage helper: {error}"));
                None
            }
        }
    }

    fn launch(sys: &SystemInfo) -> io::Result<Self> {
        // Create the shared lock as the desktop user. The helper only opens it and takes a flock.
        // SAFETY: geteuid just returns the current effective UID.
        let uid = unsafe { libc::geteuid() };
        open_lock(uid, true)?;
        // The copy `rochmonitor setup` installed, while it is this build: polkit lets that one
        // start without a password.
        let exe = super::setup::helper_program()?;
        let mut command = if uid == 0 {
            Command::new(exe)
        } else {
            let mut command = Command::new("/usr/bin/pkexec");
            command.env_remove("SHELL").arg(exe);
            command
        };
        let mut child = command.arg(HELPER_ARG).stdin(Stdio::inherit()).stdout(Stdio::piped()).spawn()?;
        let output = child.stdout.take().expect("stdout was piped");
        let fd = output.as_raw_fd();
        // SAFETY: fd belongs to output. Only the monitor's read end is made nonblocking.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            device: DeviceInfo::new("board", DeviceKind::Board, sys.board_name()).detail("ITE IT8696E + IT87952E"),
            child: Some(child),
            output,
            frame: [0; FRAME_SIZE],
            filled: 0,
            seen: [false; RAILS.len()],
            status: "Waiting for administrator authorization.",
        })
    }

    fn sample(&mut self, out: &mut Batch) {
        self.status = match self.frame[0] {
            b'S' => "Live board rails from the ITE Super I/O. Hover a rail for its register.",
            b'B' => "Super I/O bus in use. Waiting for the next sample.",
            b'K' => "A kernel driver took ownership of the Super I/O. Raw voltage readings stopped.",
            b'I' => "The confirmed ITE chips did not answer at their expected ports.",
            b'E' => "The board voltage helper could not access the I/O ports or shared lock.",
            _ => "The board voltage helper sent an invalid sample.",
        };
        let mut any_visible = false;
        for (index, rail) in RAILS.iter().enumerate() {
            let value = if self.frame[0] == b'S' { rail.decode(self.frame[index + 1]) } else { None };
            self.seen[index] |= value.is_some();
            if self.seen[index] {
                any_visible = true;
                let divider = if rail.divider == 1.0 { String::new() } else { format!(" ×{}", rail.divider) };
                let info =
                    SensorInfo::new(&self.device.id, format!("volt/{}", rail.key), Group::Voltages, rail.name, "V", 3)
                        .source(format!("{} 0x{:02X}{divider} · {}", rail.chip.name, rail.register, rail.basis));
                out.put(&info, value.unwrap_or(f64::NAN));
            }
        }
        if any_visible {
            out.guest_device(self.device.clone());
        }
    }
}

impl Source for BoardVoltages {
    fn name(&self) -> &str {
        SOURCE_NAME
    }

    fn poll(&mut self, out: &mut Batch) {
        loop {
            match self.output.read(&mut self.frame[self.filled..]) {
                Ok(0) => {
                    if let Some(child) = &mut self.child
                        && let Ok(Some(exit)) = child.try_wait()
                    {
                        match exit.code() {
                            Some(126) => self.status = "Administrator authorization was dismissed.",
                            Some(127) => {
                                self.status = "Administrator authorization is unavailable. Run from a terminal with --board-voltages."
                            }
                            _ if self.status == "Waiting for administrator authorization." => {
                                self.status = "The voltage helper exited before returning readings.";
                            }
                            _ => {}
                        }
                    }
                    break;
                }
                Ok(read) => {
                    self.filled += read;
                    if self.filled == FRAME_SIZE {
                        self.sample(out);
                        self.filled = 0;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.status = "Lost connection to the board voltage helper.";
                    break;
                }
            }
        }
        out.status(self.status);
    }
}

impl Drop for BoardVoltages {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Closing stdout makes the elevated helper stop on its next write. Don't interrupt
            // it during a register transaction. Cancel pkexec only while authorization is pending.
            if !crate::hw::is_elevated() && self.status == "Waiting for administrator authorization." {
                let _ = child.kill();
            }
            let _ =
                std::thread::Builder::new().name("Roch Monitor: reap board voltage helper".into()).spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

impl Rail {
    fn decode(&self, raw: u8) -> Option<f64> {
        let volts = f64::from(raw) * self.chip.step * self.divider;
        (self.range.0..=self.range.1).contains(&volts).then_some(volts)
    }
}

fn open_lock(uid: u32, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path(uid))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid {
        return Err(io::Error::other("Invalid Roch Viewer ISA lock file"));
    }
    Ok(file)
}

fn write_port(io: &File, port: u16, byte: u8) -> io::Result<()> {
    io.write_all_at(&[byte], u64::from(port))
}

fn read_register(io: &File, port: u16, register: u8) -> io::Result<u8> {
    write_port(io, port, register)?;
    let mut value = [0];
    io.read_exact_at(&mut value, u64::from(port + 1))?;
    Ok(value[0])
}

impl Chip {
    fn matches(&self, io: &File) -> io::Result<bool> {
        let port = self.port;
        let result = (|| {
            for value in [0x87, 0x01, 0x55, if port == 0x2e { 0x55 } else { 0xaa }] {
                write_port(io, port, value)?;
            }
            let id = u16::from_be_bytes([read_register(io, port, 0x20)?, read_register(io, port, 0x21)?]);
            if id != self.id {
                return Ok(false);
            }
            write_port(io, port, 0x07)?;
            write_port(io, port + 1, 0x04)?;
            let base = u16::from_be_bytes([read_register(io, port, 0x60)?, read_register(io, port, 0x61)?]);
            Ok(base == self.base)
        })();
        let exit = write_port(io, port, 0x02).and_then(|()| write_port(io, port + 1, 0x02));
        result.and_then(|present| exit.map(|()| present))
    }
}

fn publish(io: &File, lock: &File, out: &mut impl Write) -> io::Result<()> {
    let mut identified = None;
    loop {
        let mut frame = [0; FRAME_SIZE];
        if kernel_owns_superio() {
            frame[0] = b'K';
        } else if lock.try_lock().is_ok() {
            let sample = (|| -> io::Result<()> {
                let (primary, secondary) = match identified {
                    Some(chips) => chips,
                    None => {
                        let chips = (PRIMARY.matches(io)?, SECONDARY.matches(io)?);
                        identified = Some(chips);
                        chips
                    }
                };
                frame[0] = if primary || secondary { b'S' } else { b'I' };
                for (index, rail) in RAILS.iter().enumerate() {
                    let present = if rail.chip.id == PRIMARY.id { primary } else { secondary };
                    if present {
                        frame[index + 1] = read_register(io, rail.chip.base + 5, rail.register)?;
                    }
                }
                Ok(())
            })();
            let _ = lock.unlock();
            if sample.is_err() {
                frame[0] = b'E';
            }
        } else {
            frame[0] = b'B';
        }
        out.write_all(&frame)?;
        out.flush()?;
        if !matches!(frame[0], b'S' | b'B') {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Entry point for the isolated, board-specific collector. All selectors and ports are constants.
pub fn serve() -> i32 {
    if !crate::hw::is_elevated()
        || !text("/sys/class/dmi/id/board_name").is_some_and(|name| name.eq_ignore_ascii_case(BOARD))
    {
        return 1;
    }
    let uid = std::env::var("PKEXEC_UID").ok().and_then(|uid| uid.parse().ok()).unwrap_or(0);
    let mut stdout = io::stdout().lock();
    if kernel_owns_superio() {
        let mut frame = [0; FRAME_SIZE];
        frame[0] = b'K';
        let _ = stdout.write_all(&frame);
        return 0;
    }
    let result = open_lock(uid, false)
        .and_then(|lock| OpenOptions::new().read(true).write(true).open("/dev/port").map(|io| (lock, io)));
    match result {
        Ok((lock, io)) => {
            let _ = publish(&io, &lock, &mut stdout);
            0
        }
        Err(_) => {
            let mut frame = [0; FRAME_SIZE];
            frame[0] = b'E';
            let _ = stdout.write_all(&frame);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_ite_chips_use_their_validated_voltage_steps() {
        assert!((RAILS[1].decode(109).unwrap() - 1.308).abs() < 1e-9);
        assert!((RAILS[3].decode(101).unwrap() - 1.1009).abs() < 1e-9);
        // 168 counts behind the 6:1 divider, as the X870 TACHYON's IT8696E reads its +12V.
        assert!((RAILS[5].decode(168).unwrap() - 12.096).abs() < 1e-9);
        for rail in &RAILS {
            assert_eq!(rail.decode(0), None);
            assert_eq!(rail.decode(255), None);
        }
    }
}
