//! riptune — a niri-native TUI music client for Subsonic servers.

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
    init_logging()?;
    let cli = Cli::parse();

    let cfg = config::Config::load(cli.config)?;

    if let Some(command) = cli.command {
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(run_command(command, cfg));
    }

    let (audio_tx, audio_events, _audio_handle) = audio::spawn(cfg.clone())?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        tokio::select! {
            res = app::run(cfg, audio_tx, audio_events) => res,
            _ = tokio::signal::ctrl_c() => {
                // When Ctrl+C is pressed, returning Ok(()) drops the TUI instance,
                // triggering the `TerminalCleanup` guard in riptune-tui automatically!
                Ok(())
            }
        }
    })
}

// Logs must never land on stdout/stderr: the TUI owns the terminal's
// alternate screen, and any interleaved write (e.g. symphonia's demuxer
// warnings) punches straight through the ratatui frame.
fn init_logging() -> Result<()> {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(".local/state")
        })
        .join("riptune");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("riptune.log"))?;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,symphonia=warn,reqwest=warn,zbus=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .init();

    Ok(())
}

async fn run_command(command: Command, cfg: config::Config) -> Result<()> {
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