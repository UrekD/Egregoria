use crate::builders::CommandSink;
use crate::MetricSample;
use serde::Serialize;
use simulation::economy::Government;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SimulationOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;
use std::time::Duration;

pub const MEDIAN_TICKS_PER_SEC: &str = "median_ticks_per_sec";
pub const P99_MS_PER_TICK: &str = "p99_ms_per_tick";
const P95_MS_PER_TICK: &str = "p95_ms_per_tick";

const CHECKPOINT_INTERVAL: u64 = 10_000;
type Checkpoint = (u64, BTreeMap<String, u64>);

#[derive(Default)]
struct HashDump {
    first: Vec<Checkpoint>,
    second: Option<Vec<Checkpoint>>,
}

#[derive(Serialize)]
struct HashLine<'a> {
    scenario: &'a str,
    run: usize,
    tick: u64,
    hashes: &'a BTreeMap<String, u64>,
}

#[derive(Serialize)]
pub struct PerfTimings {
    system_timings: BTreeMap<&'static str, SystemTiming>,
    max_ms_per_tick: f64,
    max_tick: u64,
}

#[derive(Serialize)]
struct SystemTiming {
    median_ms: f64,
    p99_ms: f32,
    max_ms: f32,
    max_tick: u64,
}

#[derive(Default)]
struct PerfTimingRecorder {
    systems: BTreeMap<&'static str, Vec<(u64, f32)>>,
    peak: Option<(u64, Duration)>,
    measured: Option<PerfTimings>,
}

impl PerfTimingRecorder {
    fn record_tick(
        &mut self,
        tick: u64,
        elapsed: Duration,
        system_samples: impl IntoIterator<Item = (&'static str, f32)>,
    ) {
        for (system, ms) in system_samples {
            self.systems.entry(system).or_default().push((tick, ms));
        }
        if self.peak.is_none_or(|(_, max)| elapsed >= max) {
            self.peak = Some((tick, elapsed));
        }
    }

    fn finish_phase(&mut self, label: &str) {
        if let Some((max_tick, elapsed)) = self.peak.take().filter(|_| label == "measured") {
            let system_timings = self
                .systems
                .iter_mut()
                .filter(|(_, samples)| !samples.is_empty())
                .map(|(&system, samples)| {
                    // Preserve tick order for equal durations so the last maximum wins.
                    samples.sort_by(|a, b| a.1.total_cmp(&b.1));
                    let n = samples.len();
                    let median_ms =
                        (f64::from(samples[(n - 1) / 2].1) + f64::from(samples[n / 2].1)) / 2.0;
                    let p99_ms = samples[(n * 99).div_ceil(100) - 1].1;
                    let (max_tick, max_ms) = samples[n - 1];
                    let timing = SystemTiming {
                        median_ms,
                        p99_ms,
                        max_ms,
                        max_tick,
                    };
                    (system, timing)
                })
                .collect();
            self.measured = Some(PerfTimings {
                system_timings,
                max_ms_per_tick: elapsed.as_secs_f64() * 1_000.0,
                max_tick,
            });
        }
        for samples in self.systems.values_mut() {
            samples.clear();
        }
    }
}

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
    hash_dump: Option<HashDump>,
    perf_timings: Option<PerfTimingRecorder>,
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
            hash_dump: None,
            perf_timings: None,
        }
    }

    /// Queue commands in order for the next tick.
    pub fn submit(&mut self, cmds: &[WorldCommand]) {
        self.queued.extend_from_slice(cmds);
    }

    pub fn run_ticks(&mut self, n: u64) {
        for _ in 0..n {
            let elapsed = self.sim.tick(&mut self.sched, &self.queued);
            self.tick_times.push(elapsed);
            self.queued.clear();
            self.executed_ticks += 1;

            if let Some(timings) = &mut self.perf_timings {
                timings.record_tick(self.sim.get_tick(), elapsed, self.sched.latest_samples());
            }
            if self.executed_ticks % CHECKPOINT_INTERVAL == 0 {
                let checkpoints = self
                    .checkpoints
                    .as_mut()
                    .or_else(|| self.hash_dump.as_mut().map(|dump| &mut dump.first));
                if let Some(checkpoints) = checkpoints {
                    checkpoints.push((self.sim.get_tick(), self.sim.hashes()));
                }
            }
        }
    }

    /// Collect raw per-tick timing samples for the perf JSON report.
    pub fn enable_perf_timings(&mut self) {
        self.perf_timings = Some(PerfTimingRecorder::default());
    }

    pub fn take_measured_timings(&mut self) -> Option<PerfTimings> {
        self.perf_timings.as_mut()?.measured.take()
    }

    /// Capture current world counts and tick throughput since the previous sample.
    pub fn sample(&mut self, label: &str) -> MetricSample {
        self.sample_with_extra(label, BTreeMap::new())
    }

    pub(crate) fn sample_perf(&mut self, label: &str, houses: usize) -> MetricSample {
        let active_vehicles = self
            .sim
            .world()
            .vehicles
            .values()
            .filter(|vehicle| !vehicle.vehicle.state.is_parked())
            .count();
        let mut extra = BTreeMap::from([
            ("houses".to_string(), houses as f64),
            ("active_vehicles".to_string(), active_vehicles as f64),
        ]);
        if !self.tick_times.is_empty() {
            let times = &mut self.tick_times;
            times.sort_unstable();
            let n = times.len();
            let median_seconds =
                (times[(n - 1) / 2].as_secs_f64() + times[n / 2].as_secs_f64()) / 2.0;
            let median_ticks_per_sec = if median_seconds > 0.0 {
                median_seconds.recip()
            } else {
                0.0
            };
            // Use the nearest rank for tail latencies.
            let percentile_ms = |percent: usize| {
                let rank = (n * percent).div_ceil(100);
                times[rank - 1].as_secs_f64() * 1_000.0
            };
            extra.extend([
                (MEDIAN_TICKS_PER_SEC.to_string(), median_ticks_per_sec),
                (P95_MS_PER_TICK.to_string(), percentile_ms(95)),
                (P99_MS_PER_TICK.to_string(), percentile_ms(99)),
            ]);
        }
        self.sample_with_extra(label, extra)
    }

    fn sample_with_extra(&mut self, label: &str, mut extra: BTreeMap<String, f64>) -> MetricSample {
        let elapsed: f64 = self.tick_times.iter().map(Duration::as_secs_f64).sum();
        let ticks_per_sec = if elapsed > 0.0 {
            self.tick_times.len() as f64 / elapsed
        } else {
            0.0
        };
        self.tick_times.clear();
        if let Some(timings) = &mut self.perf_timings {
            timings.finish_phase(label);
        }

        let world = self.sim.world();
        let employed = world.humans.values().filter(|h| h.work.is_some()).count();
        extra.insert("employed".to_string(), employed as f64);
        let sample = MetricSample {
            tick: self.sim.get_tick(),
            label: label.to_string(),
            humans: world.humans.len(),
            vehicles: world.vehicles.len(),
            companies: world.companies.len(),
            trains: world.trains.len(),
            money_cents: self.sim.read::<Government>().money.cents(),
            ticks_per_sec,
            extra,
        };
        self.samples.push(sample.clone());
        sample
    }

    /// Run a scenario and emit its periodic and final checkpoints as JSON lines.
    pub fn run_with_hashes(
        &mut self,
        scenario: &str,
        out: &mut impl Write,
        run: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.hash_dump.is_some() {
            return Err("hash dumps cannot be nested".to_string());
        }
        self.hash_dump = Some(HashDump::default());
        let result = run(self);
        let HashDump { mut first, second } = self.hash_dump.take().unwrap();
        result?;
        if second.is_none() {
            first.push(self.checkpoint());
        }
        for (index, checkpoints) in std::iter::once(first).chain(second).enumerate() {
            for (tick, hashes) in &checkpoints {
                let line = HashLine {
                    scenario,
                    run: index + 1,
                    tick: *tick,
                    hashes,
                };
                serde_json::to_writer(&mut *out, &line).map_err(|err| err.to_string())?;
                out.write_all(b"\n").map_err(|err| err.to_string())?;
            }
        }
        Ok(())
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
        if let Some(dump) = &mut self.hash_dump {
            dump.first = first;
            dump.second = Some(second);
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
        // Reserve a private directory so concurrent runs cannot share a midpoint save.
        static NEXT_SAVE: AtomicUsize = AtomicUsize::new(0);
        let save_dir = simulation::save_dir();
        fs::create_dir_all(save_dir)
            .map_err(|err| format!("could not create {}: {err}", save_dir.display()))?;
        let (directory, path) = loop {
            let id = NEXT_SAVE.fetch_add(1, Ordering::Relaxed);
            let directory = format!("scenarios-resume-{}-{id}", std::process::id());
            let path = save_dir.join(&directory);
            match fs::create_dir(&path) {
                Ok(()) => break (directory, path),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("could not create {}: {err}", path.display())),
            }
        };

        let name = format!("{directory}/midpoint");
        self.sim.save_to_disk(&name);
        let sim = Simulation::load_from_disk(&name);
        fs::remove_dir_all(&path)
            .map_err(|err| format!("could not remove {}: {err}", path.display()))?;
        let sim = sim.map_err(|err| {
            format!(
                "could not round-trip save at tick {}: {err}",
                self.sim.get_tick()
            )
        })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn hashes_dump_includes_checkpoints_and_final_for_each_run() {
        let options = SimulationOptions {
            terrain_size: 0,
            save_replay: false,
        };
        let run = |ctx: &mut ScenarioCtx| {
            ctx.run_ticks(CHECKPOINT_INTERVAL + 1);
            Ok(())
        };

        for twice in [false, true] {
            let mut ctx = ScenarioCtx::new(options);
            let start = ctx.sim.get_tick();
            let mut dump = Vec::new();
            ctx.run_with_hashes("test", &mut dump, |ctx| {
                if twice {
                    ctx.verify_twice(run)
                } else {
                    run(ctx)
                }
            })
            .unwrap();

            let lines: Vec<Value> = std::str::from_utf8(&dump)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(lines.len(), if twice { 4 } else { 2 });
            for (index, pair) in lines.chunks_exact(2).enumerate() {
                for (offset, line) in pair.iter().enumerate() {
                    assert_eq!(line["scenario"], "test");
                    assert_eq!(line["run"], index + 1);
                    assert_eq!(line["tick"], start + CHECKPOINT_INTERVAL + offset as u64);
                    assert!(line["hashes"]["world"].is_u64());
                    assert!(line["hashes"]["government"].is_u64());
                    assert_eq!(line["hashes"], lines[offset]["hashes"]);
                }
            }
            assert_eq!(lines[1]["hashes"], json!(ctx.sim.hashes()));
        }
    }

    #[test]
    fn perf_timing_uses_all_raw_samples_in_the_measured_phase() {
        let mut timings = PerfTimingRecorder::default();
        timings.record_tick(1, Duration::from_millis(500), [("periodic", 200.0)]);
        timings.finish_phase("warmup");

        timings.record_tick(2, Duration::from_millis(25), [("periodic", 20.0)]);
        for tick in 3..103 {
            let ms = if tick == 3 { 40 } else { 2 };
            timings.record_tick(tick, Duration::from_millis(ms), [("periodic", 1.0)]);
        }
        timings.record_tick(103, Duration::from_millis(5), [("periodic", 4.0)]);
        timings.finish_phase("measured");

        assert_eq!(
            json!(timings.measured.unwrap()),
            json!({
                "system_timings": {
                    "periodic": {"median_ms": 1.0, "p99_ms": 4.0, "max_ms": 20.0, "max_tick": 2}
                },
                "max_ms_per_tick": 40.0,
                "max_tick": 3,
            })
        );
    }
}
