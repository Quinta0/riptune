//! Audio playback thread.
//!
//! Deliberately NOT async. Decoding+output (rodio, backed by cpal) run on a
//! dedicated OS thread; the only interface to the rest of the app is the
//! bounded `AudioCommand` channel in and `AudioEvent` channel out. A stalled
//! network fetch here can never block the tokio runtime or the TUI render
//! loop, because it isn't sharing a thread with either of them.
//!
//! Current implementation downloads a track fully before playback starts
//! (see the comment in `handle_command` below) rather than streaming
//! decode-as-you-download. That's a deliberate MVP simplification, not an
//! oversight -- real progressive streaming is a good next step once this is
//! working end to end.

use crate::config::Config;
use anyhow::Result;
pub use riptune_types::{AudioCommand, AudioEvent};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use std::io::Cursor;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

pub fn spawn(_cfg: Config) -> Result<(Sender<AudioCommand>, Receiver<AudioEvent>, JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
    let (event_tx, event_rx) = mpsc::channel::<AudioEvent>();

    let handle = std::thread::Builder::new()
        .name("riptune-audio".into())
        .spawn(move || run_audio_loop(cmd_rx, event_tx))?;

    Ok((cmd_tx, event_rx, handle))
}

fn run_audio_loop(cmd_rx: Receiver<AudioCommand>, event_tx: Sender<AudioEvent>) {
    // _stream must be kept alive for as long as we want playback to work --
    // dropping it silently stops all audio. It intentionally lives for the
    // rest of this function (i.e. the thread's whole lifetime).
    let (_stream, stream_handle) = match OutputStream::try_default() {
        Ok(pair) => pair,
        Err(e) => {
            let _ = event_tx.send(AudioEvent::Error(format!("failed to open audio output device: {e}")));
            return;
        }
    };

    let http = reqwest::blocking::Client::new();
    let mut sink: Option<Sink> = None;
    // Owned by the audio thread, not the TUI, so a freshly created Sink for
    // the next track always starts at the volume the user last set instead
    // of silently resetting to 100%.
    let mut current_volume: f32 = 1.0;

    loop {
        // Short timeout so we still get to check on playback progress
        // (position/finished) even when no new command has arrived.
        match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(AudioCommand::Stop) => {
                // Drop sink explicitly to free audio hardware resources
                drop(sink.take());
                break; // Exit the audio thread loop!
            }
            Ok(command) => handle_command(
                command,
                &stream_handle,
                &mut sink,
                &http,
                &event_tx,
                &mut current_volume,
            ),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        if let Some(active) = &sink {
            if active.empty() {
                if !active.is_paused() {
                    // Queue drained without an explicit Stop -> the track played through.
                    let _ = event_tx.send(AudioEvent::TrackFinished);
                    sink = None;
                }
            } else if !active.is_paused() {
                // Deliberately skip sending a position update while paused --
                // otherwise the TUI keeps receiving "still playing" signals
                // for a track that isn't, and flips its own paused state
                // back to Playing within a couple hundred milliseconds.
                let _ = event_tx.send(AudioEvent::PositionChanged(active.get_pos()));
            }
        }
    }
}

fn handle_command(
    command: AudioCommand,
    stream_handle: &OutputStreamHandle,
    sink: &mut Option<Sink>,
    http: &reqwest::blocking::Client,
    event_tx: &Sender<AudioEvent>,
    current_volume: &mut f32,
) {
    match command {
        AudioCommand::Play { stream_url } => {
            let _ = event_tx.send(AudioEvent::Buffering);

            // NOTE: fetches the whole track into memory before decoding. Fine
            // for typical track sizes (a few MB); the "async pipeline" this
            // project advertises still holds at the thread-isolation level
            // (this blocking call can never stall the TUI or tokio runtime),
            // but true progressive streaming -- decoding as bytes arrive --
            // is real follow-up work, not yet done.
            let response = match http.get(&stream_url).send().and_then(|r| r.error_for_status()) {
                Ok(r) => r,
                Err(e) => {
                    let _ = event_tx.send(AudioEvent::Error(format!("failed to fetch track: {e}")));
                    return;
                }
            };
            let bytes = match response.bytes() {
                Ok(b) => b,
                Err(e) => {
                    let _ = event_tx.send(AudioEvent::Error(format!("failed to read track body: {e}")));
                    return;
                }
            };

            let source = match Decoder::new(Cursor::new(bytes.to_vec())) {
                Ok(s) => s,
                Err(e) => {
                    let _ = event_tx.send(AudioEvent::Error(format!("failed to decode track: {e}")));
                    return;
                }
            };

            let new_sink = match Sink::try_new(stream_handle) {
                Ok(s) => s,
                Err(e) => {
                    let _ = event_tx.send(AudioEvent::Error(format!("failed to open playback sink: {e}")));
                    return;
                }
            };
            new_sink.set_volume(*current_volume);
            new_sink.append(source);
            new_sink.play();
            *sink = Some(new_sink);
        }
        AudioCommand::Pause => {
            if let Some(s) = sink.as_ref() {
                s.pause();
            }
        }
        AudioCommand::Resume => {
            if let Some(s) = sink.as_ref() {
                s.play();
            }
        }
        AudioCommand::Stop => {
            *sink = None;
        }
        AudioCommand::Seek(pos) => {
            if let Some(s) = sink.as_ref() {
                if let Err(e) = s.try_seek(pos) {
                    let _ = event_tx.send(AudioEvent::Error(format!("seek failed: {e}")));
                }
            }
        }
        AudioCommand::SetVolume(volume) => {
            *current_volume = volume;
            if let Some(s) = sink.as_ref() {
                s.set_volume(volume);
            }
        }
    }
}