//! Types shared across thread and crate boundaries.
//!
//! `AudioCommand`/`AudioEvent` cross the audio-thread channel. They live here
//! — deliberately dependency-free — so `riptune-mpris` and the `riptune`
//! binary share one definition instead of each inventing their own (which is
//! exactly the bug that broke the build previously: two different
//! `AudioCommand` types that looked identical but weren't).

use std::time::Duration;

/// Identifies one `Play` request. Every event about a specific track carries
/// the token of the `Play` that started it, so the TUI can drop events that
/// were already in flight when the user skipped to something else (otherwise
/// a late `Finished` from the old track would skip the new one too).
pub type PlayToken = u64;

#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play { stream_url: String, token: PlayToken },
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    /// Playback position, sent at most once per elapsed second (the UI only
    /// displays whole seconds, so finer updates would just be wasted redraws).
    Position(PlayToken, Duration),
    Finished(PlayToken),
    /// The track could not be fetched or decoded.
    LoadFailed(PlayToken, String),
    /// Anything else (device errors, failed seeks).
    Error(String),
}

/// User-overridable TUI colors, loaded from config.toml's `[theme]` table as
/// `#rrggbb` hex strings. Lives here (not in riptune-tui) so the main crate's
/// config loader can deserialize it directly and hand it to the TUI without
/// a dependency cycle. Any field left unset, or that fails to parse, falls
/// back to riptune-tui's built-in default for that color.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct ThemeColors {
    pub accent: Option<String>,
    pub now_playing: Option<String>,
    pub error: Option<String>,
    pub border_focused: Option<String>,
    pub border_unfocused: Option<String>,
    pub selection_bg: Option<String>,
}
