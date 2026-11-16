use crate::device::{self, DeviceInfo};
use crate::protocol::*;
use eframe::egui::{self, Color32, RichText};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// UI -> worker
enum Req {
    Scan,
    Select(usize),
    Refresh,
    Apply(Setting),
}

/// worker -> UI
enum Resp {
    Devices(Vec<DeviceInfo>, Option<usize>),
    State(State),
    Battery(Option<(u8, bool)>),
    Status(String),
    Error(String),
}

pub fn run() {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ROG Keris Wireless")
            .with_inner_size([430.0, 720.0])
            .with_min_inner_size([360.0, 400.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("keris-ctl", opts, Box::new(|cc| Ok(Box::new(App::new(cc))))) {
        eprintln!("GUI error: {e}");
    }
}

// ---------------------------------------------------------------------------
// Worker: owns the HID handle, does all blocking I/O
// ---------------------------------------------------------------------------

fn worker(rx: Receiver<Req>, tx: Sender<Resp>, ctx: egui::Context) {
    let send = |r: Resp| {
        let _ = tx.send(r);
        ctx.request_repaint();
    };
    let mut devices: Vec<DeviceInfo> = Vec::new();
    let mut mouse: Option<Mouse> = None;

    let open = |devs: &[DeviceInfo], i: usize, mouse: &mut Option<Mouse>| {
        *mouse = None;
        match devs.get(i).cloned().map(Mouse::open) {
            Some(Ok(m)) => *mouse = Some(m),
            Some(Err(e)) => send(Resp::Error(e.to_string())),
            None => {}
        }
    };
    let refresh = |mouse: &mut Option<Mouse>| {
        let Some(m) = mouse.as_ref() else { return };
        send(Resp::Status("Reading settings...".into()));
        match m.read_state() {
            Ok(s) => {
                send(Resp::Status(if s.asleep {
                    "Mouse is asleep/off - move it, then press Refresh".into()
                } else {
                    "Ready".into()
                }));
                send(Resp::State(s));
            }
            Err(e) => {
                if matches!(e, Error::Io(_)) {
                    *mouse = None;
                }
                send(Resp::Error(e.to_string()));
            }
        }
    };

    let mut pending = Some(Req::Scan);
    loop {
        let req = match pending.take() {
            Some(r) => r,
            None => match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => {
                    // periodic battery update
                    if let Some(m) = mouse.as_ref().filter(|m| m.info.model.battery) {
                        let b = m.read_battery().ok().map(|(p, c, _, _)| (p, c));
                        send(Resp::Battery(b));
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };

        match req {
            Req::Scan => {
                devices = device::enumerate();
                let sel = if devices.is_empty() { None } else { Some(0) };
                send(Resp::Devices(devices.clone(), sel));
                if devices.is_empty() {
                    mouse = None;
                    send(Resp::Status("No ROG Keris found".into()));
                } else {
                    open(&devices, 0, &mut mouse);
                    refresh(&mut mouse);
                }
            }
            Req::Select(i) => {
                open(&devices, i, &mut mouse);
                refresh(&mut mouse);
            }
            Req::Refresh => {
                if mouse.is_none() {
                    pending = Some(Req::Scan);
                } else {
                    refresh(&mut mouse);
                }
            }
            Req::Apply(s) => {
                let Some(m) = mouse.as_ref() else {
                    send(Resp::Error("No mouse connected".into()));
                    continue;
                };
                match m.apply(&s) {
                    Ok(()) => {
                        send(Resp::Status("Saved".into()));
                        // switching profile changes every other setting
                        if matches!(s, Setting::Profile(_) | Setting::ResetButtons) {
                            refresh(&mut mouse);
                        }
                    }
                    Err(e) => {
                        send(Resp::Error(e.to_string()));
                        // resync UI with what the mouse really has
                        refresh(&mut mouse);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

struct App {
    tx: Sender<Req>,
    rx: Receiver<Resp>,
    devices: Vec<DeviceInfo>,
    selected: Option<usize>,
    state: Option<State>,
    status: String,
    error: bool,
    /// lighting editor
    zone: Zone,
    light: Lighting,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let (tx, wrx) = channel();
        let (wtx, rx) = channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || worker(wrx, wtx, ctx));
        App {
            tx,
            rx,
            devices: Vec::new(),
            selected: None,
            state: None,
            status: "Searching...".into(),
            error: false,
            zone: Zone::All,
            light: Lighting::default(),
        }
    }

    fn send(&self, r: Req) {
        let _ = self.tx.send(r);
    }

    fn poll_worker(&mut self) {
        while let Ok(r) = self.rx.try_recv() {
            match r {
                Resp::Devices(d, sel) => {
                    self.devices = d;
                    self.selected = sel;
                    self.state = None;
                }
                Resp::State(s) => {
                    self.light = match self.zone {
                        Zone::Wheel => s.lighting[1],
                        _ => s.lighting[0],
                    };
                    self.state = Some(s);
                }
                Resp::Battery(b) => {
                    if let Some(s) = self.state.as_mut() {
                        s.battery = b.map(|x| x.0);
                        s.charging = b.is_some_and(|x| x.1);
                    }
                }
                Resp::Status(s) => {
                    self.status = s;
                    self.error = false;
                }
                Resp::Error(e) => {
                    self.status = e;
                    self.error = true;
                }
            }
        }
    }
}

/// Apply on release for sliders, immediately for keyboard changes.
fn committed(r: &egui::Response) -> bool {
    r.drag_stopped() || (r.changed() && !r.dragged())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let c = if self.error { Color32::from_rgb(230, 90, 90) } else { ui.visuals().weak_text_color() };
            ui.label(RichText::new(&self.status).color(c));
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // ---- header: device picker
            ui.horizontal(|ui| {
                let name = self
                    .selected
                    .and_then(|i| self.devices.get(i))
                    .map(|d| d.model.name)
                    .unwrap_or("No device");
                let mut sel = self.selected;
                egui::ComboBox::from_id_salt("dev").selected_text(name).width(240.0).show_ui(ui, |ui| {
                    for (i, d) in self.devices.iter().enumerate() {
                        ui.selectable_value(&mut sel, Some(i), d.model.name);
                    }
                });
                if sel != self.selected {
                    if let Some(i) = sel {
                        self.selected = sel;
                        self.state = None;
                        self.send(Req::Select(i));
                    }
                }
                if ui.button("Rescan").clicked() {
                    self.send(Req::Scan);
                }
                if ui.button("Refresh").clicked() {
                    self.send(Req::Refresh);
                }
            });

            let Some(mut s) = self.state.clone() else {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| ui.spinner());
                return;
            };
            let model = self.selected.and_then(|i| self.devices.get(i)).map(|d| d.model);

            if let Some(b) = s.battery {
                let ch = if s.charging { " (charging)" } else { "" };
                ui.label(RichText::new(format!("Battery {b}%{ch}")).strong());
            }
            if s.asleep {
                ui.add_space(10.0);
                ui.label("The mouse is asleep or turned off. Move it, then press Refresh.");
                return;
            }
            ui.separator();

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut apply: Vec<Setting> = Vec::new();

                // ---- profile
                ui.horizontal(|ui| {
                    ui.label("Profile");
                    for p in 0..PROFILES {
                        if ui.selectable_label(s.profile == p, format!(" {} ", p + 1)).clicked() && s.profile != p {
                            s.profile = p;
                            apply.push(Setting::Profile(p));
                        }
                    }
                });

                // ---- DPI
                ui.add_space(6.0);
                ui.heading("DPI");
                egui::Grid::new("dpi").num_columns(2).show(ui, |ui| {
                    for i in 0..DPI_SLOTS {
                        let active = i + 1 == s.dpi_slot as usize;
                        let label = format!("Stage {}{}", i + 1, if active { " (active)" } else { "" });
                        ui.label(label);
                        let r = ui.add(
                            egui::Slider::new(&mut s.dpi[i], DPI_MIN..=DPI_MAX)
                                .step_by(DPI_STEP as f64)
                                .logarithmic(true),
                        );
                        if committed(&r) {
                            apply.push(Setting::Dpi(i, s.dpi[i]));
                        }
                        ui.end_row();
                    }
                });

                // ---- performance
                ui.add_space(6.0);
                ui.heading("Performance");
                egui::Grid::new("perf").num_columns(2).show(ui, |ui| {
                    ui.label("Polling rate");
                    let mut hz = s.polling_hz;
                    egui::ComboBox::from_id_salt("poll").selected_text(format!("{hz} Hz")).show_ui(ui, |ui| {
                        for r in POLLING_RATES {
                            ui.selectable_value(&mut hz, r, format!("{r} Hz"));
                        }
                    });
                    if hz != s.polling_hz {
                        s.polling_hz = hz;
                        apply.push(Setting::Polling(hz));
                    }
                    ui.end_row();

                    ui.label("Debounce");
                    let mut db = s.debounce_ms;
                    egui::ComboBox::from_id_salt("deb").selected_text(format!("{db} ms")).show_ui(ui, |ui| {
                        for d in DEBOUNCE_MS {
                            ui.selectable_value(&mut db, d, format!("{d} ms"));
                        }
                    });
                    if db != s.debounce_ms {
                        s.debounce_ms = db;
                        apply.push(Setting::Debounce(db));
                    }
                    ui.end_row();

                    ui.label("Lift-off distance");
                    ui.horizontal(|ui| {
                        let before = s.lift_off_high;
                        ui.radio_value(&mut s.lift_off_high, false, "Low");
                        ui.radio_value(&mut s.lift_off_high, true, "High");
                        if before != s.lift_off_high {
                            apply.push(Setting::LiftOffHigh(s.lift_off_high));
                        }
                    });
                    ui.end_row();

                    ui.label("Angle snapping");
                    if ui.checkbox(&mut s.angle_snapping, "").changed() {
                        apply.push(Setting::AngleSnapping(s.angle_snapping));
                    }
                    ui.end_row();
                });

                // ---- lighting
                ui.add_space(6.0);
                ui.heading("Lighting");
                ui.horizontal(|ui| {
                    for z in [Zone::All, Zone::Logo, Zone::Wheel] {
                        if ui.selectable_label(self.zone == z, z.label()).clicked() {
                            self.zone = z;
                            self.light = if z == Zone::Wheel { s.lighting[1] } else { s.lighting[0] };
                        }
                    }
                });
                egui::Grid::new("light").num_columns(2).show(ui, |ui| {
                    ui.label("Effect");
                    egui::ComboBox::from_id_salt("mode").selected_text(self.light.mode.label()).show_ui(ui, |ui| {
                        for m in Mode::ALL {
                            ui.selectable_value(&mut self.light.mode, m, m.label());
                        }
                    });
                    ui.end_row();
                    if self.light.mode != Mode::Off {
                        ui.label("Brightness");
                        ui.add(egui::Slider::new(&mut self.light.brightness, 0..=MAX_BRIGHTNESS));
                        ui.end_row();
                    }
                    if self.light.mode.has_color() {
                        ui.label("Color");
                        egui::color_picker::color_edit_button_srgb(ui, &mut self.light.color);
                        ui.end_row();
                    }
                });
                if ui.button("Apply lighting").clicked() {
                    match self.zone {
                        Zone::Logo => s.lighting[0] = self.light,
                        Zone::Wheel => s.lighting[1] = self.light,
                        Zone::All => s.lighting = [self.light; 2],
                    }
                    apply.push(Setting::Lighting(self.zone, self.light));
                }

                // ---- power
                if model.is_some_and(|m| m.battery) {
                    ui.add_space(6.0);
                    ui.heading("Power");
                    egui::Grid::new("pwr").num_columns(2).show(ui, |ui| {
                        let fmt_off = |b: u8| match POWER_OFF.iter().find(|p| p.0 == b) {
                            Some((_, 0)) | None => "Never".to_string(),
                            Some((_, m)) => format!("{m} min"),
                        };
                        ui.label("Auto sleep");
                        let mut off = s.power_off;
                        egui::ComboBox::from_id_salt("off").selected_text(fmt_off(off)).show_ui(ui, |ui| {
                            for (b, _) in POWER_OFF {
                                ui.selectable_value(&mut off, b, fmt_off(b));
                            }
                        });
                        ui.end_row();

                        ui.label("Low battery warning");
                        let mut low = s.low_battery;
                        let fmt_low = |v: u8| if v == 0 { "Off".to_string() } else { format!("{v}%") };
                        egui::ComboBox::from_id_salt("low").selected_text(fmt_low(low)).show_ui(ui, |ui| {
                            for v in LOW_BATTERY {
                                ui.selectable_value(&mut low, v, fmt_low(v));
                            }
                        });
                        ui.end_row();

                        if off != s.power_off || low != s.low_battery {
                            s.power_off = off;
                            s.low_battery = low;
                            let low = *LOW_BATTERY.iter().min_by_key(|&&x| x.abs_diff(low)).unwrap();
                            apply.push(Setting::Energy(off, low));
                        }
                    });
                }

                // ---- buttons
                if let Some(mut b) = s.buttons {
                    ui.add_space(6.0);
                    ui.heading("Buttons");
                    egui::Grid::new("btn").num_columns(2).show(ui, |ui| {
                        for (i, (_, name)) in BUTTONS.iter().enumerate() {
                            ui.label(*name);
                            let mut code = b[i];
                            egui::ComboBox::from_id_salt(("btn", i))
                                .selected_text(action_label(code))
                                .width(180.0)
                                .height(320.0)
                                .show_ui(ui, |ui| {
                                    for a in actions() {
                                        ui.selectable_value(&mut code, a.code, a.label.as_str());
                                    }
                                });
                            if code != b[i] {
                                b[i] = code;
                                apply.push(Setting::Button(i, code));
                            }
                            ui.end_row();
                        }
                    });
                    s.buttons = Some(b);
                    if ui.button("Reset buttons to default").clicked() {
                        apply.push(Setting::ResetButtons);
                    }
                }

                for a in apply {
                    self.status = "Saving...".into();
                    self.error = false;
                    self.send(Req::Apply(a));
                }
            });

            self.state = Some(s);
        });
    }
}
