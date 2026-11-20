//! Audio playback thread.
//!
//! Deliberately NOT async. Decoding+output (rodio, backed by cpal) run on a
//! dedicated OS thread; the only interface to the rest of the app is the
//! `AudioCommand` channel in and `AudioEvent` channel out. A stalled network
//! fetch can never block the tokio runtime or the TUI render loop.
//!
//! Tracks are streamed progressively: a download thread appends bytes into a
//! shared buffer and the decoder reads from it as they arrive (see
//! [`StreamBuffer`]), so playback starts after the first few kilobytes rather
//! than after the whole file. Opening a track (connect + format probe) runs
//! on a short-lived loader thread so the command loop stays responsive --
//! skipping through tracks quickly never queues up a backlog of downloads.

use anyhow::Result;
pub use riptune_types::{AudioCommand, AudioEvent, PlayToken};
use rodio::{Decoder, OutputStream, Sink};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

type TrackDecoder = Decoder<StreamBuffer>;

pub fn spawn(event_tx: UnboundedSender<AudioEvent>) -> Result<(Sender<AudioCommand>, JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
    let handle = std::thread::Builder::new()
        .name("riptune-audio".into())
        .spawn(move || run_audio_loop(cmd_rx, event_tx))?;
    Ok((cmd_tx, handle))
}

fn run_audio_loop(cmd_rx: Receiver<AudioCommand>, event_tx: UnboundedSender<AudioEvent>) {
    // _stream must be kept alive for as long as we want playback to work --
    // dropping it silently stops all audio.
    let (_stream, stream_handle) = match OutputStream::try_default() {
        Ok(pair) => pair,
        Err(e) => {
            let _ = event_tx.send(AudioEvent::Error(format!("failed to open audio output device: {e}")));
            return;
        }
    };

    let http = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // No overall timeout: it would also cover the body, and a long FLAC
        // over a slow link legitimately takes longer than any fixed limit.
        .timeout(None)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = event_tx.send(AudioEvent::Error(format!("failed to build http client: {e}")));
            return;
        }
    };

    let (loaded_tx, loaded_rx) = mpsc::channel::<(PlayToken, Result<TrackDecoder, String>)>();
    let mut sink: Option<Sink> = None;
    let mut current: Option<PlayToken> = None;
    let mut last_reported_sec: Option<u64> = None;
    let mut volume: f32 = 1.0;

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(AudioCommand::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(AudioCommand::Play { stream_url, token }) => {
                // Silence the old track immediately rather than letting it
                // run on while the new one buffers.
                sink = None;
                current = Some(token);
                last_reported_sec = None;
                let http = http.clone();
                let tx = loaded_tx.clone();
                let _ = std::thread::Builder::new()
                    .name("riptune-loader".into())
                    .spawn(move || {
                        let _ = tx.send((token, open_track(&http, &stream_url)));
                    });
            }
            Ok(AudioCommand::Pause) => {
                if let Some(s) = &sink {
                    s.pause();
                }
            }
            Ok(AudioCommand::Resume) => {
                if let Some(s) = &sink {
                    s.play();
                }
            }
            Ok(AudioCommand::Seek(pos)) => {
                if let Some(s) = &sink {
                    if let Err(e) = s.try_seek(pos) {
                        let _ = event_tx.send(AudioEvent::Error(format!("seek failed: {e}")));
                    }
                    last_reported_sec = None;
                }
            }
            Ok(AudioCommand::SetVolume(v)) => {
                volume = v;
                if let Some(s) = &sink {
                    s.set_volume(v);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Finished loads. Anything not matching the current token was
        // superseded by a later Play and is simply dropped, which also
        // stops its download (see StreamBuffer's Drop).
        while let Ok((token, result)) = loaded_rx.try_recv() {
            if Some(token) != current {
                continue;
            }
            match result.and_then(|decoder| {
                let s = Sink::try_new(&stream_handle).map_err(|e| format!("failed to open playback sink: {e}"))?;
                s.set_volume(volume);
                s.append(decoder);
                Ok(s)
            }) {
                Ok(s) => sink = Some(s),
                Err(e) => {
                    current = None;
                    let _ = event_tx.send(AudioEvent::LoadFailed(token, e));
                }
            }
        }

        if let (Some(s), Some(token)) = (&sink, current) {
            if s.empty() {
                let _ = event_tx.send(AudioEvent::Finished(token));
                sink = None;
                current = None;
            } else if !s.is_paused() {
                let pos = s.get_pos();
                if last_reported_sec != Some(pos.as_secs()) {
                    last_reported_sec = Some(pos.as_secs());
                    let _ = event_tx.send(AudioEvent::Position(token, pos));
                }
            }
        }
    }
}

/// Connects, starts the background download, and probes the format. Runs on
/// a loader thread; blocks only until the decoder has seen enough header
/// bytes to identify the stream.
fn open_track(http: &reqwest::blocking::Client, url: &str) -> Result<TrackDecoder, String> {
    let response = http
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("failed to fetch track: {e}"))?;
    let buffer = StreamBuffer::start(response);
    Decoder::new(buffer).map_err(|e| format!("failed to decode track: {e}"))
}

#[derive(Default)]
struct Shared {
    data: Vec<u8>,
    total_len: Option<u64>,
    done: bool,
    error: Option<String>,
}

/// A `Read + Seek` view over an HTTP body that is still downloading. Reads
/// past the downloaded edge block until the bytes arrive (or the download
/// ends); dropping the reader makes the download thread stop early.
struct StreamBuffer {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    pos: u64,
}

impl StreamBuffer {
    fn start(mut response: reqwest::blocking::Response) -> Self {
        let total_len = response.content_length();
        let shared = Arc::new((
            Mutex::new(Shared {
                data: Vec::with_capacity(total_len.unwrap_or(0).min(512 << 20) as usize),
                total_len,
                ..Default::default()
            }),
            Condvar::new(),
        ));

        let writer = Arc::clone(&shared);
        let _ = std::thread::Builder::new().name("riptune-download".into()).spawn(move || {
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                // Reader gone (track skipped/finished): stop downloading.
                if Arc::strong_count(&writer) == 1 {
                    return;
                }
                let read = response.read(&mut chunk);
                let (lock, cvar) = &*writer;
                let mut s = lock.lock().unwrap();
                match read {
                    Ok(0) => s.done = true,
                    Ok(n) => s.data.extend_from_slice(&chunk[..n]),
                    Err(e) => {
                        s.error = Some(e.to_string());
                        s.done = true;
                    }
                }
                let finished = s.done;
                drop(s);
                cvar.notify_all();
                if finished {
                    return;
                }
            }
        });

        Self { shared, pos: 0 }
    }
}

impl Read for StreamBuffer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (lock, cvar) = &*self.shared;
        let s = cvar
            .wait_while(lock.lock().unwrap(), |s| (s.data.len() as u64) <= self.pos && !s.done)
            .unwrap();
        let available = s.data.len() as u64;
        if self.pos >= available {
            return match &s.error {
                Some(e) => Err(io::Error::other(e.clone())),
                None => Ok(0),
            };
        }
        let start = self.pos as usize;
        let n = buf.len().min(s.data.len() - start);
        buf[..n].copy_from_slice(&s.data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamBuffer {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => {
                // Without a Content-Length the end is only known once the
                // download completes.
                let (lock, cvar) = &*self.shared;
                let s = cvar.wait_while(lock.lock().unwrap(), |s| s.total_len.is_none() && !s.done).unwrap();
                s.total_len.unwrap_or(s.data.len() as u64) as i64 + d
            }
        };
        if new_pos < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start of stream"));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_with(data: &[u8], done: bool) -> StreamBuffer {
        StreamBuffer {
            shared: Arc::new((
                Mutex::new(Shared { data: data.to_vec(), total_len: Some(data.len() as u64), done, error: None }),
                Condvar::new(),
            )),
            pos: 0,
        }
    }

    #[test]
    fn reads_and_seeks_within_downloaded_data() {
        let mut b = buffer_with(b"hello world", true);
        let mut out = [0u8; 5];
        b.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"hello");
        b.seek(SeekFrom::End(-5)).unwrap();
        let mut rest = Vec::new();
        b.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"world");
    }

    #[test]
    fn read_waits_for_more_data() {
        let b = buffer_with(b"ab", false);
        let shared = Arc::clone(&b.shared);
        let reader = std::thread::spawn(move || {
            let mut b = b;
            let mut all = Vec::new();
            b.read_to_end(&mut all).unwrap();
            all
        });
        std::thread::sleep(Duration::from_millis(20));
        {
            let (lock, cvar) = &*shared;
            let mut s = lock.lock().unwrap();
            s.data.extend_from_slice(b"cd");
            s.done = true;
            cvar.notify_all();
        }
        assert_eq!(reader.join().unwrap(), b"abcd");
    }
}
