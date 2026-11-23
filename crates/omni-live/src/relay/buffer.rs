//! The relay's segment buffer and release clock. Pure: every decision takes
//! the current time.
//!
//! Segments pulled from Kick are held back as a cushion and released into the
//! served playlist at real-time pace, so the player sees a steady live
//! playlist whether Kick delivers early, late or in bursts. While the cushion
//! lasts, a Kick stall, token refresh or playlist hiccup never reaches the
//! player.

use std::collections::VecDeque;

use bytes::Bytes;

/// The advertised target duration: Kick's segments are 2 s and its playlists
/// declare 6. Players start three target durations behind the served edge,
/// which is the player-side buffer on top of the cushion. It only grows, and
/// only for a segment that would otherwise break RFC 8216 4.3.3.1.
pub const TARGET_DURATION_SECS: i64 = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferConfig {
    /// Content held back from the player.
    pub cushion_ms: i64,
    /// Released at once when playback starts: three target durations, so the
    /// player starts with its own full buffer.
    pub startup_release_ms: i64,
    /// Held back again before releasing resumes after the cushion ran dry.
    pub rebuffer_ms: i64,
    /// Cushion above `cushion_ms + excess_ms` is released early, so latency
    /// cannot creep up when Kick delivers faster than real time.
    pub excess_ms: i64,
    /// Released content listed in the served playlist.
    pub window_ms: i64,
    /// Released content kept for players still fetching older segments.
    pub retain_ms: i64,
    /// Hard cap on held segment bytes; the oldest segments go first.
    pub max_bytes: usize,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            cushion_ms: 30_000,
            startup_release_ms: 3 * TARGET_DURATION_SECS * 1000,
            rebuffer_ms: 15_000,
            excess_ms: 12_000,
            window_ms: 24_000,
            retain_ms: 36_000,
            max_bytes: 160 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferPhase {
    /// Nothing released yet; waiting for cushion plus startup release.
    Filling,
    /// Releasing one segment per segment duration.
    Playing,
    /// The cushion ran dry; waiting for `rebuffer_ms` before releasing again.
    Rebuffering,
}

#[derive(Clone, Debug)]
struct Held {
    sequence: u64,
    duration_ms: i64,
    data: Bytes,
    discontinuity: bool,
}

#[derive(Clone, Debug)]
pub struct RelayBuffer {
    config: BufferConfig,
    held: VecDeque<Held>,
    /// The first `released` entries of `held` are visible to players.
    released: usize,
    next_sequence: u64,
    next_release_at: i64,
    phase: BufferPhase,
    pending_discontinuity: bool,
    /// Discontinuity tags on segments no longer held.
    evicted_discontinuities: u64,
    bytes: usize,
    target_secs: i64,
    stalls: u32,
    dropped: u32,
    discontinuities: u32,
}

impl RelayBuffer {
    pub fn new(config: BufferConfig) -> Self {
        Self {
            config,
            held: VecDeque::new(),
            released: 0,
            next_sequence: 0,
            next_release_at: 0,
            phase: BufferPhase::Filling,
            pending_discontinuity: false,
            evicted_discontinuities: 0,
            bytes: 0,
            target_secs: TARGET_DURATION_SECS,
            stalls: 0,
            dropped: 0,
            discontinuities: 0,
        }
    }

    pub fn phase(&self) -> BufferPhase {
        self.phase
    }

    pub fn config(&self) -> &BufferConfig {
        &self.config
    }

    /// Held content not yet released.
    pub fn cushion_ms(&self) -> i64 {
        self.held
            .iter()
            .skip(self.released)
            .map(|s| s.duration_ms)
            .sum()
    }

    /// Before the first release: how much more content startup needs.
    pub fn startup_remaining_ms(&self) -> Option<i64> {
        (self.phase == BufferPhase::Filling).then(|| {
            (self.config.cushion_ms + self.config.startup_release_ms - self.cushion_ms()).max(0)
        })
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Times the cushion ran dry during playback.
    pub fn stalls(&self) -> u32 {
        self.stalls
    }

    /// Segments dropped unreleased by the byte cap.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    pub fn discontinuities(&self) -> u32 {
        self.discontinuities
    }

    /// The next pushed segment starts a new timeline (a gap, a restart, a
    /// new broadcast).
    pub fn mark_discontinuity(&mut self) {
        if !self.held.is_empty() {
            self.pending_discontinuity = true;
        }
    }

    pub fn push(&mut self, duration_ms: i64, data: Bytes, discontinuity: bool) {
        let discontinuity = (discontinuity || std::mem::take(&mut self.pending_discontinuity))
            && !self.held.is_empty();
        if discontinuity {
            self.discontinuities += 1;
        }
        self.bytes += data.len();
        self.target_secs = self.target_secs.max((duration_ms + 500) / 1000);
        self.held.push_back(Held {
            sequence: self.next_sequence,
            duration_ms: duration_ms.max(1),
            data,
            discontinuity,
        });
        self.next_sequence += 1;
        self.enforce_cap();
    }

    fn release_one(&mut self) -> Option<i64> {
        let duration = self.held.get(self.released)?.duration_ms;
        self.released += 1;
        Some(duration)
    }

    /// Releases whatever the clock says is due.
    pub fn advance(&mut self, now: i64) {
        loop {
            match self.phase {
                BufferPhase::Filling => {
                    if self.startup_remaining_ms() != Some(0) {
                        break;
                    }
                    let mut released = 0;
                    let mut last = 0;
                    while released < self.config.startup_release_ms {
                        match self.release_one() {
                            Some(d) => {
                                released += d;
                                last = d;
                            }
                            None => break,
                        }
                    }
                    self.next_release_at = now + last;
                    self.phase = BufferPhase::Playing;
                }
                BufferPhase::Rebuffering => {
                    if self.cushion_ms() < self.config.rebuffer_ms {
                        break;
                    }
                    self.next_release_at = now;
                    self.phase = BufferPhase::Playing;
                }
                BufferPhase::Playing => {
                    while now >= self.next_release_at {
                        match self.release_one() {
                            Some(d) => self.next_release_at += d,
                            None => {
                                self.phase = BufferPhase::Rebuffering;
                                self.stalls += 1;
                                break;
                            }
                        }
                    }
                    if self.phase == BufferPhase::Playing {
                        while self.cushion_ms() > self.config.cushion_ms + self.config.excess_ms {
                            if self.release_one().is_none() {
                                break;
                            }
                        }
                    }
                    break;
                }
            }
        }
        self.evict_released();
    }

    fn evict_released(&mut self) {
        loop {
            let retained: i64 = self
                .held
                .iter()
                .take(self.released)
                .map(|s| s.duration_ms)
                .sum();
            match self.held.front() {
                Some(front)
                    if self.released > 1
                        && retained - front.duration_ms >= self.config.retain_ms =>
                {
                    self.pop_front();
                }
                _ => break,
            }
        }
    }

    fn pop_front(&mut self) {
        let Some(front) = self.held.pop_front() else {
            return;
        };
        self.bytes -= front.data.len();
        if self.released > 0 {
            self.released -= 1;
            if front.discontinuity {
                self.evicted_discontinuities += 1;
            }
            return;
        }
        // Dropped unreleased: no player saw it, so its tag (or, once playing,
        // the skipped content itself) moves to the next segment.
        self.dropped += 1;
        let tag = front.discontinuity || self.phase != BufferPhase::Filling;
        match self.held.front_mut() {
            Some(next) if tag && !next.discontinuity => {
                next.discontinuity = true;
                if !front.discontinuity {
                    self.discontinuities += 1;
                }
            }
            Some(_) if front.discontinuity => self.discontinuities -= 1,
            _ => {}
        }
    }

    fn enforce_cap(&mut self) {
        while self.bytes > self.config.max_bytes && self.held.len() > 1 {
            self.pop_front();
        }
    }

    /// The released segments the served playlist lists: the newest ones
    /// within `window_ms`, at least one.
    fn window_start(&self) -> usize {
        let mut start = self.released;
        let mut total = 0;
        while start > 0 {
            let duration = self.held[start - 1].duration_ms;
            if start < self.released && total + duration > self.config.window_ms {
                break;
            }
            total += duration;
            start -= 1;
        }
        start
    }

    /// The served media playlist, or `None` before anything is released.
    /// Segment URIs are `<sequence>.ts`, relative to the playlist.
    pub fn render(&self) -> Option<String> {
        if self.released == 0 {
            return None;
        }
        let start = self.window_start();
        let first = &self.held[start];
        let earlier = self
            .held
            .iter()
            .take(start + 1)
            .filter(|s| s.discontinuity)
            .count();
        let discontinuity_sequence = self.evicted_discontinuities + earlier as u64;
        let mut out = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n\
             #EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity_sequence}\n",
            self.target_secs, first.sequence
        );
        for (index, segment) in self.held.iter().enumerate().take(self.released).skip(start) {
            if segment.discontinuity && index != start {
                out.push_str("#EXT-X-DISCONTINUITY\n");
            }
            #[allow(clippy::cast_precision_loss)]
            let seconds = segment.duration_ms as f64 / 1000.0;
            out.push_str(&format!("#EXTINF:{seconds:.3},\n{}.ts\n", segment.sequence));
        }
        Some(out)
    }

    /// A released segment's bytes.
    pub fn segment(&self, sequence: u64) -> Option<Bytes> {
        self.held
            .iter()
            .take(self.released)
            .find(|s| s.sequence == sequence)
            .map(|s| s.data.clone())
    }

    /// Releases everything and drops every byte (the relay stopped).
    pub fn clear(&mut self) {
        self.held.clear();
        self.released = 0;
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BufferConfig {
        BufferConfig {
            max_bytes: 1 << 30,
            ..BufferConfig::default()
        }
    }

    fn fill(buffer: &mut RelayBuffer, count: usize) {
        for _ in 0..count {
            buffer.push(2000, Bytes::from_static(b"ts"), false);
        }
    }

    fn listed(buffer: &RelayBuffer) -> Vec<String> {
        buffer
            .render()
            .unwrap_or_default()
            .lines()
            .filter(|l| l.ends_with(".ts"))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn nothing_is_served_until_cushion_and_startup_release_are_held() {
        let mut buffer = RelayBuffer::new(config());
        fill(&mut buffer, 23);
        buffer.advance(0);
        assert_eq!(buffer.phase(), BufferPhase::Filling);
        assert_eq!(buffer.startup_remaining_ms(), Some(2000));
        assert!(buffer.render().is_none());
        assert!(buffer.segment(0).is_none());
        fill(&mut buffer, 1);
        buffer.advance(0);
        assert_eq!(buffer.phase(), BufferPhase::Playing);
        assert_eq!(listed(&buffer).len(), 9);
        assert_eq!(buffer.cushion_ms(), 30_000);
    }

    #[test]
    fn releases_at_real_time_pace_and_rides_out_a_source_stall() {
        let mut buffer = RelayBuffer::new(config());
        fill(&mut buffer, 24);
        buffer.advance(0);
        // Kick delivers nothing for 28 s: the served edge keeps moving.
        let mut served = listed(&buffer).last().cloned();
        for second in (2..=28).step_by(2) {
            buffer.advance(second * 1000);
            let edge = listed(&buffer).last().cloned();
            assert_ne!(edge, served, "edge stopped at {second}s");
            served = edge;
        }
        assert_eq!(buffer.stalls(), 0);
        assert_eq!(buffer.phase(), BufferPhase::Playing);
        // Kick catches up from its window before the cushion runs out.
        fill(&mut buffer, 14);
        buffer.advance(29_000);
        assert_eq!(buffer.stalls(), 0);
        assert!(buffer.cushion_ms() >= 28_000);
    }

    #[test]
    fn a_dry_cushion_counts_a_stall_and_waits_to_rebuffer() {
        let mut buffer = RelayBuffer::new(config());
        fill(&mut buffer, 24);
        buffer.advance(0);
        buffer.advance(30_000);
        assert_eq!(buffer.phase(), BufferPhase::Playing);
        buffer.advance(32_000);
        assert_eq!(buffer.phase(), BufferPhase::Rebuffering);
        assert_eq!(buffer.stalls(), 1);
        let edge = listed(&buffer).last().cloned();
        fill(&mut buffer, 7);
        buffer.advance(40_000);
        assert_eq!(listed(&buffer).last().cloned(), edge);
        fill(&mut buffer, 1);
        buffer.advance(41_000);
        assert_eq!(buffer.phase(), BufferPhase::Playing);
        assert_ne!(listed(&buffer).last().cloned(), edge);
    }

    #[test]
    fn excess_cushion_is_released_early() {
        let mut buffer = RelayBuffer::new(config());
        fill(&mut buffer, 24);
        buffer.advance(0);
        fill(&mut buffer, 10);
        buffer.advance(0);
        assert_eq!(buffer.cushion_ms(), 42_000);
    }

    #[test]
    fn playlist_window_sequences_and_discontinuities() {
        let mut buffer = RelayBuffer::new(config());
        fill(&mut buffer, 24);
        buffer.advance(0);
        buffer.mark_discontinuity();
        fill(&mut buffer, 4);
        buffer.advance(0);
        // Release through the discontinuity at sequence 24.
        buffer.advance(36_000);
        let text = buffer.render().unwrap();
        assert!(text.contains("#EXT-X-TARGETDURATION:6\n"));
        let lines: Vec<&str> = text.lines().collect();
        let tag = lines
            .iter()
            .position(|l| *l == "#EXT-X-DISCONTINUITY")
            .unwrap();
        assert_eq!(lines[tag + 2], "24.ts");
        assert_eq!(listed(&buffer).len(), 12);
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:15\n"), "{text}");
        assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:0\n"));
        // Once the tagged segment heads the window it is counted, not printed.
        fill(&mut buffer, 12);
        buffer.advance(36_000);
        buffer.advance(54_000);
        let text = buffer.render().unwrap();
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:24\n"), "{text}");
        assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"));
        assert!(!text.contains("#EXT-X-DISCONTINUITY\n"));
        assert_eq!(buffer.discontinuities(), 1);
    }

    #[test]
    fn released_segments_age_out_after_the_retain_window() {
        let mut buffer = RelayBuffer::new(config());
        fill(&mut buffer, 60);
        buffer.advance(0);
        buffer.advance(60_000);
        assert!(buffer.segment(0).is_none());
        let retained: Vec<u64> = (0..60).filter(|s| buffer.segment(*s).is_some()).collect();
        assert_eq!(retained.len(), 18);
        assert!(listed(&buffer).len() <= 12);
    }

    #[test]
    fn the_byte_cap_drops_oldest_and_marks_the_gap() {
        let mut buffer = RelayBuffer::new(BufferConfig {
            max_bytes: 10,
            ..BufferConfig::default()
        });
        for _ in 0..8 {
            buffer.push(2000, Bytes::from_static(b"abcd"), false);
        }
        assert!(buffer.bytes() <= 10);
        assert_eq!(buffer.bytes(), 8);
        assert_eq!(buffer.dropped(), 6);
        // Nothing was served yet, so the skipped start is no discontinuity.
        assert_eq!(buffer.discontinuities(), 0);
    }

    #[test]
    fn a_cap_drop_while_playing_marks_the_skip_once() {
        let mut buffer = RelayBuffer::new(BufferConfig {
            cushion_ms: 2_000,
            startup_release_ms: 2_000,
            retain_ms: 2_000,
            max_bytes: 16,
            ..BufferConfig::default()
        });
        for _ in 0..2 {
            buffer.push(2000, Bytes::from_static(b"abcd"), false);
        }
        buffer.advance(0);
        assert_eq!(buffer.phase(), BufferPhase::Playing);
        // Three more fill the cap; the fourth pushes out the released one,
        // the fifth an unreleased one the player never saw.
        for _ in 0..4 {
            buffer.push(2000, Bytes::from_static(b"abcd"), false);
        }
        assert_eq!(buffer.dropped(), 1);
        assert_eq!(buffer.discontinuities(), 1);
        assert!(buffer.bytes() <= 16);
    }

    #[test]
    fn a_long_segment_raises_the_target_duration() {
        let mut buffer = RelayBuffer::new(BufferConfig {
            cushion_ms: 0,
            startup_release_ms: 1,
            ..config()
        });
        buffer.push(6_400, Bytes::from_static(b"ts"), false);
        buffer.advance(0);
        assert!(
            buffer
                .render()
                .unwrap()
                .contains("#EXT-X-TARGETDURATION:6\n")
        );
        buffer.push(6_600, Bytes::from_static(b"ts"), false);
        buffer.advance(10_000);
        assert!(
            buffer
                .render()
                .unwrap()
                .contains("#EXT-X-TARGETDURATION:7\n")
        );
    }

    #[test]
    fn clear_frees_everything() {
        let mut buffer = RelayBuffer::new(config());
        fill(&mut buffer, 30);
        buffer.advance(0);
        buffer.clear();
        assert_eq!(buffer.bytes(), 0);
        assert!(buffer.render().is_none());
    }
}
