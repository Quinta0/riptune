use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(rename = "albumCount", default)]
    pub album_count: u32,
    #[serde(rename = "coverArt", default)]
    pub cover_art: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumSummary {
    pub id: String,
    pub name: String,
    #[serde(rename = "songCount", default)]
    pub song_count: u32,
    #[serde(default)]
    pub year: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: String,
    #[serde(rename = "artistId")]
    pub artist_id: String,
    #[serde(default)]
    pub song: Vec<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(rename = "duration", default)]
    pub duration_secs: Option<u32>,
    #[serde(default)]
    pub starred: Option<String>,
    #[serde(rename = "coverArt", default)]
    pub cover_art: Option<String>,
    #[serde(rename = "track", default)]
    pub track_number: Option<u32>,
    #[serde(rename = "discNumber", default)]
    pub disc_number: Option<u32>,
}
