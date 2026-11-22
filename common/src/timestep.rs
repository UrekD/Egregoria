use std::time::{Duration, Instant};

const UP_DT: Duration = Duration::from_millis(20);

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
    sample_ticks: u32,
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
        assert!(!period.is_zero());
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
        let now = Instant::now();
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
                    f64::from(self.sample_ticks) * self.period.as_secs_f64()
                        / self.sample_time.as_secs_f64(),
                );
                self.sample_time = Duration::ZERO;
                self.sample_ticks = 0;
            }
        }

        let delta = if elapsed > self.period * 3 {
            self.period
        } else {
            elapsed
        };
        self.acc += delta * warp;
    }

    /// The measured speed multiplier for the current setting, after a full sample window.
    pub fn achieved_warp(&self, warp: u32) -> Option<f64> {
        self.achieved_warp.filter(|_| self.warp == warp)
    }

    pub fn tick(&mut self) -> bool {
        if self.acc < self.period {
            return false;
        }
        if self.last_time.elapsed() >= Self::MAXTIME {
            // Keep the fractional tick but drop catch-up work that no longer fits this frame.
            let remaining = self.acc.as_nanos() % self.period.as_nanos();
            self.acc = Duration::new(
                (remaining / 1_000_000_000) as u64,
                (remaining % 1_000_000_000) as u32,
            );
            return false;
        }
        self.acc -= self.period;
        self.sample_ticks += 1;
        true
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
}
