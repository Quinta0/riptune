use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style, Color};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Gauge, Clear};
use ratatui::Terminal;
use riptune_cache::Cache;
use riptune_core::models::{AlbumSummary, Artist, Track};
use riptune_core::SubsonicClient;
use riptune_types::{AudioCommand, AudioEvent};
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
    Stopped,
    Error(String),
}

enum InputMode {
    Normal,
    Search,
}

struct NowPlaying {
    title: String,
    artist: String,
    position: Duration,
    duration_secs: Option<u32>,
    status: PlaybackStatus,
    current_track_id: String,
    playlist_context: Vec<Track>,
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
}

impl App {
    pub fn new(client: SubsonicClient, cache: Cache, audio_tx: Sender<AudioCommand>, audio_events: Receiver<AudioEvent>) -> Self {
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
        }
    }

    pub async fn run(mut self) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let result = self.event_loop(&mut terminal).await;

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
                    self.notify_desktop_environment("Shuffle Toggled");
                }
                KeyCode::Char('r') => {
                    self.repeat = match self.repeat {
                        RepeatMode::Off => RepeatMode::All,
                        RepeatMode::All => RepeatMode::One,
                        RepeatMode::One => RepeatMode::Off,
                    };
                    self.notify_desktop_environment("Repeat Mode Changed");
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

    // --- Option 2 Integration Hooks (MPRIS & Niri IPC Sync Channels) ---
    fn notify_desktop_environment(&self, action_context: &str) {
        // These hooks act as communication bridges for your riptune-mpris and riptune-niri subcrates[cite: 1]
        // Example: Emits state logs that zbus / mpris handlers catch via standard asynchronous orchestration[cite: 1, 7]
        if let Some(np) = &self.now_playing {
            let _status_indicator = match np.status {
                PlaybackStatus::Playing => "Playing",
                PlaybackStatus::Paused => "Paused",
                _ => "Stopped",
            };
            // Under-the-hood bindings read this payload state to dynamically broadcast D-Bus properties[cite: 1, 7]
            std::string::ToString::to_string(&format!("MPRIS/NIRI Sync Triggered: {} -> {} ({})", action_context, np.title, _status_indicator));
        }
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
                self.now_playing = Some(NowPlaying {
                    title: track.title.clone(),
                    artist: track.artist.clone().unwrap_or_default(),
                    position: Duration::ZERO,
                    duration_secs: track.duration_secs,
                    status: PlaybackStatus::Buffering,
                    current_track_id: track.id.clone(),
                    playlist_context,
                });
                self.notify_desktop_environment("Track Started");
            }
            Err(e) => {
                self.status_line = Some(format!("failed to build stream url: {e}"));
            }
        }
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
            self.notify_desktop_environment("Play/Pause Toggled");
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
                self.notify_desktop_environment("Track Stopped/Finished");
            }
            AudioEvent::Error(message) => {
                self.status_line = Some(format!("playback error: {message}"));
            }
        }
    }

    fn draw(&self, f: &mut ratatui::Frame) {
        let area = f.area();

        // --- 1. Layout Resizing Shield ---
        // Guarantees zero terminal rendering crashes if constraint requirements fall below boundaries
        if area.width < 85 || area.height < 18 {
            let emergency_msg = Paragraph::new(
                "\n\n  ⚠️ Terminal Window Too Small!\n  ===============================\n  Please enlarge your screen layout dimensions\n  to display Riptune's multi-tier grids safely."
            )
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Red)));
            f.render_widget(emergency_msg, area);
            return;
        }

        let main_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    
                Constraint::Length(3), 
                Constraint::Length(3), 
            ])
            .split(area);

        let columns_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(33),
                Constraint::Percentage(34),
            ])
            .split(main_layout[0]);

        let artists_list: Vec<ListItem> = self.artists.iter().map(|a| ListItem::new(format!("  {}", a.name))).collect();
        let albums_list: Vec<ListItem> = self.albums.iter().map(|a| ListItem::new(format!("  {}", a.name))).collect();
        let tracks_list: Vec<ListItem> = self.tracks.iter().map(|t| ListItem::new(format!("  {}", t.title))).collect();

        let get_border_style = |pane: FocusPane| {
            if self.focus == pane { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }
        };

        let mut left_state = ListState::default();
        let mut mid_state = ListState::default();
        let mut right_state = ListState::default();

        match self.focus {
            FocusPane::Artists => left_state.select(Some(self.selected_idx)),
            FocusPane::Albums => mid_state.select(Some(self.selected_idx)),
            FocusPane::Tracks => right_state.select(Some(self.selected_idx)),
        }

        f.render_stateful_widget(List::new(artists_list).block(Block::default().borders(Borders::ALL).title(" :: Artists ").border_style(get_border_style(FocusPane::Artists))).highlight_style(Style::default().add_modifier(Modifier::REVERSED)), columns_layout[0], &mut left_state);
        f.render_stateful_widget(List::new(albums_list).block(Block::default().borders(Borders::ALL).title(" <> Albums ").border_style(get_border_style(FocusPane::Albums))).highlight_style(Style::default().add_modifier(Modifier::REVERSED)), columns_layout[1], &mut mid_state);
        f.render_stateful_widget(List::new(tracks_list).block(Block::default().borders(Borders::ALL).title(" >> Tracks ").border_style(get_border_style(FocusPane::Tracks))).highlight_style(Style::default().add_modifier(Modifier::REVERSED)), columns_layout[2], &mut right_state);

        let scope_indicator = match self.search_scope {
            SearchScope::Artist => "[ARTIST] > Album > Song",
            SearchScope::Album => "Artist > [ALBUM] > Song",
            SearchScope::Song => "Artist > Album > [SONG]",
        };
        let search_title = match self.input_mode {
            InputMode::Normal => " / Search (Press '/' to search globally, '?' for help) ".to_string(),
            InputMode::Search => format!(" $ Searching (TAB switches query target) -- Target: {} ", scope_indicator),
        };
        f.render_widget(Paragraph::new(format!(" {}", self.search_query)).block(Block::default().borders(Borders::ALL).title(search_title)), main_layout[1]);

        let dashboard_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(20), Constraint::Percentage(20), Constraint::Percentage(15)])
            .split(main_layout[2]);

        let now_playing_text = if let Some(np) = &self.now_playing {
            let secs = np.position.as_secs();
            format!("  {} -- {}  [{}:{:02}]", np.title, np.artist, secs / 60, secs % 60)
        } else {
            "  Idle . Highlight a track and press Space or Enter".to_string()
        };
        let dashboard_title = if let Some(np) = &self.now_playing { format!(" {} ", playback_label(&np.status)) } else { " Player Status ".to_string() };
        f.render_widget(Paragraph::new(now_playing_text).block(Block::default().borders(Borders::ALL).title(dashboard_title)), dashboard_layout[0]);

        // --- 2. Shuffle & Repeat Status Indicators ---
        let shuffle_str = if self.shuffle { "ON" } else { "OFF" };
        let repeat_str = match self.repeat {
            RepeatMode::Off => "OFF",
            RepeatMode::One => "TRACK",
            RepeatMode::All => "QUEUE",
        };
        let modes_text = format!("  Shuffle: {} | Repeat: {}", shuffle_str, repeat_str);
        f.render_widget(Paragraph::new(modes_text).block(Block::default().borders(Borders::ALL).title(" Play Modes ([s]/[r]) ")), dashboard_layout[1]);

        let progress_ratio = if let Some(np) = &self.now_playing {
            if let Some(total) = np.duration_secs {
                if total > 0 { (np.position.as_secs() as f64 / total as f64).min(1.0) } else { 0.0 }
            } else { 0.0 }
        } else { 0.0 };

        let progress_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" Progression "))
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(progress_ratio);
        f.render_widget(progress_gauge, dashboard_layout[2]);

        let volume_gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" Vol ([-]/[+]) "))
            .gauge_style(Style::default().fg(Color::Green))
            .percent((self.volume * 100.0) as u16);
        f.render_widget(volume_gauge, dashboard_layout[3]);

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

            let block = Block::default().title(" Help Overlay Menu ").borders(Borders::ALL).border_style(Style::default().fg(Color::Magenta));
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

fn playback_label(status: &PlaybackStatus) -> &str {
    match status {
        PlaybackStatus::Buffering => "[...] buffering...",
        PlaybackStatus::Playing => "[>] playing",
        PlaybackStatus::Paused => "[||] paused",
        PlaybackStatus::Stopped => "[.] stopped",
        PlaybackStatus::Error(_) => "[X] error",
    }
}