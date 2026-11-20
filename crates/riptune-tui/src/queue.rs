//! The play queue: the list a track was started from, plus a play order.
//!
//! Shuffle is a pre-computed permutation rather than a random pick per
//! track, so every track plays once before any repeats and "previous" walks
//! back through what was actually heard.

use riptune_core::models::Track;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

pub struct Queue {
    tracks: Arc<[Track]>,
    order: Vec<usize>,
    /// Index into `order`.
    pos: usize,
}

impl Queue {
    pub fn new(tracks: Arc<[Track]>, start: usize, shuffle: bool) -> Self {
        let mut q = Self { order: (0..tracks.len()).collect(), tracks, pos: start };
        if shuffle {
            q.set_shuffle(true);
        }
        q
    }

    pub fn current(&self) -> Option<&Track> {
        self.order.get(self.pos).map(|&i| &self.tracks[i])
    }

    pub fn tracks(&self) -> &Arc<[Track]> {
        &self.tracks
    }

    /// Index of the current track in `tracks()` (not in play order).
    pub fn current_index(&self) -> Option<usize> {
        self.order.get(self.pos).copied()
    }

    /// Re-orders the remaining queue in place, keeping the current track
    /// current, so toggling shuffle mid-album never interrupts playback.
    pub fn set_shuffle(&mut self, on: bool) {
        let Some(current) = self.current_index() else { return };
        self.order = (0..self.tracks.len()).collect();
        if on {
            self.order.swap(0, current);
            shuffle(&mut self.order[1..]);
            self.pos = 0;
        } else {
            self.pos = current;
        }
    }

    /// Advances and returns the next track. `wrap` restarts from the top at
    /// the end of the queue (repeat-all).
    pub fn advance(&mut self, wrap: bool) -> Option<&Track> {
        if self.pos + 1 < self.order.len() {
            self.pos += 1;
        } else if wrap && !self.order.is_empty() {
            self.pos = 0;
        } else {
            return None;
        }
        self.current()
    }

    pub fn back(&mut self, wrap: bool) -> Option<&Track> {
        if self.pos > 0 {
            self.pos -= 1;
        } else if wrap && !self.order.is_empty() {
            self.pos = self.order.len() - 1;
        } else {
            return None;
        }
        self.current()
    }

    /// (1-based position in play order, queue length)
    pub fn progress(&self) -> (usize, usize) {
        (self.pos + 1, self.order.len())
    }
}

/// Fisher–Yates with a xorshift generator seeded from std's per-process
/// random hasher keys -- plenty for shuffling a playlist, no extra crate.
fn shuffle(items: &mut [usize]) {
    let mut state = std::collections::hash_map::RandomState::new().build_hasher().finish() | 1;
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        items.swap(i, (state % (i as u64 + 1)) as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks(n: usize) -> Arc<[Track]> {
        (0..n)
            .map(|i| Track {
                id: i.to_string(),
                title: format!("t{i}"),
                artist: None,
                album: None,
                album_id: None,
                duration_secs: None,
                starred: None,
                cover_art: None,
                track_number: None,
                disc_number: None,
            })
            .collect()
    }

    #[test]
    fn plays_in_order_and_stops_at_end() {
        let mut q = Queue::new(tracks(3), 1, false);
        assert_eq!(q.current().unwrap().id, "1");
        assert_eq!(q.advance(false).unwrap().id, "2");
        assert!(q.advance(false).is_none());
        assert_eq!(q.advance(true).unwrap().id, "0");
        assert_eq!(q.back(false).map(|t| t.id.clone()), None);
    }

    #[test]
    fn shuffle_keeps_current_and_visits_every_track_once() {
        let mut q = Queue::new(tracks(50), 7, true);
        assert_eq!(q.current().unwrap().id, "7");
        let mut seen = vec![q.current_index().unwrap()];
        while q.advance(false).is_some() {
            seen.push(q.current_index().unwrap());
        }
        seen.sort();
        assert_eq!(seen, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn unshuffle_resumes_natural_order_from_current() {
        let mut q = Queue::new(tracks(10), 4, true);
        q.advance(false);
        let current = q.current_index().unwrap();
        q.set_shuffle(false);
        assert_eq!(q.current_index(), Some(current));
        if current + 1 < 10 {
            assert_eq!(q.advance(false).unwrap().id, (current + 1).to_string());
        }
    }
}
