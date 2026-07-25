//! MPRIS2 D-Bus server (org.mpris.MediaPlayer2.riptune).
//!
//! Implementing this is what lets waybar's mpris module, GNOME/KDE media
//! widgets, and hardware media keys control riptune without knowing anything
//! about Subsonic — they only ever speak the standard MPRIS interface.
//! Spec: <https://specifications.freedesktop.org/mpris-spec/latest/>

use zbus::{interface, ConnectionBuilder};

pub struct MprisHandle {
    conn: zbus::Connection,
}

impl MprisHandle {
    pub async fn shutdown(self) {
        drop(self.conn);
    }
}

// TODO: relay play_pause/next/previous to the audio thread's AudioCommand
// channel and reflect real playback state once that wiring exists.
struct Player;

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn play_pause(&self) {}

    async fn next(&self) {}

    async fn previous(&self) {}

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        "Stopped".into()
    }
}

pub async fn spawn() -> zbus::Result<MprisHandle> {
    let conn = ConnectionBuilder::session()?
        .name("org.mpris.MediaPlayer2.riptune")?
        .serve_at("/org/mpris/MediaPlayer2", Player)?
        .build()
        .await?;
    Ok(MprisHandle { conn })
}
