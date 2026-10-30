//! The companion's voice window (egui): devices, mute / deafen, push-to-talk
//! or voice activity, and the room's players by display name with speaking
//! indicators and volume. Plain controls, no nested panels.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;

use crate::core::{Cmd, UiAction};

#[derive(Clone, Debug)]
pub struct Participant {
    pub id: String,
    pub name: String,
    pub speaking: bool,
    pub volume: f32,
    pub muted: bool,
}

#[derive(Default, Debug)]
pub struct Model {
    pub status: String,
    pub attached_as: Option<String>,
    pub signed_in: Option<String>,
    /// 0 left, 1 joining, 2 joined.
    pub voice_state: i32,
    pub muted: bool,
    pub deafened: bool,
    pub push_to_talk: bool,
    pub ptt_pressed: bool,
    pub local_speaking: bool,
    pub server_muted: bool,
    pub participants: Vec<Participant>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    /// The voice window once it exists (None: no window).
    pub window: Option<Window>,
    /// Show (true) or hide (false) the window: set by the game's
    /// `show_window`, applied by the window's next update.
    pub show_request: Option<bool>,
}

/// The voice window, as the core reaches it from its own thread.
#[derive(Clone, Debug)]
pub struct Window {
    ctx: egui::Context,
    /// The Win32 window handle (0: unknown).
    #[cfg(windows)]
    hwnd: isize,
}

/// Shows or hides the window (the game's `show_window`); nothing happens
/// without a window. The window's next update applies it. A hidden window
/// gets no repaint (Windows sends no WM_PAINT to it), so its update does not
/// run: showing it also asks Windows directly, and the update that follows
/// completes it (egui's own state, focus).
pub fn show(model: &Mutex<Model>, visible: bool) {
    let mut m = model.lock().unwrap_or_else(|p| p.into_inner());
    m.show_request = Some(visible);
    let Some(window) = &m.window else { return };
    #[cfg(windows)]
    if visible && window.hwnd != 0 {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindowAsync, SW_SHOWNORMAL};
        // SAFETY: `window` is set while the window exists and cleared under
        // this lock as soon as run_native returns (see run), so the handle
        // is never used once eframe let go of the window. ShowWindowAsync
        // only posts to the window's thread (no wait while holding the lock).
        unsafe { ShowWindowAsync(window.hwnd as _, SW_SHOWNORMAL) };
    }
    window.ctx.request_repaint();
}

#[cfg(windows)]
fn win32_handle(cc: &eframe::CreationContext<'_>) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => 0,
    }
}

struct App {
    model: Arc<Mutex<Model>>,
    cmd: UnboundedSender<Cmd>,
}

impl App {
    fn send(&self, action: UiAction) {
        let _ = self.cmd.send(Cmd::Ui(action));
    }
}

fn device_picker(
    ui: &mut egui::Ui,
    label: &str,
    names: &[String],
    selected: &Option<String>,
) -> Option<Option<String>> {
    let mut choice = None;
    let shown = selected.clone().unwrap_or_else(|| "System default".into());
    egui::ComboBox::from_label(label)
        .selected_text(shown)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(selected.is_none(), "System default")
                .clicked()
            {
                choice = Some(None);
            }
            for name in names {
                if ui
                    .selectable_label(selected.as_deref() == Some(name.as_str()), name)
                    .clicked()
                {
                    choice = Some(Some(name.clone()));
                }
            }
        });
    choice
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let snapshot = {
            let mut m = self.model.lock().unwrap_or_else(|p| p.into_inner());
            if ctx.input(|i| i.viewport().close_requested()) {
                // The close button only hides the window: voice and the
                // attached session follow the game, not the window (the
                // process ends with the game, see main).
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                tracing::info!("voice window hidden (close button)");
            } else if let Some(visible) = m.show_request.take() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
                if visible {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                tracing::info!(visible, "voice window shown or hidden by the game");
            }
            (
                m.status.clone(),
                m.attached_as.clone(),
                m.voice_state,
                m.muted,
                m.deafened,
                m.push_to_talk,
                m.local_speaking,
                m.server_muted,
                m.participants.clone(),
                m.inputs.clone(),
                m.outputs.clone(),
                m.input.clone(),
                m.output.clone(),
            )
        };
        let (
            status,
            attached_as,
            voice_state,
            muted,
            deafened,
            ptt,
            speaking,
            server_muted,
            players,
            inputs,
            outputs,
            input,
            output,
        ) = snapshot;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Voice");
            let attached = attached_as.is_some();
            let who = attached_as.map(|n| format!(" as {n}")).unwrap_or_default();
            ui.label(format!("{status}{who}"));
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                match voice_state {
                    2 => {
                        if ui.button("Leave voice").clicked() {
                            self.send(UiAction::Leave);
                        }
                    }
                    1 => {
                        ui.label("Joining…");
                    }
                    _ => {
                        // Voice needs an attached session (the game attaches one).
                        if ui
                            .add_enabled(attached, egui::Button::new("Join voice"))
                            .clicked()
                        {
                            self.send(UiAction::Join);
                        }
                    }
                }
                let mut m = muted;
                if ui.checkbox(&mut m, "Mute").changed() {
                    self.send(UiAction::SetMuted(m));
                }
                let mut d = deafened;
                if ui.checkbox(&mut d, "Deafen").changed() {
                    self.send(UiAction::SetDeafened(d));
                }
            });
            ui.horizontal(|ui| {
                let mut p = ptt;
                if ui.radio_value(&mut p, false, "Voice activity").changed()
                    || ui
                        .radio_value(&mut p, true, "Push to talk (in game)")
                        .changed()
                {
                    self.send(UiAction::SetPushToTalk(p));
                }
            });
            if speaking {
                ui.label("You are speaking");
            }
            if server_muted {
                ui.label("The game server muted you");
            }
            ui.add_space(8.0);
            if let Some(choice) = device_picker(ui, "Microphone", &inputs, &input) {
                self.send(UiAction::SelectDevice {
                    input: true,
                    name: choice,
                });
            }
            if let Some(choice) = device_picker(ui, "Speakers", &outputs, &output) {
                self.send(UiAction::SelectDevice {
                    input: false,
                    name: choice,
                });
            }
            ui.add_space(8.0);
            ui.separator();
            ui.label(if players.is_empty() {
                "Nobody else in voice".to_owned()
            } else {
                format!("In voice ({})", players.len())
            });
            for p in players {
                ui.horizontal(|ui| {
                    ui.label(if p.speaking { "●" } else { "○" });
                    ui.label(if p.name.is_empty() {
                        "Player".to_owned()
                    } else {
                        p.name.clone()
                    });
                    let mut volume = p.volume;
                    if ui
                        .add(egui::Slider::new(&mut volume, 0.0..=2.0).show_value(false))
                        .changed()
                    {
                        self.send(UiAction::SetVolume(p.id.clone(), volume));
                    }
                    let mut silenced = p.muted;
                    if ui.checkbox(&mut silenced, "Mute for me").changed() {
                        self.send(UiAction::SetParticipantMuted(p.id.clone(), silenced));
                    }
                });
            }
        });
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

/// Runs the window on this (the main) thread; it returns only if the window
/// cannot open. The close button hides it; the game shows it again
/// (`show_window`); the process ends with the game (see main).
pub fn run(model: Arc<Mutex<Model>>, cmd: UnboundedSender<Cmd>) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Gridspawn voice")
            .with_inner_size([360.0, 420.0]),
        ..Default::default()
    };
    let app_model = model.clone();
    let result = eframe::run_native(
        "Gridspawn voice",
        options,
        Box::new(move |cc| {
            let window = Window {
                ctx: cc.egui_ctx.clone(),
                #[cfg(windows)]
                hwnd: win32_handle(cc),
            };
            app_model.lock().unwrap_or_else(|p| p.into_inner()).window = Some(window);
            Ok(Box::new(App {
                model: app_model,
                cmd,
            }))
        }),
    );
    // The window is gone (also after an error mid-run): its handle must not
    // be used again (show).
    model.lock().unwrap_or_else(|p| p.into_inner()).window = None;
    if let Err(error) = result {
        tracing::warn!(%error, "voice window could not open; running without it");
    }
}
