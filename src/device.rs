//! Raw HID transport. Linux uses /dev/hidraw directly, Windows uses hidapi.
//!
//! Both backends: `write` takes a 65-byte buffer whose first byte is the report
//! ID (always 0 for this mouse); `read_timeout` returns the 64-byte report
//! *without* a report-ID byte.

use std::io;

pub const VID: u16 = 0x0B05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    pub pid: u16,
    pub name: &'static str,
    pub battery: bool,
}

/// Ordered by preference: wired connections first.
pub const MODELS: &[Model] = &[
    Model { pid: 0x195E, name: "ROG Keris Wireless (USB cable)", battery: true },
    Model { pid: 0x1A57, name: "ROG Keris EVA Edition (USB cable)", battery: true },
    Model { pid: 0x195C, name: "ROG Keris (wired)", battery: false },
    Model { pid: 0x1960, name: "ROG Keris Wireless (2.4 GHz)", battery: true },
    Model { pid: 0x1A59, name: "ROG Keris EVA Edition (2.4 GHz)", battery: true },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub model: Model,
    pub path: String,
}

fn rank(d: &DeviceInfo) -> usize {
    MODELS.iter().position(|m| m.pid == d.model.pid).unwrap_or(usize::MAX)
}

/// Lists every supported mouse on the configuration interface (interface 0).
pub fn enumerate() -> Vec<DeviceInfo> {
    let mut v = imp::enumerate();
    v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.path.cmp(&b.path)));
    v.dedup_by(|a, b| a.path == b.path);
    v
}

pub use imp::Hid;

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    pub struct Hid(File);

    pub fn enumerate() -> Vec<DeviceInfo> {
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir("/sys/class/hidraw") else { return out };
        for e in dir.flatten() {
            let sys = e.path();
            let Ok(uevent) = fs::read_to_string(sys.join("device/uevent")) else { continue };
            // HID_ID=0003:00000B05:00001960
            let Some(id) = uevent.lines().find_map(|l| l.strip_prefix("HID_ID=")) else { continue };
            let mut it = id.split(':').skip(1);
            let (Some(v), Some(p)) = (it.next(), it.next()) else { continue };
            let (Ok(v), Ok(p)) = (u32::from_str_radix(v, 16), u32::from_str_radix(p, 16)) else {
                continue;
            };
            if v != VID as u32 {
                continue;
            }
            let Some(model) = MODELS.iter().find(|m| m.pid as u32 == p) else { continue };
            // device -> HID device; its parent is the USB interface
            let iface = fs::read_to_string(sys.join("device/../bInterfaceNumber"))
                .ok()
                .and_then(|s| u8::from_str_radix(s.trim(), 16).ok());
            if iface != Some(0) {
                continue;
            }
            out.push(DeviceInfo {
                model: *model,
                path: format!("/dev/{}", e.file_name().to_string_lossy()),
            });
        }
        out
    }

    impl Hid {
        pub fn open(path: &str) -> io::Result<Self> {
            match OpenOptions::new().read(true).write(true).open(path) {
                Ok(f) => Ok(Hid(f)),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Err(io::Error::new(
                    e.kind(),
                    format!(
                        "no permission to open {path}. Install the udev rule: \
                         sudo cp 70-rog-keris.rules /etc/udev/rules.d/ && \
                         sudo udevadm control --reload && sudo udevadm trigger, \
                         then replug the mouse"
                    ),
                )),
                Err(e) => Err(e),
            }
        }

        pub fn write(&self, data: &[u8]) -> io::Result<()> {
            (&self.0).write_all(data)
        }

        /// Returns Ok(0) on timeout.
        pub fn read_timeout(&self, buf: &mut [u8], ms: i32) -> io::Result<usize> {
            let mut pfd = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r < 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::Interrupted { Ok(0) } else { Err(e) };
            }
            if r == 0 {
                return Ok(0);
            }
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "device disconnected"));
            }
            (&self.0).read(buf)
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use hidapi::{HidApi, HidDevice};
    use std::cell::RefCell;
    use std::ffi::CString;

    thread_local! {
        static API: RefCell<Option<HidApi>> = const { RefCell::new(None) };
    }

    fn with_api<T>(f: impl FnOnce(&mut HidApi) -> io::Result<T>) -> io::Result<T> {
        API.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                *slot = Some(HidApi::new().map_err(err)?);
            }
            f(slot.as_mut().unwrap())
        })
    }

    fn err(e: hidapi::HidError) -> io::Error {
        io::Error::other(e.to_string())
    }

    pub struct Hid(HidDevice);

    pub fn enumerate() -> Vec<DeviceInfo> {
        with_api(|api| {
            api.refresh_devices().map_err(err)?;
            Ok(api
                .device_list()
                .filter(|d| d.vendor_id() == VID && d.interface_number() == 0)
                .filter_map(|d| {
                    let model = MODELS.iter().find(|m| m.pid == d.product_id())?;
                    Some(DeviceInfo { model: *model, path: d.path().to_string_lossy().into_owned() })
                })
                .collect())
        })
        .unwrap_or_default()
    }

    impl Hid {
        pub fn open(path: &str) -> io::Result<Self> {
            let c = CString::new(path).map_err(io::Error::other)?;
            with_api(|api| api.open_path(&c).map(Hid).map_err(err))
        }

        pub fn write(&self, data: &[u8]) -> io::Result<()> {
            self.0.write(data).map(|_| ()).map_err(err)
        }

        pub fn read_timeout(&self, buf: &mut [u8], ms: i32) -> io::Result<usize> {
            self.0.read_timeout(buf, ms).map_err(err)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("keris-ctl supports only Linux and Windows");
