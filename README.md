# riptune

*Rip and tune.* A terminal-native music client for [Subsonic-compatible](http://www.subsonic.org/pages/api.jsp) servers (Navidrome, Airsonic, Gonic), built for the [niri](https://github.com/YaLTeR/niri) scrollable-tiling compositor. Because if id Software's engine can run on a smart fridge, your music player can run on a compositor IPC socket.

## Why

Most terminal music clients treat the desktop as a black box: no MPRIS, no awareness of tiling layout, no way to hook into compositor-level keybinds. `riptune` is built the other way around, it assumes it's running inside a modern Wayland tiling setup and integrates accordingly.

## Features

- **MPRIS2 D-Bus interface** — waybar, GNOME/KDE media widgets, and hardware media keys control riptune out of the box, no riptune-specific config needed on their end.
- **Decoupled async pipeline** — network I/O (tokio) and audio decode/playback (dedicated OS thread) never share a runtime, so a slow server response can't stutter playback or freeze the UI.
- **niri IPC integration** — listens on niri's event-stream socket (not polling) to react to workspace changes; exposes a local control socket so niri keybinds can trigger in-app actions like favoriting a track.
- **Local SQLite index** — the remote library is mirrored locally so the TUI opens instantly, even on servers with slow `getArtists`/`getAlbum` responses over large libraries; a fresh library sync fetches artists/albums/tracks concurrently rather than one request at a time.
- **Inline cover art** — renders actual album art in the Now Playing panel via the Kitty/iTerm2/Sixel graphics protocol where the terminal supports it, falling back to a half-block truecolor approximation otherwise.
- **Soundtrack-aware track order** — albums list by disc/track number, not alphabetically, so a game or film OST plays in the order the composer intended.
- **Live search with match highlighting** — search narrows the Artists/Albums/Tracks pane it targets in real time, with the matched substring highlighted in each result.
- **Configurable theme** — an optional `[theme]` table in `config.toml` overrides individual UI colors (accent, now-playing, borders, ...) without touching code.

## Terminal UI

![riptune TUI screenshot](image.png)

Three browsing panes (Artists / Albums / Tracks) whose widths shift to favor whichever one is focused, a search bar, and a unified Now Playing deck (art, title/artist/album, a horizontal progress bar, and volume/shuffle/repeat) with a hotkey legend along the bottom.

| Key | Action |
| --- | --- |
| `j` / `k` / `↓` / `↑` | Move selection |
| `h` / `l` / `←` / `→` / `Enter` | Move between panes / expand selection / play |
| `Space` | Play / pause |
| `/` | Search (scope cycles with `Tab`; `Esc`/`Enter` to leave) |
| `s` | Toggle shuffle |
| `r` | Cycle repeat (off → queue → track) |
| `+` / `-` | Volume up / down |
| `?` | Toggle the help overlay |
| `q` | Quit |

## Architecture

```mermaid
flowchart TB
    subgraph Main["Main Thread"]
        TUI["TUI (ratatui)<br/>render loop, input handling"]
    end

    subgraph AudioThread["Dedicated OS Thread"]
        Decoder["symphonia decoder"]
        Sink["cpal output stream"]
        Decoder --> Sink
    end

    subgraph Tokio["Tokio Runtime"]
        Subsonic["Subsonic client<br/>(reqwest)"]
        Cache["SQLite cache<br/>(sqlx)"]
        Mpris["MPRIS2 server<br/>(zbus)"]
        Niri["niri IPC listener<br/>(event-stream socket)"]
    end

    TUI <-- "AppEvent (mpsc)" --> Tokio
    TUI <-- "reads local state" --> Cache
    Tokio -- "AudioCommand (mpsc)" --> AudioThread
    AudioThread -- "AudioEvent: position, buffering" --> TUI
    Subsonic -- "sync_library()" --> Cache
    Subsonic -- "authenticated stream URL" --> AudioThread
    Mpris -- "translates D-Bus calls into" --> Tokio
    Niri -- "WorkspaceChanged" --> TUI

    External1["waybar / notification daemon"] -.->|D-Bus| Mpris
    External2["niri compositor"] -.->|IPC socket| Niri
```

**Why three execution contexts instead of one?** Audio playback is latency-sensitive in a way that async task scheduling doesn't guarantee a busy tokio runtime (e.g. mid-sync with the server) shouldn't be able to introduce a buffer underrun. Giving audio its own OS thread with a bounded channel as the only interface makes that impossible by construction, not by careful scheduling.

## Repository layout

```
riptune/
├── src/                 # binary crate: wires everything together
│   ├── main.rs
│   ├── app.rs           # async orchestration (tokio runtime)
│   ├── audio.rs         # audio thread entrypoint
│   ├── config.rs
│   └── events.rs
├── crates/
│   ├── riptune-core/     # Subsonic REST client, auth, models
│   ├── riptune-cache/    # SQLite local index
│   ├── riptune-mpris/    # MPRIS2 D-Bus server
│   ├── riptune-niri/     # niri IPC event-stream listener
│   ├── riptune-tui/      # ratatui UI
│   └── riptune-types/    # shared types crossing thread/crate boundaries
├── Cargo.toml            # workspace root
└── config.example.toml
```

## Configuration

`~/.config/riptune/config.toml`:

```toml
[server]
url = "https://music.example.com"
username = "you"
# Either a legacy Subsonic password (salted-token auth is computed at runtime,
# never sent in the clear) or a Navidrome API key, depending on server support.
password = "your-password-or-api-key"

[niri]
enabled = true
workspace_notifications = true

[cache]
path = "~/.cache/riptune/library.sqlite"

# Optional. Every key is optional too -- anything left out keeps riptune's
# built-in default for that color. Values are "#rrggbb" hex.
# [theme]
# accent = "#89b4fa"
# now_playing = "#a6e3a1"
# error = "#f38ba8"
# border_focused = "#89b4fa"
# border_unfocused = "#585b70"
# selection_bg = "#313244"
```

## Building

```sh
cargo build --release
```

Requires a D-Bus session bus (for MPRIS) and, optionally, a running niri instance (for IPC features — riptune degrades gracefully without them).

