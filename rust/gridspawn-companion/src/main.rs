//! `gridspawn-companion.exe`: the 64-bit half of the Gridspawn companion for
//! rAthena / Ragexe clients (docs/platforms/rathena-companion.md).
//!
//! Started by `gridspawn_rathena.dll`, which writes one JSON line to stdin
//! (`{v, token, gamePid, appId, environment, sdkBaseUrl?, logPath?,
//! showWindow, testTone}`). The companion serves the DLL over two local pipes
//! authenticated with that token, runs voice through the operating system's
//! audio devices, shows a small voice window, and exits with the game.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod core;
mod frames;
#[cfg(windows)]
mod pipes;
mod ui;

use std::{
    io::BufRead,
    sync::{Arc, Mutex},
};

use serde_json::Value;

/// What the DLL writes to stdin.
#[derive(Clone, Debug)]
pub struct Launch {
    pub token: String,
    pub game_pid: u32,
    pub app_id: String,
    pub environment: String,
    pub sdk_base_url: Option<String>,
    pub log_path: Option<String>,
    pub show_window: bool,
    /// Testing only: a tone instead of the microphone, nothing played. Only
    /// builds with the `testing` feature honour it; others ignore it.
    pub test_tone: bool,
    test_tone_requested: bool,
}

impl Launch {
    fn parse(line: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("v")?.as_u64()? != frames::VERSION {
            return None;
        }
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let token = text("token")?;
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let test_tone_requested = v.get("testTone").and_then(Value::as_bool).unwrap_or(false);
        Some(Self {
            test_tone: test_tone_requested && cfg!(feature = "testing"),
            test_tone_requested,
            token,
            game_pid: u32::try_from(v.get("gamePid")?.as_u64()?).ok()?,
            app_id: text("appId")?,
            environment: text("environment").unwrap_or_else(|| "production".into()),
            sdk_base_url: text("sdkBaseUrl"),
            log_path: text("logPath"),
            show_window: v.get("showWindow").and_then(Value::as_bool).unwrap_or(true),
        })
    }
}

fn init_logging(path: Option<&str>) {
    let Some(path) = path else { return };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .try_init();
}

fn main() {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        std::process::exit(2);
    }
    let Some(launch) = Launch::parse(line.trim()) else {
        std::process::exit(2);
    };
    init_logging(launch.log_path.as_deref());
    tracing::info!(
        game_pid = launch.game_pid,
        window = launch.show_window,
        "companion starting"
    );
    if launch.test_tone_requested && !launch.test_tone {
        tracing::warn!("testTone ignored: this build has no testing feature; using the microphone");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => std::process::exit(3),
    };
    let model = Arc::new(Mutex::new(ui::Model::default()));
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let launch = launch.clone();
        let model = model.clone();
        let cmd_tx = cmd_tx.clone();
        runtime.spawn(async move {
            core::run(launch, model, cmd_tx, cmd_rx).await;
            let _ = done_tx.send(());
        });
    }
    core::watch_game(launch.game_pid, cmd_tx.clone());

    // The process ends when the core does (game exit, gs_rag_shutdown, pipe
    // closed), whatever the window's state: the window never ends it, and a
    // hidden window would not see a request to close (it gets no repaint).
    let finisher = std::thread::Builder::new()
        .name("gridspawn-companion-exit".into())
        .spawn(move || {
            let _ = runtime.block_on(done_rx);
            tracing::info!("companion exiting");
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            std::process::exit(0);
        });
    let Ok(finisher) = finisher else {
        std::process::exit(3);
    };
    if launch.show_window {
        ui::run(model.clone(), cmd_tx.clone());
    }
    let _ = finisher.join();
    std::process::exit(0);
}
