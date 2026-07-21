//! Types shared across thread and crate boundaries.
//!
//! `AudioCommand`/`AudioEvent` cross the audio-thread channel; `AppEvent`
//! crosses between the TUI, MPRIS, and niri IPC layers. They live here —
//! deliberately dependency-free — so `riptune-mpris`, `riptune-niri`, and the
//! `riptune` binary all share one definition instead of each inventing their
//! own (which is exactly the bug that broke the build previously: two
//! different `AudioCommand` types that looked identical but weren't).

use std::time::Duration;

#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play { stream_url: String },
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    PositionChanged(Duration),
    TrackFinished,
    Buffering,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// User favorited/unfavorited a track (mirrors Subsonic star/unstar + can be
    /// triggered from a niri keybind, MPRIS client, or the TUI itself).
    ToggleFavorite { track_id: String },
    /// Fired by the niri IPC listener when the focused workspace changes.
    WorkspaceChanged { name: Option<String> },
    NextTrack,
    PreviousTrack,
}
