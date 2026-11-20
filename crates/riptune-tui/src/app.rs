//! App state and everything that changes it: key handling, browsing,
//! search, and playback control. Rendering lives in `ui.rs`.

use crate::cover::Covers;
use crate::queue::{Queue, RepeatMode};
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use riptune_cache::{Cache, CacheError, SyncEvent};
use riptune_core::models::{AlbumSummary, Artist, Track};
use riptune_core::SubsonicClient;
use riptune_types::{AudioCommand, AudioEvent, PlayToken};
use std::ops::Range;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SEEK_STEP: Duration = Duration::from_secs(10);
const VOLUME_STEP: f32 = 0.05;
/// "Previous" within this much of a track's start goes to the previous
/// track; later than that it restarts the current one (like most players).
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Artists,
    Albums,
    Tracks,
}

impl Focus {
    pub fn left(self) -> Self {
        match self {
            Self::Tracks => Self::Albums,
            _ => Self::Artists,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Artists => Self::Albums,
            Self::Albums => Self::Tracks,
            Self::Tracks => Self::Artists,
        }
    }
    pub fn prev(self) -> Self {
        self.next().next()
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Artists => "artists",
            Self::Albums => "albums",
            Self::Tracks => "songs",
        }
    }
}

/// One browsable column. Keeps its own cursor and scroll offset, so moving
/// between panes never loses your place.
pub struct Pane<T> {
    pub items: Vec<T>,
    pub selected: usize,
    pub offset: usize,
    /// Rows visible at last render; used for page up/down.
    pub height: usize,
}

impl<T> Default for Pane<T> {
    fn default() -> Self {
        Self { items: Vec::new(), selected: 0, offset: 0, height: 10 }
    }
}

impl<T> Pane<T> {
    fn set(&mut self, items: Vec<T>) {
        self.items = items;
        self.selected = 0;
        self.offset = 0;
    }

    pub fn current(&self) -> Option<&T> {
        self.items.get(self.selected)
    }

    fn move_by(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    fn select_where(&mut self, pred: impl Fn(&T) -> bool) {
        if let Some(i) = self.items.iter().position(pred) {
            self.selected = i;
        }
    }

    /// Scrolls just enough to keep the cursor visible and returns the range
    /// of rows to draw -- only those rows get built into widgets, so a
    /// 50k-track list costs the same to render as a 20-track one.
    pub fn window(&mut self, height: usize) -> Range<usize> {
        self.height = height.max(1);
        let len = self.items.len();
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + self.height {
            self.offset = self.selected + 1 - self.height;
        }
        self.offset = self.offset.min(len.saturating_sub(self.height));
        self.offset..(self.offset + self.height).min(len)
    }
}

pub struct Search {
    pub scope: Focus,
    pub query: String,
    /// True while the query is being typed; false once confirmed with Enter
    /// (the results stay filtered until Esc).
    pub editing: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Loading,
    Playing,
    Paused,
}

pub struct NowPlaying {
    pub track: Track,
    pub token: PlayToken,
    pub position: Duration,
    pub status: Status,
}

pub struct Message {
    pub text: String,
    pub is_error: bool,
    pub expires: Instant,
}

pub struct App {
    pub(crate) client: SubsonicClient,
    cache: Cache,
    audio_tx: Sender<AudioCommand>,

    pub focus: Focus,
    pub artists: Pane<Artist>,
    pub albums: Pane<AlbumSummary>,
    pub tracks: Pane<Track>,
    /// Which artist/album the albums/tracks panes currently show; None when
    /// they hold search results or the queue instead.
    pub albums_of: Option<String>,
    pub tracks_of: Option<String>,
    pub search: Option<Search>,

    pub queue: Option<Queue>,
    pub now_playing: Option<NowPlaying>,
    next_token: PlayToken,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,

    pub message: Option<Message>,
    pub sync: Option<(usize, usize)>,
    /// First run (empty cache): refresh the artist list while the sync runs
    /// instead of only once at the end.
    live_reload: bool,
    pub show_help: bool,
    pub quit: bool,

    pub covers: Covers,
    pub theme: Theme,
}

impl App {
    pub(crate) fn new(client: SubsonicClient, cache: Cache, audio_tx: Sender<AudioCommand>, theme: Theme) -> Self {
        let volume = 0.8;
        // The audio thread starts at full volume; make it match what we show.
        let _ = audio_tx.send(AudioCommand::SetVolume(volume));
        Self {
            client,
            cache,
            audio_tx,
            focus: Focus::Artists,
            artists: Pane::default(),
            albums: Pane::default(),
            tracks: Pane::default(),
            albums_of: None,
            tracks_of: None,
            search: None,
            queue: None,
            now_playing: None,
            next_token: 0,
            volume,
            shuffle: false,
            repeat: RepeatMode::Off,
            message: None,
            sync: Some((0, 0)),
            live_reload: false,
            show_help: false,
            quit: false,
            covers: Covers::new(),
            theme,
        }
    }

    pub(crate) async fn load_library(&mut self) {
        match self.cache.all_artists().await {
            Ok(artists) => {
                self.live_reload = artists.is_empty();
                self.artists.set(artists);
                self.show_albums_for_selected_artist().await;
            }
            Err(e) => self.error(format!("couldn't read the library cache: {e}")),
        }
    }

    // ---- messages -------------------------------------------------------

    pub fn info(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), is_error: false, expires: Instant::now() + Duration::from_secs(4) });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), is_error: true, expires: Instant::now() + Duration::from_secs(8) });
    }

    fn report<T>(&mut self, what: &str, result: Result<T, CacheError>) -> Option<T> {
        result.map_err(|e| self.error(format!("{what}: {e}"))).ok()
    }

    // ---- keys -----------------------------------------------------------

    pub(crate) async fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.show_help {
            // Any key closes help; q still quits.
            self.show_help = false;
            self.quit = key.code == KeyCode::Char('q');
            return;
        }
        if self.search.as_ref().is_some_and(|s| s.editing) {
            self.on_search_key(key).await;
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('/') => self.start_search(),

            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1).await,
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1).await,
            KeyCode::Char('d') if ctrl => self.move_cursor(self.page() / 2).await,
            KeyCode::Char('u') if ctrl => self.move_cursor(-self.page() / 2).await,
            KeyCode::PageDown => self.move_cursor(self.page()).await,
            KeyCode::PageUp => self.move_cursor(-self.page()).await,
            KeyCode::Home | KeyCode::Char('g') => self.move_cursor(isize::MIN / 2).await,
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(isize::MAX / 2).await,
            KeyCode::Left | KeyCode::Char('h') => self.focus = self.focus.left(),
            KeyCode::Right | KeyCode::Char('l') => self.open().await,
            KeyCode::Enter => self.open().await,
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.prev(),
            KeyCode::Esc => {
                if self.search.is_some() {
                    self.clear_search().await;
                } else {
                    self.focus = self.focus.left();
                }
            }
            KeyCode::Char('o') => self.jump_to_playing(),

            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('n') => self.skip(true),
            KeyCode::Char('p') => self.skip(false),
            KeyCode::Char('.') => self.seek_by(SEEK_STEP, true),
            KeyCode::Char(',') => self.seek_by(SEEK_STEP, false),
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char(']') => self.change_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('[') => self.change_volume(-VOLUME_STEP),
            KeyCode::Char('s') => {
                self.shuffle = !self.shuffle;
                if let Some(q) = &mut self.queue {
                    q.set_shuffle(self.shuffle);
                }
                self.info(if self.shuffle { "shuffle on" } else { "shuffle off" });
            }
            KeyCode::Char('r') => {
                self.repeat = self.repeat.cycle();
                self.info(match self.repeat {
                    RepeatMode::Off => "repeat off",
                    RepeatMode::All => "repeat all",
                    RepeatMode::One => "repeat one",
                });
            }
            _ => {}
        }
    }

    fn page(&self) -> isize {
        let h = match self.focus {
            Focus::Artists => self.artists.height,
            Focus::Albums => self.albums.height,
            Focus::Tracks => self.tracks.height,
        };
        h.max(2) as isize
    }

    // ---- browsing -------------------------------------------------------

    async fn move_cursor(&mut self, delta: isize) {
        // Moving in a pane re-fills the panes to its right; any search
        // results shown there are about to be replaced.
        self.drop_search_right_of(self.focus);
        match self.focus {
            Focus::Artists => {
                self.artists.move_by(delta);
                self.show_albums_for_selected_artist().await;
            }
            Focus::Albums => {
                self.albums.move_by(delta);
                self.show_tracks_for_selected_album().await;
            }
            Focus::Tracks => self.tracks.move_by(delta),
        }
    }

    fn drop_search_right_of(&mut self, pane: Focus) {
        let scope = self.search.as_ref().map(|s| s.scope);
        let below = match pane {
            Focus::Artists => matches!(scope, Some(Focus::Albums | Focus::Tracks)),
            Focus::Albums => scope == Some(Focus::Tracks),
            Focus::Tracks => false,
        };
        if below {
            self.search = None;
        }
    }

    /// Miller-column preview: the albums pane always shows the highlighted
    /// artist's albums (and the tracks pane the first album's tracks).
    /// Skipped when it already shows them, so h/l never resets the cursor.
    async fn show_albums_for_selected_artist(&mut self) {
        let Some(id) = self.artists.current().map(|a| a.id.clone()) else {
            return;
        };
        if self.albums_of.as_deref() == Some(id.as_str()) {
            return;
        }
        let result = self.cache.get_albums_by_artist(&id).await;
        if let Some(albums) = self.report("couldn't load albums", result) {
            self.albums.set(albums);
            self.albums_of = Some(id);
            self.show_tracks_for_selected_album().await;
        }
    }

    async fn show_tracks_for_selected_album(&mut self) {
        let Some(album) = self.albums.current().cloned() else {
            self.tracks.set(Vec::new());
            self.tracks_of = None;
            return;
        };
        if self.tracks_of.as_deref() == Some(album.id.as_str()) {
            return;
        }
        let result = self.cache.get_tracks_by_album(&album).await;
        if let Some(tracks) = self.report("couldn't load tracks", result) {
            self.tracks.set(tracks);
            self.tracks_of = Some(album.id);
        }
    }

    async fn open(&mut self) {
        self.drop_search_right_of(self.focus);
        match self.focus {
            Focus::Artists => {
                self.show_albums_for_selected_artist().await;
                self.focus = Focus::Albums;
            }
            Focus::Albums => {
                self.show_tracks_for_selected_album().await;
                self.focus = Focus::Tracks;
            }
            Focus::Tracks => self.play_from_cursor(),
        }
    }

    fn jump_to_playing(&mut self) {
        let Some(q) = &self.queue else {
            self.info("nothing is playing");
            return;
        };
        let (tracks, idx) = (q.tracks().to_vec(), q.current_index().unwrap_or(0));
        self.tracks.set(tracks);
        self.tracks.selected = idx;
        self.tracks_of = None;
        if self.search.as_ref().is_some_and(|s| s.scope == Focus::Tracks) {
            self.search = None;
        }
        self.focus = Focus::Tracks;
    }

    // ---- search ---------------------------------------------------------

    fn start_search(&mut self) {
        match &mut self.search {
            // Re-open the active filter for editing rather than starting over.
            Some(s) => s.editing = true,
            None => self.search = Some(Search { scope: self.focus, query: String::new(), editing: true }),
        }
    }

    async fn on_search_key(&mut self, key: KeyEvent) {
        let Some(search) = &mut self.search else { return };
        match key.code {
            KeyCode::Esc => self.clear_search().await,
            KeyCode::Enter => {
                if search.query.is_empty() {
                    self.clear_search().await;
                } else {
                    search.editing = false;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let old = search.scope;
                search.scope = if key.code == KeyCode::Tab { old.next() } else { old.prev() };
                self.restore_pane(old).await;
                self.run_search().await;
            }
            KeyCode::Backspace => {
                if search.query.pop().is_none() {
                    self.clear_search().await;
                } else {
                    self.run_search().await;
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                search.query.clear();
                self.run_search().await;
            }
            KeyCode::Char(c) => {
                search.query.push(c);
                self.run_search().await;
            }
            // Browse the results without leaving the prompt.
            KeyCode::Down => self.move_cursor(1).await,
            KeyCode::Up => self.move_cursor(-1).await,
            _ => {}
        }
    }

    async fn run_search(&mut self) {
        let Some(search) = &self.search else { return };
        let (scope, query) = (search.scope, search.query.clone());
        self.focus = scope;
        match scope {
            Focus::Artists => {
                let result = self.cache.search_artists(&query).await;
                if let Some(items) = self.report("search failed", result) {
                    self.artists.set(items);
                    self.show_albums_for_selected_artist().await;
                }
            }
            Focus::Albums => {
                let result = self.cache.search_albums(&query).await;
                if let Some(items) = self.report("search failed", result) {
                    self.albums.set(items);
                    self.albums_of = None;
                    self.show_tracks_for_selected_album().await;
                }
            }
            Focus::Tracks => {
                let result = self.cache.search_tracks(&query).await;
                if let Some(items) = self.report("search failed", result) {
                    self.tracks.set(items);
                    self.tracks_of = None;
                }
            }
        }
    }

    async fn clear_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.restore_pane(search.scope).await;
        }
    }

    /// Puts a pane back to normal browsing after a search, keeping the
    /// cursor on whatever was highlighted in the results when possible --
    /// search "radio", Esc, and you're still on Radiohead.
    async fn restore_pane(&mut self, scope: Focus) {
        match scope {
            Focus::Artists => {
                let keep = self.artists.current().map(|a| a.id.clone());
                let result = self.cache.all_artists().await;
                if let Some(all) = self.report("couldn't reload artists", result) {
                    self.artists.set(all);
                    if let Some(id) = keep {
                        self.artists.select_where(|a| a.id == id);
                    }
                    self.show_albums_for_selected_artist().await;
                }
            }
            Focus::Albums => {
                let keep = self.albums.current().map(|a| a.id.clone());
                self.albums_of = None;
                self.show_albums_for_selected_artist().await;
                if let Some(id) = keep {
                    self.albums.select_where(|a| a.id == id);
                    self.show_tracks_for_selected_album().await;
                }
            }
            Focus::Tracks => {
                let keep = self.tracks.current().map(|t| t.id.clone());
                self.tracks_of = None;
                self.show_tracks_for_selected_album().await;
                if let Some(id) = keep {
                    self.tracks.select_where(|t| t.id == id);
                }
            }
        }
    }

    // ---- playback -------------------------------------------------------

    fn play_from_cursor(&mut self) {
        if self.tracks.items.is_empty() {
            return;
        }
        let tracks: Arc<[Track]> = self.tracks.items.clone().into();
        self.queue = Some(Queue::new(tracks, self.tracks.selected, self.shuffle));
        self.play_current();
    }

    fn play_current(&mut self) {
        let Some(track) = self.queue.as_ref().and_then(|q| q.current()).cloned() else {
            return;
        };
        let stream_url = match self.client.stream_url(&track.id) {
            Ok(url) => url,
            Err(e) => return self.error(format!("couldn't build stream url: {e}")),
        };
        self.next_token += 1;
        let token = self.next_token;
        let _ = self.audio_tx.send(AudioCommand::Play { stream_url, token });
        if let Some(id) = &track.cover_art {
            self.covers.request(&self.client, id);
        }
        self.now_playing = Some(NowPlaying { track, token, position: Duration::ZERO, status: Status::Loading });
    }

    fn toggle_pause(&mut self) {
        let Some(np) = &mut self.now_playing else {
            if self.focus == Focus::Tracks {
                self.play_from_cursor();
            } else if self.queue.is_some() {
                self.play_current();
            }
            return;
        };
        match np.status {
            Status::Playing => {
                let _ = self.audio_tx.send(AudioCommand::Pause);
                np.status = Status::Paused;
            }
            Status::Paused => {
                let _ = self.audio_tx.send(AudioCommand::Resume);
                np.status = Status::Playing;
            }
            Status::Loading => {}
        }
    }

    fn skip(&mut self, forward: bool) {
        let Some(queue) = &mut self.queue else { return };
        let wrap = self.repeat == RepeatMode::All;
        if !forward {
            if let Some(np) = &mut self.now_playing {
                if np.position > RESTART_THRESHOLD {
                    let _ = self.audio_tx.send(AudioCommand::Seek(Duration::ZERO));
                    np.position = Duration::ZERO;
                    return;
                }
            }
        }
        let moved = if forward { queue.advance(wrap) } else { queue.back(wrap) }.is_some();
        if moved {
            self.play_current();
        } else {
            self.info(if forward { "end of queue" } else { "start of queue" });
        }
    }

    fn seek_by(&mut self, step: Duration, forward: bool) {
        let Some(np) = &mut self.now_playing else { return };
        if np.status == Status::Loading {
            return;
        }
        let target = if forward { np.position + step } else { np.position.saturating_sub(step) };
        if let Some(total) = np.track.duration_secs.map(|d| Duration::from_secs(d as u64)) {
            if target >= total {
                // Seeking past the end means "next track".
                return self.skip(true);
            }
        }
        let _ = self.audio_tx.send(AudioCommand::Seek(target));
        np.position = target;
    }

    fn change_volume(&mut self, delta: f32) {
        self.volume = ((self.volume + delta) * 20.0).round().clamp(0.0, 20.0) / 20.0;
        let _ = self.audio_tx.send(AudioCommand::SetVolume(self.volume));
    }

    pub(crate) fn on_audio(&mut self, event: AudioEvent) {
        let current = self.now_playing.as_ref().map(|np| np.token);
        match event {
            AudioEvent::Position(token, pos) if Some(token) == current => {
                if let Some(np) = &mut self.now_playing {
                    np.position = pos;
                    if np.status == Status::Loading {
                        np.status = Status::Playing;
                    }
                }
            }
            AudioEvent::Finished(token) if Some(token) == current => {
                let next = match self.repeat {
                    RepeatMode::One => true,
                    mode => self.queue.as_mut().is_some_and(|q| q.advance(mode == RepeatMode::All).is_some()),
                };
                if next {
                    self.play_current();
                } else {
                    self.now_playing = None;
                }
            }
            AudioEvent::LoadFailed(token, message) if Some(token) == current => {
                let title = self.now_playing.take().map(|np| np.track.title).unwrap_or_default();
                self.error(format!("couldn't play \"{title}\": {message}"));
            }
            AudioEvent::Error(message) => self.error(message),
            _ => {} // stale event from a track we already moved past
        }
    }

    pub(crate) async fn on_sync(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::Progress { done, total } => {
                self.sync = Some((done, total));
                if self.live_reload && (done % 25 == 0 || done == 1) {
                    self.reload_artists().await;
                }
            }
            SyncEvent::Finished => {
                self.sync = None;
                self.live_reload = false;
                self.reload_artists().await;
                self.info("library up to date");
            }
            SyncEvent::Failed(e) => {
                self.sync = None;
                self.error(format!("library sync failed (showing cached library): {e}"));
            }
        }
    }

    /// Refreshes the artist list from the cache, unless the user is looking
    /// at search results there.
    async fn reload_artists(&mut self) {
        if self.search.as_ref().is_some_and(|s| s.scope == Focus::Artists) {
            return;
        }
        let keep = self.artists.current().map(|a| a.id.clone());
        if let Ok(all) = self.cache.all_artists().await {
            let offset = self.artists.offset;
            self.artists.set(all);
            self.artists.offset = offset;
            if let Some(id) = keep {
                self.artists.select_where(|a| a.id == id);
            }
            self.show_albums_for_selected_artist().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_window_follows_cursor() {
        let mut p = Pane::<u32>::default();
        p.set((0..100).collect());
        assert_eq!(p.window(10), 0..10);
        p.move_by(15);
        assert_eq!(p.window(10), 6..16);
        p.move_by(-100);
        assert_eq!(p.selected, 0);
        assert_eq!(p.window(10), 0..10);
        p.move_by(isize::MAX / 2);
        assert_eq!(p.selected, 99);
        assert_eq!(p.window(10), 90..100);
    }

    #[test]
    fn pane_window_shrinks_with_short_lists() {
        let mut p = Pane::<u32>::default();
        p.set(vec![1, 2, 3]);
        assert_eq!(p.window(10), 0..3);
        p.set(Vec::new());
        assert_eq!(p.window(10), 0..0);
    }
}
