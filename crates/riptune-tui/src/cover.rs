//! Cover art: fetched and decoded off the UI thread, kept in a small cache.

use image::DynamicImage;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use riptune_core::SubsonicClient;
use std::collections::{HashSet, VecDeque};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Recently shown covers kept ready to render. Small on purpose: each entry
/// holds a decoded image, and only the now-playing cover is ever on screen.
const CAPACITY: usize = 16;

pub struct Covers {
    /// None when the terminal has no usable image protocol; art is then skipped.
    picker: Option<Picker>,
    ready: VecDeque<(String, Box<dyn StatefulProtocol>)>,
    in_flight: HashSet<String>,
    tx: UnboundedSender<(String, Option<DynamicImage>)>,
    pub rx: UnboundedReceiver<(String, Option<DynamicImage>)>,
}

impl Covers {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self { picker: None, ready: VecDeque::new(), in_flight: HashSet::new(), tx, rx }
    }

    /// Must run while the terminal is in raw mode: the picker queries the
    /// terminal for its graphics protocol and font size.
    pub fn detect(&mut self) {
        self.picker = Picker::from_termios().ok().map(|mut p| {
            p.guess_protocol();
            p
        });
    }

    pub fn request(&mut self, client: &SubsonicClient, id: &str) {
        if self.picker.is_none() || self.in_flight.contains(id) || self.ready.iter().any(|(k, _)| k == id) {
            return;
        }
        self.in_flight.insert(id.to_string());
        let client = client.clone();
        let tx = self.tx.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let image = match client.get_cover_art(&id).await {
                // Decoding a JPEG is CPU work; keep it off the async workers.
                Ok(bytes) => tokio::task::spawn_blocking(move || image::load_from_memory(&bytes).ok())
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            let _ = tx.send((id, image));
        });
    }

    pub fn on_loaded(&mut self, id: String, image: Option<DynamicImage>) {
        self.in_flight.remove(&id);
        let (Some(image), Some(picker)) = (image, &mut self.picker) else { return };
        if self.ready.len() >= CAPACITY {
            self.ready.pop_front();
        }
        self.ready.push_back((id, picker.new_resize_protocol(image)));
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Box<dyn StatefulProtocol>> {
        self.ready.iter_mut().find(|(k, _)| k == id).map(|(_, p)| p)
    }
}
