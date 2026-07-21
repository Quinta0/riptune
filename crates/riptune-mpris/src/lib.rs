//! MPRIS2 D-Bus server (org.mpris.MediaPlayer2.riptune).
//!
//! Implementing this is what lets waybar's mpris module, GNOME/KDE media
//! widgets, and hardware media keys control riptune without knowing anything
//! about Subsonic — they only ever speak the standard MPRIS interface.
//! Spec: <https://specifications.freedesktop.org/mpris-spec/latest/>

use riptune_types::AudioCommand;
use std::sync::mpsc::Sender;
use zbus::{interface, ConnectionBuilder};

pub struct MprisHandle {
    conn: zbus::Connection,
}

impl MprisHandle {
    pub async fn shutdown(self) {
        drop(self.conn);
    }
}

struct Player {
    // Commands are relayed to the audio thread over this channel; MPRIS itself
    // never touches audio state directly.
    #[allow(dead_code)]
    audio_tx: Sender<AudioCommand>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn play_pause(&self) {
        // TODO: relay to audio_tx based on current playback state
    }

    async fn next(&self) {
        // TODO: emit AppEvent::NextTrack for app.rs to handle (advance in
        // playlist, fetch new stream_url, send AudioCommand::Play)
    }

    async fn previous(&self) {
        // TODO
    }

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        "Stopped".into() // TODO: reflect real state
    }
}

pub async fn spawn(audio_tx: Sender<AudioCommand>) -> zbus::Result<MprisHandle> {
    let player = Player { audio_tx };
    let conn = ConnectionBuilder::session()?
        .name("org.mpris.MediaPlayer2.riptune")?
        .serve_at("/org/mpris/MediaPlayer2", player)?
        .build()
        .await?;
    Ok(MprisHandle { conn })
}
