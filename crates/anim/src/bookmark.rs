//! Media bookmarks during a slide show.
//!
//! A pause effect whose `bookmark` is set (`onMediaBookmark`) is not a click step. While the
//! show plays that shape, crossing the bookmark's time pauses it; the next advance resumes it.

use deckcraft_model::ShapeId;

const MAX_MARKS: usize = 4_096;
/// Playback jitter smaller than this is not a seek backward (milliseconds).
const BACK_MS: u32 = 80;

/// Fired bookmark pauses for the current visit to a slide.
#[derive(Clone, Debug, Default)]
pub struct BookmarkClock {
    fired: Vec<(ShapeId, u32)>,
    last: Vec<(ShapeId, u32)>,
    /// Shape paused because playback reached one of its pause bookmarks.
    pub hold: Option<ShapeId>,
}

impl BookmarkClock {
    /// The earliest unfired pause bookmark at or before `position_ms`.
    ///
    /// `marks` is the clip's `(name, time ms)` list. `pauses` is the names of pause effects on
    /// this shape. A position that jumps backward by more than a frame forgets marks after the
    /// new position, so the same bookmark can pause again.
    pub fn due(&mut self, shape: ShapeId, marks: &[(String, u32)], pauses: &[String], position_ms: u32) -> Option<u32> {
        let position_ms = position_ms.min(86_400_000);
        if let Some(prev) = self.last.iter().find(|(s, _)| *s == shape).map(|(_, t)| *t)
            && position_ms.saturating_add(BACK_MS) < prev
        {
            self.fired.retain(|(s, t)| *s != shape || *t <= position_ms);
        }
        if let Some((_, t)) = self.last.iter_mut().find(|(s, _)| *s == shape) {
            *t = position_ms;
        } else if self.last.len() < MAX_MARKS {
            self.last.push((shape, position_ms));
        }
        let mut hit: Option<u32> = None;
        for (name, time) in marks.iter().take(1_000) {
            if *time > position_ms || !pauses.iter().any(|p| p == name) {
                continue;
            }
            if self.fired.iter().any(|(s, t)| *s == shape && *t == *time) {
                continue;
            }
            hit = Some(hit.map_or(*time, |h| h.min(*time)));
        }
        hit
    }

    /// Remember that `shape` paused at `time_ms`, and hold the show there.
    pub fn mark(&mut self, shape: ShapeId, time_ms: u32) {
        if self.fired.len() < MAX_MARKS && !self.fired.iter().any(|(s, t)| *s == shape && *t == time_ms) {
            self.fired.push((shape, time_ms));
        }
        self.hold = Some(shape);
        if let Some((_, t)) = self.last.iter_mut().find(|(s, _)| *s == shape) {
            *t = time_ms;
        } else if self.last.len() < MAX_MARKS {
            self.last.push((shape, time_ms));
        }
    }

    pub fn clear(&mut self) {
        self.fired.clear();
        self.last.clear();
        self.hold = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks() -> Vec<(String, u32)> {
        vec![("stop 1".into(), 1000), ("stop 2".into(), 3000)]
    }

    fn pauses() -> Vec<String> {
        vec!["stop 1".into(), "stop 2".into()]
    }

    #[test]
    fn a_pause_bookmark_fires_once_until_playback_moves_backward() {
        let mut c = BookmarkClock::default();
        let id = ShapeId(1);
        assert_eq!(c.due(id, &marks(), &pauses(), 400), None);
        assert_eq!(c.due(id, &marks(), &pauses(), 1500), Some(1000));
        c.mark(id, 1000);
        assert_eq!(c.hold, Some(id));
        c.hold = None;
        // Still sitting on the bookmark, including a frame of jitter, does not fire again.
        assert_eq!(c.due(id, &marks(), &pauses(), 990), None);
        assert_eq!(c.due(id, &marks(), &pauses(), 1600), None);
        assert_eq!(c.due(id, &marks(), &pauses(), 3100), Some(3000));
        c.mark(id, 3000);
        c.hold = None;
        // A seek back before both marks lets the first one pause again.
        assert_eq!(c.due(id, &marks(), &pauses(), 100), None);
        assert_eq!(c.due(id, &marks(), &pauses(), 1000), Some(1000));
    }

    #[test]
    fn a_bookmark_without_a_pause_effect_does_not_hold() {
        let mut c = BookmarkClock::default();
        let marks = vec![("intro".into(), 500u32)];
        assert_eq!(c.due(ShapeId(2), &marks, &["stop 1".into()], 800), None);
    }
}
