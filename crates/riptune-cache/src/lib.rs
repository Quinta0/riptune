//! Local SQLite index of the remote Subsonic library.
//!
//! Why this exists: hitting `getArtists`/`getAlbum` over the network on every TUI
//! launch is slow on large libraries (multi-second waits are common on Airsonic
//! instances with 50k+ tracks). We index artists/albums/tracks locally and treat
//! the network as a background sync source, not the read path for rendering[cite: 13].

use futures_util::stream::{self, StreamExt};
use riptune_core::SubsonicClient;
use riptune_core::models::{Artist, Track, AlbumSummary};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use sqlx::Row;
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

#[derive(Clone)]
pub struct Cache {
    pool: SqlitePool,
}

impl Cache {
    pub async fn open(path: &Path) -> Result<Self, CacheError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new().max_connections(4).connect(&url).await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS artists (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                album_count INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS albums (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                song_count INTEGER NOT NULL DEFAULT 0,
                artist_id TEXT NOT NULL
            );
            "#,
        )
        .execute(&pool)
        .await?;

        // FIXED: Changed starred format to TEXT to cleanly accommodate Subsonic String timestamps[cite: 14]
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS tracks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                artist TEXT,
                album TEXT,
                duration_secs INTEGER,
                starred TEXT
            );
            "#,
        )
        .execute(&pool)
        .await?;

        // This is a rebuildable cache, not source-of-truth data, so schema
        // evolution is a best-effort ALTER rather than a real migration --
        // ignore the error if a column already exists. SQLite only allows
        // one ADD COLUMN per statement.
        for alter in [
            "ALTER TABLE tracks ADD COLUMN cover_art TEXT",
            "ALTER TABLE tracks ADD COLUMN track_number INTEGER",
            "ALTER TABLE tracks ADD COLUMN disc_number INTEGER",
            "ALTER TABLE albums ADD COLUMN year INTEGER",
        ] {
            let _ = sqlx::query(alter).execute(&pool).await;
        }

        Ok(Self { pool })
    }

    pub async fn get_albums_by_artist(&self, artist_id: &str) -> Result<Vec<AlbumSummary>, CacheError> {
        let rows = sqlx::query(
            "SELECT id, name, song_count, year FROM albums WHERE artist_id = ? ORDER BY name ASC"
        )
        .bind(artist_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_album).collect())
    }

    /// Tracklist for an album, in soundtrack order (disc, then track number)
    /// rather than alphabetically -- falls back to title only for the rare
    /// track missing a number, so it still sorts predictably.
    pub async fn get_tracks_by_album(&self, album_name: &str) -> Result<Vec<Track>, CacheError> {
        // FIXED: Added CAST protection to match what we did in the search query!
        let rows = sqlx::query(
            "SELECT id, title, artist, album, duration_secs, CAST(starred AS TEXT) as starred, cover_art, track_number, disc_number \
             FROM tracks WHERE album = ? \
             ORDER BY COALESCE(disc_number, 1) ASC, COALESCE(track_number, 999999) ASC, title ASC"
        )
        .bind(album_name)
        .fetch_all(&self.pool)
        .await?;

        let records = rows.into_iter().map(row_to_track).collect();

        Ok(records)
    }

    pub async fn search_local_artists(&self, query: &str) -> Result<Vec<Artist>, CacheError> {
        let sql_pattern = format!("%{}%", query);
        let rows = sqlx::query(
            "SELECT id, name, album_count FROM artists WHERE name LIKE ? COLLATE NOCASE ORDER BY name ASC"
        )
        .bind(sql_pattern)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_artist).collect())
    }

    pub async fn search_local_albums(&self, query: &str) -> Result<Vec<AlbumSummary>, CacheError> {
        let sql_pattern = format!("%{}%", query);
        let rows = sqlx::query(
            "SELECT id, name, song_count, year FROM albums WHERE name LIKE ? COLLATE NOCASE ORDER BY name ASC"
        )
        .bind(sql_pattern)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_album).collect())
    }

    pub async fn search_local_tracks(&self, query: &str) -> Result<Vec<Track>, CacheError> {
        let sql_pattern = format!("%{}%", query);
        // FIXED: Using CAST forces SQLite to evaluate the column as a string primitive
        // regardless of whether the internal row data was stored as an integer or text.
        let rows = sqlx::query(
            "SELECT id, title, artist, album, duration_secs, CAST(starred AS TEXT) as starred, cover_art, track_number, disc_number \
             FROM tracks WHERE title LIKE ? COLLATE NOCASE ORDER BY title ASC"
        )
        .bind(sql_pattern)
        .fetch_all(&self.pool)
        .await?;

        let records = rows.into_iter().map(row_to_track).collect();

        Ok(records)
    }
}

fn row_to_artist(row: sqlx::sqlite::SqliteRow) -> Artist {
    let count: i64 = row.get("album_count");
    Artist {
        id: row.get("id"),
        name: row.get("name"),
        album_count: count as u32,
        cover_art: None,
    }
}

fn row_to_album(row: sqlx::sqlite::SqliteRow) -> AlbumSummary {
    let count: i64 = row.get("song_count");
    let year: Option<i64> = row.get("year");
    AlbumSummary {
        id: row.get("id"),
        name: row.get("name"),
        song_count: count as u32,
        year: year.map(|y| y as u32),
    }
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
        duration_secs: duration.map(|d| d as u32),
        starred: row.get("starred"),
        cover_art: row.get("cover_art"),
        track_number: track_number.map(|n| n as u32),
        disc_number: disc_number.map(|n| n as u32),
    }
}

// How many artist/album fetches run concurrently during a sync. Bounded so a
// large library doesn't fire hundreds of simultaneous requests at once --
// this is a request-count/latency tradeoff, not a hard server limit.
const SYNC_CONCURRENCY: usize = 8;

/// Pulls the full artist/album/track tree from the server and upserts it into
/// the local cache. Intended to run as a background task on startup, and
/// periodically thereafter — never blocks the TUI's first paint[cite: 13].
///
/// Fetching is done concurrently (bounded by `SYNC_CONCURRENCY`) since the
/// Subsonic API requires one request per artist for albums and one more per
/// album for full track data -- doing those serially is what made sync slow
/// on real libraries. Writes stay sequential below: SQLite only has one
/// writer anyway, and it's cheap compared to the network round trips.
pub async fn sync_library(client: &SubsonicClient, cache: &Cache) -> Result<(), CacheError> {
    let remote_artists = client.get_artists().await
        .map_err(|e| CacheError::Client(e.to_string()))?;

    let fetched = stream::iter(remote_artists)
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
        .buffer_unordered(SYNC_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    for (artist, albums) in fetched {
        let mut tx = cache.pool.begin().await?;

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
                sqlx::query("INSERT OR REPLACE INTO tracks (id, title, artist, album, duration_secs, starred, cover_art, track_number, disc_number) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(&song.id)
                    .bind(&song.title)
                    .bind(&song.artist)
                    .bind(&song.album)
                    .bind(song.duration_secs)
                    .bind(&song.starred) // Safely saves text strings directly into SQLite schema rows[cite: 14]
                    .bind(&song.cover_art)
                    .bind(song.track_number)
                    .bind(song.disc_number)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        tx.commit().await?;
    }

    Ok(())
}