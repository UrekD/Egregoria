use crate::MetricSample;
use simulation::economy::Government;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SimulationOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Once;
use std::time::Duration;

const CHECKPOINT_INTERVAL: u64 = 10_000;
type Checkpoint = (u64, BTreeMap<String, u64>);

pub struct Scenario {
    pub name: &'static str,
    pub run: fn(&mut ScenarioCtx) -> Result<(), String>,
}

pub struct ScenarioCtx {
    pub sim: Simulation,
    sched: SeqSchedule,
    pub samples: Vec<MetricSample>,
    tick_times: Vec<Duration>,
    queued: Vec<WorldCommand>,
    checkpoints: Option<Vec<Checkpoint>>,
}

/// Set the process working directory to the repository root for prototype loading.
pub fn ensure_repo_root_cwd() {
    std::env::set_current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .expect("could not set the working directory to the repository root");
}

impl ScenarioCtx {
    pub fn new(opts: SimulationOptions) -> Self {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            ensure_repo_root_cwd();
            simulation::init::init();
        });

        Self {
            sim: Simulation::new_with_options(opts),
            sched: Simulation::schedule(),
            samples: Vec::new(),
            tick_times: Vec::new(),
            queued: Vec::new(),
            checkpoints: None,
        }
    }

    /// Queue commands in order for the next tick.
    pub fn submit(&mut self, cmds: &[WorldCommand]) {
        self.queued.extend_from_slice(cmds);
    }

    pub fn run_ticks(&mut self, n: u64) {
        for _ in 0..n {
            self.tick_times
                .push(self.sim.tick(&mut self.sched, &self.queued));
            self.queued.clear();

            if let Some(checkpoints) = &mut self.checkpoints {
                let tick = self.sim.get_tick();
                if tick % CHECKPOINT_INTERVAL == 0 {
                    checkpoints.push((tick, self.sim.hashes()));
                }
            }
        }
    }

    pub(crate) fn flush_commands(&mut self) {
        if !self.queued.is_empty() {
            self.run_ticks(1);
        }
    }

    /// Capture current world counts and tick throughput since the previous sample.
    pub fn sample(&mut self, label: &str) -> MetricSample {
        let elapsed: f64 = self.tick_times.iter().map(Duration::as_secs_f64).sum();
        let ticks_per_sec = if elapsed > 0.0 {
            self.tick_times.len() as f64 / elapsed
        } else {
            0.0
        };
        self.tick_times.clear();

        let world = self.sim.world();
        let employed = world.humans.values().filter(|h| h.work.is_some()).count();
        let sample = MetricSample {
            tick: self.sim.get_tick(),
            label: label.to_string(),
            humans: world.humans.len(),
            vehicles: world.vehicles.len(),
            companies: world.companies.len(),
            trains: world.trains.len(),
            money_cents: self.sim.read::<Government>().money.cents(),
            ticks_per_sec,
            extra: BTreeMap::from([("employed".to_string(), employed as f64)]),
        };
        self.samples.push(sample.clone());
        sample
    }

    /// Run from this fresh context and another with identical options, retaining the first run.
    pub fn verify_twice(
        &mut self,
        mut run: impl FnMut(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let opts = *self.sim.read::<SimulationOptions>();
        let first = self.record_checkpoints(&mut run)?;
        let second = Self::new(opts).record_checkpoints(&mut run)?;

        for ((tick, hashes), (other_tick, other_hashes)) in first.iter().zip(&second) {
            if tick != other_tick {
                return Err(format!(
                    "determinism checkpoint ticks differ ({tick} and {other_tick})"
                ));
            }
            if hashes != other_hashes {
                let changed: BTreeSet<_> = hashes
                    .keys()
                    .chain(other_hashes.keys())
                    .filter(|key| hashes.get(*key) != other_hashes.get(*key))
                    .map(String::as_str)
                    .collect();
                let changed: Vec<_> = changed.into_iter().collect();
                return Err(format!(
                    "determinism mismatch at tick {tick} ({})",
                    changed.join(", ")
                ));
            }
        }
        if first.len() != second.len() {
            return Err(format!(
                "determinism checkpoint counts differ ({} and {})",
                first.len(),
                second.len()
            ));
        }
        Ok(())
    }

    fn record_checkpoints(
        &mut self,
        run: &mut impl FnMut(&mut Self) -> Result<(), String>,
    ) -> Result<Vec<Checkpoint>, String> {
        if self.checkpoints.is_some() {
            return Err("determinism comparisons cannot be nested".to_string());
        }
        self.checkpoints = Some(Vec::new());
        let result = run(self);
        let mut checkpoints = self.checkpoints.take().unwrap();
        result?;
        checkpoints.push((self.sim.get_tick(), self.sim.hashes()));
        Ok(checkpoints)
    }
}
