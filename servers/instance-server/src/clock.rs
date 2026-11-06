//! The instance actor's frame clock: absolute deadlines `base + n * tick`, like official
//! servers (docs/protocol/movement.md "Timing").
//!
//! Official servers keep a 33 ms clock without drift and catch a stall up with a burst of the
//! missed frames sent back to back (a 1,353 ms gap was followed by 41 ticks). The client plays
//! each frame with its recorded `frame_ms` whatever the real time, and speeds playback up by 5x
//! once 60 or more frames are queued; so a burst is capped at [`MAX_BURST`] frames. Beyond the
//! cap the rest of the missed time is dropped and the deadline base moves forward. The
//! simulation step stays fixed (`World::tick`, `dt = tick_ms / 1000`); only when frames run
//! changes.

use std::time::Duration;

use tokio::time::Instant;

/// Most frames one catch-up burst runs: one below the client's 5x playback threshold (60
/// queued frames, `0x14213f4f0`).
pub const MAX_BURST: u32 = 59;

#[derive(Debug, Clone)]
pub struct FrameClock {
    base: Instant,
    tick: Duration,
    /// Index of the next frame: its deadline is `base + next * tick`.
    next: u64,
    cap: u32,
}

impl FrameClock {
    /// A clock whose first frame is due one tick after `now`.
    pub fn new(now: Instant, tick_ms: u32, cap: u32) -> Self {
        Self {
            base: now,
            tick: Duration::from_millis(u64::from(tick_ms.max(1))),
            next: 1,
            cap: cap.max(1),
        }
    }

    /// Restarts the clock at `now` (an empty instance got its first player).
    pub fn reset(&mut self, now: Instant) {
        self.base = now;
        self.next = 1;
    }

    /// When the next frame is due.
    pub fn deadline(&self) -> Instant {
        self.base + self.tick * self.next as u32
    }

    /// How many frames are due at `now` (0 before the deadline), and advances past them. More
    /// than the cap: returns the cap and moves the base to `now`, dropping the rest.
    pub fn due(&mut self, now: Instant) -> u32 {
        if now < self.deadline() {
            return 0;
        }
        let elapsed = (now - self.base).as_nanos() / self.tick.as_nanos();
        let last = elapsed as u64;
        let count = last + 1 - self.next;
        if count > u64::from(self.cap) {
            self.reset(now);
            return self.cap;
        }
        // Keep the base from growing without bound: rebase on a whole number of ticks.
        self.next = last + 1;
        if self.next > 1 << 20 {
            self.base += self.tick * last as u32;
            self.next = 1;
        }
        count as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[tokio::test(start_paused = true)]
    async fn steady_ticks_follow_absolute_deadlines() {
        let start = Instant::now();
        let mut c = FrameClock::new(start, 33, MAX_BURST);
        assert_eq!(c.deadline(), at(start, 33));
        assert_eq!(c.due(at(start, 32)), 0);
        // Each tick wakes a little late; the next deadline does not move with it.
        for n in 1..=100u64 {
            assert_eq!(c.due(at(start, n * 33 + 5)), 1, "frame {n}");
            assert_eq!(c.deadline(), at(start, (n + 1) * 33));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stall_is_caught_up_with_a_burst() {
        let start = Instant::now();
        let mut c = FrameClock::new(start, 33, MAX_BURST);
        assert_eq!(c.due(at(start, 33)), 1);
        // Official: a 1,353 ms gap, then 41 ticks back to back.
        assert_eq!(c.due(at(start, 33 + 1353)), 41);
        assert_eq!(c.deadline(), at(start, 43 * 33));
        assert_eq!(c.due(at(start, 43 * 33)), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_stall_is_capped_and_moves_the_base() {
        let start = Instant::now();
        let mut c = FrameClock::new(start, 33, MAX_BURST);
        assert_eq!(c.due(at(start, 33)), 1);
        assert_eq!(c.due(at(start, 5033)), MAX_BURST);
        assert_eq!(c.deadline(), at(start, 5033 + 33));
        assert_eq!(c.due(at(start, 5033 + 33)), 1);
        // Exactly the cap is still a full catch-up.
        let mut c = FrameClock::new(start, 33, MAX_BURST);
        assert_eq!(c.due(at(start, 59 * 33)), 59);
        assert_eq!(c.deadline(), at(start, 60 * 33));
    }

    #[tokio::test(start_paused = true)]
    async fn the_clock_does_not_drift_over_a_long_session() {
        let start = Instant::now();
        let mut c = FrameClock::new(start, 33, MAX_BURST);
        let mut frames = 0u64;
        let mut now = start;
        // Wake 1 ms after each deadline for an hour of simulated time.
        while frames < 109_090 {
            now = c.deadline() + Duration::from_millis(1);
            frames += u64::from(c.due(now));
        }
        assert_eq!(c.deadline(), at(start, (frames + 1) * 33));
        assert!(now <= at(start, frames * 33 + 1));
    }
}
