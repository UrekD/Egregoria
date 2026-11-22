use std::time::{Duration, Instant};

const UP_DT: Duration = Duration::from_millis(20);

/// The "as fast as possible" speed. It is high enough that the per-frame budget
/// (`Timestep::MAXTIME`), not the multiplier, limits the simulation, so the UI stays responsive.
pub const MAX_WARP: u32 = 10_000;

pub fn debug_up_dt() -> Duration {
    UP_DT
}

/// A timestep that can be used to update the game state.
/// It will try to keep a constant update rate.
/// Based on <https://gafferongames.com/post/fix_your_timestep/>
pub struct Timestep {
    last_time: Instant,
    acc: Duration,
    pub period: Duration,
    warp: u32,
    sample_time: Duration,
    sample_ticks: u64,
    achieved_warp: Option<f64>,
}

impl Default for Timestep {
    fn default() -> Self {
        Self::new(UP_DT)
    }
}

impl Timestep {
    const MAXTIME: Duration = Duration::from_millis(25);

    pub fn new(period: Duration) -> Self {
        let period = if period.is_zero() { UP_DT } else { period };
        Self {
            last_time: Instant::now(),
            acc: Default::default(),
            period,
            warp: 0,
            sample_time: Duration::ZERO,
            sample_ticks: 0,
            achieved_warp: None,
        }
    }

    pub fn prepare_frame(&mut self, warp: u32) {
        self.prepare_frame_at(warp, Instant::now());
    }

    fn prepare_frame_at(&mut self, warp: u32, now: Instant) {
        let elapsed = now.duration_since(self.last_time);
        self.last_time = now;

        // Measure completed ticks against wall time, including frames curtailed by the budget.
        if warp != self.warp {
            self.warp = warp;
            self.sample_time = Duration::ZERO;
            self.sample_ticks = 0;
            self.achieved_warp = None;
        } else if warp != 0 {
            self.sample_time += elapsed;
            if self.sample_time >= Duration::from_secs(1) {
                self.achieved_warp = Some(
                    self.sample_ticks as f64 * self.period.as_secs_f64()
                        / self.sample_time.as_secs_f64(),
                );
                self.sample_time = Duration::ZERO;
                self.sample_ticks = 0;
            }
        }

        // Some callers poll only once per frame. Whole ticks left over from
        // those polls should not accumulate indefinitely.
        self.discard_backlog();
        self.acc = self.acc.saturating_add(elapsed.saturating_mul(warp));
    }

    /// The measured speed multiplier for the current setting, after a full sample window.
    pub fn achieved_warp(&self, warp: u32) -> Option<f64> {
        self.achieved_warp.filter(|_| self.warp == warp)
    }

    pub fn tick(&mut self) -> bool {
        self.tick_at(Instant::now())
    }

    fn tick_at(&mut self, now: Instant) -> bool {
        if self.acc < self.period {
            return false;
        }
        if now.duration_since(self.last_time) >= Self::MAXTIME {
            // Keep the fractional tick but drop catch-up work that no longer fits this frame.
            self.discard_backlog();
            return false;
        }
        self.acc -= self.period;
        true
    }

    fn discard_backlog(&mut self) {
        if self.acc >= self.period {
            let remaining = self.acc.as_nanos() % self.period.as_nanos();
            self.acc = Duration::new(
                (remaining / 1_000_000_000) as u64,
                (remaining % 1_000_000_000) as u32,
            );
        }
    }

    /// Record a completed simulation tick, including a tick forced by commands.
    pub fn record_tick(&mut self) {
        self.sample_ticks = self.sample_ticks.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qf1_tick_drops_backlog_when_the_frame_budget_is_spent() {
        let mut step = Timestep::default();
        step.last_time = Instant::now() - Timestep::MAXTIME;
        step.acc = UP_DT * 10 + Duration::from_millis(5);

        assert!(!step.tick());
        assert_eq!(step.acc, Duration::from_millis(5));
    }

    #[test]
    fn qf1_zero_period_uses_the_default() {
        assert_eq!(Timestep::new(Duration::ZERO).period, UP_DT);
    }

    #[test]
    fn qf1_slow_frames_credit_the_time_available_to_the_budget() {
        for (warp, elapsed_ms) in [(20, 100), (50, 100), (50, 300)] {
            let mut step = Timestep::default();
            let frame = step.last_time + Duration::from_millis(elapsed_ms);
            step.prepare_frame_at(warp, frame);
            let mut count = 0;
            while step.tick_at(frame + Duration::from_micros(count * 30)) {
                count += 1;
            }
            assert_eq!(count, elapsed_ms * u64::from(warp) / 20);
        }
    }

    #[test]
    fn qf1_achieved_warp_counts_completed_and_forced_ticks() {
        let mut step = Timestep::default();
        let start = step.last_time;
        step.prepare_frame_at(1, start);
        assert!(!step.tick_at(start));
        step.record_tick(); // A command forced a tick even though none was due.

        let next_frame = start + UP_DT;
        step.prepare_frame_at(1, next_frame);
        assert!(step.tick_at(next_frame));
        step.record_tick();
        step.prepare_frame_at(1, start + Duration::from_secs(1));
        assert_eq!(step.achieved_warp(1), Some(0.04));
    }

    #[test]
    fn ux2_max_warp_runs_until_the_frame_budget_without_backlog() {
        let mut step = Timestep::default();
        let frame = step.last_time + Duration::from_millis(16);
        step.prepare_frame_at(MAX_WARP, frame);
        let tick_cost = Duration::from_millis(1);
        let mut count = 0;
        while step.tick_at(frame + tick_cost * count) {
            count += 1;
        }
        assert_eq!(count, 25);
        assert!(step.acc < step.period, "{:?} carried over", step.acc);

        // A slow frame still gets only one budget of ticks.
        let frame = frame + Duration::from_secs(2);
        step.prepare_frame_at(MAX_WARP, frame);
        let mut count = 0;
        while step.tick_at(frame + tick_cost * count) {
            count += 1;
        }
        assert_eq!(count, 25);
    }

    #[test]
    fn qf1_single_poll_callers_carry_the_fractional_remainder() {
        let mut step = Timestep::default();
        let frame = step.last_time + Duration::from_millis(105);
        step.prepare_frame_at(1, frame);
        assert!(step.tick_at(frame));
        let next_frame = frame + Duration::from_millis(15);
        step.prepare_frame_at(1, next_frame);
        assert!(step.tick_at(next_frame));
        assert!(!step.tick_at(next_frame));
    }
}
