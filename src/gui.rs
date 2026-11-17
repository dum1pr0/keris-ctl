use crate::device::{self, DeviceInfo};
use crate::protocol::*;
use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense,
    Stroke, StrokeKind, TextStyle,
};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// UI -> worker
enum Req {
    Select(usize),
    Refresh,
    Apply(Setting),
}

/// worker -> UI
enum Resp {
    Devices(Vec<DeviceInfo>, Option<usize>),
    State(State),
    /// active DPI slot (1-based) changed on the mouse
    DpiSlot(u8),
    /// mouse went to sleep / lost connection to the dongle
    Asleep,
    Battery(Option<(u8, bool)>),
    Status(String),
    Error(String),
}

pub fn run() {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ROG Keris")
            .with_app_id("keris-ctl")
            .with_inner_size([480.0, 800.0])
            .with_min_inner_size([420.0, 560.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("keris-ctl", opts, Box::new(|cc| Ok(Box::new(App::new(cc))))) {
        eprintln!("GUI error: {e}");
    }
}

// ---------------------------------------------------------------------------
// Worker: owns the HID handle, does all blocking I/O
// ---------------------------------------------------------------------------

/// How often the active DPI slot is polled (on top of event reports).
const LIVE_EVERY: Duration = Duration::from_millis(1000);
const ASLEEP_EVERY: Duration = Duration::from_secs(3);
const BATTERY_EVERY: Duration = Duration::from_secs(30);
const RESCAN_EVERY: Duration = Duration::from_secs(3);

struct Worker {
    tx: Sender<Resp>,
    ctx: egui::Context,
    devices: Vec<DeviceInfo>,
    mouse: Option<Mouse>,
    /// last known (profile, dpi slot); None = asleep / unknown
    live: Option<(u8, u8)>,
}

impl Worker {
    fn send(&self, r: Resp) {
        let _ = self.tx.send(r);
        self.ctx.request_repaint();
    }

    fn open(&mut self, i: usize) {
        self.mouse = None;
        self.live = None;
        match self.devices.get(i).cloned().map(Mouse::open) {
            Some(Ok(m)) => self.mouse = Some(m),
            Some(Err(e)) => self.send(Resp::Error(e.to_string())),
            None => {}
        }
    }

    fn scan(&mut self, quiet: bool) {
        let devs = device::enumerate();
        if quiet && devs.is_empty() {
            return;
        }
        self.devices = devs;
        let sel = if self.devices.is_empty() { None } else { Some(0) };
        self.send(Resp::Devices(self.devices.clone(), sel));
        if self.devices.is_empty() {
            self.mouse = None;
            self.send(Resp::Status("No ROG Keris found".into()));
        } else {
            self.open(0);
            self.refresh(true);
        }
    }

    fn refresh(&mut self, loud: bool) {
        let Some(m) = self.mouse.as_ref() else { return };
        if loud {
            self.send(Resp::Status("Reading settings...".into()));
        }
        match m.read_state() {
            Ok(s) => {
                self.live = (!s.asleep).then_some((s.profile, s.dpi_slot));
                self.send(Resp::Status(if s.asleep {
                    "Mouse is asleep - move it to wake it up".into()
                } else {
                    "Connected".into()
                }));
                self.send(Resp::State(s));
            }
            Err(e) => {
                if matches!(e, Error::Io(_)) {
                    self.mouse = None;
                }
                self.send(Resp::Error(e.to_string()));
            }
        }
    }

    /// Poll the active profile/DPI slot so presses of the DPI button on the
    /// mouse show up in the app, and notice when the mouse wakes up.
    fn live_check(&mut self) {
        let Some(m) = self.mouse.as_ref() else { return };
        match m.read_profile() {
            Ok((profile, slot)) => match self.live {
                None => self.refresh(false), // just woke up
                Some((p, _)) if p != profile => self.refresh(false),
                Some((_, s)) if s != slot => {
                    self.live = Some((profile, slot));
                    self.send(Resp::DpiSlot(slot));
                }
                _ => {}
            },
            Err(Error::Asleep) if m.info.model.battery => {
                if self.live.take().is_some() {
                    self.send(Resp::Asleep);
                    self.send(Resp::Status("Mouse is asleep - move it to wake it up".into()));
                }
            }
            Err(Error::Io(e)) => {
                self.mouse = None;
                self.live = None;
                self.send(Resp::Error(format!("Disconnected: {e}")));
            }
            Err(_) => {}
        }
    }

    fn handle(&mut self, req: Req) {
        match req {
            Req::Select(i) => {
                self.open(i);
                self.refresh(true);
            }
            Req::Refresh => {
                if self.mouse.is_none() {
                    self.scan(false);
                } else {
                    self.refresh(true);
                }
            }
            Req::Apply(s) => {
                let Some(m) = self.mouse.as_ref() else {
                    self.send(Resp::Error("No mouse connected".into()));
                    return;
                };
                match m.apply(&s) {
                    Ok(()) => {
                        self.send(Resp::Status("Saved to mouse".into()));
                        // switching profile changes every other setting
                        if matches!(s, Setting::Profile(_) | Setting::ResetButtons) {
                            self.refresh(false);
                        }
                    }
                    Err(e) => {
                        self.send(Resp::Error(e.to_string()));
                        // resync UI with what the mouse really has
                        self.refresh(false);
                    }
                }
            }
        }
    }
}

fn worker(rx: Receiver<Req>, tx: Sender<Resp>, ctx: egui::Context) {
    let mut w = Worker { tx, ctx, devices: Vec::new(), mouse: None, live: None };
    w.scan(false);

    let mut next_live = Instant::now() + LIVE_EVERY;
    let mut next_battery = Instant::now() + BATTERY_EVERY;
    let mut next_rescan = Instant::now() + RESCAN_EVERY;

    loop {
        match rx.try_recv() {
            Ok(r) => {
                w.handle(r);
                next_live = Instant::now() + LIVE_EVERY;
                continue;
            }
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }

        let now = Instant::now();
        if let Some(m) = w.mouse.as_ref() {
            // waits up to 100 ms for input reports from the mouse
            match m.poll_events(100) {
                // the slot changes right after the button report: read a bit later
                Ok(true) => next_live = next_live.min(Instant::now() + Duration::from_millis(60)),
                Ok(false) => {}
                Err(_) => next_live = now,
            }
            if Instant::now() >= next_live {
                w.live_check();
                next_live = Instant::now() + if w.live.is_some() { LIVE_EVERY } else { ASLEEP_EVERY };
            }
            if now >= next_battery {
                next_battery = now + BATTERY_EVERY;
                if let Some(m) = w.mouse.as_ref().filter(|m| m.info.model.battery) {
                    if w.live.is_some() {
                        let b = m.read_battery().ok().map(|(p, c, _, _)| (p, c));
                        w.send(Resp::Battery(b));
                    }
                }
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(r) => w.handle(r),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    // auto-connect when the mouse / dongle gets plugged in
                    if Instant::now() >= next_rescan {
                        next_rescan = Instant::now() + RESCAN_EVERY;
                        w.scan(true);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

const ACCENT: Color32 = Color32::from_rgb(0xE8, 0x17, 0x3A);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0xFF, 0x3B, 0x5C);
const BG: Color32 = Color32::from_rgb(0x0E, 0x10, 0x14);
const CARD: Color32 = Color32::from_rgb(0x17, 0x1A, 0x21);
const CARD_STROKE: Color32 = Color32::from_rgb(0x25, 0x29, 0x33);
const FIELD: Color32 = Color32::from_rgb(0x21, 0x25, 0x2E);
const FIELD_HOVER: Color32 = Color32::from_rgb(0x2B, 0x30, 0x3B);
const TEXT: Color32 = Color32::from_rgb(0xE8, 0xEA, 0xEF);
const WEAK: Color32 = Color32::from_rgb(0x8B, 0x91, 0xA0);
const OK: Color32 = Color32::from_rgb(0x3D, 0xD6, 0x8C);
const WARN: Color32 = Color32::from_rgb(0xF5, 0xB0, 0x41);
const BAD: Color32 = Color32::from_rgb(0xF2, 0x5C, 0x5C);

fn setup_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.5)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Heading, FontId::proportional(17.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    style.spacing.item_spacing = vec2(8.0, 8.0);
    style.spacing.button_padding = vec2(12.0, 6.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.slider_width = 220.0;
    style.spacing.combo_width = 180.0;

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = CARD;
    v.window_stroke = Stroke::new(1.0_f32, CARD_STROKE);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, TEXT);
    v.hyperlink_color = ACCENT_HOVER;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.override_text_color = None;

    let r = CornerRadius::same(7);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, CARD_STROKE);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = r;
    for (wv, fill) in [(&mut w.inactive, FIELD), (&mut w.hovered, FIELD_HOVER), (&mut w.active, FIELD_HOVER), (&mut w.open, FIELD_HOVER)] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.bg_stroke = Stroke::NONE;
        wv.corner_radius = r;
        wv.expansion = 0.0;
    }
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.hovered.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0x3A, 0x40, 0x4D));
    w.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    ctx.set_style(style);
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, CARD_STROKE))
        .corner_radius(12)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(WEAK).strong().extra_letter_spacing(1.2));
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).heading().strong().color(TEXT));
    ui.add_space(2.0);
}

/// Label on the left, control on the right.
fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(32.0);
        ui.label(RichText::new(label).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), add);
    });
}

/// A pill that animates between normal and selected.
fn chip(ui: &mut egui::Ui, id: impl std::hash::Hash, selected: bool, text: &str, size: egui::Vec2) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = ui.ctx().animate_bool_with_time(ui.id().with(id), selected, 0.15);
    let base = if resp.hovered() { FIELD_HOVER } else { FIELD };
    let fill = lerp_color(base, if resp.hovered() { ACCENT_HOVER } else { ACCENT }, t);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(7), fill);
    let fg = lerp_color(if resp.hovered() { Color32::WHITE } else { TEXT }, Color32::WHITE, t);
    p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(13.5), fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Row of equally sized chips; returns the clicked value.
fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, salt: &str, current: T, options: &[(T, String)], width: f32) -> Option<T> {
    let n = options.len().max(1) as f32;
    let gap = 4.0;
    let w = ((width - gap * (n - 1.0)) / n).max(36.0);
    let mut out = None;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.horizontal(|ui| {
            for (i, (v, label)) in options.iter().enumerate() {
                if chip(ui, (salt, i), *v == current, label, vec2(w, 30.0)).clicked() && *v != current {
                    out = Some(*v);
                }
            }
        });
    });
    out
}

fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = vec2(42.0, 24.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.15);
    let p = ui.painter();
    let fill = lerp_color(if resp.hovered() { FIELD_HOVER } else { FIELD }, ACCENT, t);
    p.rect_filled(rect, CornerRadius::same(12), fill);
    let r = rect.height() / 2.0 - 3.0;
    let x = egui::lerp((rect.left() + r + 3.0)..=(rect.right() - r - 3.0), t);
    p.circle_filled(egui::pos2(x, rect.center().y), r, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    let fill = if resp.is_pointer_button_down_on() {
        ACCENT
    } else if resp.hovered() {
        ACCENT_HOVER
    } else {
        ACCENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn battery_badge(ui: &mut egui::Ui, pct: u8, charging: bool) {
    let color = if charging || pct > 50 {
        OK
    } else if pct > 25 {
        WARN
    } else {
        BAD
    };
    egui::Frame::new().fill(FIELD).corner_radius(14).inner_margin(Margin::symmetric(10, 3)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(22.0, 12.0), Sense::hover());
            let p = ui.painter();
            let body = egui::Rect::from_min_size(rect.min, vec2(19.0, 12.0));
            p.rect_stroke(body, CornerRadius::same(3), Stroke::new(1.2_f32, WEAK), StrokeKind::Inside);
            p.rect_filled(
                egui::Rect::from_min_size(egui::pos2(body.right() + 0.5, body.center().y - 2.5), vec2(2.0, 5.0)),
                CornerRadius::same(1),
                WEAK,
            );
            let inner = body.shrink(2.5);
            let fill_w = inner.width() * (pct as f32 / 100.0).clamp(0.08, 1.0);
            p.rect_filled(egui::Rect::from_min_size(inner.min, vec2(fill_w, inner.height())), CornerRadius::same(1), color);
            let txt = if charging { format!("{pct}% ⚡") } else { format!("{pct}%") };
            ui.label(RichText::new(txt).color(TEXT).size(13.0));
        });
    });
}

/// Slot indices in the order the DPI button cycles, starting at the lowest DPI.
/// Rotating the stage values (to change the active DPI) keeps this order stable.
fn cycle_order(dpi: &[u16; DPI_SLOTS]) -> [usize; DPI_SLOTS] {
    let start = (0..DPI_SLOTS).min_by_key(|&i| (dpi[i], i)).unwrap_or(0);
    std::array::from_fn(|i| (start + i) % DPI_SLOTS)
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Performance,
    Lighting,
    Buttons,
    Power,
}

struct App {
    tx: Sender<Req>,
    rx: Receiver<Resp>,
    devices: Vec<DeviceInfo>,
    selected: Option<usize>,
    state: Option<State>,
    status: String,
    error: bool,
    tab: Tab,
    /// lighting editor
    zone: Zone,
    light: Lighting,
    /// time the active DPI last changed (for the highlight pulse)
    dpi_changed_at: f64,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        setup_theme(&cc.egui_ctx);
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
            tab: Tab::Performance,
            zone: Zone::All,
            light: Lighting::default(),
            dpi_changed_at: f64::NEG_INFINITY,
        }
    }

    fn send(&self, r: Req) {
        let _ = self.tx.send(r);
    }

    fn poll_worker(&mut self, now: f64) {
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
                Resp::DpiSlot(slot) => {
                    if let Some(s) = self.state.as_mut() {
                        s.dpi_slot = slot;
                        self.dpi_changed_at = now;
                    }
                }
                Resp::Asleep => {
                    if let Some(s) = self.state.as_mut() {
                        s.asleep = true;
                    }
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

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new("ROG").size(20.0).strong().color(ACCENT).extra_letter_spacing(1.5));
            ui.label(RichText::new("KERIS").size(20.0).strong().color(TEXT).extra_letter_spacing(1.5));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⟳").on_hover_text("Re-read settings / rescan devices").clicked() {
                    self.send(Req::Refresh);
                }
                if let Some(b) = self.state.as_ref().and_then(|s| s.battery) {
                    let ch = self.state.as_ref().is_some_and(|s| s.charging);
                    battery_badge(ui, b, ch);
                }
            });
        });

        let name = self.selected.and_then(|i| self.devices.get(i)).map(|d| d.model.name).unwrap_or("No device");
        if self.devices.len() > 1 {
            let mut sel = self.selected;
            egui::ComboBox::from_id_salt("dev").selected_text(name).width(ui.available_width()).show_ui(ui, |ui| {
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
        } else {
            ui.label(RichText::new(name).color(WEAK));
        }
    }

    /// Big "current DPI" card with clickable stages.
    fn dpi_card(&mut self, ui: &mut egui::Ui, s: &mut State, apply: &mut Vec<Setting>) {
        let now = ui.input(|i| i.time);
        let pulse = (1.0 - ((now - self.dpi_changed_at) / 0.8) as f32).clamp(0.0, 1.0);
        if pulse > 0.0 {
            ui.ctx().request_repaint();
        }
        let stroke = lerp_color(CARD_STROKE, ACCENT, pulse);
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0 + pulse, stroke))
            .corner_radius(12)
            .inner_margin(Margin::same(16))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let active = (s.dpi_slot as usize).clamp(1, DPI_SLOTS) - 1;
                let order = cycle_order(&s.dpi);
                let pos = order.iter().position(|&i| i == active).unwrap_or(0);

                ui.horizontal(|ui| {
                    caption(ui, "Current DPI");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let opts: Vec<(u8, String)> = (0..PROFILES).rev().map(|p| (p, format!("{}", p + 1))).collect();
                        if let Some(p) = segmented(ui, "profile", s.profile, &opts, 108.0) {
                            s.profile = p;
                            apply.push(Setting::Profile(p));
                        }
                        ui.label(RichText::new("Profile").color(WEAK).size(12.5));
                    });
                });

                ui.horizontal(|ui| {
                    let col = lerp_color(TEXT, ACCENT_HOVER, pulse);
                    ui.label(RichText::new(s.dpi[active].to_string()).size(52.0).strong().color(col));
                    ui.vertical(|ui| {
                        ui.add_space(22.0);
                        ui.label(RichText::new(format!("Stage {} of {DPI_SLOTS}", pos + 1)).color(WEAK));
                    });
                });

                ui.add_space(4.0);
                let gap = 6.0;
                let w = (ui.available_width() - gap * (DPI_SLOTS as f32 - 1.0)) / DPI_SLOTS as f32;
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    ui.horizontal(|ui| {
                        for (n, &slot) in order.iter().enumerate() {
                            let r = chip(ui, ("stage", n), slot == active, &s.dpi[slot].to_string(), vec2(w, 40.0))
                                .on_hover_text(format!("Stage {} - click to use this DPI", n + 1));
                            if r.clicked() && slot != active {
                                s.dpi = rotate_dpi(&s.dpi, active, slot);
                                self.dpi_changed_at = now;
                                apply.push(Setting::DpiStages(s.dpi));
                            }
                        }
                    });
                });
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Click a stage to switch. The DPI button on the mouse still cycles in this order.")
                        .small()
                        .color(WEAK),
                );
            });
    }

    fn tabs(&mut self, ui: &mut egui::Ui, battery: bool) {
        let mut opts = vec![
            (Tab::Performance, "Performance".to_string()),
            (Tab::Lighting, "Lighting".to_string()),
            (Tab::Buttons, "Buttons".to_string()),
        ];
        if battery {
            opts.push((Tab::Power, "Power".to_string()));
        }
        egui::Frame::new().fill(CARD).corner_radius(10).inner_margin(Margin::same(4)).show(ui, |ui| {
            let w = ui.available_width();
            if let Some(t) = segmented(ui, "tabs", self.tab, &opts, w) {
                self.tab = t;
            }
        });
    }

    fn performance(&mut self, ui: &mut egui::Ui, s: &mut State, apply: &mut Vec<Setting>) {
        card(ui, |ui| {
            section_title(ui, "DPI stages");
            let active = (s.dpi_slot as usize).clamp(1, DPI_SLOTS) - 1;
            let order = cycle_order(&s.dpi);
            for (n, &slot) in order.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, if slot == active { ACCENT } else { FIELD_HOVER });
                    ui.label(RichText::new(format!("Stage {}", n + 1)).color(if slot == active { TEXT } else { WEAK }));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let v = ui.add_sized(
                            [64.0, 26.0],
                            egui::DragValue::new(&mut s.dpi[slot]).range(DPI_MIN..=DPI_MAX).speed(10.0),
                        );
                        ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(80.0);
                        let r = ui.add(
                            egui::Slider::new(&mut s.dpi[slot], DPI_MIN..=DPI_MAX)
                                .step_by(DPI_STEP as f64)
                                .logarithmic(true)
                                .trailing_fill(true)
                                .show_value(false),
                        );
                        // DragValue: apply on release / after typing, rounded to the DPI step
                        let typed = v.lost_focus() || v.drag_stopped() || (v.changed() && !v.dragged() && !v.has_focus());
                        if committed(&r) || typed {
                            s.dpi[slot] = (s.dpi[slot] + DPI_STEP / 2) / DPI_STEP * DPI_STEP;
                            s.dpi[slot] = s.dpi[slot].clamp(DPI_MIN, DPI_MAX);
                            apply.push(Setting::Dpi(slot, s.dpi[slot]));
                        }
                    });
                });
            }
        });

        card(ui, |ui| {
            section_title(ui, "Sensor");
            caption(ui, "Polling rate");
            let opts: Vec<(u16, String)> = POLLING_RATES.iter().map(|&r| (r, format!("{r} Hz"))).collect();
            if let Some(hz) = segmented(ui, "poll", s.polling_hz, &opts, ui.available_width()) {
                s.polling_hz = hz;
                apply.push(Setting::Polling(hz));
            }
            ui.add_space(4.0);
            caption(ui, "Lift-off distance");
            let opts = [(false, "Low".to_string()), (true, "High".to_string())];
            if let Some(v) = segmented(ui, "lod", s.lift_off_high, &opts, ui.available_width()) {
                s.lift_off_high = v;
                apply.push(Setting::LiftOffHigh(v));
            }
            ui.add_space(4.0);
            row(ui, "Button debounce", |ui| {
                let mut db = s.debounce_ms;
                egui::ComboBox::from_id_salt("deb").selected_text(format!("{db} ms")).width(110.0).show_ui(ui, |ui| {
                    for d in DEBOUNCE_MS {
                        ui.selectable_value(&mut db, d, format!("{d} ms"));
                    }
                });
                if db != s.debounce_ms {
                    s.debounce_ms = db;
                    apply.push(Setting::Debounce(db));
                }
            });
            row(ui, "Angle snapping", |ui| {
                if toggle(ui, &mut s.angle_snapping).changed() {
                    apply.push(Setting::AngleSnapping(s.angle_snapping));
                }
            });
        });
    }

    fn lighting(&mut self, ui: &mut egui::Ui, s: &mut State, apply: &mut Vec<Setting>) {
        card(ui, |ui| {
            section_title(ui, "Lighting");
            caption(ui, "Zone");
            let opts: Vec<(Zone, String)> =
                [Zone::All, Zone::Logo, Zone::Wheel].iter().map(|&z| (z, z.label().to_string())).collect();
            if let Some(z) = segmented(ui, "zone", self.zone, &opts, ui.available_width()) {
                self.zone = z;
                self.light = if z == Zone::Wheel { s.lighting[1] } else { s.lighting[0] };
            }

            ui.add_space(4.0);
            caption(ui, "Effect");
            let w = ui.available_width();
            for chunk in Mode::ALL.chunks(3) {
                let opts: Vec<(Mode, String)> = chunk.iter().map(|&m| (m, m.label().to_string())).collect();
                if let Some(m) = segmented(ui, &format!("mode{:?}", chunk[0]), self.light.mode, &opts, w) {
                    self.light.mode = m;
                }
            }

            if self.light.mode != Mode::Off {
                ui.add_space(4.0);
                caption(ui, "Brightness");
                let opts: Vec<(u8, String)> = (0..=MAX_BRIGHTNESS)
                    .map(|b| (b, if b == 0 { "Min".to_string() } else { format!("{}", b) }))
                    .collect();
                if let Some(b) = segmented(ui, "bright", self.light.brightness, &opts, ui.available_width()) {
                    self.light.brightness = b;
                }
            }

            if self.light.mode.has_color() {
                ui.add_space(4.0);
                caption(ui, "Color");
                const PRESETS: [[u8; 3]; 9] = [
                    [255, 0, 0],
                    [255, 80, 0],
                    [255, 200, 0],
                    [0, 255, 60],
                    [0, 220, 255],
                    [0, 70, 255],
                    [140, 0, 255],
                    [255, 0, 160],
                    [255, 255, 255],
                ];
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for c in PRESETS {
                        let (rect, r) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                        let col = Color32::from_rgb(c[0], c[1], c[2]);
                        ui.painter().circle_filled(rect.center(), 11.0, col);
                        if self.light.color == c {
                            ui.painter().circle_stroke(rect.center(), 13.0, Stroke::new(2.0_f32, TEXT));
                        } else if r.hovered() {
                            ui.painter().circle_stroke(rect.center(), 13.0, Stroke::new(1.5_f32, WEAK));
                        }
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.light.color = c;
                        }
                    }
                    ui.add_space(4.0);
                    egui::color_picker::color_edit_button_srgb(ui, &mut self.light.color).on_hover_text("Custom color");
                });
            }

            ui.add_space(8.0);
            if primary_button(ui, "Apply lighting").clicked() {
                match self.zone {
                    Zone::Logo => s.lighting[0] = self.light,
                    Zone::Wheel => s.lighting[1] = self.light,
                    Zone::All => s.lighting = [self.light; 2],
                }
                apply.push(Setting::Lighting(self.zone, self.light));
            }
        });
    }

    fn buttons(&mut self, ui: &mut egui::Ui, s: &mut State, apply: &mut Vec<Setting>) {
        let Some(mut b) = s.buttons else {
            card(ui, |ui| {
                ui.label(RichText::new("This firmware does not report button bindings.").color(WEAK));
            });
            return;
        };
        card(ui, |ui| {
            section_title(ui, "Button mapping");
            for (i, (_, name)) in BUTTONS.iter().enumerate() {
                row(ui, name, |ui| {
                    let mut code = b[i];
                    egui::ComboBox::from_id_salt(("btn", i))
                        .selected_text(action_label(code))
                        .width(190.0)
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
                });
            }
            ui.add_space(6.0);
            if ui.add_sized([ui.available_width(), 32.0], egui::Button::new("Reset to default")).clicked() {
                apply.push(Setting::ResetButtons);
            }
        });
        s.buttons = Some(b);
    }

    fn power(&mut self, ui: &mut egui::Ui, s: &mut State, apply: &mut Vec<Setting>) {
        card(ui, |ui| {
            section_title(ui, "Power");
            caption(ui, "Auto sleep");
            let opts: Vec<(u8, String)> = POWER_OFF
                .iter()
                .map(|&(b, m)| (b, if m == 0 { "Never".to_string() } else { format!("{m}m") }))
                .collect();
            let mut off = s.power_off;
            if let Some(v) = segmented(ui, "off", off, &opts, ui.available_width()) {
                off = v;
            }
            ui.add_space(4.0);
            caption(ui, "Low battery warning");
            let opts: Vec<(u8, String)> =
                LOW_BATTERY.iter().map(|&v| (v, if v == 0 { "Off".to_string() } else { format!("{v}%") })).collect();
            let cur_low = *LOW_BATTERY.iter().min_by_key(|&&x| x.abs_diff(s.low_battery)).unwrap();
            let mut low = cur_low;
            if let Some(v) = segmented(ui, "low", cur_low, &opts, ui.available_width()) {
                low = v;
            }
            if off != s.power_off || low != cur_low {
                s.power_off = off;
                s.low_battery = low;
                apply.push(Setting::Energy(off, low));
            }
        });
    }
}

/// Apply on release for sliders, immediately for keyboard changes.
fn committed(r: &egui::Response) -> bool {
    r.drag_stopped() || (r.changed() && !r.dragged())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = ctx.input(|i| i.time);
        self.poll_worker(now);

        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 16, right: 16, top: 14, bottom: 6 }))
            .show_separator_line(false)
            .show(ctx, |ui| self.header(ui));

        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0_f32, CARD_STROKE))
                    .inner_margin(Margin::symmetric(16, 7)),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let col = if self.error {
                        BAD
                    } else if self.state.as_ref().is_some_and(|s| !s.asleep) {
                        OK
                    } else {
                        WARN
                    };
                    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, col);
                    let tc = if self.error { BAD } else { WEAK };
                    ui.add(egui::Label::new(RichText::new(&self.status).color(tc).size(12.5)).truncate());
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 16, right: 16, top: 6, bottom: 10 }))
            .show(ctx, |ui| {
                let Some(mut s) = self.state.clone() else {
                    ui.add_space(80.0);
                    ui.vertical_centered(|ui| {
                        if self.devices.is_empty() && self.status != "Searching..." {
                            ui.label(RichText::new("No mouse found").size(18.0).strong().color(TEXT));
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Plug in the USB cable or the 2.4 GHz dongle.\nThe app connects automatically.")
                                    .color(WEAK),
                            );
                            #[cfg(target_os = "linux")]
                            ui.label(RichText::new("Linux: install 70-rog-keris.rules (see README).").small().color(WEAK));
                        } else {
                            ui.add(egui::Spinner::new().size(28.0).color(ACCENT));
                        }
                    });
                    return;
                };
                if s.asleep {
                    ui.add_space(80.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("Mouse is asleep").size(18.0).strong().color(TEXT));
                        ui.add_space(4.0);
                        ui.label(RichText::new("Move it or click a button - the app reconnects by itself.").color(WEAK));
                    });
                    return;
                }
                let battery = self.selected.and_then(|i| self.devices.get(i)).is_some_and(|d| d.model.battery);
                if self.tab == Tab::Power && !battery {
                    self.tab = Tab::Performance;
                }

                let mut apply: Vec<Setting> = Vec::new();
                self.dpi_card(ui, &mut s, &mut apply);
                ui.add_space(4.0);
                self.tabs(ui, battery);
                ui.add_space(4.0);

                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    match self.tab {
                        Tab::Performance => self.performance(ui, &mut s, &mut apply),
                        Tab::Lighting => self.lighting(ui, &mut s, &mut apply),
                        Tab::Buttons => self.buttons(ui, &mut s, &mut apply),
                        Tab::Power => self.power(ui, &mut s, &mut apply),
                    }
                });

                for a in apply {
                    self.status = "Saving...".into();
                    self.error = false;
                    self.send(Req::Apply(a));
                }
                self.state = Some(s);
            });
    }
}
