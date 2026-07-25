mod theme;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use image::DynamicImage;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Terminal;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use riptune_cache::Cache;
use riptune_core::models::{AlbumSummary, Artist, Track};
use riptune_core::SubsonicClient;
use riptune_types::{AudioCommand, AudioEvent};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum FocusPane {
    Artists,
    Albums,
    Tracks,
}

#[derive(Clone, Copy, PartialEq)]
enum SearchScope {
    Artist,
    Album,
    Song,
}

#[derive(Clone, Copy, PartialEq)]
enum RepeatMode {
    Off,
    One,
    All,
}

#[derive(PartialEq)]
pub enum PlaybackStatus {
    Buffering,
    Playing,
    Paused,
}

enum InputMode {
    Normal,
    Search,
}

struct NowPlaying {
    title: String,
    artist: String,
    album: String,
    position: Duration,
    duration_secs: Option<u32>,
    status: PlaybackStatus,
    current_track_id: String,
    playlist_context: Vec<Track>,
    cover_art_id: Option<String>,
}

pub struct App {
    client: SubsonicClient,
    cache: Cache,
    audio_tx: Sender<AudioCommand>,
    audio_events: Receiver<AudioEvent>,

    // Library Views (Cached Locally)[cite: 7]
    artists: Vec<Artist>,
    albums: Vec<AlbumSummary>,
    tracks: Vec<Track>,

    // Dynamic Navigation States[cite: 7]
    focus: FocusPane,
    selected_artist: Option<Artist>,
    selected_album: Option<AlbumSummary>,
    
    // Core States
    selected_idx: usize,
    now_playing: Option<NowPlaying>,
    status_line: Option<String>,
    volume: f32,
    input_mode: InputMode,
    search_query: String,
    search_scope: SearchScope,
    show_help: bool,

    // Playback Engine Mods
    shuffle: bool,
    repeat: RepeatMode,

    // List scroll state, persisted across frames (see `draw`) so the
    // viewport doesn't reset to the top on every render.
    artists_state: ListState,
    albums_state: ListState,
    tracks_state: ListState,

    // Cover art. `cover_picker` is None if the terminal doesn't support (or
    // detection of) an image protocol failed -- the art panel is then just
    // skipped rather than erroring. Fetches run on the tokio runtime and
    // report back over `cover_art_rx`; `cover_in_flight` stops the same id
    // from being fetched twice while a request is outstanding.
    cover_picker: Option<Picker>,
    cover_cache: HashMap<String, Box<dyn StatefulProtocol>>,
    cover_in_flight: HashSet<String>,
    cover_art_tx: tokio::sync::mpsc::UnboundedSender<(String, Option<DynamicImage>)>,
    cover_art_rx: tokio::sync::mpsc::UnboundedReceiver<(String, Option<DynamicImage>)>,

    theme: theme::Theme,
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

impl App {
    pub fn new(
        client: SubsonicClient,
        cache: Cache,
        audio_tx: Sender<AudioCommand>,
        audio_events: Receiver<AudioEvent>,
        theme_colors: riptune_types::ThemeColors,
    ) -> Self {
        let (cover_art_tx, cover_art_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            client,
            cache,
            audio_tx,
            audio_events,
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
            focus: FocusPane::Artists,
            selected_artist: None,
            selected_album: None,
            selected_idx: 0,
            now_playing: None,
            status_line: Some("Initializing Offline Cache...".into()),
            volume: 0.8,
            input_mode: InputMode::Normal,
            search_query: String::new(),
            search_scope: SearchScope::Song,
            show_help: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            artists_state: ListState::default(),
            albums_state: ListState::default(),
            tracks_state: ListState::default(),
            cover_picker: None,
            cover_cache: HashMap::new(),
            cover_in_flight: HashSet::new(),
            cover_art_tx,
            cover_art_rx,
            theme: theme::Theme::from_config(&theme_colors),
        }
    }

    pub async fn run(mut self) -> Result<()> {
        enable_raw_mode()?;

        // Best-effort: query the terminal for an image graphics protocol
        // (Kitty/iTerm2/Sixel, falling back to unicode half-blocks). If
        // detection fails outright, cover art is simply not shown.
        self.cover_picker = Picker::from_termios()
            .ok()
            .map(|mut picker| {
                picker.guess_protocol();
                picker
            });

        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let _cleanup_guard = TerminalCleanup;

        let result = self.event_loop(&mut terminal).await;

        // Explicit cleanup before returning
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }

    async fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
        if let Ok(records) = self.cache.search_local_artists("").await {
            self.artists = records;
            self.status_line = None;
        }

        let mut input = EventStream::new();
        let mut ticker = tokio::time::interval(Duration::from_millis(200));

        loop {
            while let Ok(event) = self.audio_events.try_recv() {
                self.apply_audio_event(event);
            }

            terminal.draw(|f| self.draw(f))?;

            tokio::select! {
                maybe_event = input.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            if self.handle_key(key.code).await {
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            self.status_line = Some(format!("input error: {e}"));
                        }
                        None => break,
                        _ => {}
                    }
                }
                _ = ticker.tick() => {}
                Some((cover_id, image)) = self.cover_art_rx.recv() => {
                    self.cover_in_flight.remove(&cover_id);
                    if let (Some(image), Some(picker)) = (image, &mut self.cover_picker) {
                        self.cover_cache.insert(cover_id, picker.new_resize_protocol(image));
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_key(&mut self, code: KeyCode) -> bool {
        match self.input_mode {
            InputMode::Normal => match code {
                KeyCode::Char('q') => return true,
                KeyCode::Char('?') => self.show_help = !self.show_help,
                KeyCode::Char('/') => {
                    if !self.show_help {
                        self.input_mode = InputMode::Search;
                    }
                }
                // Shuffle & Repeat Controls
                KeyCode::Char('s') => {
                    self.shuffle = !self.shuffle;
                }
                KeyCode::Char('r') => {
                    self.repeat = match self.repeat {
                        RepeatMode::Off => RepeatMode::All,
                        RepeatMode::All => RepeatMode::One,
                        RepeatMode::One => RepeatMode::Off,
                    };
                }
                KeyCode::Esc | KeyCode::Backspace => self.go_back().await,
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Left | KeyCode::Char('h') => self.move_pane(-1).await,
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.activate_selection().await,
                KeyCode::Char(' ') => self.toggle_play_pause(),
                KeyCode::Char(']') | KeyCode::Char('+') => {
                    self.volume = (self.volume + 0.05).min(1.0);
                    let _ = self.audio_tx.send(AudioCommand::SetVolume(self.volume));
                }
                KeyCode::Char('[') | KeyCode::Char('-') => {
                    self.volume = (self.volume - 0.05).max(0.0);
                    let _ = self.audio_tx.send(AudioCommand::SetVolume(self.volume));
                }
                _ => {}
            },
            InputMode::Search => match code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Tab => {
                    self.search_scope = match self.search_scope {
                        SearchScope::Song => SearchScope::Artist,
                        SearchScope::Artist => SearchScope::Album,
                        SearchScope::Album => SearchScope::Song,
                    };
                    let query = self.search_query.clone();
                    self.execute_db_search(&query).await;
                }
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                    let query = self.search_query.clone();
                    self.execute_db_search(&query).await;
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                    let query = self.search_query.clone();
                    self.execute_db_search(&query).await;
                }
                _ => {}
            },
        }
        false
    }

    async fn execute_db_search(&mut self, query: &str) {
        self.selected_idx = 0;
        
        // If the user clears the search query, reset the panels to show everything parsed so far
        if query.is_empty() {
            if let Ok(records) = self.cache.search_local_artists("").await {
                self.artists = records;
            }
            self.albums.clear();
            self.tracks.clear();
            self.focus = FocusPane::Artists;
            return;
        }

        match self.search_scope {
            SearchScope::Artist => {
                if let Ok(records) = self.cache.search_local_artists(query).await {
                    self.artists = records;
                    self.focus = FocusPane::Artists;
                }
            }
            SearchScope::Album => {
                if let Ok(records) = self.cache.search_local_albums(query).await {
                    self.albums = records;
                    self.focus = FocusPane::Albums;
                }
            }
            SearchScope::Song => {
                // FIXED: Global song searches fill the right-hand Tracks window directly!
                if let Ok(records) = self.cache.search_local_tracks(query).await {
                    self.tracks = records;
                    self.focus = FocusPane::Tracks;
                }
            }
        }
    }

    async fn move_pane(&mut self, direction: i32) {
        self.selected_idx = 0;
        if direction > 0 {
            self.focus = match self.focus {
                FocusPane::Artists => FocusPane::Albums,
                FocusPane::Albums => FocusPane::Tracks,
                FocusPane::Tracks => FocusPane::Tracks,
            };
        } else {
            self.focus = match self.focus {
                FocusPane::Artists => FocusPane::Artists,
                FocusPane::Albums => FocusPane::Artists,
                FocusPane::Tracks => FocusPane::Albums,
            };
        }
    }

    async fn go_back(&mut self) {
        self.search_query.clear();
        self.move_pane(-1).await;
    }

    fn current_len(&self) -> usize {
        match self.focus {
            FocusPane::Artists => self.artists.len(),
            FocusPane::Albums => self.albums.len(),
            FocusPane::Tracks => self.tracks.len(),
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.current_len();
        if len == 0 { return; }
        let next = (self.selected_idx as i32 + delta).rem_euclid(len as i32);
        self.selected_idx = next as usize;
    }

    async fn activate_selection(&mut self) {
        match self.focus {
            FocusPane::Artists => {
                if let Some(artist) = self.artists.get(self.selected_idx).cloned() {
                    self.selected_artist = Some(artist.clone());
                    if let Ok(records) = self.cache.get_albums_by_artist(&artist.id).await {
                        self.albums = records;
                        self.focus = FocusPane::Albums;
                        self.selected_idx = 0;
                    }
                }
            }
            FocusPane::Albums => {
                if let Some(album) = self.albums.get(self.selected_idx).cloned() {
                    self.selected_album = Some(album.clone());
                    if let Ok(records) = self.cache.get_tracks_by_album(&album.name).await {
                        self.tracks = records;
                        self.focus = FocusPane::Tracks;
                        self.selected_idx = 0;
                    }
                }
            }
            FocusPane::Tracks => {
                if let Some(track) = self.tracks.get(self.selected_idx).cloned() {
                    self.play_track(track.clone(), self.tracks.clone());
                }
            }
        }
    }

    fn play_track(&mut self, track: Track, playlist_context: Vec<Track>) {
        match self.client.stream_url(&track.id) {
            Ok(stream_url) => {
                let _ = self.audio_tx.send(AudioCommand::Play { stream_url });
                if let Some(cover_id) = &track.cover_art {
                    self.request_cover_art(cover_id.clone());
                }
                self.now_playing = Some(NowPlaying {
                    title: track.title.clone(),
                    artist: track.artist.clone().unwrap_or_default(),
                    album: track.album.clone().unwrap_or_default(),
                    position: Duration::ZERO,
                    duration_secs: track.duration_secs,
                    status: PlaybackStatus::Buffering,
                    current_track_id: track.id.clone(),
                    playlist_context,
                    cover_art_id: track.cover_art.clone(),
                });
            }
            Err(e) => {
                self.status_line = Some(format!("failed to build stream url: {e}"));
            }
        }
    }

    /// Kicks off a background fetch+decode for a cover art id, unless it's
    /// already cached or a fetch for it is already in flight. Errors are
    /// swallowed -- missing art just means the panel stays empty.
    fn request_cover_art(&mut self, cover_id: String) {
        if self.cover_picker.is_none() || self.cover_cache.contains_key(&cover_id) || self.cover_in_flight.contains(&cover_id) {
            return;
        }
        self.cover_in_flight.insert(cover_id.clone());

        let client = self.client.clone();
        let tx = self.cover_art_tx.clone();
        tokio::spawn(async move {
            let image = match client.get_cover_art(&cover_id).await {
                Ok(bytes) => image::load_from_memory(&bytes).ok(),
                Err(_) => None,
            };
            let _ = tx.send((cover_id, image));
        });
    }

    fn toggle_play_pause(&mut self) {
        if let Some(now_playing) = &mut self.now_playing {
            match &now_playing.status {
                PlaybackStatus::Playing => {
                    let _ = self.audio_tx.send(AudioCommand::Pause);
                    now_playing.status = PlaybackStatus::Paused;
                }
                PlaybackStatus::Paused => {
                    let _ = self.audio_tx.send(AudioCommand::Resume);
                    now_playing.status = PlaybackStatus::Playing;
                }
                _ => {}
            }
        } else if let Some(track) = self.tracks.get(self.selected_idx).cloned() {
            self.play_track(track, self.tracks.clone());
        }
    }

    fn apply_audio_event(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Buffering => {
                if let Some(np) = &mut self.now_playing { np.status = PlaybackStatus::Buffering; }
            }
            AudioEvent::PositionChanged(pos) => {
                if let Some(np) = &mut self.now_playing {
                    np.position = pos;
                    np.status = PlaybackStatus::Playing;
                }
            }
            AudioEvent::TrackFinished => {
                if let Some(np) = self.now_playing.take() {
                    let current_id = np.current_track_id;
                    let context = np.playlist_context;
                    
                    if self.repeat == RepeatMode::One {
                        if let Some(track) = context.iter().find(|t| t.id == current_id).cloned() {
                            self.play_track(track, context);
                            return;
                        }
                    }

                    if self.shuffle && !context.is_empty() {
                        // Clean fallback: Use a timestamp to pick a random index without needing the 'rand' crate dependency
                        let pseudo_random_idx = (std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros() as usize) % context.len();
                        
                        if let Some(random_track) = context.get(pseudo_random_idx).cloned() {
                            self.play_track(random_track, context);
                            return;
                        }
                    } else if let Some(current_idx) = context.iter().position(|t| t.id == current_id) {
                        let next_idx = current_idx + 1;
                        if next_idx < context.len() {
                            if let Some(next_track) = context.get(next_idx).cloned() {
                                self.play_track(next_track, context);
                                return;
                            }
                        } else if self.repeat == RepeatMode::All {
                            if let Some(first_track) = context.first().cloned() {
                                self.play_track(first_track, context);
                                return;
                            }
                        }
                    }
                }
                self.now_playing = None;
            }
            AudioEvent::Error(message) => {
                self.status_line = Some(format!("playback error: {message}"));
            }
        }
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();

        // --- 1. Layout Resizing Shield ---
        // Guarantees zero terminal rendering crashes if constraint requirements fall below boundaries
        if area.width < 85 || area.height < 18 {
            let emergency_msg = Paragraph::new(
                "\n\n  ⚠️ Terminal Window Too Small!\n  ===============================\n  Please enlarge your screen layout dimensions\n  to display Riptune's multi-tier grids safely."
            )
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(self.theme.error)));
            f.render_widget(emergency_msg, area);
            return;
        }

        let main_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(7),
                Constraint::Length(1),
            ])
            .split(area);

        // Give the focused pane the room -- with a single artist and a
        // handful of albums, an equal three-way split leaves the two
        // breadcrumb panes mostly empty while Tracks (denser, especially
        // once titles carry track numbers + durations) is cramped.
        let (artists_pct, albums_pct, tracks_pct) = match self.focus {
            FocusPane::Artists => (50, 30, 20),
            FocusPane::Albums => (25, 50, 25),
            FocusPane::Tracks => (20, 20, 60),
        };
        let columns_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(artists_pct),
                Constraint::Percentage(albums_pct),
                Constraint::Percentage(tracks_pct),
            ])
            .split(main_layout[0]);

        let now_playing_id = self.now_playing.as_ref().map(|np| np.current_track_id.as_str());

        // Whether each pane's contents are currently the result of an
        // active search in that pane's scope -- drives both the "Matching:
        // N" title phrasing and per-row substring highlighting below. The
        // pane's list itself is already filtered to matches by
        // `execute_db_search`; this only affects how it's *labeled/shown*.
        let searching = matches!(self.input_mode, InputMode::Search) && !self.search_query.is_empty();
        let searching_artists = searching && self.search_scope == SearchScope::Artist;
        let searching_albums = searching && self.search_scope == SearchScope::Album;
        let searching_tracks = searching && self.search_scope == SearchScope::Song;
        let match_style = Style::default().fg(self.theme.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

        let artists_col_width = columns_layout[0].width.saturating_sub(4) as usize;
        let artists_list: Vec<ListItem> = self.artists.iter().map(|a| {
            let query = searching_artists.then_some(self.search_query.as_str());
            let mut spans = vec![Span::raw("  ")];
            spans.extend(highlighted_spans(&a.name, artists_col_width, query, Style::default(), match_style));
            ListItem::new(Line::from(spans))
        }).collect();

        let albums_col_width = columns_layout[1].width.saturating_sub(4) as usize;
        let albums_list: Vec<ListItem> = self.albums.iter().map(|a| {
            let query = searching_albums.then_some(self.search_query.as_str());
            let suffix = album_suffix(a);
            let name_budget = albums_col_width.saturating_sub(suffix.chars().count()).max(3);
            let mut spans = vec![Span::raw("  ")];
            spans.extend(highlighted_spans(&a.name, name_budget, query, Style::default(), match_style));
            spans.push(Span::styled(suffix, Style::default().fg(self.theme.text_muted)));
            ListItem::new(Line::from(spans))
        }).collect();

        // Duration right-aligned within the column, so it reads as a
        // consistent right rail rather than trailing the title unevenly.
        let tracks_col_width = columns_layout[2].width.saturating_sub(2) as usize;
        let tracks_list: Vec<ListItem> = self.tracks.iter().map(|t| {
            let is_now_playing = Some(t.id.as_str()) == now_playing_id;
            let icon = if is_now_playing { " \u{266a} " } else { "  " };
            let base_style = if is_now_playing {
                Style::default().fg(self.theme.now_playing).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let duration = t.duration_secs.map(|d| format!("{}:{:02}", d / 60, d % 60)).unwrap_or_else(|| "--:--".into());
            let title_budget = tracks_col_width.saturating_sub(icon.chars().count() + duration.chars().count() + 1).max(3);
            let query = searching_tracks.then_some(self.search_query.as_str());
            let title_spans = highlighted_spans(&t.title, title_budget, query, base_style, match_style);
            let title_len: usize = title_spans.iter().map(|s| s.content.chars().count()).sum();

            let mut spans = vec![Span::styled(icon, base_style)];
            spans.extend(title_spans);
            let pad = tracks_col_width.saturating_sub(icon.chars().count() + title_len + duration.chars().count()).max(1);
            spans.push(Span::styled(" ".repeat(pad), base_style));
            spans.push(Span::styled(duration, base_style));

            ListItem::new(Line::from(spans))
        }).collect();

        let get_border_style = |pane: FocusPane| {
            if self.focus == pane { Style::default().fg(self.theme.border_focused) } else { Style::default().fg(self.theme.border_unfocused) }
        };
        // A background band for the keyboard cursor -- kept distinct from
        // NOW_PLAYING's foreground-color-plus-icon treatment so the two
        // states stay legible whether they land on the same row or not.
        let cursor_highlight = Style::default().bg(self.theme.selection_bg).add_modifier(Modifier::BOLD);

        self.artists_state.select(if self.focus == FocusPane::Artists { Some(self.selected_idx) } else { None });
        self.albums_state.select(if self.focus == FocusPane::Albums { Some(self.selected_idx) } else { None });
        self.tracks_state.select(if self.focus == FocusPane::Tracks { Some(self.selected_idx) } else { None });

        let pane_title = |label: &str, len: usize, matching: bool| {
            if matching { format!(" {label} (Matching: {len}) ") } else { format!(" {label} ({len}) ") }
        };

        f.render_stateful_widget(List::new(artists_list).block(Block::default().borders(Borders::ALL).title(pane_title(":: Artists", self.artists.len(), searching_artists)).border_style(get_border_style(FocusPane::Artists))).highlight_style(cursor_highlight), columns_layout[0], &mut self.artists_state);
        f.render_stateful_widget(List::new(albums_list).block(Block::default().borders(Borders::ALL).title(pane_title("<> Albums", self.albums.len(), searching_albums)).border_style(get_border_style(FocusPane::Albums))).highlight_style(cursor_highlight), columns_layout[1], &mut self.albums_state);
        f.render_stateful_widget(List::new(tracks_list).block(Block::default().borders(Borders::ALL).title(pane_title(">> Tracks", self.tracks.len(), searching_tracks)).border_style(get_border_style(FocusPane::Tracks))).highlight_style(cursor_highlight), columns_layout[2], &mut self.tracks_state);

        render_scrollbar(f, columns_layout[0], self.artists.len(), self.artists_state.offset(), self.theme.border_unfocused);
        render_scrollbar(f, columns_layout[1], self.albums.len(), self.albums_state.offset(), self.theme.border_unfocused);
        render_scrollbar(f, columns_layout[2], self.tracks.len(), self.tracks_state.offset(), self.theme.border_unfocused);

        let search_border = match self.input_mode {
            InputMode::Search => self.theme.accent,
            InputMode::Normal => self.theme.border_unfocused,
        };
        let (search_line, search_style) = match self.input_mode {
            InputMode::Search => {
                let scope_name = match self.search_scope {
                    SearchScope::Artist => "Artist",
                    SearchScope::Album => "Album",
                    SearchScope::Song => "Song",
                };
                let left = format!(" $ search: {}\u{2588}", self.search_query);
                let right = format!("[Target: {scope_name}] (TAB to cycle target) ");
                let width = main_layout[1].width.saturating_sub(2) as usize;
                let pad = width.saturating_sub(left.chars().count() + right.chars().count()).max(1);
                (format!("{left}{}{right}", " ".repeat(pad)), Style::default())
            }
            InputMode::Normal => (" Press / to search".to_string(), Style::default().fg(self.theme.text_muted)),
        };
        f.render_widget(
            Paragraph::new(search_line).style(search_style)
                .block(Block::default().borders(Borders::ALL).title(" Search ").border_style(Style::default().fg(search_border))),
            main_layout[1],
        );

        // --- Now Playing deck: title/artist/album/progress/volume unified
        // into one panel instead of four separate blocks, so the elapsed
        // time, the bar, and the volume/shuffle/repeat state read as one
        // cohesive status rather than scattered fragments.
        let deck_block = Block::default().borders(Borders::ALL).title(" Now Playing ");
        let deck_inner = deck_block.inner(main_layout[2]);
        f.render_widget(deck_block, main_layout[2]);

        let cover_protocol = self.now_playing.as_ref()
            .and_then(|np| np.cover_art_id.as_ref())
            .and_then(|id| self.cover_cache.get_mut(id));
        let art_width = if cover_protocol.is_some() { 14 } else { 0 };
        let deck_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(art_width), Constraint::Min(20)])
            .split(deck_inner);

        if let Some(protocol) = cover_protocol {
            f.render_stateful_widget(StatefulImage::new(None), deck_split[0], protocol);
        }

        let title_line = if let Some(np) = &self.now_playing {
            Line::from(vec![
                Span::styled(format!("{} ", playback_label(&np.status)), Style::default().fg(self.theme.accent)),
                Span::styled(np.title.clone(), Style::default().add_modifier(Modifier::BOLD)),
            ])
        } else {
            Line::from(Span::styled(
                "Idle -- highlight a track and press Space or Enter",
                Style::default().fg(self.theme.text_muted),
            ))
        };
        let artist_line = Line::from(format!("Artist: {}", self.now_playing.as_ref().map(|np| np.artist.as_str()).unwrap_or("")));
        let album_line = Line::from(format!("Album:  {}", self.now_playing.as_ref().map(|np| np.album.as_str()).unwrap_or("")));

        let (elapsed_str, total_str, progress_ratio) = if let Some(np) = &self.now_playing {
            let elapsed = np.position.as_secs();
            let total = np.duration_secs.map(|d| format!("{}:{:02}", d / 60, d % 60)).unwrap_or_else(|| "--:--".into());
            let ratio = np.duration_secs
                .filter(|&d| d > 0)
                .map(|d| (np.position.as_secs() as f64 / d as f64).min(1.0))
                .unwrap_or(0.0);
            (format!("{}:{:02}", elapsed / 60, elapsed % 60), total, ratio)
        } else {
            ("--:--".to_string(), "--:--".to_string(), 0.0)
        };
        let text_width = deck_split[1].width as usize;
        let progress_bar_width = text_width
            .saturating_sub("Progress: [] ".len() + elapsed_str.len() + total_str.len() + 3)
            .clamp(10, 60);
        let progress_line = Line::from(vec![
            Span::styled("Progress: ", Style::default().fg(self.theme.text_muted)),
            Span::raw("["),
            Span::styled(ascii_bar(progress_ratio, progress_bar_width, '=', '.', Some('>')), Style::default().fg(self.theme.progress)),
            Span::raw(format!("] {elapsed_str} / {total_str}")),
        ]);

        let shuffle_str = if self.shuffle { "ON" } else { "OFF" };
        let repeat_str = match self.repeat {
            RepeatMode::Off => "OFF",
            RepeatMode::One => "TRACK",
            RepeatMode::All => "QUEUE",
        };
        let volume_line = Line::from(vec![
            Span::styled("Volume:   \u{1f50a} ", Style::default().fg(self.theme.text_muted)),
            Span::styled(ascii_bar(self.volume as f64, 10, '\u{2588}', '\u{2591}', None), Style::default().fg(self.theme.volume)),
            Span::raw(format!(
                " {}%     |     Shuffle: {shuffle_str}     Repeat: {repeat_str}",
                (self.volume * 100.0) as u16
            )),
        ]);

        f.render_widget(
            Paragraph::new(vec![title_line, artist_line, album_line, progress_line, volume_line]),
            deck_split[1],
        );

        // --- Hotkey legend: one line at the very bottom instead of hints
        // scattered across pane titles.
        let legend = " [Space] Play/Pause   [/] Search   [h/l] Pane   [j/k] Move   [s] Shuffle   [r] Repeat   [+/-] Vol   [?] Help   [q] Quit";
        f.render_widget(Paragraph::new(legend).style(Style::default().fg(self.theme.text_muted)), main_layout[3]);

        if self.show_help {
            let help_text = r#"
   RIPTUNE NAVIGATION SHORTCUTS
   =======================================
   [h] / [Left]   : Focus left column
   [l] / [Right]  : Focus right column / Expand directory
   [j] / [Down]   : Move selection cursor down
   [k] / [Up]     : Move selection cursor up

   [Enter]        : Expand selection or Play track
   [Space]        : Toggle Play / Pause state
   [Esc]          : Clear query context or escape layout back

   [/]            : Open universal search context box
   [TAB]          : Cycle target filtering scope ([SONG], [ARTIST])

   [s]            : Toggle Shuffle playback order
   [r]            : Cycle Repeat Mode (Off -> Queue -> Track)
   [-] / [[]      : Decrease system volume output
   [+] / []]      : Increase system volume output

   [?]            : Toggle this floating instructions overview overlay
   [q]            : Exit riptune application safely
            "#;

            let block = Block::default().title(" Help Overlay Menu ").borders(Borders::ALL).border_style(Style::default().fg(self.theme.help_border));
            let paragraph = Paragraph::new(help_text).block(block);

            let popup_area = Rect {
                x: area.width / 4,
                y: area.height / 5,
                width: area.width / 2,
                height: 3 * area.height / 5,
            };

            f.render_widget(Clear, popup_area);
            f.render_widget(paragraph, popup_area);
        }
    }
}

/// Renders a vertical scrollbar along the right edge of `area`, inset to sit
/// inside a bordered block's margins. A no-op when everything already fits.
fn render_scrollbar(f: &mut ratatui::Frame, area: Rect, content_len: usize, offset: usize, color: ratatui::style::Color) {
    if content_len <= area.height.saturating_sub(2) as usize {
        return;
    }
    let mut state = ScrollbarState::new(content_len).position(offset);
    let scrollbar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .style(Style::default().fg(color));
    f.render_stateful_widget(
        scrollbar,
        area.inner(ratatui::layout::Margin { vertical: 1, horizontal: 0 }),
        &mut state,
    );
}

/// Track count + year suffix for an album row (`" — 18 tracks, 2025"`), so
/// near-identical entries (duplicate imports, case-variant folder names,
/// etc.) are distinguishable before the user commits to opening one.
fn album_suffix(album: &AlbumSummary) -> String {
    let tracks = format!("{} track{}", album.song_count, if album.song_count == 1 { "" } else { "s" });
    match album.year {
        Some(year) => format!(" — {tracks}, {year}"),
        None => format!(" — {tracks}"),
    }
}

/// Renders a horizontal `[===>....]`-style bar (or solid blocks when `tip`
/// is `None`, for the volume meter) instead of a filled rectangle, so it
/// reads as a line-based indicator consistent with the rest of the UI
/// rather than a clashing solid block of color.
fn ascii_bar(ratio: f64, width: usize, filled_char: char, empty_char: char, tip: Option<char>) -> String {
    if width == 0 {
        return String::new();
    }
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    match tip {
        Some(tip_char) if filled > 0 && filled < width => {
            let body = filled_char.to_string().repeat(filled - 1);
            let rest = empty_char.to_string().repeat(width - filled);
            format!("{body}{tip_char}{rest}")
        }
        _ => {
            let body = filled_char.to_string().repeat(filled);
            let rest = empty_char.to_string().repeat(width - filled);
            format!("{body}{rest}")
        }
    }
}

/// Truncates to `max_chars`, appending an ellipsis when content was cut,
/// instead of letting long names clip abruptly at the pane border.
fn truncate_ellipsis(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    if max_chars == 1 {
        return "\u{2026}".to_string();
    }
    let truncated: String = s.chars().take(max_chars - 1).collect();
    format!("{truncated}\u{2026}")
}

/// Truncates `text` to `budget` characters, then -- if `query` is active and
/// found (case-insensitive) within the truncated text -- splits it into
/// styled spans so the matched substring pops instead of leaving the reader
/// to guess why a row matched.
fn highlighted_spans(text: &str, budget: usize, query: Option<&str>, base: Style, match_style: Style) -> Vec<Span<'static>> {
    let text = truncate_ellipsis(text, budget);
    let query = match query.filter(|q| !q.is_empty()) {
        Some(q) => q,
        None => return vec![Span::styled(text, base)],
    };

    let chars: Vec<char> = text.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.is_empty() || query_chars.len() > chars.len() {
        return vec![Span::styled(text, base)];
    }

    let match_start = (0..=chars.len() - query_chars.len()).find(|&i| {
        chars[i..i + query_chars.len()]
            .iter()
            .zip(&query_chars)
            .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
    });

    match match_start {
        Some(start) => {
            let before: String = chars[..start].iter().collect();
            let matched: String = chars[start..start + query_chars.len()].iter().collect();
            let after: String = chars[start + query_chars.len()..].iter().collect();
            let mut spans = Vec::new();
            if !before.is_empty() {
                spans.push(Span::styled(before, base));
            }
            spans.push(Span::styled(matched, match_style));
            if !after.is_empty() {
                spans.push(Span::styled(after, base));
            }
            spans
        }
        None => vec![Span::styled(text, base)],
    }
}

fn playback_label(status: &PlaybackStatus) -> &str {
    match status {
        PlaybackStatus::Buffering => "[...] buffering...",
        PlaybackStatus::Playing => "[>] playing",
        PlaybackStatus::Paused => "[||] paused",
    }
}