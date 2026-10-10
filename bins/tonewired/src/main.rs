#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod backend;
mod daemon;
mod history;
mod launch;
mod persist;
mod server;
mod tray;

use std::path::PathBuf;
use std::process::ExitCode;

use backend::BackendChoice;
use daemon::{Daemon, Inbound, Options};
use persist::Store;

const USAGE: &str = "\
tonewired: the tonewire routing daemon

Usage: tonewired [options]

Options:
  --backend <auto|null>   Audio backend (default: auto)
  --config-dir <dir>      Where graph.toml and settings.toml live
  --socket <address>      IPC socket path or pipe name
  --no-tray               Do not show a tray icon
  --autostart <on|off>    Start the daemon at login, then exit
  --version               Print the version
  --help                  Print this help
";

struct Args {
    backend: BackendChoice,
    config_dir: Option<PathBuf>,
    socket: Option<String>,
    tray: bool,
    autostart: Option<bool>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args {
        backend: BackendChoice::Auto,
        config_dir: None,
        socket: None,
        tray: true,
        autostart: None,
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--backend" => parsed.backend = value("--backend")?.parse()?,
            "--config-dir" => parsed.config_dir = Some(value("--config-dir")?.into()),
            "--socket" => parsed.socket = Some(value("--socket")?),
            "--no-tray" => parsed.tray = false,
            "--autostart" => {
                parsed.autostart = Some(match value("--autostart")?.as_str() {
                    "on" => true,
                    "off" => false,
                    other => return Err(format!("--autostart takes on or off, not `{other}`")),
                })
            }
            "--version" => {
                println!("tonewired {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(None);
            }
            other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
        }
    }
    Ok(Some(parsed))
}

fn default_config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "tonewire")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".tonewire"))
}

fn init_logging(config_dir: &std::path::Path) {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if cfg!(windows) {
        let _ = std::fs::create_dir_all(config_dir);
        if let Ok(file) = std::fs::File::create(config_dir.join("tonewired.log")) {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Some(enabled) = args.autostart {
        return match autostart::set(enabled) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("cannot change autostart: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let config_dir = args.config_dir.unwrap_or_else(default_config_dir);
    init_logging(&config_dir);
    let store = match Store::new(config_dir) {
        Ok(store) => store,
        Err(e) => {
            log::error!("cannot create config directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let options = Options {
        backend: args.backend,
        socket: args
            .socket
            .unwrap_or_else(tonewire_proto::transport::default_address),
        store,
    };
    let external = tonewire_nvafx::factory(options.store.load_settings().nvafx_sdk.as_deref());
    let mut daemon = match Daemon::new(options, external) {
        Ok(daemon) => daemon,
        Err(e) => {
            log::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = daemon.serve() {
        log::error!("cannot listen for editors: {e}");
        return ExitCode::FAILURE;
    }
    let inbox = daemon.sender();
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = inbox.send(Inbound::Terminate);
    }) {
        log::warn!("cannot install signal handler: {e}");
    }
    if args.tray
        && let Some(tray) = tray::spawn(daemon.sender())
    {
        daemon.set_tray(tray);
    }
    daemon.run();
    ExitCode::SUCCESS
}
