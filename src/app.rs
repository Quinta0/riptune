use crate::audio::{AudioCommand, AudioEvent};
use crate::config::Config;
use anyhow::Result;
use riptune_cache::SyncEvent;
use std::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedReceiver;

/// Wires together everything that lives on the tokio runtime:
///   - riptune_core::SubsonicClient  (REST API + streaming URLs)
///   - riptune_cache::Cache          (SQLite index, synced in the background)
///   - riptune_mpris::Server         (D-Bus MPRIS2 interface)
///   - riptune_tui::App              (ratatui render loop)
pub async fn run(cfg: Config, audio_tx: Sender<AudioCommand>, audio_events: UnboundedReceiver<AudioEvent>) -> Result<()> {
    let client = riptune_core::SubsonicClient::new(&cfg.server.url, &cfg.server.username, &cfg.server.password)?;
    let cache = riptune_cache::Cache::open(&cfg.cache.path).await?;

    // Background: sync the library index into the local cache. The TUI reads
    // only from the cache, so it opens instantly from the previous run's data
    // and picks up new entries as the sync reports progress.
    let (sync_tx, sync_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_client = client.clone();
    let sync_cache = cache.clone();
    let sync_task = tokio::spawn(async move {
        let progress_tx = sync_tx.clone();
        let result = riptune_cache::sync_library(&sync_client, &sync_cache, |done, total| {
            let _ = progress_tx.send(SyncEvent::Progress { done, total });
        })
        .await;
        let _ = sync_tx.send(match result {
            Ok(()) => SyncEvent::Finished,
            Err(e) => {
                tracing::warn!(error = %e, "library sync failed");
                SyncEvent::Failed(e.to_string())
            }
        });
    });

    // MPRIS is a nice-to-have; a missing session bus must not stop playback.
    let mpris_handle = match riptune_mpris::spawn().await {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(error = %e, "MPRIS unavailable");
            None
        }
    };

    let result = riptune_tui::App::new(client, cache, audio_tx.clone(), audio_events, sync_rx, cfg.theme.clone())
        .run()
        .await;

    sync_task.abort();
    if let Some(h) = mpris_handle {
        h.shutdown().await;
    }
    let _ = audio_tx.send(AudioCommand::Stop);

    result
}
