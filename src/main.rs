//! riptune — a niri-native TUI music client for Subsonic servers.
//!
//! Threading model (see README.md#architecture for the diagram):
//!   - Tokio runtime: Subsonic API calls, streaming downloads, MPRIS D-Bus server,
//!     niri IPC event listener. All async, all non-blocking.
//!   - Audio thread: owns the cpal/symphonia decode+playback pipeline. Talks to the
//!     rest of the app only via a bounded mpsc channel (commands in, position/state out).
//!   - TUI thread (main): ratatui render loop. Never blocks on I/O — reads from
//!     channels/shared state populated by the other two.

mod app;
mod audio;
mod config;
mod events;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "riptune", version, about = "Rip and tune: a TUI Subsonic client for niri")]
struct Cli {
    /// Path to config file (defaults to $XDG_CONFIG_HOME/riptune/config.toml)
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check connectivity and auth against the configured server, then exit.
    Ping,
    /// List all artists from the server, then exit. Useful for confirming
    /// auth + JSON parsing work before the TUI can display anything.
    Artists,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let cfg = crate::config::Config::load(cli.config)?;

    // Quick one-shot commands skip the audio thread, MPRIS, and TUI entirely —
    // they exist to smoke-test the Subsonic client in isolation.
    if let Some(command) = cli.command {
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(run_command(command, cfg));
    }

    // Audio runs on its own OS thread — decode/playback must not share a runtime
    // with network I/O, or a slow server response can stutter playback.
    let (audio_tx, audio_events, audio_handle) = audio::spawn(cfg.clone())?;

    // Everything async (Subsonic client, MPRIS, niri IPC) shares one tokio runtime.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(app::run(cfg, audio_tx, audio_events))?;

    audio_handle.join().ok();
    Ok(())
}

async fn run_command(command: Command, cfg: crate::config::Config) -> Result<()> {
    let client = riptune_core::SubsonicClient::new(&cfg.server.url, &cfg.server.username, &cfg.server.password)?;

    match command {
        Command::Ping => {
            client.ping().await?;
            println!("connected to {}", cfg.server.url);
        }
        Command::Artists => {
            let artists = client.get_artists().await?;
            for artist in &artists {
                println!("{:<10} {:<40} {} album(s)", artist.id, artist.name, artist.album_count);
            }
            println!("\n{} artist(s) total", artists.len());
        }
    }
    Ok(())
}
