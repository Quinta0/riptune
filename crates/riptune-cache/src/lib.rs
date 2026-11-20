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

        for (table, column) in SEARCHABLE {
            create_fts_index(&pool, table, column).await?;
        }

        Ok(Self { pool })
    }

    pub async fn all_artists(&self) -> Result<Vec<Artist>, CacheError> {
        let rows = sqlx::query("SELECT id, name, album_count FROM artists ORDER BY name COLLATE NOCASE")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_artist).collect())
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
        let rows = self.search("id, name, album_count", "artists", "name", query).await?;
        Ok(rows.into_iter().map(row_to_artist).collect())
    }

    pub async fn search_albums(&self, query: &str) -> Result<Vec<AlbumSummary>, CacheError> {
        let rows = self.search("id, name, song_count, year", "albums", "name", query).await?;
        Ok(rows.into_iter().map(row_to_album).collect())
    }

    pub async fn search_tracks(&self, query: &str) -> Result<Vec<Track>, CacheError> {
        let rows = self.search(TRACK_COLUMNS, "tracks", "title", query).await?;
        Ok(rows.into_iter().map(row_to_track).collect())
    }

    /// Case-insensitive substring search on `table.column`.
    ///
    /// Queries of 3+ characters go through the FTS5 trigram index, which
    /// on a 100k-track library is ~40x faster than a LIKE scan. Trigrams
    /// can't match anything shorter, so 1-2 character queries fall back to
    /// LIKE -- capped at `SHORT_QUERY_LIMIT` rows, since those match most of
    /// the library and loading every row is what made them slow.
    async fn search(
        &self,
        columns: &str,
        table: &str,
        column: &str,
        query: &str,
    ) -> Result<Vec<sqlx::sqlite::SqliteRow>, CacheError> {
        let (sql, arg) = if query.chars().count() >= 3 {
            (
                format!(
                    "SELECT {columns} FROM {table} \
                     WHERE rowid IN (SELECT rowid FROM {table}_fts WHERE {table}_fts MATCH ?) \
                     ORDER BY {column} COLLATE NOCASE"
                ),
                fts_phrase(query),
            )
        } else {
            (
                format!(
                    "SELECT {columns} FROM {table} WHERE {column} LIKE ? ESCAPE '\\' \
                     ORDER BY {column} COLLATE NOCASE LIMIT {SHORT_QUERY_LIMIT}"
                ),
                like_pattern(query),
            )
        };
        Ok(sqlx::query(&sql).bind(arg).fetch_all(&self.pool).await?)
    }
}

/// (table, column) pairs with a trigram full-text index.
const SEARCHABLE: [(&str, &str); 3] = [("artists", "name"), ("albums", "name"), ("tracks", "title")];

/// Max rows for 1-2 character searches (see `Cache::search`).
pub const SHORT_QUERY_LIMIT: usize = 500;

/// Creates `{table}_fts`, an external-content FTS5 index over one column,
/// plus the triggers that keep it in step with the table. Builds the index
/// from existing rows the first time, so caches from older versions get
/// search without a re-sync.
async fn create_fts_index(pool: &SqlitePool, table: &str, column: &str) -> Result<(), CacheError> {
    let exists: Option<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
        .bind(format!("{table}_fts"))
        .fetch_optional(pool)
        .await?;

    let fts = format!("{table}_fts");
    for stmt in [
        format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {fts} \
             USING fts5({column}, content='{table}', content_rowid='rowid', tokenize='trigram')"
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS {fts}_ai AFTER INSERT ON {table} BEGIN \
             INSERT INTO {fts}(rowid, {column}) VALUES (new.rowid, new.{column}); END"
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS {fts}_ad AFTER DELETE ON {table} BEGIN \
             INSERT INTO {fts}({fts}, rowid, {column}) VALUES ('delete', old.rowid, old.{column}); END"
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS {fts}_au AFTER UPDATE OF {column} ON {table} BEGIN \
             INSERT INTO {fts}({fts}, rowid, {column}) VALUES ('delete', old.rowid, old.{column}); \
             INSERT INTO {fts}(rowid, {column}) VALUES (new.rowid, new.{column}); END"
        ),
    ] {
        sqlx::query(&stmt).execute(pool).await?;
    }

    if exists.is_none() {
        sqlx::query(&format!("INSERT INTO {fts}({fts}) VALUES ('rebuild')")).execute(pool).await?;
    }
    Ok(())
}

/// Wraps user input as a single FTS5 phrase, so characters like `-`, `*`
/// or `AND` are searched literally instead of parsed as query syntax.
fn fts_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
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

    // Upserts rather than INSERT OR REPLACE: REPLACE deletes and re-inserts,
    // and that implicit delete doesn't fire the FTS delete triggers (unless
    // recursive_triggers is on), which would leave stale search entries.
    sqlx::query(
        "INSERT INTO artists (id, name, album_count) VALUES (?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, album_count = excluded.album_count",
    )
        .bind(&artist.id)
        .bind(&artist.name)
        .bind(artist.album_count)
        .execute(&mut *tx)
        .await?;

    for (album, songs) in albums {
        sqlx::query(
            "INSERT INTO albums (id, name, song_count, artist_id, year) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, song_count = excluded.song_count, \
             artist_id = excluded.artist_id, year = excluded.year",
        )
            .bind(&album.id)
            .bind(&album.name)
            .bind(album.song_count)
            .bind(&artist.id)
            .bind(album.year)
            .execute(&mut *tx)
            .await?;

        for song in songs {
            sqlx::query(
                "INSERT INTO tracks \
                 (id, title, artist, album, album_id, duration_secs, starred, cover_art, track_number, disc_number) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET title = excluded.title, artist = excluded.artist, \
                 album = excluded.album, album_id = excluded.album_id, duration_secs = excluded.duration_secs, \
                 starred = excluded.starred, cover_art = excluded.cover_art, \
                 track_number = excluded.track_number, disc_number = excluded.disc_number",
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

    fn track(id: &str, title: &str) -> Track {
        Track {
            id: id.into(),
            title: title.into(),
            artist: None,
            album: None,
            album_id: None,
            duration_secs: None,
            starred: None,
            cover_art: None,
            track_number: None,
            disc_number: None,
        }
    }

    async fn temp_cache(name: &str) -> (Cache, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("riptune-cache-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Cache::open(&dir.join("lib.sqlite")).await.unwrap(), dir)
    }

    async fn write(cache: &Cache, artist: &str, tracks: Vec<Track>) {
        let a = Artist { id: "ar1".into(), name: artist.into(), album_count: 1, cover_art: None };
        let al = AlbumSummary { id: "al1".into(), name: "Album".into(), song_count: tracks.len() as u32, year: None };
        write_artist(&cache.pool, &a, &[(al, tracks)]).await.unwrap();
    }

    fn titles(tracks: Vec<Track>) -> Vec<String> {
        tracks.into_iter().map(|t| t.title).collect()
    }

    #[tokio::test]
    async fn fts_search_is_case_insensitive_substring_and_stays_in_sync() {
        let (cache, dir) = temp_cache("sync").await;
        write(&cache, "Radiohead", vec![track("1", "Weird Fishes/Arpeggi"), track("2", "Reckoner")]).await;

        assert_eq!(titles(cache.search_tracks("FISH").await.unwrap()), ["Weird Fishes/Arpeggi"]);
        assert_eq!(titles(cache.search_tracks("s/a").await.unwrap()), ["Weird Fishes/Arpeggi"]);
        assert_eq!(cache.search_artists("diohe").await.unwrap().len(), 1);

        // A re-sync that renames a track must drop the old title from the index.
        write(&cache, "Radiohead", vec![track("1", "Nude"), track("2", "Reckoner")]).await;
        assert!(cache.search_tracks("fish").await.unwrap().is_empty());
        assert_eq!(titles(cache.search_tracks("nud").await.unwrap()), ["Nude"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn short_queries_and_query_syntax_are_literal() {
        let (cache, dir) = temp_cache("literal").await;
        write(&cache, "x", vec![track("1", "AC-DC \"Live\" * 100%"), track("2", "Other")]).await;

        assert_eq!(titles(cache.search_tracks("c-d").await.unwrap()), ["AC-DC \"Live\" * 100%"]);
        assert_eq!(titles(cache.search_tracks("\"live\"").await.unwrap()).len(), 1);
        assert_eq!(titles(cache.search_tracks("0%").await.unwrap()).len(), 1);
        assert_eq!(titles(cache.search_tracks("ot").await.unwrap()), ["Other"]);
        assert!(cache.search_tracks("zzz").await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn existing_rows_are_indexed_when_upgrading_an_old_cache() {
        let (cache, dir) = temp_cache("upgrade").await;
        write(&cache, "Portishead", vec![track("1", "Glory Box")]).await;
        // Simulate a cache from before FTS existed.
        for t in ["artists", "albums", "tracks"] {
            sqlx::query(&format!("DROP TABLE {t}_fts")).execute(&cache.pool).await.unwrap();
        }
        cache.pool.close().await;

        let reopened = Cache::open(&dir.join("lib.sqlite")).await.unwrap();
        assert_eq!(titles(reopened.search_tracks("glory").await.unwrap()), ["Glory Box"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern(""), "%%");
    }
}
