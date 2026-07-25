//! Types shared across thread and crate boundaries.
//!
//! `AudioCommand`/`AudioEvent` cross the audio-thread channel. They live here
//! — deliberately dependency-free — so `riptune-mpris` and the `riptune`
//! binary share one definition instead of each inventing their own (which is
//! exactly the bug that broke the build previously: two different
//! `AudioCommand` types that looked identical but weren't).

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

/// User-overridable TUI colors, loaded from config.toml's `[theme]` table as
/// `#rrggbb` hex strings. Lives here (not in riptune-tui) so the main crate's
/// config loader can deserialize it directly and hand it to the TUI without
/// a dependency cycle. Any field left unset, or that fails to parse, falls
/// back to riptune-tui's built-in default for that color.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ThemeColors {
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub now_playing: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub border_focused: Option<String>,
    #[serde(default)]
    pub border_unfocused: Option<String>,
    #[serde(default)]
    pub selection_bg: Option<String>,
}
