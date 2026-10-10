#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod canvas;
mod client;
mod model;
mod widgets;

fn main() -> eframe::Result<()> {
    env_logger::init();
    let address = nodeaudio_proto::transport::default_address();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("nodeaudio")
            .with_app_id("nodeaudio")
            .with_inner_size([1200.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "nodeaudio",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let connection = client::Connection::start(address, move || ctx.request_repaint());
            Ok(Box::new(app::App::new(connection)))
        }),
    )
}
