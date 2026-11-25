use crate::utils::scheduler::SeqSchedule;
use crate::world_command::{ReplayWorldCommand, WorldCommand};
use crate::{SaveLoadError, Simulation, SimulationOptions, INITIAL_TICK, START_COMMANDS};
use common::saveload::{Encoder, JSONPretty, SnapshotRevision, COMMAND_PROTOCOL_REVISION, JSON};
use prototypes::Tick;
use serde::{Deserialize, Serialize};
use std::io;
use std::ops::Range;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replay {
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) registry: Option<u64>,
    #[serde(deserialize_with = "deserialize_replay_commands")]
    commands: Vec<(Tick, WorldCommand)>,
    pub last_tick_recorded: Tick,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline: Option<ReplayTimeline>,
    #[serde(skip)]
    obsolete_recording_discarded: bool,
}

fn deserialize_replay_commands<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(Tick, WorldCommand)>, D::Error> {
    // Recorded 0.10.0 entries can contain zones larger than the live command limit.
    #[derive(Deserialize)]
    struct ReplayEntry(
        Tick,
        #[serde(deserialize_with = "ReplayWorldCommand::deserialize")] WorldCommand,
    );

    Ok(Vec::<ReplayEntry>::deserialize(deserializer)?
        .into_iter()
        .map(|ReplayEntry(tick, command)| (tick, command))
        .collect())
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
            version: replay_version(),
            enabled: false,
            registry: None,
            commands: Vec::new(),
            last_tick_recorded: Tick::default(),
            timeline: None,
            obsolete_recording_discarded: false,
        }
    }
}

impl Replay {
    /// Whether loading the city discarded an embedded recording from P6.
    pub fn obsolete_recording_discarded(&self) -> bool {
        self.obsolete_recording_discarded
    }

    fn validate_embedded_registry(&self) -> io::Result<()> {
        if self.enabled || self.registry.is_some() {
            self.check_registry()?;
        }
        Ok(())
    }

    pub(crate) fn check_registry(&self) -> io::Result<()> {
        crate::check_prototype_registry(self.registry)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if self.registry.is_some() {
            return Ok(());
        }

        // Older recordings do not have a fingerprint. Check their explicit
        // prototype references before starting playback.
        use crate::map::ZoneKind::MixedUse;
        for (_, command) in &self.commands {
            let available = match command {
                WorldCommand::ZonePaint {
                    kind: Some(MixedUse),
                    density,
                    ..
                } => {
                    *density == crate::map::Density::Low
                        || prototypes::try_prototypes()
                            .is_some_and(|registry| registry.mixed_use_pair(*density).is_some())
                }
                WorldCommand::MapBuildSpecialBuilding { kind, .. } => {
                    crate::building_kind_prototype_exists(*kind)
                }
                WorldCommand::SpawnTrain { wagons, .. } => wagons
                    .iter()
                    .all(|&id| prototypes::try_prototype(id).is_some()),
                _ => true,
            };
            if !available {
                return Err(invalid_replay("Replay requires a missing prototype"));
            }
        }
        Ok(())
    }

    pub(crate) fn bootstrap_options(&self) -> io::Result<SimulationOptions> {
        match self.commands.first() {
            Some((Tick(INITIAL_TICK), WorldCommand::Init(options)))
                if !options.new_game_options().is_valid() || !options.valid_for_bootstrap() =>
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
        let tiled = self.bootstrap_options()?.land_mode == crate::map::LandMode::Tiled;
        let Some(timeline) = &self.timeline else {
            if tiled {
                return Err(invalid_replay("A tiled replay requires a batch timeline"));
            }
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
        // The constructor can contain the optional plot layout followed by
        // the starter rail. Validate it before allocating or initializing a
        // world. Import and ordinary editor commands belong to playback.
        let valid = self.commands.get(1..end).is_some_and(|commands| {
            let commands = match commands {
                [(Tick(INITIAL_TICK), WorldCommand::SetLotLayout(_)), rest @ ..] => rest,
                _ => commands,
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
        });
        if !valid || (tiled && end != 1) {
            return Err(invalid_replay("Invalid replay constructor batch"));
        }

        let mut previous_end = end;
        let mut first_step = 0;
        for batch in &timeline.batches {
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
        self.commands[range].iter().map(|(_, command)| command)
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
        if matches!(command, WorldCommand::Init(_)) && self.commands.is_empty() {
            self.registry = prototypes::try_prototypes().map(|p| p.fingerprint());
        }
        self.commands.push((tick, command));
    }
}

fn invalid_replay(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn replay_version() -> String {
    format!("{}+{COMMAND_PROTOCOL_REVISION}", crate::VERSION)
}

pub(crate) fn replay_version_error(version: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "Replay has version {version:?}, expected {:?}",
            replay_version()
        ),
    )
}

fn replay_corrupt(error: io::Error) -> SaveLoadError {
    SaveLoadError::Corrupt {
        version: None,
        reason: format!("replay sidecar: {error}"),
    }
}

fn replay_decode_error(error: SaveLoadError) -> io::Error {
    match error {
        SaveLoadError::Incompatible { version } | SaveLoadError::IncompatibleReplay { version } => {
            replay_version_error(&version)
        }
        error => io::Error::new(io::ErrorKind::InvalidData, error),
    }
}

pub(crate) struct ReplayJson;

#[derive(Deserialize)]
struct ReplayHeader {
    #[serde(default)]
    version: String,
    #[serde(default)]
    revision: Option<u128>,
    #[serde(default)]
    registry: Option<u64>,
}

impl ReplayHeader {
    fn decode(bytes: &[u8]) -> Result<Self, SaveLoadError> {
        // Read the header before decoding commands, regardless of JSON field order.
        let header: Self = JSON::decode(bytes).map_err(replay_corrupt)?;
        if header.version != replay_version() {
            let version = header.version;
            let release = version
                .split_once('+')
                .map_or(version.as_str(), |(release, _)| release);
            return Err(if release == crate::VERSION {
                SaveLoadError::IncompatibleReplay { version }
            } else {
                SaveLoadError::Incompatible { version }
            });
        }
        crate::check_prototype_registry(header.registry)?;
        Ok(header)
    }
}

impl ReplayJson {
    pub(crate) fn decode_snapshot(bytes: &[u8]) -> io::Result<Replay> {
        match ReplayHeader::decode(bytes) {
            Ok(header) => {
                if header.enabled || header.registry.is_some() {
                    crate::check_prototype_registry(header.registry).map_err(replay_decode_error)?;
                }
                JSON::decode(bytes)
            }
            Err(SaveLoadError::IncompatibleReplay { version }) if version == "0.11.0" => {
                // The city has its own schema and registry checks. Inspect the
                // recording envelope without decoding obsolete commands.
                let legacy: LegacySnapshotReplay = JSON::decode(bytes)?;
                let discarded = legacy.enabled || legacy.commands || legacy.timeline.is_some();
                if discarded || legacy.registry.is_some() {
                    crate::check_prototype_registry(legacy.registry).map_err(replay_decode_error)?;
                }
                Ok(Replay {
                    obsolete_recording_discarded: discarded,
                    ..Replay::default()
                })
            }
            Err(SaveLoadError::IncompatibleReplay { version })
                if version == "0.11.0+commands.1" =>
            {
                // Wave 2 only appends a command variant. JSON recordings
                // embedded by Wave 1 have the same interpretation here.
                let mut replay: Replay = JSON::decode(bytes)?;
                replay.validate_embedded_registry()?;
                replay.version = replay_version();
                Ok(replay)
            }
            Err(error) => Err(replay_decode_error(error)),
        }
    }

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
    ) -> Result<Replay, SaveLoadError> {
        let header = ReplayHeader::decode(bytes)?;
        if header.revision != revision.map(SnapshotRevision::get) {
            return Err(replay_corrupt(invalid_replay(
                "Replay revision does not match the snapshot",
            )));
        }
        let replay: Replay = JSON::decode(bytes).map_err(replay_corrupt)?;
        replay.check_registry().map_err(replay_corrupt)?;
        Ok(replay)
    }
}

#[derive(Deserialize)]
struct LegacySnapshotReplay {
    #[serde(default)]
    registry: Option<u64>,
    enabled: bool,
    #[serde(deserialize_with = "contains_legacy_commands")]
    commands: bool,
    #[serde(default)]
    timeline: Option<serde::de::IgnoredAny>,
}

fn contains_legacy_commands<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    struct Commands;
    impl<'de> serde::de::Visitor<'de> for Commands {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a replay command sequence")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
            let found = seq.next_element::<serde::de::IgnoredAny>()?.is_some();
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(found)
        }
    }
    deserializer.deserialize_seq(Commands)
}

impl Encoder for ReplayJson {
    const EXTENSION: &'static str = JSON::EXTENSION;

    fn encode(replay: &impl Serialize) -> io::Result<Vec<u8>> {
        JSON::encode(replay)
    }

    fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
        let header = ReplayHeader::decode(bytes).map_err(replay_decode_error)?;
        if header.enabled || header.registry.is_some() {
            crate::check_prototype_registry(header.registry).map_err(replay_decode_error)?;
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
    pub(crate) fn new(replay: Replay, pastt: Tick, idx: usize) -> Self {
        let mut legacy_groups = Vec::new();
        if !replay.has_timeline() {
            for (offset, pair) in replay
                .commands
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
            if !fits(&self.replay.commands[first..end]) {
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
            if budget.remaining > 0 && next.is_some() && tick >= self.replay.commands[self.idx].0 {
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
        &self.replay.commands[operation.start..operation.end.unwrap_or(operation.start)]
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
        batches: vec![ReplayBatch {
            step: 1,
            end: 1,
            steps_systems: false,
        }],
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

#[cfg(test)]
#[test]
fn p7_snapshot_discards_active_or_nonempty_p6_replay() {
    let mut replay = Replay {
        version: crate::VERSION.to_owned(),
        enabled: true,
        ..Replay::default()
    };
    let assert_discarded = |replay: &Replay| {
        let bytes = JSON::encode(replay).unwrap();
        let restored = ReplayJson::decode_snapshot(&bytes).unwrap();
        assert!(restored.obsolete_recording_discarded());
        assert!(!restored.enabled);
        assert_eq!(restored.command_count(), 0);
    };
    assert_discarded(&replay);

    replay.enabled = false;
    replay.push(Tick(INITIAL_TICK), WorldCommand::MapLoadParis);
    assert_discarded(&replay);

    let obsolete = br#"{"version":"0.11.0","enabled":true,"commands":[[1,{"Obsolete":[]}]]}"#;
    assert!(ReplayJson::decode_snapshot(obsolete)
        .unwrap()
        .obsolete_recording_discarded());
    assert!(matches!(
        ReplayJson::decode_sidecar(obsolete, None),
        Err(crate::SaveLoadError::IncompatibleReplay { .. })
    ));
}

#[cfg(test)]
#[test]
fn p7_replay_rejects_floor_identity_before_decoding_commands() {
    use crate::map::road_edit::{RoadEditStamp, RoadQuote, RoadUpgrade};
    use crate::map::{LanePatternBuilder, RoadID};
    use common::saveload::Bincode;

    let zero = Bincode::encode(&0u64).unwrap();
    let make = |version: &str| {
        let mut replay = Replay::default();
        replay.version = version.to_owned();
        replay.push(
            Tick(1),
            WorldCommand::MapUpgradeRoad {
                road: RoadID::default(),
                expected: Bincode::decode::<RoadEditStamp>(&zero).unwrap(),
                change: RoadUpgrade::Replace(LanePatternBuilder::new().build()),
                quote: Bincode::decode::<RoadQuote>(&zero).unwrap(),
            },
        );
        replay
    };
    let floor = crate::VERSION;
    #[derive(Serialize)]
    struct InvalidPayload<'a> {
        version: &'a str,
        commands: &'static str,
    }
    let bytes = JSON::encode(&InvalidPayload {
        version: floor,
        commands: "not a command list",
    })
    .unwrap();
    assert!(matches!(
        ReplayJson::decode_sidecar(&bytes, None),
        Err(SaveLoadError::IncompatibleReplay { version }) if version == floor
    ));
    let current = Replay::default().version;
    assert_ne!(current, floor);
    let revision = SnapshotRevision::new(12);
    let bytes = ReplayJson::encode_sidecar(&make(&current), revision).unwrap();
    let sidecar = ReplayJson::decode_sidecar(&bytes, Some(revision)).unwrap();
    assert!(matches!(
        sidecar.commands[0].1,
        WorldCommand::MapUpgradeRoad { .. }
    ));
    let old_header: ReplayHeader = JSON::decode(&bytes).unwrap();
    assert_ne!(
        old_header.version, floor,
        "a floor reader must reject the header"
    );
}
