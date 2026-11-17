use crate::device::{self, DeviceInfo};
use crate::protocol::*;

const HELP: &str = "\
keris-ctl - ROG Keris Wireless settings

USAGE:
  keris-ctl                         open the GUI
  keris-ctl [--device N] <command>

COMMANDS:
  list                              list connected mice
  info                              show current settings
  profile <1-3>                     switch onboard profile
  dpi <slot 1-4> <100-16000>        set DPI of a slot (step 100)
  dpi use <slot 1-4>                make that slot's DPI the active one
  poll <125|250|500|1000>           polling rate (Hz)
  debounce <12|16|20|24|28|32>      button debounce (ms)
  angle <on|off>                    angle snapping
  liftoff <low|high>                lift-off distance
  light <logo|wheel|all> <static|breathing|cycle|react|battery|off> [brightness 0-4] [RRGGBB]
  sleep <1|2|3|5|10|never>          auto power-off (minutes)
  lowbat <0|25|50>                  low battery warning (%)
  bind <button 1-8> <action>        remap a button
  bind list                         list buttons and actions
  bind reset                        restore default buttons

OPTIONS:
  --device N                        use mouse N from `list` (default 1)
";

pub fn run(args: &[String]) -> i32 {
    match run_inner(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_inner(args: &[String]) -> std::result::Result<(), String> {
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut dev_idx = 1usize;
    if let Some(i) = args.iter().position(|a| *a == "--device" || *a == "-d") {
        let n = args.get(i + 1).ok_or("--device needs a number")?;
        dev_idx = n.parse().map_err(|_| format!("bad device number: {n}"))?;
        args.drain(i..=i + 1);
    }

    let Some((&cmd, rest)) = args.split_first() else {
        print!("{HELP}");
        return Ok(());
    };

    match cmd {
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            return Ok(());
        }
        "list" => {
            let devs = device::enumerate();
            if devs.is_empty() {
                println!("No ROG Keris found.");
            }
            for (i, d) in devs.iter().enumerate() {
                println!("{}: {} [{:04X}] {}", i + 1, d.model.name, d.model.pid, d.path);
            }
            return Ok(());
        }
        "bind" if rest.first() == Some(&"list") => {
            println!("Buttons:");
            for (i, (_, name)) in BUTTONS.iter().enumerate() {
                println!("  {}: {name}", i + 1);
            }
            println!("Actions:");
            for a in actions() {
                println!("  {:<12} {}", a.id, a.label);
            }
            return Ok(());
        }
        _ => {}
    }

    let mouse = open(dev_idx)?;
    let e = |e: Error| e.to_string();

    if cmd == "info" {
        let s = mouse.read_state().map_err(e)?;
        print_state(&mouse.info, &s);
        return Ok(());
    }

    let setting = parse_setting(cmd, rest, &mouse)?;
    mouse.apply(&setting).map_err(e)?;
    println!("OK");
    Ok(())
}

fn open(idx: usize) -> std::result::Result<Mouse, String> {
    let devs = device::enumerate();
    if devs.is_empty() {
        return Err("no ROG Keris found".into());
    }
    let info = devs
        .get(idx.wrapping_sub(1))
        .cloned()
        .ok_or_else(|| format!("device {idx} not found (found {})", devs.len()))?;
    Mouse::open(info).map_err(|e| e.to_string())
}

fn num<T: std::str::FromStr>(s: Option<&&str>, what: &str) -> std::result::Result<T, String> {
    let s = s.ok_or_else(|| format!("missing {what}"))?;
    s.parse().map_err(|_| format!("invalid {what}: {s}"))
}

fn on_off(s: Option<&&str>) -> std::result::Result<bool, String> {
    match s.copied() {
        Some("on" | "1" | "true") => Ok(true),
        Some("off" | "0" | "false") => Ok(false),
        _ => Err("expected on|off".into()),
    }
}

fn parse_setting(cmd: &str, a: &[&str], mouse: &Mouse) -> std::result::Result<Setting, String> {
    Ok(match cmd {
        "profile" => Setting::Profile(num::<u8>(a.first(), "profile")?.wrapping_sub(1)),
        "dpi" if a.first() == Some(&"use") => {
            let target: usize = num(a.get(1), "slot")?;
            if !(1..=DPI_SLOTS).contains(&target) {
                return Err(format!("slot must be 1..{DPI_SLOTS}"));
            }
            let s = mouse.read_state().map_err(|e| e.to_string())?;
            if s.asleep || s.dpi_slot == 0 {
                return Err("mouse is asleep".into());
            }
            let active = s.dpi_slot as usize - 1;
            println!("active DPI: {} -> {}", s.dpi[active], s.dpi[target - 1]);
            Setting::DpiStages(rotate_dpi(&s.dpi, active, target - 1))
        }
        "dpi" => {
            let slot: usize = num(a.first(), "slot")?;
            Setting::Dpi(slot.wrapping_sub(1), num(a.get(1), "DPI")?)
        }
        "poll" => Setting::Polling(num(a.first(), "polling rate")?),
        "debounce" => Setting::Debounce(num(a.first(), "debounce")?),
        "angle" => Setting::AngleSnapping(on_off(a.first())?),
        "liftoff" => match a.first().copied() {
            Some("low") => Setting::LiftOffHigh(false),
            Some("high") => Setting::LiftOffHigh(true),
            _ => return Err("expected low|high".into()),
        },
        "light" => {
            let zone = match a.first().copied() {
                Some("logo") => Zone::Logo,
                Some("wheel") => Zone::Wheel,
                Some("all") => Zone::All,
                _ => return Err("zone must be logo|wheel|all".into()),
            };
            let mode_s = a.get(1).copied().unwrap_or("");
            let mode = Mode::ALL
                .into_iter()
                .find(|m| m.id() == mode_s)
                .ok_or("mode must be static|breathing|cycle|react|battery|off")?;
            let mut l = Lighting { mode, brightness: MAX_BRIGHTNESS, color: [255, 0, 0] };
            for extra in &a[2..] {
                if extra.trim_start_matches('#').len() == 6 {
                    l.color = parse_hex(extra)?;
                } else if let Ok(b) = extra.parse::<u8>() {
                    if b > MAX_BRIGHTNESS {
                        return Err(format!("brightness must be 0..{MAX_BRIGHTNESS}"));
                    }
                    l.brightness = b;
                } else {
                    l.color = parse_hex(extra)?;
                }
            }
            Setting::Lighting(zone, l)
        }
        "sleep" | "lowbat" => {
            // power-off and low-battery are written together: keep the other value
            let (_, _, cur_off, cur_low) = mouse.read_battery().map_err(|e| e.to_string())?;
            if cmd == "sleep" {
                let v = a.first().copied().unwrap_or("");
                let mins: u8 = if v == "never" { 0 } else { v.parse().map_err(|_| "invalid minutes")? };
                let off = POWER_OFF
                    .iter()
                    .find(|&&(_, m)| m == mins)
                    .ok_or("sleep must be 1|2|3|5|10|never")?
                    .0;
                Setting::Energy(off, nearest_low(cur_low))
            } else {
                Setting::Energy(cur_off, num(a.first(), "percent")?)
            }
        }
        "bind" => match a.first().copied() {
            Some("reset") => Setting::ResetButtons,
            _ => {
                let b: usize = num(a.first(), "button")?;
                let id = a.get(1).copied().ok_or("missing action (see `bind list`)")?;
                let act = actions()
                    .iter()
                    .find(|x| x.id == id.to_ascii_lowercase())
                    .ok_or_else(|| format!("unknown action `{id}` (see `bind list`)"))?;
                Setting::Button(b.wrapping_sub(1), act.code)
            }
        },
        _ => return Err(format!("unknown command `{cmd}`, see --help")),
    })
}

fn nearest_low(v: u8) -> u8 {
    *LOW_BATTERY.iter().min_by_key(|&&x| x.abs_diff(v)).unwrap()
}

fn parse_hex(s: &str) -> std::result::Result<[u8; 3], String> {
    let h = s.trim_start_matches('#');
    if h.len() != 6 {
        return Err(format!("invalid color `{s}` (use RRGGBB)"));
    }
    let v = u32::from_str_radix(h, 16).map_err(|_| format!("invalid color `{s}`"))?;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn print_state(info: &DeviceInfo, s: &State) {
    println!("{} ({})", info.model.name, info.path);
    if let Some(b) = s.battery {
        println!("Battery:        {b}%{}", if s.charging { " (charging)" } else { "" });
    }
    if s.asleep {
        println!("Mouse is asleep or off. Move it and try again.");
        return;
    }
    println!("Profile:        {}", s.profile + 1);
    for (i, d) in s.dpi.iter().enumerate() {
        let mark = if i + 1 == s.dpi_slot as usize { " *" } else { "" };
        println!("DPI {}:          {d}{mark}", i + 1);
    }
    println!("Polling rate:   {} Hz", s.polling_hz);
    println!("Debounce:       {} ms", s.debounce_ms);
    println!("Angle snapping: {}", if s.angle_snapping { "on" } else { "off" });
    println!("Lift-off:       {}", if s.lift_off_high { "high" } else { "low" });
    for (i, l) in s.lighting.iter().enumerate() {
        let z = if i == 0 { Zone::Logo } else { Zone::Wheel };
        println!(
            "Light {:<9} {} brightness {}/{} #{:02X}{:02X}{:02X}",
            format!("{}:", z.label()),
            l.mode.label(),
            l.brightness,
            MAX_BRIGHTNESS,
            l.color[0],
            l.color[1],
            l.color[2]
        );
    }
    if info.model.battery {
        let mins = POWER_OFF.iter().find(|p| p.0 == s.power_off).map(|p| p.1);
        match mins {
            Some(0) | None => println!("Auto sleep:     never"),
            Some(m) => println!("Auto sleep:     {m} min"),
        }
        println!("Low battery:    {}%", s.low_battery);
    }
    if let Some(b) = s.buttons {
        for (i, (_, name)) in BUTTONS.iter().enumerate() {
            println!("Button {} {:<13} {}", i + 1, format!("({name}):"), action_label(b[i]));
        }
    }
}
