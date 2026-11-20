//! Local SQLite index of the remote Subsonic library.
//!
//! Why this exists: hitting `getArtists`/`getAlbum` over the network on every TUI
//! launch is slow on large libraries (multi-second waits are common on Airsonic
//! instances with 50k+ tracks). We index artists/albums/tracks locally and treat
//! the network as a background sync source, not the read path for rendering.

use futures_util::stream::{self, StreamExt};
use riptune_core::models::{AlbumSummary, Artist, Track};
use riptune_core::SubsonicClient;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::Row;
use sqlx::SqlitePool;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network or client error: {0}")]
    Client(String),
}

/// Reported by the background sync so the UI can show progress and pick up
/// newly indexed entries without a restart.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    Progress { done: usize, total: usize },
    Finished,
    Failed(String),
}

#[derive(Clone)]
pub struct Cache {
    pool: SqlitePool,
}

const TRACK_COLUMNS: &str =
    "id, title, artist, album, album_id, duration_secs, CAST(starred AS TEXT) AS starred, cover_art, track_number, disc_number";

impl Cache {
    pub async fn open(path: &Path) -> Result<Self, CacheError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // WAL lets the UI keep reading while the sync writes; NORMAL sync is
        // safe under WAL and much faster for a rebuildable cache.
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new().max_connections(4).connect_with(options).await?;

        for stmt in [
            "CREATE TABLE IF NOT EXISTS artists (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                album_count INTEGER NOT NULL DEFAULT 0
            )",
            "CREATE TABLE IF NOT EXISTS albums (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                song_count INTEGER NOT NULL DEFAULT 0,
                artist_id TEXT NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS tracks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                artist TEXT,
                album TEXT,
                duration_secs INTEGER,
                starred TEXT
            )",
        ] {
            sqlx::query(stmt).execute(&pool).await?;
        }

        // This is a rebuildable cache, not source-of-truth data, so schema
        // evolution is a best-effort ALTER rather than a real migration --
        // ignore the error if a column already exists.
        for alter in [
            "ALTER TABLE tracks ADD COLUMN cover_art TEXT",
            "ALTER TABLE tracks ADD COLUMN track_number INTEGER",
            "ALTER TABLE tracks ADD COLUMN disc_number INTEGER",
            "ALTER TABLE tracks ADD COLUMN album_id TEXT",
            "ALTER TABLE albums ADD COLUMN year INTEGER",
        ] {
            let _ = sqlx::query(alter).execute(&pool).await;
        }

        // Every browse query filters on one of these; without them each
        // cursor move in the artist/album panes is a full table scan.
        for index in [
            "CREATE INDEX IF NOT EXISTS albums_artist_id ON albums(artist_id)",
            "CREATE INDEX IF NOT EXISTS tracks_album_id ON tracks(album_id)",
            "CREATE INDEX IF NOT EXISTS tracks_album ON tracks(album)",
        ] {
            sqlx::query(index).execute(&pool).await?;
        }

        Ok(Self { pool })
    }

    pub async fn all_artists(&self) -> Result<Vec<Artist>, CacheError> {
        self.search_artists("").await
    }

    pub async fn get_albums_by_artist(&self, artist_id: &str) -> Result<Vec<AlbumSummary>, CacheError> {
        let rows = sqlx::query(
            "SELECT id, name, song_count, year FROM albums WHERE artist_id = ? \
             ORDER BY COALESCE(year, 9999), name COLLATE NOCASE",
        )
        .bind(artist_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_album).collect())
    }

    /// Tracklist for an album, in disc/track order. Matches on album id, so
    /// two different albums that share a name ("Greatest Hits") stay apart;
    /// falls back to the name only for rows indexed before album ids were
    /// stored (the next sync fills those in).
    pub async fn get_tracks_by_album(&self, album: &AlbumSummary) -> Result<Vec<Track>, CacheError> {
        let rows = sqlx::query(&format!(
            "SELECT {TRACK_COLUMNS} FROM tracks \
             WHERE album_id = ? OR (album_id IS NULL AND album = ?) \
             ORDER BY COALESCE(disc_number, 1), COALESCE(track_number, 999999), title"
        ))
        .bind(&album.id)
        .bind(&album.name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_track).collect())
    }

    pub async fn search_artists(&self, query: &str) -> Result<Vec<Artist>, CacheError> {
        let rows = sqlx::query(
            "SELECT id, name, album_count FROM artists WHERE name LIKE ? ESCAPE '\\' \
             ORDER BY name COLLATE NOCASE",
        )
        .bind(like_pattern(query))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artist).collect())
    }

    pub async fn search_albums(&self, query: &str) -> Result<Vec<AlbumSummary>, CacheError> {
        let rows = sqlx::query(
            "SELECT id, name, song_count, year FROM albums WHERE name LIKE ? ESCAPE '\\' \
             ORDER BY name COLLATE NOCASE",
        )
        .bind(like_pattern(query))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_album).collect())
    }

    pub async fn search_tracks(&self, query: &str) -> Result<Vec<Track>, CacheError> {
        let rows = sqlx::query(&format!(
            "SELECT {TRACK_COLUMNS} FROM tracks WHERE title LIKE ? ESCAPE '\\' ORDER BY title COLLATE NOCASE"
        ))
        .bind(like_pattern(query))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_track).collect())
    }
}

/// `%query%` with LIKE's own wildcards escaped, so searching for "100%" or
/// "a_b" matches literally. LIKE is case-insensitive for ASCII by default.
fn like_pattern(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

fn row_to_artist(row: sqlx::sqlite::SqliteRow) -> Artist {
    let count: i64 = row.get("album_count");
    Artist { id: row.get("id"), name: row.get("name"), album_count: count as u32, cover_art: None }
}

fn row_to_album(row: sqlx::sqlite::SqliteRow) -> AlbumSummary {
    let count: i64 = row.get("song_count");
    let year: Option<i64> = row.get("year");
    AlbumSummary { id: row.get("id"), name: row.get("name"), song_count: count as u32, year: year.map(|y| y as u32) }
}

fn row_to_track(row: sqlx::sqlite::SqliteRow) -> Track {
    let duration: Option<i64> = row.get("duration_secs");
    let track_number: Option<i64> = row.get("track_number");
    let disc_number: Option<i64> = row.get("disc_number");
    Track {
        id: row.get("id"),
        title: row.get("title"),
        artist: row.get("artist"),
        album: row.get("album"),
        album_id: row.get("album_id"),
        duration_secs: duration.map(|d| d as u32),
        starred: row.get("starred"),
        cover_art: row.get("cover_art"),
        track_number: track_number.map(|n| n as u32),
        disc_number: disc_number.map(|n| n as u32),
    }
}

// How many artist/album fetches run concurrently during a sync. Bounded so a
// large library doesn't fire hundreds of simultaneous requests at once.
const SYNC_CONCURRENCY: usize = 8;

/// Pulls the full artist/album/track tree from the server and upserts it into
/// the local cache, calling `on_progress(done, total)` after each artist is
/// committed. Never blocks the TUI's first paint.
///
/// Fetches run concurrently (bounded by `SYNC_CONCURRENCY`) and each artist
/// is written as soon as its albums arrive, so memory stays flat on large
/// libraries and the UI can show entries before the whole sync is done.
pub async fn sync_library(
    client: &SubsonicClient,
    cache: &Cache,
    on_progress: impl Fn(usize, usize),
) -> Result<(), CacheError> {
    let remote_artists = client.get_artists().await.map_err(|e| CacheError::Client(e.to_string()))?;
    let total = remote_artists.len();

    let mut fetched = stream::iter(remote_artists)
        .map(|artist| async move {
            let albums = client.get_artist_albums(&artist.id).await.unwrap_or_default();
            let albums = stream::iter(albums)
                .map(|album| async move {
                    let songs = client.get_album(&album.id).await.map(|a| a.song).unwrap_or_default();
                    (album, songs)
                })
                .buffer_unordered(SYNC_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            (artist, albums)
        })
        .buffer_unordered(SYNC_CONCURRENCY);

    let mut done = 0;
    while let Some((artist, albums)) = fetched.next().await {
        write_artist(&cache.pool, &artist, &albums).await?;
        done += 1;
        on_progress(done, total);
    }
    Ok(())
}

async fn write_artist(pool: &SqlitePool, artist: &Artist, albums: &[(AlbumSummary, Vec<Track>)]) -> Result<(), CacheError> {
    let mut tx = pool.begin().await?;

    sqlx::query("INSERT OR REPLACE INTO artists (id, name, album_count) VALUES (?, ?, ?)")
        .bind(&artist.id)
        .bind(&artist.name)
        .bind(artist.album_count)
        .execute(&mut *tx)
        .await?;

    for (album, songs) in albums {
        sqlx::query("INSERT OR REPLACE INTO albums (id, name, song_count, artist_id, year) VALUES (?, ?, ?, ?, ?)")
            .bind(&album.id)
            .bind(&album.name)
            .bind(album.song_count)
            .bind(&artist.id)
            .bind(album.year)
            .execute(&mut *tx)
            .await?;

        for song in songs {
            sqlx::query(
                "INSERT OR REPLACE INTO tracks \
                 (id, title, artist, album, album_id, duration_secs, starred, cover_art, track_number, disc_number) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&song.id)
            .bind(&song.title)
            .bind(&song.artist)
            .bind(&song.album)
            .bind(song.album_id.as_deref().unwrap_or(&album.id))
            .bind(song.duration_secs)
            .bind(&song.starred)
            .bind(&song.cover_art)
            .bind(song.track_number)
            .bind(song.disc_number)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern(""), "%%");
    }
}
