//! niri IPC integration.
//!
//! Important distinction: `niri msg <cmd>` is for one-off commands (fine for
//! e.g. a keybind that shells out to `riptune favorite`). For *reactive*
//! behavior — adjusting the TUI or firing a notification when the focused
//! workspace changes — we want the persistent event-stream socket, not
//! polling `niri msg` in a loop. The `niri-ipc` crate exposes both; this
//! module only uses the event stream.
//!
//! NOTE: pin the `niri-ipc` crate version to match your installed niri
//! release — the IPC protocol has changed across niri versions and is not
//! guaranteed stable yet.

use riptune_types::AudioCommand;
use std::sync::mpsc::Sender;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NiriError {
    #[error("failed to connect to niri IPC socket: {0}")]
    Connect(#[from] std::io::Error),
}

pub async fn listen(_audio_tx: Sender<AudioCommand>) -> Result<(), NiriError> {
    // TODO:
    // 1. Connect to the socket at $NIRI_SOCKET (niri sets this env var).
    // 2. Send the `EventStream` request per niri-ipc's protocol.
    // 3. Loop reading newline-delimited JSON events off the socket.
    // 4. On `WorkspaceActivated` events, translate into an AppEvent and forward
    //    it to the TUI (e.g. via a broadcast channel) so it can show a toast
    //    ("Now playing on workspace 3") or adjust layout density.
    //
    // Custom bindings (e.g. "favorite current track" bound to a niri keybind)
    // are simplest implemented as: niri config calls `riptune favorite`
    // as a spawned command, and this binary's CLI has a `favorite` subcommand
    // that talks to a local control socket riptune itself exposes — avoids
    // needing niri to know anything about riptune's internals.
    std::future::pending::<()>().await;
    Ok(())
}
