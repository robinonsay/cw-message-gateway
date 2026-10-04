//! Finding the radio's serial port.
//!
//! The IC-7300's USB port is a Silicon Labs CP210x USB-to-UART bridge, USB vendor
//! 10C4h, product EA60h, which ICOM programs with a serial number naming the radio
//! (for example "IC-7300 03001234"). Each system names the port differently:
//!
//! - Linux: `/dev/ttyUSB0` (the number follows plug-in order), and a stable
//!   `/dev/serial/by-id/usb-Silicon_Labs_CP2102_..._IC-7300_...-if00-port0` link.
//! - macOS: a callout device `/dev/cu.<name>` and a dial-in device `/dev/tty.<name>`
//!   for the same port. A program starting a connection uses the `cu.` one (the
//!   `tty.` one is for answering an incoming modem call), so only `cu.` is listed.
//! - Windows: `COM3` and so on.
//!
//! Listing ports only reads what the system knows about its devices; nothing is
//! opened, so nothing is sent to any radio.

/// Silicon Labs' USB vendor id.
pub const CP210X_VID: u16 = 0x10C4;
/// The CP210x's default USB product id, as on the IC-7300.
pub const CP210X_PID: u16 = 0xEA60;

/// A serial port this computer has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// What to put in `station.serial_port`.
    pub path: String,
    /// Another name for the same port that does not change between reboots or with
    /// the order devices are plugged in, where the system has one.
    pub stable_path: Option<String>,
    pub usb: Option<UsbInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbInfo {
    pub vid: u16,
    pub pid: u16,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

/// How sure we are that a port is the IC-7300's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Match {
    No,
    /// A CP210x bridge: the IC-7300 has one, but so do many other devices.
    Cp210x,
    /// A CP210x whose USB strings name the IC-7300.
    Ic7300,
}

impl PortInfo {
    pub fn radio_match(&self) -> Match {
        let Some(u) = &self.usb else {
            return Match::No;
        };
        let names_radio = [&u.serial_number, &u.product, &u.manufacturer]
            .iter()
            .filter_map(|s| s.as_deref())
            .chain(self.stable_path.as_deref())
            .any(|s| {
                let s = s.to_ascii_uppercase();
                s.contains("IC-7300") || s.contains("IC7300")
            });
        match (u.vid, u.pid) {
            (CP210X_VID, CP210X_PID) if names_radio => Match::Ic7300,
            (CP210X_VID, CP210X_PID) => Match::Cp210x,
            _ => Match::No,
        }
    }
}

/// The serial ports this computer has, most likely radio first.
pub fn list() -> std::io::Result<Vec<PortInfo>> {
    // The Linux scan without libudev panics where /sys/class/tty is missing (some
    // containers); report that as an error instead.
    let ports = std::panic::catch_unwind(serialport::available_ports)
        .map_err(|_| std::io::Error::other("the system's serial port list could not be read"))?
        .map_err(std::io::Error::other)?;
    let mut out: Vec<PortInfo> = ports
        .into_iter()
        .filter(|p| listed(&p.port_name))
        .map(|p| PortInfo {
            stable_path: stable_path(&p.port_name),
            usb: match p.port_type {
                serialport::SerialPortType::UsbPort(u) => Some(UsbInfo {
                    vid: u.vid,
                    pid: u.pid,
                    serial_number: u.serial_number,
                    manufacturer: u.manufacturer,
                    product: u.product,
                }),
                _ => None,
            },
            path: p.port_name,
        })
        .collect();
    out.sort_by(|a, b| {
        b.radio_match()
            .cmp(&a.radio_match())
            .then_with(|| a.path.cmp(&b.path))
    });
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

/// Whether a port the system reports is one to offer.
fn listed(path: &str) -> bool {
    // macOS reports each port twice; see the module notes.
    !(cfg!(target_os = "macos") && is_macos_dialin(path))
}

/// A macOS dial-in device (`/dev/tty.*`), where `/dev/cu.*` is the one to use.
pub fn is_macos_dialin(path: &str) -> bool {
    path.starts_with("/dev/tty.")
}

/// On Linux, the `/dev/serial/by-id` link to `path`.
#[cfg(target_os = "linux")]
fn stable_path(path: &str) -> Option<String> {
    let target = std::fs::canonicalize(path).ok()?;
    std::fs::read_dir("/dev/serial/by-id")
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|link| std::fs::canonicalize(link).is_ok_and(|t| t == target))
        .map(|link| link.display().to_string())
}

/// macOS and Windows have no second name: theirs is kept from one plug-in to the
/// next, though it can change when the radio goes into another USB socket.
#[cfg(not(target_os = "linux"))]
fn stable_path(_path: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(path: &str, usb: Option<(u16, u16, &str)>) -> PortInfo {
        PortInfo {
            path: path.into(),
            stable_path: None,
            usb: usb.map(|(vid, pid, serial)| UsbInfo {
                vid,
                pid,
                serial_number: Some(serial.into()),
                manufacturer: Some("Silicon Labs".into()),
                product: Some("CP2102 USB to UART Bridge Controller".into()),
            }),
        }
    }

    #[test]
    fn recognises_the_radio() {
        let radio = port("COM3", Some((0x10C4, 0xEA60, "IC-7300 03001234")));
        assert_eq!(radio.radio_match(), Match::Ic7300);
        let bridge = port("/dev/cu.usbserial-0001", Some((0x10C4, 0xEA60, "0001")));
        assert_eq!(bridge.radio_match(), Match::Cp210x);
        let other = port("/dev/ttyACM0", Some((0x2341, 0x0043, "IC-7300")));
        assert_eq!(other.radio_match(), Match::No);
        assert_eq!(port("/dev/ttyS0", None).radio_match(), Match::No);
        let mut by_id = port("/dev/ttyUSB0", Some((0x10C4, 0xEA60, "")));
        by_id.stable_path = Some(
            "/dev/serial/by-id/usb-Silicon_Labs_CP2102_USB_to_UART_Bridge_Controller_IC-7300_03001234-if00-port0"
                .into(),
        );
        assert_eq!(by_id.radio_match(), Match::Ic7300);
    }

    #[test]
    fn macos_dialin_devices_are_spotted() {
        assert!(is_macos_dialin("/dev/tty.usbserial-0001"));
        assert!(!is_macos_dialin("/dev/cu.usbserial-0001"));
        assert!(!is_macos_dialin("/dev/ttyUSB0"));
    }

    #[test]
    fn listing_does_not_fail() {
        // Whatever this machine has (often nothing), listing must not error or open
        // anything.
        let ports = list().unwrap_or_default();
        assert!(ports
            .windows(2)
            .all(|w| w[0].radio_match() >= w[1].radio_match()));
    }
}
