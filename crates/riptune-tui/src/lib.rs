//! ratatui front end. `App` (app.rs) owns state and behavior, `ui.rs`
//! renders it; this file is just terminal setup and the event loop.

mod app;
mod cover;
mod keymap;
mod queue;
mod text;
mod theme;
mod ui;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use riptune_cache::{Cache, SyncEvent};
use riptune_core::SubsonicClient;
use riptune_types::{AudioCommand, AudioEvent, ThemeColors};
use std::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedReceiver;

pub struct App {
    state: app::App,
    audio_events: UnboundedReceiver<AudioEvent>,
    sync_events: UnboundedReceiver<SyncEvent>,
}

impl App {
    pub fn new(
        client: SubsonicClient,
        cache: Cache,
        audio_tx: Sender<AudioCommand>,
        audio_events: UnboundedReceiver<AudioEvent>,
        sync_events: UnboundedReceiver<SyncEvent>,
        theme_colors: ThemeColors,
    ) -> Self {
        Self {
            state: app::App::new(client, cache, audio_tx, theme::Theme::from_config(&theme_colors)),
            audio_events,
            sync_events,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        // Image-protocol detection talks to the terminal, so it needs raw
        // mode but must happen before the alternate screen takes over.
        crossterm::terminal::enable_raw_mode()?;
        self.state.covers.detect();
        // ratatui::init also installs a panic hook that restores the
        // terminal, so a crash never leaves the shell in raw mode.
        let mut terminal = ratatui::init();
        let result = self.event_loop(&mut terminal).await;
        ratatui::restore();
        result
    }

    /// Redraws only when something changed. Idle CPU is ~zero; while a
    /// track plays the audio thread's once-per-second position update is
    /// the only wakeup.
    async fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let app = &mut self.state;
        app.load_library().await;

        let mut input = EventStream::new();
        let mut sync_open = true;
        let mut dirty = true;

        while !app.quit {
            if dirty {
                terminal.draw(|f| ui::draw(app, f))?;
                dirty = false;
            }
            let message_expiry = app.message.as_ref().map(|m| tokio::time::Instant::from_std(m.expires));

            tokio::select! {
                event = input.next() => match event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        app.on_key(key).await;
                        dirty = true;
                    }
                    Some(Ok(Event::Resize(..) | Event::FocusGained)) => dirty = true,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        app.error(format!("input error: {e}"));
                        dirty = true;
                    }
                    None => break,
                },
                Some(event) = self.audio_events.recv() => {
                    app.on_audio(event);
                    // Drain whatever else is queued so a burst becomes one frame.
                    while let Ok(event) = self.audio_events.try_recv() {
                        app.on_audio(event);
                    }
                    dirty = true;
                }
                event = self.sync_events.recv(), if sync_open => match event {
                    Some(event) => {
                        app.on_sync(event).await;
                        dirty = true;
                    }
                    None => sync_open = false,
                },
                Some((id, image)) = app.covers.rx.recv() => {
                    app.covers.on_loaded(id, image);
                    dirty = true;
                }
                _ = async { tokio::time::sleep_until(message_expiry.unwrap()).await }, if message_expiry.is_some() => {
                    app.message = None;
                    dirty = true;
                }
            }
        }
        Ok(())
    }
}
