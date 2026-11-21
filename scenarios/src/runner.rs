use crate::builders::CommandSink;
use crate::MetricSample;
use simulation::economy::Government;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SimulationOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    executed_ticks: u64,
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

        Self::from_sim(Simulation::new_with_options(opts))
    }

    fn from_sim(sim: Simulation) -> Self {
        Self {
            sim,
            sched: Simulation::schedule(),
            samples: Vec::new(),
            tick_times: Vec::new(),
            queued: Vec::new(),
            executed_ticks: 0,
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
            self.executed_ticks += 1;

            if let Some(checkpoints) = &mut self.checkpoints {
                if self.executed_ticks % CHECKPOINT_INTERVAL == 0 {
                    checkpoints.push((self.sim.get_tick(), self.sim.hashes()));
                }
            }
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

        for (first, second) in first.iter().zip(&second) {
            compare_checkpoint(first, second)?;
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

    pub(crate) fn verify_resumed(
        &mut self,
        ticks: u64,
        run_for: impl Fn(&mut Self, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let opts = *self.sim.read::<SimulationOptions>();
        run_for(self, ticks)?;

        let midpoint = ticks / 2;
        let mut resumed = Self::new(opts);
        run_for(&mut resumed, midpoint)?;
        resumed.reload()?;
        resumed.run_ticks(ticks - midpoint);

        compare_checkpoint(&self.checkpoint(), &resumed.checkpoint())
    }

    pub(crate) fn reload(&mut self) -> Result<(), String> {
        // The public save API uses compressed Bincode under world/. Reserve a
        // private directory so concurrent runs cannot overwrite each other's saves.
        static NEXT_SAVE: AtomicUsize = AtomicUsize::new(0);
        fs::create_dir_all("world").map_err(|err| format!("could not create world/: {err}"))?;
        let (directory, path) = loop {
            let id = NEXT_SAVE.fetch_add(1, Ordering::Relaxed);
            let directory = format!("scenarios-resume-{}-{id}", std::process::id());
            let path = format!("world/{directory}");
            match fs::create_dir(&path) {
                Ok(()) => break (directory, path),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("could not create {path}: {err}")),
            }
        };

        let name = format!("{directory}/midpoint");
        self.sim.save_to_disk(&name);
        let sim = Simulation::load_from_disk(&name);
        fs::remove_dir_all(&path).map_err(|err| format!("could not remove {path}: {err}"))?;
        let sim = sim
            .ok_or_else(|| format!("could not round-trip save at tick {}", self.sim.get_tick()))?;
        compare_checkpoint(&self.checkpoint(), &(sim.get_tick(), sim.hashes()))?;
        self.sim = sim;
        self.sched = Simulation::schedule();
        Ok(())
    }

    fn checkpoint(&self) -> Checkpoint {
        (self.sim.get_tick(), self.sim.hashes())
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
        checkpoints.push(self.checkpoint());
        Ok(checkpoints)
    }
}

fn compare_checkpoint(first: &Checkpoint, second: &Checkpoint) -> Result<(), String> {
    let (tick, hashes) = first;
    let (other_tick, other_hashes) = second;
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
    Ok(())
}

impl CommandSink for ScenarioCtx {
    fn sim(&self) -> &Simulation {
        &self.sim
    }

    fn submit(&mut self, commands: &[WorldCommand]) {
        ScenarioCtx::submit(self, commands);
    }

    fn has_pending_commands(&self) -> bool {
        !self.queued.is_empty()
    }

    fn flush(&mut self) {
        self.run_ticks(1);
    }
}
