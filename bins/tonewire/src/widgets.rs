use egui::{Color32, ComboBox, Pos2, Sense, Slider, Stroke, Ui, Vec2};
use tonewire_dsp::{BiquadCoeffs, BiquadKind, gain_to_db};
use tonewire_graph::catalog::{self, EQ_BANDS, ParamInfo, ParamKind, Unit};
use tonewire_graph::{
    AppMode, AppRule, DeviceRef, Direction, EndpointKind, NodeId, NodeKind, NodeSpec,
};
use tonewire_proto::{Command, MeterReading};

use crate::model::Model;

pub const METER_FLOOR_DB: f32 = -60.0;

/// Edits queued by widgets during a frame.
#[derive(Default)]
pub struct Outbox {
    pub commands: Vec<Command>,
}

impl Outbox {
    pub fn push(&mut self, command: Command) {
        self.commands.push(command);
    }
}

fn unit_suffix(unit: Unit) -> &'static str {
    match unit {
        Unit::Db => " dB",
        Unit::Hz => " Hz",
        Unit::Ms => " ms",
        Unit::Ratio => ":1",
        Unit::Percent | Unit::None => "",
    }
}

/// Draws one param control and queues a `SetParam` when it changes.
pub fn param(ui: &mut Ui, model: &mut Model, outbox: &mut Outbox, node: NodeId, info: &ParamInfo) {
    let Some(spec) = model.graph.nodes.get(&node) else {
        return;
    };
    let mut value = spec.param(&info.key);
    let mut edit: Option<bool> = None;
    match &info.kind {
        ParamKind::Toggle => {
            let mut on = value >= 0.5;
            if ui.checkbox(&mut on, &info.label).changed() {
                value = f32::from(u8::from(on));
                edit = Some(false);
            }
        }
        ParamKind::Choice(options) => {
            let mut index = value as usize;
            ComboBox::from_id_salt((node, &info.key))
                .selected_text(options.get(index).cloned().unwrap_or_default())
                .show_ui(ui, |ui| {
                    for (i, option) in options.iter().enumerate() {
                        if ui.selectable_value(&mut index, i, option).changed() {
                            edit = Some(false);
                        }
                    }
                });
            value = index as f32;
        }
        ParamKind::Continuous { log } => {
            let (shown, scale) = if info.unit == Unit::Percent {
                (value * 100.0, 100.0)
            } else {
                (value, 1.0)
            };
            let mut shown = shown;
            let suffix = if info.unit == Unit::Percent {
                " %"
            } else {
                unit_suffix(info.unit)
            };
            let response = ui.add(
                Slider::new(&mut shown, info.min * scale..=info.max * scale)
                    .logarithmic(*log)
                    .text(&info.label)
                    .suffix(suffix)
                    .max_decimals(if info.unit == Unit::Hz { 0 } else { 1 }),
            );
            if response.changed() {
                value = shown / scale;
                edit = Some(response.dragged());
            } else if response.drag_stopped() {
                edit = Some(false);
            }
        }
    }
    if let Some(dragging) = edit {
        if let Some(spec) = model.graph.nodes.get_mut(&node) {
            spec.params.insert(info.key.clone(), info.clamp(value));
        }
        model.dragging = dragging.then(|| (node, info.key.clone()));
        outbox.push(Command::SetParam {
            node,
            key: info.key.clone(),
            value,
            dragging,
        });
    }
}

/// A level bar from -60 dBFS to 0 dBFS, with gain reduction underneath for
/// dynamics nodes.
pub fn meter(ui: &mut Ui, reading: Option<&MeterReading>, show_reduction: bool) {
    let width = ui.available_width().clamp(120.0, 220.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 1.0, Color32::from_gray(35));
    if let Some(reading) = reading {
        let level =
            |gain: f32| ((gain_to_db(gain) - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
        let peak = level(reading.peak);
        let color = if reading.peak >= 0.99 {
            Color32::from_rgb(230, 70, 60)
        } else if peak > 0.85 {
            Color32::from_rgb(230, 200, 60)
        } else {
            Color32::from_rgb(80, 200, 120)
        };
        let mut bar = rect;
        bar.set_width(rect.width() * peak);
        painter.rect_filled(bar, 1.0, color);
        let rms_x = rect.left() + rect.width() * level(reading.rms);
        painter.line_segment(
            [
                Pos2::new(rms_x, rect.top()),
                Pos2::new(rms_x, rect.bottom()),
            ],
            Stroke::new(1.0, Color32::WHITE),
        );
        if show_reduction && reading.reduction_db < -0.1 {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
            let fraction = (-reading.reduction_db / 24.0).clamp(0.0, 1.0);
            let mut bar = rect;
            bar.set_left(rect.right() - rect.width() * fraction);
            ui.painter_at(rect)
                .rect_filled(bar, 1.0, Color32::from_rgb(240, 150, 50));
        }
    }
}

fn eq_kind(index: f32) -> BiquadKind {
    match index as usize {
        0 => BiquadKind::LowShelf,
        2 => BiquadKind::HighShelf,
        3 => BiquadKind::LowPass,
        4 => BiquadKind::HighPass,
        5 => BiquadKind::Notch,
        6 => BiquadKind::BandPass,
        _ => BiquadKind::Peak,
    }
}

/// Combined magnitude response of the equalizer's enabled bands.
pub fn eq_response(spec: &NodeSpec, freqs: &[f32]) -> Vec<f32> {
    const SR: f32 = 48_000.0;
    let mut total = vec![0.0f32; freqs.len()];
    for band in 1..=EQ_BANDS {
        if spec.param(&format!("band{band}_on")) < 0.5 {
            continue;
        }
        let coeffs = BiquadCoeffs::design(
            eq_kind(spec.param(&format!("band{band}_type"))),
            spec.param(&format!("band{band}_freq")),
            spec.param(&format!("band{band}_q")),
            spec.param(&format!("band{band}_gain_db")),
            SR,
        );
        for (t, &f) in total.iter_mut().zip(freqs) {
            *t += coeffs.magnitude_db(f, SR);
        }
    }
    total
}

pub fn eq_curve(ui: &mut Ui, spec: &NodeSpec) {
    let size = Vec2::new(ui.available_width().clamp(160.0, 260.0), 56.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(25));
    let mid = rect.center().y;
    painter.line_segment(
        [Pos2::new(rect.left(), mid), Pos2::new(rect.right(), mid)],
        Stroke::new(1.0, Color32::from_gray(60)),
    );
    let points = 96;
    let freqs: Vec<f32> = (0..points)
        .map(|i| 20.0 * 1000f32.powf(i as f32 / (points - 1) as f32))
        .collect();
    let db = eq_response(spec, &freqs);
    let to_screen = |i: usize, d: f32| {
        let x = rect.left() + rect.width() * i as f32 / (points - 1) as f32;
        let y = mid - (d / 24.0).clamp(-1.0, 1.0) * rect.height() * 0.5;
        Pos2::new(x, y)
    };
    let line: Vec<Pos2> = db
        .iter()
        .enumerate()
        .map(|(i, &d)| to_screen(i, d))
        .collect();
    painter.add(egui::Shape::line(
        line,
        Stroke::new(1.5, Color32::from_rgb(90, 170, 250)),
    ));
}

fn wanted_direction(kind: EndpointKind, loopback: bool) -> Direction {
    match kind {
        EndpointKind::DeviceInput if !loopback => Direction::Capture,
        _ => Direction::Playback,
    }
}

/// Device picker for I/O nodes, with "follow system default" where allowed.
pub fn device_picker(
    ui: &mut Ui,
    model: &Model,
    outbox: &mut Outbox,
    node: NodeId,
    spec: &NodeSpec,
) {
    let Some(kind) = spec.kind.endpoint_kind() else {
        return;
    };
    let virtual_only = matches!(
        kind,
        EndpointKind::VirtualMic | EndpointKind::VirtualSpeaker | EndpointKind::AppSource
    );
    if virtual_only && model.backend == "pipewire" && kind != EndpointKind::AppSource {
        ui.weak("Created by tonewire");
        return;
    }
    let loopback = spec.device.as_ref().is_some_and(|d| d.loopback);
    if kind == EndpointKind::DeviceInput {
        let mut capture_output = loopback;
        if ui
            .checkbox(&mut capture_output, "Capture a playback device")
            .changed()
        {
            outbox.push(Command::SetDevice { node, device: None });
            if capture_output
                && let Some(d) = model
                    .devices
                    .iter()
                    .find(|d| d.direction == Direction::Playback && d.is_default)
            {
                outbox.push(Command::SetDevice {
                    node,
                    device: Some(DeviceRef {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        loopback: true,
                    }),
                });
            }
            return;
        }
    }
    let direction = wanted_direction(kind, loopback);
    let current = spec
        .device
        .as_ref()
        .map_or("System default".to_owned(), |d| d.name.clone());
    ComboBox::from_id_salt(("device", node))
        .selected_text(current)
        .width(200.0)
        .show_ui(ui, |ui| {
            let allow_default = !virtual_only || model.backend == "pipewire";
            if allow_default
                && ui
                    .selectable_label(spec.device.is_none(), "System default")
                    .clicked()
            {
                outbox.push(Command::SetDevice { node, device: None });
            }
            for device in model.devices.iter().filter(|d| d.direction == direction) {
                if virtual_only && !device.is_virtual {
                    continue;
                }
                let selected = spec.device.as_ref().is_some_and(|d| d.id == device.id);
                let mut label = device.name.clone();
                if device.is_virtual {
                    label.push_str("  [virtual]");
                }
                if ui.selectable_label(selected, label).clicked() {
                    outbox.push(Command::SetDevice {
                        node,
                        device: Some(DeviceRef {
                            id: device.id.clone(),
                            name: device.name.clone(),
                            loopback,
                        }),
                    });
                }
            }
        });
}

pub fn channels_picker(ui: &mut Ui, outbox: &mut Outbox, node: NodeId, spec: &NodeSpec) {
    let mut channels = spec.channels;
    ui.horizontal(|ui| {
        ui.selectable_value(&mut channels, 1, "Mono");
        ui.selectable_value(&mut channels, 2, "Stereo");
    });
    if channels != spec.channels {
        outbox.push(Command::SetChannels { node, channels });
    }
}

/// Picks which apps an App Source captures or a Virtual Mic serves.
pub fn apps_editor(
    ui: &mut Ui,
    model: &Model,
    outbox: &mut Outbox,
    node: NodeId,
    spec: &NodeSpec,
    new_rule: &mut String,
) {
    let mut apps = spec.apps.clone();
    let mut mode = spec.app_mode;
    let wanted = if spec.kind == NodeKind::AppSource {
        Direction::Playback
    } else {
        Direction::Capture
    };
    if spec.kind == NodeKind::AppSource {
        ui.horizontal(|ui| {
            ui.radio_value(&mut mode, AppMode::Move, "Move")
                .on_hover_text("The app is heard only through this graph");
            ui.radio_value(&mut mode, AppMode::Tap, "Tap")
                .on_hover_text("The graph gets a copy; the app keeps playing where it was");
        });
    } else {
        ui.weak("Apps that should record from this mic:");
    }
    let mut remove = None;
    for (i, rule) in apps.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(&rule.binary);
            if ui.small_button("x").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        apps.remove(i);
    }
    ui.menu_button("Add running app", |ui| {
        let mut seen = std::collections::BTreeSet::new();
        for app in model.apps.iter().filter(|a| a.direction == wanted) {
            if app.binary.is_empty() || !seen.insert(app.binary.clone()) {
                continue;
            }
            if ui
                .button(format!("{} ({})", app.name, app.binary))
                .clicked()
            {
                apps.push(AppRule {
                    binary: app.binary.clone(),
                });
                ui.close();
            }
        }
        if seen.is_empty() {
            ui.weak("No apps are playing audio right now");
        }
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(new_rule)
                .hint_text("program name")
                .desired_width(120.0),
        );
        if ui.button("Add").clicked() && !new_rule.trim().is_empty() {
            apps.push(AppRule {
                binary: new_rule.trim().to_owned(),
            });
            new_rule.clear();
        }
    });
    if apps != spec.apps || mode != spec.app_mode {
        outbox.push(Command::SetApps { node, apps, mode });
    }
}

/// The params small enough to sit on the node itself.
pub fn inline_params(kind: &NodeKind) -> Vec<ParamInfo> {
    let params = catalog::describe(kind).params;
    if params.len() <= 6 {
        params
    } else {
        Vec::new()
    }
}

pub fn status_line(ui: &mut Ui, model: &Model, node: NodeId) {
    if let Some(status) = model.status.get(&node) {
        if !status.connected {
            let message = status
                .message
                .clone()
                .unwrap_or_else(|| "Not connected".into());
            ui.colored_label(Color32::from_rgb(230, 110, 90), message);
        } else if status.xruns > 0 {
            ui.weak(format!("{} dropouts", status.xruns));
        }
    }
    if let Some(warning) = model.warnings.get(&node) {
        ui.colored_label(Color32::from_rgb(230, 180, 60), warning);
    }
}

pub fn channel_color(channels: u8) -> Color32 {
    if channels == 1 {
        Color32::from_rgb(120, 200, 160)
    } else {
        Color32::from_rgb(110, 160, 240)
    }
}
