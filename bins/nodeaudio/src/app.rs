use std::time::{Duration, Instant};

use egui::{Color32, RichText};
use egui_snarl::ui::{SnarlStyle, SnarlWidget};
use nodeaudio_graph::catalog;
use nodeaudio_graph::{NodeId, NodeKind};
use nodeaudio_proto::{Command, EngineSettings, Reply, ServerMsg};

use crate::canvas::{Canvas, Viewer};
use crate::client::{Connection, Incoming};
use crate::model::Model;
use crate::widgets::{self, Outbox};

const NOTICE_FOR: Duration = Duration::from_secs(6);

pub struct App {
    connection: Connection,
    model: Model,
    canvas: Canvas,
    selected: Option<NodeId>,
    notice: Option<(String, Instant)>,
    rename: String,
    new_rule: String,
    import: Option<String>,
    style: SnarlStyle,
}

impl App {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            model: Model::default(),
            canvas: Canvas::default(),
            selected: None,
            notice: None,
            rename: String::new(),
            new_rule: String::new(),
            import: None,
            style: SnarlStyle::new(),
        }
    }

    fn notify(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now()));
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(incoming) = self.connection.incoming.try_recv() {
            match incoming {
                Incoming::Connected => {
                    self.model.connected = true;
                    self.model.connection_error = None;
                }
                Incoming::Disconnected(reason) => {
                    self.model.connected = false;
                    self.model.connection_error = Some(reason);
                }
                Incoming::Message(ServerMsg::Event(event)) => self.model.apply(event),
                Incoming::Message(ServerMsg::Reply { reply, .. }) => match reply {
                    Reply::Ok => {}
                    Reply::Error(message) => self.notify(message),
                    Reply::Exported(toml) => {
                        ctx.copy_text(toml);
                        self.notify("Graph copied to the clipboard");
                    }
                },
                Incoming::Message(ServerMsg::Hello { .. }) => {}
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, outbox: &mut Outbox) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Graph", |ui| {
                if ui.button("Copy graph to clipboard").clicked() {
                    outbox.push(Command::ExportGraph);
                    ui.close();
                }
                if ui.button("Import graph").clicked() {
                    self.import = Some(String::new());
                    ui.close();
                }
            });
            ui.menu_button("Engine", |ui| {
                let current = self.model.engine;
                ui.label("Sample rate");
                for rate in [44_100, 48_000, 96_000] {
                    if ui
                        .radio(current.sample_rate == rate, format!("{} Hz", rate))
                        .clicked()
                    {
                        outbox.push(Command::SetEngineConfig(EngineSettings {
                            sample_rate: rate,
                            ..current
                        }));
                        ui.close();
                    }
                }
                ui.separator();
                ui.label("Buffer");
                for block in [64, 128, 256, 512] {
                    let ms = block as f32 * 1000.0 / current.sample_rate as f32;
                    if ui
                        .radio(
                            current.block_frames == block,
                            format!("{block} frames ({ms:.1} ms)"),
                        )
                        .clicked()
                    {
                        outbox.push(Command::SetEngineConfig(EngineSettings {
                            block_frames: block,
                            ..current
                        }));
                        ui.close();
                    }
                }
            });
            ui.separator();
            if ui
                .add_enabled(self.model.can_undo, egui::Button::new("Undo"))
                .clicked()
            {
                outbox.push(Command::Undo);
            }
            if ui
                .add_enabled(self.model.can_redo, egui::Button::new("Redo"))
                .clicked()
            {
                outbox.push(Command::Redo);
            }
            ui.separator();
            let mut muted = self.model.muted;
            if ui.toggle_value(&mut muted, "Mute all").changed() {
                outbox.push(Command::SetMuteAll(muted));
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if !self.model.connected {
                let reason = self
                    .model
                    .connection_error
                    .clone()
                    .unwrap_or_else(|| "Connecting...".into());
                ui.colored_label(Color32::from_rgb(230, 110, 90), reason);
                return;
            }
            ui.label(format!("Backend: {}", self.model.backend));
            ui.separator();
            match &self.model.stats {
                Some(stats) => {
                    if stats.running {
                        ui.label(RichText::new("Running").color(Color32::from_rgb(80, 200, 120)));
                    } else {
                        ui.label(
                            RichText::new("Idle: no output").color(Color32::from_rgb(230, 180, 60)),
                        );
                    }
                    ui.separator();
                    ui.label(format!(
                        "{} Hz, {} frames",
                        stats.sample_rate, stats.block_frames
                    ));
                    ui.separator();
                    ui.label(format!("Latency ~{:.1} ms", stats.graph_latency_ms));
                    ui.separator();
                    ui.label(format!("DSP {:.1}%", stats.dsp_load * 100.0));
                }
                None => {
                    ui.label("Waiting for stats");
                }
            }
            if let Some((notice, at)) = &self.notice
                && at.elapsed() < NOTICE_FOR
            {
                ui.separator();
                ui.colored_label(Color32::from_rgb(230, 180, 60), notice);
            }
        });
    }

    fn details(&mut self, ui: &mut egui::Ui, outbox: &mut Outbox) {
        let Some(id) = self.selected else {
            ui.weak("Pick \"Details\" on a node to edit all of its settings.");
            ui.weak("Right-click the canvas to add nodes.");
            return;
        };
        let Some(spec) = self.model.graph.nodes.get(&id).cloned() else {
            self.selected = None;
            return;
        };
        ui.horizontal(|ui| {
            ui.heading(&spec.name);
            if ui.small_button("Close").clicked() {
                self.selected = None;
            }
        });
        ui.horizontal(|ui| {
            if self.rename.is_empty() {
                self.rename = spec.name.clone();
            }
            ui.text_edit_singleline(&mut self.rename);
            if ui.button("Rename").clicked() && self.rename != spec.name {
                outbox.push(Command::RenameNode {
                    node: id,
                    name: self.rename.clone(),
                });
            }
        });
        let mut bypass = spec.bypass;
        if ui.checkbox(&mut bypass, "Bypass").changed() {
            outbox.push(Command::SetBypass { node: id, bypass });
        }
        widgets::channels_picker(ui, outbox, id, &spec);
        widgets::status_line(ui, &self.model, id);
        ui.separator();
        if spec.kind.is_endpoint() {
            widgets::device_picker(ui, &self.model, outbox, id, &spec);
        }
        if matches!(spec.kind, NodeKind::AppSource | NodeKind::VirtualMic) {
            ui.separator();
            widgets::apps_editor(ui, &self.model, outbox, id, &spec, &mut self.new_rule);
        }
        if spec.kind == NodeKind::Eq {
            widgets::eq_curve(ui, &spec);
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for info in catalog::describe(&spec.kind).params {
                widgets::param(ui, &mut self.model, outbox, id, &info);
            }
        });
        ui.separator();
        if ui.button("Remove node").clicked() {
            outbox.push(Command::RemoveNode { node: id });
            self.selected = None;
        }
    }

    fn import_window(&mut self, ctx: &egui::Context, outbox: &mut Outbox) {
        let Some(mut text) = self.import.take() else {
            return;
        };
        let mut open = true;
        let mut done = false;
        egui::Window::new("Import graph").open(&mut open).show(ctx, |ui| {
            ui.label("Paste a graph exported from nodeaudio. It replaces the current graph (undo restores it).");
            ui.add(egui::TextEdit::multiline(&mut text).desired_rows(12).code_editor());
            if ui.button("Import").clicked() {
                outbox.push(Command::ImportGraph { toml: text.clone() });
                done = true;
            }
        });
        if open && !done {
            self.import = Some(text);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.drain(ctx);
        let mut outbox = Outbox::default();
        let pointer_down = ctx.input(|i| i.pointer.any_down());
        if self.selected.is_none() {
            self.rename.clear();
        }

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, &mut outbox));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.selected.is_some() {
            egui::Panel::right("details")
                .default_size(300.0)
                .show(ui, |ui| self.details(ui, &mut outbox));
        }
        self.import_window(ctx, &mut outbox);

        self.canvas.sync(&self.model.graph, pointer_down);
        egui::CentralPanel::default().show(ui, |ui| {
            if !self.model.connected {
                ui.centered_and_justified(|ui| ui.label("Connecting to the nodeaudio daemon..."));
                return;
            }
            if self.model.graph.nodes.is_empty() {
                ui.weak("Right-click to add a node. Try Device input, Equalizer, De-esser and Virtual mic.");
            }
            let mut viewer = Viewer {
                model: &mut self.model,
                outbox: &mut outbox,
                selected: &mut self.selected,
            };
            SnarlWidget::new()
                .id_salt("graph")
                .style(self.style)
                .show(&mut self.canvas.snarl, &mut viewer, ui);
        });

        if !pointer_down {
            if self.model.dragging.is_some() {
                self.model.dragging = None;
            }
            for (node, pos) in self.canvas.moved_nodes(&self.model.graph) {
                if let Some(spec) = self.model.graph.nodes.get_mut(&node) {
                    spec.pos = pos;
                }
                outbox.push(Command::MoveNode { node, pos });
            }
        }
        for command in outbox.commands {
            self.connection.send(command);
        }
        if self.model.connected {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
