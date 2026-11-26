use crate::economy::population::PopulationScale;
use crate::utils::scheduler::SeqSchedule;
use crate::world_command::{ReplayWorldCommand, WorldCommand};
use crate::{SaveLoadError, Simulation, SimulationOptions, INITIAL_TICK, START_COMMANDS};
use common::saveload::{Encoder, JSONPretty, SnapshotRevision, JSON};
use prototypes::Tick;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

// Captures share the old blocks. Recording after a capture copies at most
// one partial block. Playback flattens once to keep its borrowed batch API.
const REPLAY_BLOCK_LEN: usize = 256;

#[derive(Debug, Clone)]
struct ReplayLog<T> {
    blocks: Vec<Arc<Vec<T>>>,
    len: usize,
}

impl<T> Default for ReplayLog<T> {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            len: 0,
        }
    }
}

impl<T> ReplayLog<T> {
    fn iter(&self) -> impl Iterator<Item = &T> {
        self.blocks.iter().flat_map(|block| block.iter())
    }

    fn get(&self, index: usize) -> Option<&T> {
        self.blocks
            .get(index / REPLAY_BLOCK_LEN)?
            .get(index % REPLAY_BLOCK_LEN)
    }
}

impl<T: Clone> ReplayLog<T> {
    fn push(&mut self, item: T) {
        if self.len % REPLAY_BLOCK_LEN == 0 {
            self.blocks
                .push(Arc::new(Vec::with_capacity(REPLAY_BLOCK_LEN)));
        }
        Arc::make_mut(self.blocks.last_mut().unwrap()).push(item);
        self.len += 1;
    }
}

impl<T: Serialize> Serialize for ReplayLog<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_replay_log(serializer, self.len, self.iter())
    }
}

impl<'de, T: Clone + Deserialize<'de>> Deserialize<'de> for ReplayLog<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut log = Self::default();
        for item in Vec::<T>::deserialize(deserializer)? {
            log.push(item);
        }
        Ok(log)
    }
}

fn serialize_replay_log<'a, T: Serialize + 'a, S: serde::Serializer>(
    serializer: S,
    len: usize,
    items: impl Iterator<Item = &'a T>,
) -> Result<S::Ok, S::Error> {
    let mut sequence = serializer.serialize_seq(Some(len))?;
    for item in items {
        sequence.serialize_element(item)?;
    }
    sequence.end()
}

type ReplayEntry = (Tick, WorldCommand);

#[derive(Debug, Clone)]
enum ReplayCommands {
    Recording(ReplayLog<ReplayEntry>),
    Playback(Arc<Vec<ReplayEntry>>),
}

impl ReplayCommands {
    fn len(&self) -> usize {
        match self {
            Self::Recording(log) => log.len,
            Self::Playback(commands) => commands.len(),
        }
    }

    fn get(&self, index: usize) -> Option<&ReplayEntry> {
        match self {
            Self::Recording(log) => log.get(index),
            Self::Playback(commands) => commands.get(index),
        }
    }

    fn push(&mut self, entry: ReplayEntry) {
        match self {
            Self::Recording(log) => log.push(entry),
            Self::Playback(commands) => Arc::make_mut(commands).push(entry),
        }
    }

    fn make_contiguous(&mut self) {
        if let Self::Recording(log) = self {
            let mut commands = Vec::with_capacity(log.len);
            for block in std::mem::take(&mut log.blocks) {
                let mut block = Arc::unwrap_or_clone(block);
                commands.append(&mut block);
            }
            *self = Self::Playback(Arc::new(commands));
        }
    }

    fn contiguous(&self) -> &[ReplayEntry] {
        match self {
            Self::Playback(commands) => commands,
            Self::Recording(_) => unreachable!("replay cursors use contiguous commands"),
        }
    }
}

impl Serialize for ReplayCommands {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Recording(log) => log.serialize(serializer),
            Self::Playback(commands) => commands.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replay {
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) registry: Option<u64>,
    #[serde(deserialize_with = "deserialize_replay_commands")]
    commands: ReplayCommands,
    pub last_tick_recorded: Tick,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline: Option<ReplayTimeline>,
}

fn deserialize_replay_commands<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ReplayCommands, D::Error> {
    // Recorded 0.10.0 entries can contain zones larger than the live command limit.
    #[derive(Deserialize)]
    struct ReplayEntry(
        Tick,
        #[serde(deserialize_with = "ReplayWorldCommand::deserialize")] WorldCommand,
    );

    let mut log = ReplayLog::default();
    for ReplayEntry(tick, command) in Vec::<ReplayEntry>::deserialize(deserializer)? {
        log.push((tick, command));
    }
    Ok(ReplayCommands::Recording(log))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplayTimeline {
    constructor_end: usize,
    steps: u64,
    batches: ReplayLog<ReplayBatch>,
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
            registry: None,
            commands: ReplayCommands::Recording(ReplayLog::default()),
            last_tick_recorded: Tick::default(),
            timeline: None,
        }
    }
}

impl Replay {
    pub(crate) fn check_registry(&self) -> io::Result<()> {
        crate::check_prototype_registry(self.registry)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn bootstrap_options(&self) -> io::Result<SimulationOptions> {
        match self.commands.get(0) {
            Some((Tick(INITIAL_TICK), WorldCommand::Init(options))) => {
                self.initial_population_scale(options)?;
                if !options.valid_for_bootstrap() {
                    return Err(invalid_replay(
                        "Replay Init contains invalid new-game options",
                    ));
                }
                Ok(**options)
            }
            _ => Err(invalid_replay(
                "Replay must begin with Init at the initial tick",
            )),
        }
    }

    pub(crate) fn validate_population_scale(&self, scale: PopulationScale) -> io::Result<()> {
        if self
            .recorded_population_scale()?
            .is_some_and(|recorded| recorded != scale)
        {
            return Err(invalid_replay(
                "Replay Init population ratio does not match the snapshot",
            ));
        }
        Ok(())
    }

    fn recorded_population_scale(&self) -> io::Result<Option<PopulationScale>> {
        match self.commands.get(0) {
            Some((_, WorldCommand::Init(options))) => {
                self.initial_population_scale(options).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn initial_population_scale(&self, options: &SimulationOptions) -> io::Result<PopulationScale> {
        let scale = options.population_scale().map_err(|error| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Replay Init: {error}"))
        })?;
        if self.version == crate::PRE_SCALE_VERSION && scale != PopulationScale::REALISTIC {
            return Err(invalid_replay(
                "pre-SCALE replay must use Realistic population",
            ));
        }
        Ok(scale)
    }

    pub(crate) fn bootstrap_end(&self) -> io::Result<usize> {
        let tiled = self.bootstrap_options()?.land_mode == crate::map::LandMode::Tiled;
        let Some(timeline) = &self.timeline else {
            if tiled {
                return Err(invalid_replay("A tiled replay requires a batch timeline"));
            }
            // Older recordings include the commands but no constructor boundary.
            let starters: Vec<(u32, WorldCommand)> = JSON::decode(START_COMMANDS.as_bytes())?;
            let end = 1 + starters.len();
            let matches_starters = end <= self.commands.len()
                && (1..end).zip(starters).all(|(index, (_, starter))| {
                    let (tick, actual) = self.commands.get(index).unwrap();
                    *tick == Tick(INITIAL_TICK)
                        && JSON::encode(actual).is_ok_and(|actual| {
                            JSON::encode(&starter).is_ok_and(|starter| actual == starter)
                        })
                });
            return Ok(if matches_starters { end } else { 1 });
        };

        let end = timeline.constructor_end;
        // The constructor can contain the optional plot layout followed by
        // the starter rail. Validate it before allocating or initializing a
        // world. Import and ordinary editor commands belong to playback.
        if end == 0 || end > 5 || end > self.commands.len() {
            return Err(invalid_replay("Invalid replay constructor batch"));
        }
        let commands: Vec<_> = (1..end)
            .map(|index| self.commands.get(index).unwrap())
            .collect();
        let valid = {
            let commands = match commands.as_slice() {
                [(Tick(INITIAL_TICK), WorldCommand::SetLotLayout(_)), rest @ ..] => rest,
                _ => commands.as_slice(),
            };
            match commands {
                [] => true,
                [(Tick(INITIAL_TICK), WorldCommand::MapMakeMultipleConnections(projects, links)), (
                    Tick(INITIAL_TICK),
                    WorldCommand::MapBuildSpecialBuilding {
                        kind: crate::map::BuildingKind::RailFreightStation(_),
                        zone: None,
                        ..
                    },
                ), (Tick(INITIAL_TICK), WorldCommand::AddTrain { n_wagons, .. })] => {
                    projects.len() <= 512
                        && links.len() <= 256
                        && links
                            .iter()
                            .map(|(_, _, _, pattern)| pattern.lanes().count())
                            .try_fold(0usize, |total, count| {
                                total.checked_add(count).filter(|&total| total <= 512)
                            })
                            .is_some()
                        && *n_wagons <= 128
                }
                _ => false,
            }
        };
        if !valid || (tiled && end != 1) {
            return Err(invalid_replay("Invalid replay constructor batch"));
        }

        let mut previous_end = end;
        let mut first_step = 0;
        for batch in timeline.batches.iter() {
            if batch.end <= previous_end
                || (tiled && batch.end - previous_end > 256)
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
        range.map(|index| &self.commands.get(index).unwrap().1)
    }

    #[cfg(test)]
    pub(crate) fn batch_boundaries(&self) -> Vec<(u64, usize, bool)> {
        self.timeline
            .as_ref()
            .unwrap()
            .batches
            .iter()
            .map(|batch| (batch.step, batch.end, batch.steps_systems))
            .collect()
    }

    pub(crate) fn command_count(&self) -> usize {
        self.commands.len()
    }

    pub(crate) fn record_constructor_end(&mut self, tick: Tick) {
        if self.enabled {
            self.timeline = Some(ReplayTimeline {
                constructor_end: self.commands.len(),
                steps: 0,
                batches: ReplayLog::default(),
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
        if matches!(command, WorldCommand::Init(_)) && self.commands.len() == 0 {
            self.registry = prototypes::try_prototypes().map(|p| p.fingerprint());
        }
        self.commands.push((tick, command));
    }
}

fn invalid_replay(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn replay_version_error(version: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "Replay has version {version:?}, expected {:?}",
            crate::VERSION
        ),
    )
}

fn replay_corrupt(error: io::Error) -> SaveLoadError {
    SaveLoadError::Corrupt {
        version: None,
        reason: format!("replay sidecar: {error}"),
    }
}

pub(crate) struct ReplayJson;

#[derive(Deserialize)]
struct ReplayHeader {
    #[serde(default)]
    version: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    revision: Option<u128>,
    #[serde(default)]
    registry: Option<u64>,
}

impl ReplayHeader {
    fn decode(bytes: &[u8]) -> Result<Self, SaveLoadError> {
        // Read the header before decoding commands, regardless of JSON field order.
        let header: Self = JSON::decode(bytes).map_err(replay_corrupt)?;
        if !crate::compatible_save_version(&header.version) {
            return Err(SaveLoadError::Incompatible {
                version: header.version,
            });
        }
        Ok(header)
    }
}

impl ReplayJson {
    pub(crate) const RESOURCE_NAME: &'static str = "replay";

    #[cfg(test)]
    pub(crate) fn encode_sidecar(
        replay: &Replay,
        revision: SnapshotRevision,
    ) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        Self::write_sidecar(replay, revision, &mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn write_sidecar(
        replay: &Replay,
        revision: SnapshotRevision,
        output: impl Write,
    ) -> io::Result<()> {
        #[derive(Serialize)]
        struct Sidecar<'a> {
            revision: u128,
            #[serde(flatten)]
            replay: &'a Replay,
        }

        JSONPretty::encode_writer(
            &Sidecar {
                revision: revision.get(),
                replay,
            },
            output,
        )
    }

    pub(crate) fn decode_sidecar(
        bytes: &[u8],
        revision: Option<SnapshotRevision>,
    ) -> Result<Replay, SaveLoadError> {
        let header = ReplayHeader::decode(bytes)?;
        crate::check_prototype_registry(header.registry)?;
        if header.revision != revision.map(SnapshotRevision::get) {
            return Err(replay_corrupt(invalid_replay(
                "Replay revision does not match the snapshot",
            )));
        }
        let replay: Replay = JSON::decode(bytes).map_err(replay_corrupt)?;
        replay.recorded_population_scale().map_err(replay_corrupt)?;
        Ok(replay)
    }
}

impl Encoder for ReplayJson {
    const EXTENSION: &'static str = JSON::EXTENSION;

    fn encode(replay: &impl Serialize) -> io::Result<Vec<u8>> {
        JSON::encode(replay)
    }

    fn encode_writer(replay: &impl Serialize, output: impl Write) -> io::Result<()> {
        JSON::encode_writer(replay, output)
    }

    fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
        let header = ReplayHeader::decode(bytes).map_err(|error| match error {
            SaveLoadError::Incompatible { version } => replay_version_error(&version),
            error => io::Error::new(io::ErrorKind::InvalidData, error),
        })?;
        // Snapshots may embed a disabled replay that was never started.
        if header.enabled || header.registry.is_some() {
            crate::check_prototype_registry(header.registry)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
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
    legacy_groups: Vec<usize>,
}

impl SimulationReplayLoader {
    pub(crate) fn new(mut replay: Replay, pastt: Tick, idx: usize) -> Self {
        replay.commands.make_contiguous();
        let mut legacy_groups = Vec::new();
        if !replay.has_timeline() {
            for (offset, pair) in replay
                .commands
                .contiguous()
                .get(idx..)
                .unwrap_or_default()
                .windows(2)
                .enumerate()
            {
                if pair[0].0 != pair[1].0 {
                    legacy_groups.push(idx + offset + 1);
                }
            }
            if idx < replay.commands.len() {
                legacy_groups.push(replay.commands.len());
            }
        }
        Self {
            replay,
            pastt,
            idx,
            speed: 1,
            advance_n_ticks: 0,
            steps: 0,
            batch: 0,
            legacy_groups,
        }
    }

    /// Check the complete playback envelope before handing the cursor to a
    /// driver. Constructor operations have already run during bootstrap.
    pub fn validate_batches(
        &self,
        mut fits: impl FnMut(&[(Tick, WorldCommand)]) -> bool,
    ) -> io::Result<()> {
        let mut first = self.idx;
        let ends = self
            .replay
            .timeline
            .iter()
            .flat_map(|timeline| {
                timeline
                    .batches
                    .iter()
                    .skip(self.batch)
                    .map(|batch| batch.end)
            })
            .chain(self.legacy_groups.iter().skip(self.batch).copied());
        for end in ends {
            if !fits(&self.replay.commands.contiguous()[first..end]) {
                return Err(invalid_replay(
                    "Replay batch exceeds the simulation admission limit",
                ));
            }
            first = end;
        }
        Ok(())
    }

    pub fn step_progress(&self) -> Option<(u64, u64)> {
        self.replay
            .timeline
            .as_ref()
            .map(|timeline| (self.steps, timeline.steps))
    }

    /// Returns true if the replay is finished.
    pub fn advance_tick(&mut self, sim: &mut Simulation, schedule: &mut SeqSchedule) -> bool {
        let count = if self.speed == 0 {
            std::mem::take(&mut self.advance_n_ticks)
        } else {
            self.speed
        };
        let mut budget = ReplayBudget::new(count);
        loop {
            if let ReplayAdvance::Settled { complete } =
                self.advance_one(sim, schedule, &mut budget)
            {
                self.pastt = Tick(sim.get_tick());
                return complete;
            }
        }
    }

    /// Apply at most one atomic batch or run one schedule step. A positive
    /// timeline budget settles instant edits at the destination step.
    pub fn advance_one(
        &mut self,
        sim: &mut Simulation,
        schedule: &mut SeqSchedule,
        budget: &mut ReplayBudget,
    ) -> ReplayAdvance {
        let operation = match self.next_operation(Tick(sim.get_tick()), budget) {
            ReplayNext::Run(operation) => operation,
            ReplayNext::Settled { complete } => return ReplayAdvance::Settled { complete },
        };
        let commands = self
            .commands_for(operation)
            .iter()
            .map(|(_, command)| command);
        if operation.advances() {
            sim.tick(schedule, commands);
        } else {
            sim.apply_commands(commands);
        }
        self.commit_operation(operation, Tick(sim.get_tick()), budget)
    }

    /// Plan the next operation without consuming the cursor or its step budget.
    pub fn next_operation(&self, tick: Tick, budget: &ReplayBudget) -> ReplayNext {
        let (end, advanced) = if let Some(timeline) = &self.replay.timeline {
            let next = timeline.batches.get(self.batch);
            let ready = next.filter(|batch| batch.step == self.steps);
            if let Some(batch) = ready.filter(|batch| budget.started && !batch.steps_systems) {
                (Some(batch.end), false)
            } else if self.steps == timeline.steps || budget.remaining == 0 {
                return ReplayNext::Settled {
                    complete: self.steps == timeline.steps && next.is_none(),
                };
            } else {
                (ready.map(|batch| batch.end), true)
            }
        } else {
            let next = self.legacy_groups.get(self.batch).copied();
            if budget.remaining > 0
                && next.is_some()
                && tick >= self.replay.commands.contiguous()[self.idx].0
            {
                (next, false)
            } else if budget.remaining == 0
                || (next.is_none() && tick >= self.replay.last_tick_recorded)
            {
                return ReplayNext::Settled {
                    complete: next.is_none() && tick >= self.replay.last_tick_recorded,
                };
            } else {
                (None, true)
            }
        };

        ReplayNext::Run(ReplayOperation {
            start: self.idx,
            end,
            advanced,
        })
    }

    pub fn commands_for(&self, operation: ReplayOperation) -> &[(Tick, WorldCommand)] {
        &self.replay.commands.contiguous()
            [operation.start..operation.end.unwrap_or(operation.start)]
    }

    /// Consume an operation once it has been applied to the published world.
    pub fn commit_operation(
        &mut self,
        operation: ReplayOperation,
        tick: Tick,
        budget: &mut ReplayBudget,
    ) -> ReplayAdvance {
        debug_assert_eq!(operation.start, self.idx);
        if operation.advanced {
            self.steps += 1;
            budget.remaining -= 1;
        }
        let batch = operation.end.map(|end| {
            self.idx = end;
            self.batch += 1;
            self.batch - 1
        });
        self.pastt = tick;
        ReplayAdvance::Operation {
            step: self.steps,
            batch,
            tick: self.pastt,
            advanced: operation.advanced,
        }
    }
}

pub type ReplayCursor = SimulationReplayLoader;

pub enum ReplayNext {
    Run(ReplayOperation),
    Settled { complete: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayOperation {
    start: usize,
    end: Option<usize>,
    advanced: bool,
}

impl ReplayOperation {
    pub fn advances(self) -> bool {
        self.advanced
    }
}

pub struct ReplayBudget {
    remaining: usize,
    started: bool,
}

impl ReplayBudget {
    pub fn new(steps: usize) -> Self {
        Self {
            remaining: steps,
            started: steps != 0,
        }
    }
}

#[derive(Debug)]
pub enum ReplayAdvance {
    Operation {
        step: u64,
        batch: Option<usize>,
        tick: Tick,
        advanced: bool,
    },
    Settled {
        complete: bool,
    },
}

#[cfg(test)]
#[test]
fn scale_t1_replay_boundary_keeps_legacy_edit_pending() {
    let mut sim = crate::tests::TestCtx::new().g;
    let mut schedule = Simulation::schedule();
    let initial = sim.get_tick();
    let command = WorldCommand::SetGameTime(prototypes::GameTime::new(Tick(initial + 20)));
    let mut legacy = Replay::default();
    legacy.push(Tick(initial + 1), command.clone());
    let mut timeline = legacy.clone();
    timeline.timeline = Some(ReplayTimeline {
        constructor_end: 0,
        steps: 1,
        batches: ReplayLog::default(),
    });
    timeline
        .timeline
        .as_mut()
        .unwrap()
        .batches
        .push(ReplayBatch {
            step: 1,
            end: 1,
            steps_systems: false,
        });
    let mut legacy = SimulationReplayLoader::new(legacy, Tick(initial), 0);
    let mut timeline = SimulationReplayLoader::new(timeline, Tick(initial), 0);
    for (cursor, expected, pending) in [
        (&mut legacy, initial + 1, 0),
        (&mut timeline, initial + 20, 1),
    ] {
        let mut budget = ReplayBudget::new(1);
        loop {
            if matches!(
                cursor.advance_one(&mut sim, &mut schedule, &mut budget),
                ReplayAdvance::Settled { .. }
            ) {
                break;
            }
        }
        assert_eq!(sim.get_tick(), expected);
        assert_eq!(cursor.idx, pending);
        *sim.write::<prototypes::GameTime>() = prototypes::GameTime::new(Tick(initial));
    }
}

#[cfg(test)]
#[test]
fn scale_t1_fix_rejects_import_in_replay_constructor() {
    drop(crate::tests::TestCtx::new());
    let mut replay = Replay::default();
    let options = SimulationOptions {
        terrain_size: crate::NewGameOptions::MIN_TERRAIN_CHUNKS,
        preset: crate::MapPreset::Flat,
        ..SimulationOptions::LEGACY
    };
    replay.push(Tick(INITIAL_TICK), WorldCommand::Init(Box::new(options)));
    replay.push(Tick(INITIAL_TICK), WorldCommand::MapLoadParis);
    replay.enabled = true;
    replay.record_constructor_end(Tick(INITIAL_TICK));
    assert!(matches!(
        Simulation::try_from_replay(replay),
        Err(err) if err.to_string() == "Invalid replay constructor batch"
    ));
}
