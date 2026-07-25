//! Lightweight async wrapper over the Subsonic REST API (compatible with
//! Navidrome, Airsonic, Gonic, etc).
//!
//! Handles the legacy salted-token auth scheme (`t = md5(password + salt)`), since
//! not all servers support Navidrome's newer API-key auth yet. See
//! <http://www.subsonic.org/pages/api.jsp> for the wire format.

pub mod models;

use md5::{Digest, Md5};
use rand::Rng;
use thiserror::Error;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "riptune";

#[derive(Debug, Error)]
pub enum SubsonicError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("server returned error {code}: {message}")]
    Api { code: i32, message: String },
    #[error("invalid server URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
}

#[derive(Clone)]
pub struct SubsonicClient {
    http: reqwest::Client,
    base_url: url::Url,
    username: String,
    password: String,
}

impl SubsonicClient {
    pub fn new(base_url: &str, username: &str, password: &str) -> Result<Self, SubsonicError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base_url: url::Url::parse(base_url)?,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    /// Builds the auth query params required on every request: `u`, `t`, `s`, `v`, `c`, `f`.
    fn auth_params(&self) -> Vec<(String, String)> {
        let salt: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(12)
            .map(char::from)
            .collect();
        let mut hasher = Md5::new();
        hasher.update(format!("{}{}", self.password, salt));
        let token = format!("{:x}", hasher.finalize());

        vec![
            ("u".into(), self.username.clone()),
            ("t".into(), token),
            ("s".into(), salt),
            ("v".into(), API_VERSION.into()),
            ("c".into(), CLIENT_NAME.into()),
            ("f".into(), "json".into()),
        ]
    }

    /// Returns a fully-authenticated streaming URL for a track ID. Handed directly
    /// to the audio thread's HTTP fetcher — no extra round trip needed.
    pub fn stream_url(&self, track_id: &str) -> Result<String, SubsonicError> {
        let mut url = self.base_url.join("rest/stream")?;
        {
            let mut qp = url.query_pairs_mut();
            for (k, v) in self.auth_params() {
                qp.append_pair(&k, &v);
            }
            qp.append_pair("id", track_id);
        }
        Ok(url.to_string())
    }

    /// Returns a fully-authenticated cover art URL for an id (album, artist,
    /// or track -- whatever Navidrome/Subsonic returned as `coverArt`).
    pub fn cover_art_url(&self, cover_art_id: &str) -> Result<String, SubsonicError> {
        let mut url = self.base_url.join("rest/getCoverArt")?;
        {
            let mut qp = url.query_pairs_mut();
            for (k, v) in self.auth_params() {
                qp.append_pair(&k, &v);
            }
            qp.append_pair("id", cover_art_id);
            qp.append_pair("size", "300");
        }
        Ok(url.to_string())
    }

    /// Fetches the raw image bytes for a cover art id. Unlike every other
    /// endpoint here, `getCoverArt` returns the image directly rather than a
    /// `subsonic-response` JSON envelope, so it bypasses `call<T>`.
    pub async fn get_cover_art(&self, cover_art_id: &str) -> Result<Vec<u8>, SubsonicError> {
        let url = self.cover_art_url(cover_art_id)?;
        let bytes = self.http.get(url).send().await?.error_for_status()?.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Sends an authenticated GET to `rest/{endpoint}`, unwraps the
    /// `subsonic-response` envelope, and either deserializes the payload into
    /// `T` or converts a server-side error into `SubsonicError::Api`.
    ///
    /// All Subsonic endpoints share this envelope shape, so every real method
    /// (`get_artists`, `get_album`, `ping`, ...) is a thin wrapper around this.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        extra_params: &[(&str, &str)],
    ) -> Result<T, SubsonicError> {
        let mut url = self.base_url.join(&format!("rest/{endpoint}"))?;
        {
            let mut qp = url.query_pairs_mut();
            for (k, v) in self.auth_params() {
                qp.append_pair(&k, &v);
            }
            for (k, v) in extra_params {
                qp.append_pair(k, v);
            }
        }

        let body: serde_json::Value = self.http.get(url).send().await?.json().await?;
        let root = body.get("subsonic-response").ok_or_else(|| SubsonicError::Api {
            code: -1,
            message: "response was missing the subsonic-response envelope".into(),
        })?;

        let status = root.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if status != "ok" {
            let error = root.get("error");
            let code = error.and_then(|e| e.get("code")).and_then(|c| c.as_i64()).unwrap_or(-1) as i32;
            let message = error
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("server returned an unspecified error")
                .to_string();
            return Err(SubsonicError::Api { code, message });
        }

        serde_json::from_value(root.clone()).map_err(|e| SubsonicError::Api {
            code: -1,
            message: format!("failed to parse response: {e}"),
        })
    }

    /// Cheapest possible round trip to confirm the URL, auth, and network
    /// path all work — hits `rest/ping` and checks for a non-error status.
    pub async fn ping(&self) -> Result<(), SubsonicError> {
        #[derive(serde::Deserialize)]
        struct PingResponse {
            #[allow(dead_code)]
            status: String,
        }
        self.call::<PingResponse>("ping", &[]).await?;
        Ok(())
    }

    pub async fn get_artists(&self) -> Result<Vec<models::Artist>, SubsonicError> {
        // We fetch the raw JSON value first so we can robustly extract the array
        // regardless of whether the server uses a flat index or nested structure.
        let response: serde_json::Value = self.call("getArtists", &[]).await?;

        // Some servers nest the index under 'artists', others put it at the
        // root -- try 'artists.index' first, then fall back to root 'index'.
        let index_array = response
            .get("artists")
            .and_then(|a| a.get("index"))
            .or_else(|| response.get("index"))
            .and_then(|i| i.as_array());

        let Some(index_array) = index_array else {
            return Ok(Vec::new());
        };

        Ok(index_array
            .iter()
            .filter_map(|group| group.get("artist").and_then(|a| a.as_array()))
            .flatten()
            .filter_map(|artist_val| serde_json::from_value::<models::Artist>(artist_val.clone()).ok())
            .collect())
    }

    pub async fn get_album(&self, album_id: &str) -> Result<models::Album, SubsonicError> {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            album: models::Album,
        }
        let wrapper: Wrapper = self.call("getAlbum", &[("id", album_id)]).await?;
        Ok(wrapper.album)
    }

    /// Fetches the album list for a single artist (rest/getArtist.view). Distinct
    /// from `get_album`, which fetches one album's full track listing.
    pub async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<models::AlbumSummary>, SubsonicError> {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            artist: ArtistDetail,
        }
        #[derive(serde::Deserialize)]
        struct ArtistDetail {
            #[serde(default)]
            album: Vec<models::AlbumSummary>,
        }
        let wrapper: Wrapper = self.call("getArtist", &[("id", artist_id)]).await?;
        Ok(wrapper.artist.album)
    }

    pub async fn star(&self, _track_id: &str) -> Result<(), SubsonicError> {
        // TODO: GET rest/star
        todo!("wire up star.view")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_url_includes_required_auth_params() {
        let client = SubsonicClient::new("https://music.example.com", "alice", "hunter2").unwrap();
        let url = client.stream_url("track-123").unwrap();

        assert!(url.starts_with("https://music.example.com/rest/stream"));
        for param in ["u=alice", "t=", "s=", "v=", "c=riptune", "f=json", "id=track-123"] {
            assert!(url.contains(param), "expected `{param}` in {url}");
        }
    }

    #[test]
    fn stream_url_never_leaks_the_raw_password() {
        let client = SubsonicClient::new("https://music.example.com", "alice", "hunter2").unwrap();
        let url = client.stream_url("track-123").unwrap();
        assert!(!url.contains("hunter2"), "raw password must never appear in the URL: {url}");
    }

    #[test]
    fn cover_art_url_includes_required_auth_params() {
        let client = SubsonicClient::new("https://music.example.com", "alice", "hunter2").unwrap();
        let url = client.cover_art_url("cover-123").unwrap();

        assert!(url.starts_with("https://music.example.com/rest/getCoverArt"));
        for param in ["u=alice", "t=", "s=", "v=", "c=riptune", "f=json", "id=cover-123"] {
            assert!(url.contains(param), "expected `{param}` in {url}");
        }
        assert!(!url.contains("hunter2"), "raw password must never appear in the URL: {url}");
    }
}
