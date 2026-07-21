//! Local SQLite index of the remote Subsonic library.
//!
//! Why this exists: hitting `getArtists`/`getAlbum` over the network on every TUI
//! launch is slow on large libraries (multi-second waits are common on Airsonic
//! instances with 50k+ tracks). We index artists/albums/tracks locally and treat
//! the network as a background sync source, not the read path for rendering[cite: 13].

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

        Ok(Self { pool })
    }

    pub async fn get_albums_by_artist(&self, artist_id: &str) -> Result<Vec<AlbumSummary>, CacheError> {
        let rows = sqlx::query(
            "SELECT id, name, song_count FROM albums WHERE artist_id = ? ORDER BY name ASC"
        )
        .bind(artist_id)
        .fetch_all(&self.pool)
        .await?;

        let records = rows.into_iter().map(|row| {
            let count: i64 = row.get("song_count");
            AlbumSummary {
                id: row.get("id"),
                name: row.get("name"),
                song_count: count as u32,
            }
        }).collect();

        Ok(records)
    }

    pub async fn get_tracks_by_album(&self, album_name: &str) -> Result<Vec<Track>, CacheError> {
        // FIXED: Added CAST protection to match what we did in the search query!
        let rows = sqlx::query(
            "SELECT id, title, artist, album, duration_secs, CAST(starred AS TEXT) as starred FROM tracks WHERE album = ? ORDER BY title ASC"
        )
        .bind(album_name)
        .fetch_all(&self.pool)
        .await?;

        let records = rows.into_iter().map(|row| {
            let duration: Option<i64> = row.get("duration_secs");
            Track {
                id: row.get("id"),
                title: row.get("title"),
                artist: row.get("artist"),
                album: row.get("album"),
                duration_secs: duration.map(|d| d as u32),
                starred: row.get("starred"),
            }
        }).collect();

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

        let records = rows.into_iter().map(|row| {
            let count: i64 = row.get("album_count");
            Artist {
                id: row.get("id"),
                name: row.get("name"),
                album_count: count as u32,
                cover_art: None,
            }
        }).collect();

        Ok(records)
    }

    pub async fn search_local_albums(&self, query: &str) -> Result<Vec<AlbumSummary>, CacheError> {
        let sql_pattern = format!("%{}%", query);
        let rows = sqlx::query(
            "SELECT id, name, song_count FROM albums WHERE name LIKE ? COLLATE NOCASE ORDER BY name ASC"
        )
        .bind(sql_pattern)
        .fetch_all(&self.pool)
        .await?;

        let records = rows.into_iter().map(|row| {
            let count: i64 = row.get("song_count");
            AlbumSummary {
                id: row.get("id"),
                name: row.get("name"),
                song_count: count as u32,
            }
        }).collect();

        Ok(records)
    }

    pub async fn search_local_tracks(&self, query: &str) -> Result<Vec<Track>, CacheError> {
        let sql_pattern = format!("%{}%", query);
        // FIXED: Using CAST forces SQLite to evaluate the column as a string primitive 
        // regardless of whether the internal row data was stored as an integer or text.
        let rows = sqlx::query(
            "SELECT id, title, artist, album, duration_secs, CAST(starred AS TEXT) as starred FROM tracks WHERE title LIKE ? COLLATE NOCASE ORDER BY title ASC"
        )
        .bind(sql_pattern)
        .fetch_all(&self.pool)
        .await?;

        let records = rows.into_iter().map(|row| {
            let duration: Option<i64> = row.get("duration_secs");
            Track {
                id: row.get("id"),
                title: row.get("title"),
                artist: row.get("artist"),
                album: row.get("album"),
                duration_secs: duration.map(|d| d as u32),
                starred: row.get("starred"), 
            }
        }).collect();

        Ok(records)
    }
}

/// Pulls the full artist/album/track tree from the server and upserts it into
/// the local cache. Intended to run as a background task on startup, and
/// periodically thereafter — never blocks the TUI's first paint[cite: 13].
pub async fn sync_library(client: &SubsonicClient, cache: &Cache) -> Result<(), CacheError> {
    let remote_artists = client.get_artists().await
        .map_err(|e| CacheError::Client(e.to_string()))?;

    for artist in remote_artists {
        let mut tx = cache.pool.begin().await?;

        sqlx::query("INSERT OR REPLACE INTO artists (id, name, album_count) VALUES (?, ?, ?)")
            .bind(&artist.id)
            .bind(&artist.name)
            .bind(artist.album_count)
            .execute(&mut *tx)
            .await?;

        if let Ok(albums) = client.get_artist_albums(&artist.id).await {
            for album in albums {
                sqlx::query("INSERT OR REPLACE INTO albums (id, name, song_count, artist_id) VALUES (?, ?, ?, ?)")
                    .bind(&album.id)
                    .bind(&album.name)
                    .bind(album.song_count)
                    .bind(&artist.id)
                    .execute(&mut *tx)
                    .await?;

                if let Ok(full_album) = client.get_album(&album.id).await {
                    for song in full_album.song {
                        sqlx::query("INSERT OR REPLACE INTO tracks (id, title, artist, album, duration_secs, starred) VALUES (?, ?, ?, ?, ?, ?)")
                            .bind(&song.id)
                            .bind(&song.title)
                            .bind(&song.artist)
                            .bind(&song.album)
                            .bind(song.duration_secs)
                            .bind(&song.starred) // Safely saves text strings directly into SQLite schema rows[cite: 14]
                            .execute(&mut *tx)
                            .await?;
                    }
                }
            }
        }

        tx.commit().await?;
    }

    Ok(())
}