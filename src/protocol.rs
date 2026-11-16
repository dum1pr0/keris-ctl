//! ROG Keris Wireless HID protocol.
//! Ported from G-Helper: app/Peripherals/Mouse/AsusMouse.cs + Models/KerisWireless.cs
//!
//! Packets are 65 bytes: [report id 0x00, cmd, sub, ...]. Responses echo the
//! command bytes. Indices below match the G-Helper code (byte 0 = report id).

use crate::device::{DeviceInfo, Hid};
use std::fmt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const PKT: usize = 65;
const TIMEOUT: Duration = Duration::from_millis(300);

pub const DPI_SLOTS: usize = 4;
pub const DPI_MIN: u16 = 100;
pub const DPI_MAX: u16 = 16_000;
pub const DPI_STEP: u16 = 100;
pub const PROFILES: u8 = 3;
pub const MAX_BRIGHTNESS: u8 = 4;
pub const POLLING_RATES: [u16; 4] = [125, 250, 500, 1000];
pub const DEBOUNCE_MS: [u8; 6] = [12, 16, 20, 24, 28, 32];
pub const LOW_BATTERY: [u8; 3] = [0, 25, 50];
/// (device byte, minutes; 0 = never)
pub const POWER_OFF: [(u8, u8); 6] = [(0, 1), (1, 2), (2, 3), (3, 5), (4, 10), (0xFF, 0)];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Timeout,
    /// Mouse answered FF AA: command not supported by firmware
    Unsupported,
    /// Dongle answered with an empty packet: mouse is off / asleep
    Asleep,
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Timeout => write!(f, "mouse did not respond (timeout)"),
            Error::Unsupported => write!(f, "mouse firmware rejected the command"),
            Error::Asleep => write!(f, "mouse is asleep or turned off (move it to wake it up)"),
            Error::Invalid(s) => write!(f, "{s}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Lighting
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Static,
    Breathing,
    ColorCycle,
    React,
    Battery,
    Off,
}

impl Mode {
    pub const ALL: [Mode; 6] =
        [Mode::Static, Mode::Breathing, Mode::ColorCycle, Mode::React, Mode::Battery, Mode::Off];

    fn byte(self) -> u8 {
        match self {
            Mode::Static => 0x00,
            Mode::Breathing => 0x01,
            Mode::ColorCycle => 0x02,
            Mode::React => 0x04,
            Mode::Battery => 0x06,
            Mode::Off => 0xFF,
        }
    }

    fn from_byte(b: u8) -> Mode {
        match b {
            0x00 => Mode::Static,
            0x01 => Mode::Breathing,
            0x02 => Mode::ColorCycle,
            0x04 => Mode::React,
            0x06 => Mode::Battery,
            _ => Mode::Off,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Mode::Static => "static",
            Mode::Breathing => "breathing",
            Mode::ColorCycle => "cycle",
            Mode::React => "react",
            Mode::Battery => "battery",
            Mode::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Static => "Static",
            Mode::Breathing => "Breathing",
            Mode::ColorCycle => "Color cycle",
            Mode::React => "Reactive",
            Mode::Battery => "Battery level",
            Mode::Off => "Off",
        }
    }

    pub fn has_color(self) -> bool {
        matches!(self, Mode::Static | Mode::Breathing | Mode::React)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Logo = 0,
    Wheel = 1,
    All = 3,
}

impl Zone {
    pub fn label(self) -> &'static str {
        match self {
            Zone::Logo => "Logo",
            Zone::Wheel => "Scroll wheel",
            Zone::All => "Both",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lighting {
    pub mode: Mode,
    /// 0..=MAX_BRIGHTNESS
    pub brightness: u8,
    pub color: [u8; 3],
}

impl Default for Lighting {
    fn default() -> Self {
        Lighting { mode: Mode::Static, brightness: 2, color: [255, 0, 0] }
    }
}

// ---------------------------------------------------------------------------
// Button bindings
// ---------------------------------------------------------------------------

/// Physical buttons: (source code, label)
pub const BUTTONS: [(u16, &str); 8] = [
    (0x01F0, "Left click"),
    (0x01F1, "Right click"),
    (0x01F2, "Wheel click"),
    (0x01E4, "Side back"),
    (0x01E5, "Side forward"),
    (0x01E6, "DPI button"),
    (0x01E8, "Scroll up"),
    (0x01E9, "Scroll down"),
];

pub struct Action {
    pub code: u16,
    /// short id used by the CLI
    pub id: String,
    pub label: String,
}

/// All actions a button can be bound to.
pub fn actions() -> &'static [Action] {
    static A: OnceLock<Vec<Action>> = OnceLock::new();
    A.get_or_init(|| {
        let mut v = Vec::new();
        let mut add = |code: u16, id: &str, label: &str| {
            v.push(Action { code, id: id.to_string(), label: label.to_string() })
        };
        // mouse
        add(0x01F0, "left", "Left click");
        add(0x01F1, "right", "Right click");
        add(0x01F2, "middle", "Middle click");
        add(0x01E3, "double", "Double click");
        add(0x01E4, "back", "Back");
        add(0x01E5, "forward", "Forward");
        add(0x01E6, "dpi", "DPI switch");
        add(0x01E7, "focus", "Target focus");
        add(0x01E8, "scrollup", "Scroll up");
        add(0x01E9, "scrolldown", "Scroll down");
        add(0x0000, "disabled", "Disabled");
        // multimedia
        add(0x00F6, "volup", "Volume up");
        add(0x00F5, "voldown", "Volume down");
        add(0x00F4, "mute", "Mute");
        add(0x00F0, "playpause", "Play/Pause");
        add(0x00F1, "stop", "Stop");
        add(0x00F2, "next", "Next track");
        add(0x00F3, "prev", "Previous track");
        // keyboard: letters, digits, F-keys (HID usage codes)
        for (i, c) in ('a'..='z').enumerate() {
            add(0x04 + i as u16, &c.to_string(), &format!("Key {}", c.to_ascii_uppercase()));
        }
        for (i, c) in "1234567890".chars().enumerate() {
            add(0x1E + i as u16, &c.to_string(), &format!("Key {c}"));
        }
        for i in 0..12u16 {
            add(0x3A + i, &format!("f{}", i + 1), &format!("Key F{}", i + 1));
        }
        for (code, id, label) in [
            (0x28, "enter", "Enter"),
            (0x29, "esc", "Escape"),
            (0x2A, "backspace", "Backspace"),
            (0x2B, "tab", "Tab"),
            (0x2C, "space", "Space"),
            (0x46, "printscreen", "Print Screen"),
            (0x49, "insert", "Insert"),
            (0x4A, "home", "Home"),
            (0x4B, "pageup", "Page Up"),
            (0x4C, "delete", "Delete"),
            (0x4D, "end", "End"),
            (0x4E, "pagedown", "Page Down"),
            (0x4F, "right_arrow", "Arrow right"),
            (0x50, "left_arrow", "Arrow left"),
            (0x51, "down", "Arrow down"),
            (0x52, "up", "Arrow up"),
            (0xE0, "lctrl", "Left Ctrl"),
            (0xE1, "lshift", "Left Shift"),
            (0xE2, "lalt", "Left Alt"),
            (0xE3, "lwin", "Left Win/Super"),
            (0xE4, "rctrl", "Right Ctrl"),
            (0xE5, "rshift", "Right Shift"),
            (0xE6, "ralt", "Right Alt"),
            (0xE7, "rwin", "Right Win/Super"),
        ] {
            add(code, id, label);
        }
        v
    })
}

pub fn action_label(code: u16) -> String {
    actions()
        .iter()
        .find(|a| a.code == code)
        .map(|a| a.label.clone())
        .unwrap_or_else(|| format!("Unknown (0x{code:04X})"))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct State {
    /// Mouse is off/asleep (only battery info is meaningful)
    pub asleep: bool,
    pub battery: Option<u8>,
    pub charging: bool,
    /// raw device byte, see POWER_OFF
    pub power_off: u8,
    /// percent: 0, 25, 50
    pub low_battery: u8,
    /// 0-based
    pub profile: u8,
    /// 1-based, active DPI slot (read only)
    pub dpi_slot: u8,
    pub dpi: [u16; DPI_SLOTS],
    pub polling_hz: u16,
    pub debounce_ms: u8,
    pub angle_snapping: bool,
    pub lift_off_high: bool,
    /// [logo, wheel]
    pub lighting: [Lighting; 2],
    pub buttons: Option<[u16; 8]>,
}

#[derive(Clone, Debug)]
pub enum Setting {
    /// 0-based
    Profile(u8),
    /// (0-based slot, dpi)
    Dpi(usize, u16),
    Polling(u16),
    Debounce(u8),
    AngleSnapping(bool),
    LiftOffHigh(bool),
    Lighting(Zone, Lighting),
    /// power-off byte (see POWER_OFF), low battery percent
    Energy(u8, u8),
    /// (button index into BUTTONS, action code)
    Button(usize, u16),
    ResetButtons,
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

pub struct Mouse {
    hid: Hid,
    pub info: DeviceInfo,
}

impl Mouse {
    pub fn open(info: DeviceInfo) -> Result<Self> {
        Ok(Mouse { hid: Hid::open(&info.path)?, info })
    }

    fn drain(&self) {
        let mut buf = [0u8; PKT];
        for _ in 0..64 {
            match self.hid.read_timeout(&mut buf, 1) {
                Ok(n) if n > 0 => continue,
                _ => break,
            }
        }
    }

    /// Sends a request and waits for the reply whose first `match_len` bytes
    /// (report id + command) equal the request's.
    fn xfer(&self, req: &[u8], match_len: usize) -> Result<[u8; PKT]> {
        let mut pkt = [0u8; PKT];
        pkt[..req.len()].copy_from_slice(req);
        self.drain();

        for _ in 0..3 {
            self.hid.write(&pkt)?;
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let mut resp = [0u8; PKT];
                // backends return the report without the report-id byte
                let n = self.hid.read_timeout(&mut resp[1..], left.as_millis().max(1) as i32)?;
                if n == 0 {
                    break;
                }
                if resp[1] == 0xFF && resp[2] == 0xAA {
                    return Err(Error::Unsupported);
                }
                if resp[1] == 0 && resp[2] == 0 && resp[3] == 0 {
                    return Err(Error::Asleep);
                }
                if resp[1..match_len] == pkt[1..match_len] {
                    return Ok(resp);
                }
                // unrelated event report (e.g. 12 01 button bitmap): keep reading
            }
        }
        Err(Error::Timeout)
    }

    /// Write a setting and persist it to the mouse's flash.
    fn set(&self, req: &[u8]) -> Result<()> {
        self.xfer(req, 3)?;
        self.xfer(&[0x00, 0x50, 0x03], 3)?;
        Ok(())
    }

    /// Battery: (percent, charging, power-off byte, low-battery %).
    /// Err(Asleep) when the dongle can't reach the mouse.
    pub fn read_battery(&self) -> Result<(u8, bool, u8, u8)> {
        let p = match self.xfer(&[0x00, 0x12, 0x07], 3) {
            Err(Error::Unsupported) => return Err(Error::Asleep),
            r => r?,
        };
        let pct = (p[5] as u16 * 25).min(100) as u8;
        let charging = p[10] > 0;
        if pct == 0 && !charging {
            return Err(Error::Asleep);
        }
        Ok((pct, charging, p[6], p[7].saturating_mul(25)))
    }

    pub fn read_state(&self) -> Result<State> {
        let mut s = State::default();

        if self.info.model.battery {
            match self.read_battery() {
                Ok((pct, ch, off, low)) => {
                    s.battery = Some(pct);
                    s.charging = ch;
                    s.power_off = off;
                    s.low_battery = low;
                }
                Err(Error::Asleep) => {
                    s.asleep = true;
                    return Ok(s);
                }
                Err(e) => return Err(e),
            }
        }

        let p = self.xfer(&[0x00, 0x12, 0x00], 4)?;
        s.profile = p[11];
        s.dpi_slot = p[12];

        let p = self.xfer(&[0x00, 0x12, 0x04, 0x00], 4)?;
        for i in 0..DPI_SLOTS {
            let raw = u16::from_le_bytes([p[5 + i * 2], p[6 + i * 2]]);
            s.dpi[i] = raw.saturating_mul(DPI_STEP).saturating_add(DPI_STEP).clamp(DPI_MIN, DPI_MAX);
        }
        s.polling_hz = *POLLING_RATES.get((p[13] & 0x07) as usize).unwrap_or(&1000);
        s.debounce_ms = (p[15].clamp(2, 7) + 1) * 4;
        s.angle_snapping = p[17] == 0x01;

        let p = self.xfer(&[0x00, 0x12, 0x06], 3)?;
        s.lift_off_high = p[5] == 0x01;

        // Keris ignores the zone byte here and returns every zone in one reply,
        // 5 bytes each (mode, brightness, r, g, b): logo at 5, wheel at 10.
        // (G-Helper reads zone 0 twice; verified on hardware.)
        let p = self.xfer(&[0x00, 0x12, 0x03, 0x00], 3)?;
        for (i, l) in s.lighting.iter_mut().enumerate() {
            let o = 5 + i * 5;
            *l = Lighting {
                mode: Mode::from_byte(p[o]),
                brightness: p[o + 1].min(MAX_BRIGHTNESS),
                color: [p[o + 2], p[o + 3], p[o + 4]],
            };
        }

        // Not fatal if unsupported
        if let Ok(p) = self.xfer(&[0x00, 0x12, 0x05, 0x00, 0x00, 0x00], 3) {
            let mut b = [0u16; 8];
            for (i, v) in b.iter_mut().enumerate() {
                *v = u16::from_le_bytes([p[5 + i * 2], p[6 + i * 2]]);
            }
            s.buttons = Some(b);
        }

        Ok(s)
    }

    pub fn apply(&self, setting: &Setting) -> Result<()> {
        let bad = |m: String| Err(Error::Invalid(m));
        match *setting {
            Setting::Profile(p) => {
                if p >= PROFILES {
                    return bad(format!("profile must be 1..{PROFILES}"));
                }
                self.set(&[0x00, 0x50, 0x02, p])
            }
            Setting::Dpi(slot, dpi) => {
                if slot >= DPI_SLOTS {
                    return bad(format!("DPI slot must be 1..{DPI_SLOTS}"));
                }
                if !(DPI_MIN..=DPI_MAX).contains(&dpi) {
                    return bad(format!("DPI must be {DPI_MIN}..{DPI_MAX}"));
                }
                let enc = ((dpi - DPI_STEP) / DPI_STEP).to_le_bytes();
                self.set(&[0x00, 0x51, 0x31, slot as u8, 0x00, enc[0], enc[1]])
            }
            Setting::Polling(hz) => {
                let Some(i) = POLLING_RATES.iter().position(|&r| r == hz) else {
                    return bad(format!("polling rate must be one of {POLLING_RATES:?}"));
                };
                self.set(&[0x00, 0x51, 0x31, 0x04, 0x00, i as u8])
            }
            Setting::Debounce(ms) => {
                if !DEBOUNCE_MS.contains(&ms) {
                    return bad(format!("debounce must be one of {DEBOUNCE_MS:?} ms"));
                }
                self.set(&[0x00, 0x51, 0x31, 0x05, 0x00, ms / 4 - 1])
            }
            Setting::AngleSnapping(on) => self.set(&[0x00, 0x51, 0x31, 0x06, 0x00, on as u8]),
            Setting::LiftOffHigh(high) => self.set(&[0x00, 0x51, 0x35, 0x00, 0x00, high as u8]),
            Setting::Lighting(zone, l) => {
                let [r, g, b] = l.color;
                self.set(&[
                    0x00,
                    0x51,
                    0x28,
                    zone as u8,
                    0x00,
                    l.mode.byte(),
                    l.brightness.min(MAX_BRIGHTNESS),
                    r,
                    g,
                    b,
                    0x00, // direction (unused by Keris modes)
                    0x00, // random color
                    0x00, // speed
                ])
            }
            Setting::Energy(off, low) => {
                if !self.info.model.battery {
                    return bad("this model has no battery".into());
                }
                if !POWER_OFF.iter().any(|&(b, _)| b == off) {
                    return bad("invalid auto power-off value".into());
                }
                if !LOW_BATTERY.contains(&low) {
                    return bad(format!("low battery warning must be one of {LOW_BATTERY:?}"));
                }
                self.set(&[0x00, 0x51, 0x37, 0x00, 0x00, off, 0x00, low / 25])
            }
            Setting::Button(idx, dst) => {
                let Some(&(src, _)) = BUTTONS.get(idx) else {
                    return bad(format!("button must be 1..{}", BUTTONS.len()));
                };
                self.set(&bind_packet(src, dst))
            }
            Setting::ResetButtons => {
                for &(src, _) in &BUTTONS {
                    self.xfer(&bind_packet(src, src), 3)?;
                }
                self.xfer(&[0x00, 0x50, 0x03], 3)?;
                Ok(())
            }
        }
    }
}

fn bind_packet(src: u16, dst: u16) -> [u8; 9] {
    let s = src.to_le_bytes();
    let d = dst.to_le_bytes();
    [0x00, 0x51, 0x21, 0x00, 0x00, s[0], s[1], d[0], d[1]]
}
