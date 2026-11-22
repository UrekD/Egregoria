use crate::utils::scheduler::SeqSchedule;
use crate::world_command::WorldCommand;
use crate::{Simulation, SimulationOptions, INITIAL_TICK, START_COMMANDS};
use common::saveload::{Encoder, JSONPretty, SnapshotRevision, JSON};
use prototypes::Tick;
use serde::{Deserialize, Serialize};
use std::io;
use std::ops::Range;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replay {
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    commands: Vec<(Tick, WorldCommand)>,
    pub last_tick_recorded: Tick,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline: Option<ReplayTimeline>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplayTimeline {
    constructor_end: usize,
    steps: u64,
    batches: Vec<ReplayBatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplayBatch {
    // Count completed schedule steps, independent of changes to GameTime.
    step: u64,
    end: usize,
    steps_systems: bool,
}

impl Default for Replay {
    fn default() -> Self {
        Self {
            version: crate::VERSION.to_owned(),
            enabled: false,
            commands: Vec::new(),
            last_tick_recorded: Tick::default(),
            timeline: None,
        }
    }
}

impl Replay {
    pub(crate) fn bootstrap_options(&self) -> io::Result<SimulationOptions> {
        match self.commands.first() {
            Some((Tick(INITIAL_TICK), WorldCommand::Init(options)))
                if !options.new_game_options().is_valid() =>
            {
                Err(invalid_replay(
                    "Replay Init contains invalid new-game options",
                ))
            }
            Some((Tick(INITIAL_TICK), WorldCommand::Init(options))) => Ok(**options),
            _ => Err(invalid_replay(
                "Replay must begin with Init at the initial tick",
            )),
        }
    }

    pub(crate) fn bootstrap_end(&self) -> io::Result<usize> {
        let Some(timeline) = &self.timeline else {
            // Older recordings include the commands but no constructor boundary.
            let starters: Vec<(u32, WorldCommand)> = JSON::decode(START_COMMANDS.as_bytes())?;
            let end = 1 + starters.len();
            let matches_starters = self.commands.get(1..end).is_some_and(|recorded| {
                recorded
                    .iter()
                    .zip(starters)
                    .all(|((tick, actual), (_, starter))| {
                        *tick == Tick(INITIAL_TICK)
                            && JSON::encode(actual).is_ok_and(|actual| {
                                JSON::encode(&starter).is_ok_and(|starter| actual == starter)
                            })
                    })
            });
            return Ok(if matches_starters { end } else { 1 });
        };

        let end = timeline.constructor_end;
        if end == 0
            || !self.commands.get(1..end).is_some_and(|commands| {
                commands.iter().all(|(tick, command)| {
                    *tick == Tick(INITIAL_TICK) && !matches!(command, WorldCommand::Init(_))
                })
            })
        {
            return Err(invalid_replay("Invalid replay constructor batch"));
        }

        let mut previous_end = end;
        let mut first_step = 0;
        for batch in &timeline.batches {
            if batch.end <= previous_end
                || batch.end > self.commands.len()
                || batch.step < first_step
                || batch.step > timeline.steps
                || (batch.steps_systems && batch.step == timeline.steps)
            {
                return Err(invalid_replay("Invalid replay batch boundary"));
            }
            previous_end = batch.end;
            first_step = batch.step + u64::from(batch.steps_systems);
        }
        if previous_end != self.commands.len() {
            return Err(invalid_replay(
                "Replay timeline does not cover all commands",
            ));
        }
        Ok(end)
    }

    pub(crate) fn has_timeline(&self) -> bool {
        self.timeline.is_some()
    }

    pub(crate) fn commands(&self, range: Range<usize>) -> impl Iterator<Item = &WorldCommand> {
        self.commands[range].iter().map(|(_, command)| command)
    }

    pub(crate) fn command_count(&self) -> usize {
        self.commands.len()
    }

    pub(crate) fn record_constructor_end(&mut self, tick: Tick) {
        if self.enabled {
            self.timeline = Some(ReplayTimeline {
                constructor_end: self.commands.len(),
                steps: 0,
                batches: Vec::new(),
            });
            self.last_tick_recorded = tick;
        }
    }

    pub(crate) fn record_batch(&mut self, start: usize, steps_systems: bool, tick: Tick) {
        if let Some(timeline) = &mut self.timeline {
            let end = self.commands.len();
            if start != end {
                timeline.batches.push(ReplayBatch {
                    step: timeline.steps,
                    end,
                    steps_systems,
                });
                self.last_tick_recorded = tick;
            }
        }
    }

    pub(crate) fn record_step(&mut self, tick: Tick) {
        self.last_tick_recorded = tick;
        if let Some(timeline) = &mut self.timeline {
            timeline.steps += 1;
        }
    }

    pub fn push(&mut self, tick: Tick, command: WorldCommand) {
        self.commands.push((tick, command));
    }
}

fn invalid_replay(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) struct ReplayJson;

#[derive(Deserialize)]
struct ReplayHeader {
    #[serde(default)]
    version: String,
    #[serde(default)]
    revision: Option<u128>,
}

impl ReplayHeader {
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        // Read the header before decoding commands, regardless of JSON field order.
        let header: Self = JSON::decode(bytes)?;
        if header.version != crate::VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Replay has version {:?}, expected {:?}",
                    header.version,
                    crate::VERSION
                ),
            ));
        }
        Ok(header)
    }
}

impl ReplayJson {
    pub(crate) fn encode_sidecar(
        replay: &Replay,
        revision: SnapshotRevision,
    ) -> io::Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Sidecar<'a> {
            revision: u128,
            #[serde(flatten)]
            replay: &'a Replay,
        }

        JSONPretty::encode(&Sidecar {
            revision: revision.get(),
            replay,
        })
    }

    pub(crate) fn decode_sidecar(
        bytes: &[u8],
        revision: Option<SnapshotRevision>,
    ) -> io::Result<Replay> {
        let header = ReplayHeader::decode(bytes)?;
        if header.revision != revision.map(SnapshotRevision::get) {
            return Err(invalid_replay(
                "Replay revision does not match the snapshot",
            ));
        }
        JSON::decode(bytes)
    }
}

impl Encoder for ReplayJson {
    const EXTENSION: &'static str = JSON::EXTENSION;

    fn encode(replay: &impl Serialize) -> io::Result<Vec<u8>> {
        JSON::encode(replay)
    }

    fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
        ReplayHeader::decode(bytes)?;
        JSON::decode(bytes)
    }
}

pub struct SimulationReplayLoader {
    pub replay: Replay,
    pub pastt: Tick,
    pub idx: usize,
    pub speed: usize,
    pub advance_n_ticks: usize,
    steps: u64,
    batch: usize,
}

impl SimulationReplayLoader {
    pub(crate) fn new(replay: Replay, pastt: Tick, idx: usize) -> Self {
        Self {
            replay,
            pastt,
            idx,
            speed: 1,
            advance_n_ticks: 0,
            steps: 0,
            batch: 0,
        }
    }

    pub fn step_progress(&self) -> Option<(u64, u64)> {
        self.replay
            .timeline
            .as_ref()
            .map(|timeline| (self.steps, timeline.steps))
    }

    /// Returns true if the replay is finished
    pub fn advance_tick(&mut self, sim: &mut Simulation, schedule: &mut SeqSchedule) -> bool {
        let ticks_left = if self.speed == 0 {
            std::mem::take(&mut self.advance_n_ticks)
        } else {
            self.speed
        };
        let finished = if self.replay.has_timeline() {
            self.advance_timeline(sim, schedule, ticks_left)
        } else {
            self.advance_legacy(sim, schedule, ticks_left)
        };
        self.pastt = Tick(sim.get_tick());
        finished
    }

    fn advance_timeline(
        &mut self,
        sim: &mut Simulation,
        schedule: &mut SeqSchedule,
        mut ticks_left: usize,
    ) -> bool {
        let timeline = self.replay.timeline.as_ref().unwrap();
        // Leave paused batches pending while paused. After stepping, apply any
        // paused edits at the reached boundary before reporting completion.
        let playing = ticks_left != 0;
        loop {
            let batch = timeline.batches.get(self.batch);
            let ready = batch.filter(|batch| batch.step == self.steps);
            if let Some(batch) = ready.filter(|batch| playing && !batch.steps_systems) {
                sim.apply_commands(self.replay.commands(self.idx..batch.end));
                self.idx = batch.end;
                self.batch += 1;
                continue;
            }
            if self.steps == timeline.steps {
                return batch.is_none();
            }
            if ticks_left == 0 {
                return false;
            }
            if let Some(batch) = ready {
                sim.tick(schedule, self.replay.commands(self.idx..batch.end));
                self.idx = batch.end;
                self.batch += 1;
            } else {
                sim.tick(schedule, &[]);
            }
            self.steps += 1;
            ticks_left -= 1;
        }
    }

    fn advance_legacy(
        &mut self,
        sim: &mut Simulation,
        schedule: &mut SeqSchedule,
        mut ticks_left: usize,
    ) -> bool {
        let mut tick = Tick(sim.get_tick());
        while self.idx < self.replay.commands.len() && ticks_left > 0 {
            let curt = self.replay.commands[self.idx].0;
            while tick < curt {
                sim.tick(schedule, &[]);
                tick = Tick(sim.get_tick());
                ticks_left -= 1;
                if ticks_left == 0 {
                    return false;
                }
            }

            let idx_start = self.idx;
            while self.idx < self.replay.commands.len() && self.replay.commands[self.idx].0 == curt
            {
                self.idx += 1;
            }
            sim.apply_commands(self.replay.commands(idx_start..self.idx));
            tick = Tick(sim.get_tick());
        }
        if self.idx < self.replay.commands.len() {
            return false;
        }
        while ticks_left > 0 && tick < self.replay.last_tick_recorded {
            sim.tick(schedule, &[]);
            tick = Tick(sim.get_tick());
            ticks_left -= 1;
        }
        tick >= self.replay.last_tick_recorded
    }
}
