use crate::audio::{AudioCommand, AudioEvent};
use crate::config::Config;
use anyhow::Result;
use std::sync::mpsc::{Receiver, Sender};

/// Wires together everything that lives on the tokio runtime:
///   - riptune_core::SubsonicClient  (REST API + streaming URLs)
///   - riptune_cache::Cache          (SQLite index, warmed on startup)
///   - riptune_mpris::Server         (D-Bus MPRIS2 interface)
///   - riptune_tui::App              (ratatui render loop)
///
/// The TUI is the only piece that technically needs to run on the main thread on
/// some terminal backends, so it's driven from here rather than spawned as a task.
pub async fn run(cfg: Config, audio_tx: Sender<AudioCommand>, audio_events: Receiver<AudioEvent>) -> Result<()> {
    let client = riptune_core::SubsonicClient::new(&cfg.server.url, &cfg.server.username, &cfg.server.password)?;
    let cache = riptune_cache::Cache::open(&cfg.cache.path).await?;

    // Background: sync library index into the local cache so the TUI opens instantly
    // on subsequent runs, and refreshes lazily rather than blocking startup.
    let sync_client = client.clone();
    let sync_cache = cache.clone();
    let sync_task = tokio::spawn(async move {
        if let Err(e) = riptune_cache::sync_library(&sync_client, &sync_cache).await {
            tracing::warn!(error = %e, "library sync failed");
        }
    });

    let mpris_handle = riptune_mpris::spawn().await?;

    riptune_tui::App::new(client, cache, audio_tx.clone(), audio_events, cfg.theme.clone()).run().await?;

    sync_task.abort();
    mpris_handle.shutdown().await;

    let _ = audio_tx.send(AudioCommand::Stop);

    Ok(())
}