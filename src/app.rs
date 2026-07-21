use crate::audio::{AudioCommand, AudioEvent};
use crate::config::Config; 
use anyhow::Result;
use std::sync::mpsc::{Receiver, Sender};

/// Wires together everything that lives on the tokio runtime
pub async fn run(cfg: Config, audio_tx: Sender<AudioCommand>, audio_events: Receiver<AudioEvent>) -> Result<()> {
    let client = riptune_core::SubsonicClient::new(&cfg.server.url, &cfg.server.username, &cfg.server.password)?;
    let cache = riptune_cache::Cache::open(&cfg.cache.path).await?;

    // Background: sync library index into the local cache
    let sync_client = client.clone();
    let sync_cache = cache.clone();
    tokio::spawn(async move {
        if let Err(e) = riptune_cache::sync_library(&sync_client, &sync_cache).await {
            tracing::warn!(error = %e, "library sync failed");
        }
    });

    if cfg.niri.enabled {
        let audio_tx = audio_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = riptune_niri::listen(audio_tx).await {
                tracing::warn!(error = %e, "niri IPC listener exited");
            }
        });
    }

    let mpris_handle = riptune_mpris::spawn(audio_tx.clone()).await?;

    // This invokes the actual TUI implementation from crates/riptune-tui
    riptune_tui::App::new(client, cache, audio_tx, audio_events).run().await?;

    mpris_handle.shutdown().await;
    Ok(())
}