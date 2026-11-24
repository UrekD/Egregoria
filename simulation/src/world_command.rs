use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use prototypes::{try_prototype, Money, RollingStockID};
use serde::{Deserialize, Serialize};

use geom::{vec3, Vec2, Vec3, OBB};
use prototypes::BuildingGen;
use prototypes::GameTime;
use WorldCommand::*;

use crate::economy::cargo::{CargoBook, WarehousePolicy};
use crate::economy::progression::{can_build, BuildCapability, ProgressionState};
use crate::economy::{Budget, BudgetCategory, Government};
use crate::map::procgen::{load_parismap, load_testfield};
use crate::map::{
    Building, BuildingID, BuildingKind, Density, Environment, IntersectionID, LaneID, LaneKind,
    LanePattern, LanePatternBuilder, LightPolicy, LotID, LotLayout, Map, MapProject, ProjectKind,
    RoadID, TerraformKind, TurnPolicy, WaterEdit, WaterEditError, WaterEditOutcome, Zone,
    ZoneCellCoord, ZoneGrid, ZoneKind, ZonePaintError, MAX_WATER_BATCH_WORK,
};
use crate::map_dynamic::{transit, BuildingInfos, LineID, ParkingManagement, StopID};
use crate::multiplayer::chat::Message;
use crate::multiplayer::MultiplayerState;
use crate::souls::residence;
use crate::transportation::testing_vehicles::RandomVehicles;
use crate::transportation::train::{spawn_train, RailWagonKind};
use crate::transportation::{spawn_parked_vehicle_with_spot, unpark, VehicleKind};
use crate::utils::rand_provider::RandProvider;
use crate::{CompanyID, MapPreset, Replay, Simulation, SimulationOptions, SoulID};

#[derive(Clone, Default)]
pub struct WorldCommands {
    pub(crate) commands: Vec<WorldCommand>,
}

defer_serialize!(WorldCommands, Vec<WorldCommand>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorldCommand {
    Init(Box<SimulationOptions>),
    MapRemoveIntersection(IntersectionID),
    MapRemoveRoad(RoadID),
    MapRemoveBuilding(BuildingID),
    MapBuildHouse(LotID),
    Terraform {
        kind: TerraformKind,
        center: Vec2,
        radius: f32,
        amount: f32,
        level: f32,                  // only for flatten
        slope: Option<(Vec3, Vec3)>, // start and end of slope
    },
    SendMessage {
        message: Message,
    },
    SpawnRandomCars {
        n_cars: usize,
    },
    AddTrain {
        dist: f32,
        n_wagons: u32,
        lane: LaneID,
    },
    SpawnTrain {
        wagons: Vec<RollingStockID>,
        lane: LaneID,
        dist: f32,
    },
    MapMakeConnection {
        from: MapProject,
        to: MapProject,
        inter: Option<Vec2>,
        pat: LanePattern,
    }, // todo: allow lane pattern builder
    MapMakeMultipleConnections(
        Vec<MapProject>,
        Vec<(usize, usize, Option<Vec2>, LanePattern)>,
    ),
    MapUpdateIntersectionPolicy {
        inter: IntersectionID,
        turn: TurnPolicy,
        light: LightPolicy,
    },
    MapBuildSpecialBuilding {
        pos: OBB,
        kind: BuildingKind,
        gen: BuildingGen,
        #[serde(default)]
        zone: Option<Zone>,
        #[serde(default)]
        connected_road: Option<RoadID>,
    },
    MapLoadParis,
    MapLoadTestField {
        pos: Vec2,
        size: u32,
        spacing: f32,
    },
    UpdateZone {
        building: BuildingID,
        zone: Zone,
    },
    SetGameTime(GameTime),
    ZonePaint {
        area: OBB,
        kind: Option<ZoneKind>,
        density: Density,
    },
    SetTaxRate {
        zone: ZoneKind,
        rate_permille: u16,
    },
    TransitAddStop {
        pos: Vec3,
        dir: Vec2,
    },
    TransitRemoveStop {
        stop: StopID,
    },
    TransitCreateLine {
        stops: Vec<StopID>,
    },
    TransitEditLine {
        line: LineID,
        stops: Vec<StopID>,
    },
    TransitRemoveLine {
        line: LineID,
    },
    TransitSetVehicles {
        line: LineID,
        n: u8,
    },
    TransitSetFare {
        line: LineID,
        fare: Money,
    },
    SetWarehousePolicy {
        building: BuildingID,
        policy: WarehousePolicy,
    },
    /// Sizes new lots and regenerates the empty lots along every frontage road.
    SetLotLayout(LotLayout),
    EditWater(WaterEdit),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    MissingReference,
    InvalidConnection,
    InvalidBatchIndex,
    InvalidBuilding,
    InvalidZone,
    InvalidTaxRate,
    InvalidTrain,
    InvalidTerraform,
    TooManyRandomCars,
    CostOverflow,
    InsufficientFunds,
    MutationFailed,
    TransitNotImplemented,
    UnexpectedInit,
    InvalidInitOptions,
    Locked { milestone: u8 },
    InvalidWarehouse,
    WaterEditFailed,
    WetFootprint,
    TileLocked,
    WaterWorkLimit,
    WaterConfigured,
    WaterTerrainConflict,
}

impl From<ZonePaintError> for Rejection {
    fn from(error: ZonePaintError) -> Self {
        match error {
            ZonePaintError::InvalidArea => Self::InvalidZone,
            ZonePaintError::TileLocked(_) => Self::TileLocked,
            ZonePaintError::WetFootprint => Self::WetFootprint,
        }
    }
}

impl Rejection {
    fn water_error(error: &WaterEditError) -> Self {
        match error {
            WaterEditError::Limit => Self::WaterWorkLimit,
            WaterEditError::OccupiedFootprint { .. } => Self::WetFootprint,
            _ => Self::WaterEditFailed,
        }
    }

    fn water_connection_error(error: WaterEditError) -> Self {
        match error {
            WaterEditError::Limit => Self::WaterWorkLimit,
            WaterEditError::TopologyConflict { .. } => Self::WetFootprint,
            _ => Self::InvalidConnection,
        }
    }
}

pub(crate) struct WaterBatch {
    guard_terraform: bool,
    reject_surface: bool,
    remaining: usize,
}

impl WaterBatch {
    pub(crate) fn new(commands: &[&WorldCommand], environment: &Environment) -> Self {
        let guard_terraform = environment.has_water_configuration()
            || commands.iter().any(|command| {
                matches!(
                    command,
                    EditWater(
                        WaterEdit::SetSeaLevel { level_dm: Some(_) }
                            | WaterEdit::CarveRiver { .. }
                            | WaterEdit::FillLake { .. }
                    )
                )
            });
        let mut batch = Self {
            guard_terraform,
            reject_surface: false,
            remaining: MAX_WATER_BATCH_WORK,
        };
        if !guard_terraform || commands.len() < 2 {
            return batch;
        }

        let initial_sea = environment.sea_level_dm();
        let mut possibly_configured_sea = initial_sea.is_some();
        let mut sea_level_may_change = false;
        let mut sea_rebuilds = 0;
        let mut reserved = 0usize;
        for command in commands {
            let work = match command {
                EditWater(WaterEdit::SetSeaLevel { level_dm }) => {
                    if !sea_level_may_change && *level_dm == initial_sea {
                        continue;
                    }
                    sea_level_may_change = true;
                    possibly_configured_sea |= level_dm.is_some();
                    sea_rebuilds += 1;
                    // A full rebuild may also re-evaluate every river's sea contacts.
                    MAX_WATER_BATCH_WORK
                }
                EditWater(edit) => {
                    sea_rebuilds += usize::from(possibly_configured_sea);
                    if possibly_configured_sea {
                        MAX_WATER_BATCH_WORK
                    } else {
                        dry_water_edit_reservation(edit)
                    }
                }
                Terraform { kind, radius, .. } => {
                    sea_rebuilds += usize::from(possibly_configured_sea);
                    if possibly_configured_sea || *kind == TerraformKind::Erode {
                        MAX_WATER_BATCH_WORK
                    } else {
                        let side = (radius.clamp(0.0, 2_000.0) / 256.0).ceil() as usize + 2;
                        side * side * 1024 * 18
                    }
                }
                _ => continue,
            };
            reserved = reserved.saturating_add(work);
            if sea_rebuilds > 1 || reserved > MAX_WATER_BATCH_WORK {
                batch.reject_surface = true;
                break;
            }
        }
        batch
    }
}

fn dry_water_edit_reservation(edit: &WaterEdit) -> usize {
    // The per-tile allowance covers base writes, contact checks and both river
    // banks at a contact, even beside an existing river at the vertex cap.
    const PER_TILE: usize = 2_100;
    let cells = |min: Vec2, max: Vec2| {
        let side = ((max - min) / 16.0).max(Vec2::ZERO);
        let count = |length: f32| (length.ceil() as usize).saturating_add(2);
        count(side.x).saturating_mul(count(side.y))
    };
    let (vertices, coverage, tests) = match edit {
        WaterEdit::CarveRiver {
            polyline, width_m, ..
        } => {
            let radius = Vec2::splat(width_m.clamp(32.0, 512.0) * 0.5 + 16.0);
            let span = polyline.points().windows(2).fold(0usize, |total, segment| {
                total.saturating_add(cells(
                    segment[0].min(segment[1]) - radius,
                    segment[0].max(segment[1]) + radius,
                ))
            });
            (polyline.points().len(), span, span.saturating_mul(2))
        }
        WaterEdit::FillLake { basin, .. } => {
            let mut min = basin.points()[0];
            let mut max = min;
            for &point in basin.points() {
                min = min.min(point);
                max = max.max(point);
            }
            let coverage = cells(min, max);
            (
                basin.points().len(),
                coverage,
                coverage.saturating_mul(basin.points().len()),
            )
        }
        _ => return MAX_WATER_BATCH_WORK,
    };
    (vertices * vertices)
        .saturating_add(coverage.min(crate::map::MAX_FRESHWATER_CANDIDATES) * PER_TILE)
        .saturating_add(tests)
        .min(MAX_WATER_BATCH_WORK)
}

pub type LinkOutcome = (usize, Result<(), (&'static str, Rejection)>);

struct CommandOutcomeRanges {
    rejections: Range<usize>,
    links: Range<usize>,
}

#[derive(Default)]
pub struct CommandOutcomes {
    /// Process-wide sequence of the most recent nonempty outer batch, including across world loads.
    pub sequence: u64,
    /// Rejections from the most recent nonempty batch, in command order.
    /// Leave this list intact while using indexed attribution. Use [Self::clear]
    /// to discard the batch's outcome details.
    pub last: Vec<(&'static str, Rejection)>,
    /// Input indices of commands that completed without any rejection.
    pub accepted: Vec<usize>,
    command_ranges: Vec<CommandOutcomeRanges>,
    links: Vec<LinkOutcome>,
    batch_in_progress: bool,
    water_edits: BTreeMap<usize, Result<WaterEditOutcome, WaterEditError>>,
}

impl CommandOutcomes {
    pub fn batch_seq(&self) -> u64 {
        self.sequence
    }

    /// Rejections for the given input index in the most recent nonempty batch.
    /// Returns an empty slice for accepted commands and indices outside the batch.
    pub fn rejections_for(&self, index: usize) -> &[(&'static str, Rejection)] {
        self.command_ranges
            .get(index)
            .and_then(|ranges| self.last.get(ranges.rejections.clone()))
            .unwrap_or_default()
    }

    /// Results of the attempted links in a multi-connection command, in input order.
    /// Each entry carries its zero-based link index. A command-level rejection
    /// before link processing yields an empty slice.
    pub fn link_outcomes(&self, index: usize) -> &[LinkOutcome] {
        self.command_ranges
            .get(index)
            .map(|ranges| &self.links[ranges.links.clone()])
            .unwrap_or_default()
    }

    /// The result of an EditWater command at the given input index.
    pub fn water_edit(&self, index: usize) -> Option<&Result<WaterEditOutcome, WaterEditError>> {
        self.water_edits.get(&index)
    }

    /// Discards the completed batch's outcomes while preserving its sequence.
    pub fn clear(&mut self) {
        self.last.clear();
        self.accepted.clear();
        self.command_ranges.clear();
        self.links.clear();
        self.water_edits.clear();
    }

    pub(crate) fn begin_batch(&mut self) -> Option<Self> {
        static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
        // A callback can apply a nested batch. Keep the outer outcomes intact
        // while the nested commands and their callbacks run.
        let parent = self.batch_in_progress.then(|| std::mem::take(self));
        self.clear();
        self.sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.batch_in_progress = true;
        parent
    }

    pub(crate) fn finish_batch(&mut self, parent: Option<Self>) {
        if let Some(parent) = parent {
            *self = parent;
        } else {
            self.batch_in_progress = false;
        }
    }

    pub(crate) fn command_start(&self) -> (usize, usize) {
        (self.last.len(), self.links.len())
    }

    pub(crate) fn record_command(
        &mut self,
        index: usize,
        (rejection_start, link_start): (usize, usize),
    ) {
        let rejection_end = self.last.len();
        if rejection_start == rejection_end {
            self.accepted.push(index);
        }
        self.command_ranges.push(CommandOutcomeRanges {
            rejections: rejection_start..rejection_end,
            links: link_start..self.links.len(),
        });
    }

    fn record_link(&mut self, index: usize, result: Result<(), (&'static str, Rejection)>) {
        if let Err(rejection) = result {
            self.last.push(rejection);
        }
        self.links.push((index, result));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestCtx;
    use crate::{ParCommandBuffer, VehicleEnt, VehicleID};

    #[test]
    fn scale_t0_rejections_follow_batch_input_indices() {
        let mut sim = TestCtx::new().g;
        let accepted = WorldCommand::SetGameTime(*sim.read::<GameTime>());
        let rejected = WorldCommand::MapBuildHouse(LotID::default());
        let pattern = LanePatternBuilder::default().build();
        let multi = WorldCommand::MapMakeMultipleConnections(
            vec![
                MapProject::ground(vec3(100.0, 100.0, 0.0)),
                MapProject::ground(vec3(100.5, 100.0, 0.0)),
                MapProject {
                    pos: vec3(300.0, 100.0, 0.0),
                    kind: ProjectKind::Road(RoadID::default()),
                },
            ],
            vec![(0, 1, None, pattern.clone()), (0, 2, None, pattern)],
        );
        sim.apply_commands([&rejected, &accepted, &multi, &rejected]);

        let house = ("MapBuildHouse", Rejection::MissingReference);
        let invalid = ("MapMakeMultipleConnections", Rejection::InvalidConnection);
        let missing = ("MapMakeMultipleConnections", Rejection::MissingReference);
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(outcomes.accepted, [1]);
        assert_eq!(outcomes.last, [house, invalid, missing, house]);
        assert_eq!(outcomes.rejections_for(0), [house]);
        assert!(outcomes.rejections_for(1).is_empty());
        assert_eq!(outcomes.rejections_for(2), [invalid, missing]);
        assert_eq!(outcomes.rejections_for(3), [house]);
        assert!(outcomes.rejections_for(4).is_empty());
        let sequence = outcomes.batch_seq();
        drop(outcomes);

        sim.apply_commands([]);
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(outcomes.batch_seq(), sequence);
        assert_eq!(outcomes.rejections_for(2), [invalid, missing]);
        drop(outcomes);

        sim.apply_commands([&accepted, &rejected]);
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(outcomes.accepted, [0]);
        assert_eq!(outcomes.last, [house]);
        assert!(outcomes.rejections_for(0).is_empty());
        assert_eq!(outcomes.rejections_for(1), [house]);
        assert!(outcomes.rejections_for(2).is_empty());
    }

    #[test]
    fn scale_t0_callback_rejection_belongs_to_command_that_drains_it() {
        let mut sim = TestCtx::new().g;
        let command = WorldCommand::SetGameTime(*sim.read::<GameTime>());
        sim.read::<ParCommandBuffer<VehicleEnt>>()
            .exec_ent(VehicleID::default(), |sim| {
                WorldCommand::MapBuildHouse(LotID::default())
                    .reject(sim, Rejection::MissingReference);
            });

        sim.apply_commands([&command, &command]);

        let outcomes = sim.read::<CommandOutcomes>();
        let rejection = ("MapBuildHouse", Rejection::MissingReference);
        assert_eq!(outcomes.accepted, [1]);
        assert_eq!(outcomes.last, [rejection]);
        assert_eq!(outcomes.rejections_for(0), [rejection]);
        assert!(outcomes.rejections_for(1).is_empty());
    }

    #[test]
    fn scale_t0_nested_batches_preserve_outer_outcomes() {
        let mut sim = TestCtx::new().g;
        let accepted = WorldCommand::SetGameTime(*sim.read::<GameTime>());
        let rejected = WorldCommand::MapBuildHouse(LotID::default());
        sim.read::<ParCommandBuffer<VehicleEnt>>()
            .exec_ent(VehicleID::default(), |sim| {
                let nested_accepted = WorldCommand::SetGameTime(*sim.read::<GameTime>());
                let nested_rejected = WorldCommand::MapRemoveRoad(RoadID::default());
                nested_rejected.apply(sim);
                nested_accepted.apply(sim);
            });

        sim.apply_commands([&rejected, &accepted]);

        let outcomes = sim.read::<CommandOutcomes>();
        let house = ("MapBuildHouse", Rejection::MissingReference);
        assert_eq!(outcomes.accepted, [1]);
        assert_eq!(outcomes.last, [house]);
        assert_eq!(outcomes.rejections_for(0), [house]);
        assert!(outcomes.rejections_for(1).is_empty());
    }

    #[test]
    fn scale_t0_link_outcomes_distinguish_a_success_between_identical_rejections() {
        let mut sim = TestCtx::new().g;
        let accepted = WorldCommand::SetGameTime(*sim.read::<GameTime>());
        let pattern = LanePatternBuilder::default().build();
        let projects = vec![
            MapProject::ground(vec3(100.0, 100.0, 0.0)),
            MapProject::ground(vec3(100.5, 100.0, 0.0)),
            MapProject::ground(vec3(310.0, 100.0, 0.0)),
            MapProject::ground(vec3(310.5, 100.0, 0.0)),
        ];
        let multi = WorldCommand::MapMakeMultipleConnections(
            projects.clone(),
            vec![
                (0, 1, None, pattern.clone()),
                (0, 2, None, pattern.clone()),
                (2, 3, None, pattern.clone()),
            ],
        );
        let malformed = WorldCommand::MapMakeMultipleConnections(
            projects,
            vec![(0, 2, None, pattern.clone()), (0, 4, None, pattern)],
        );
        let road_count = sim.map().roads().len();

        sim.apply_commands([&accepted, &multi, &malformed, &accepted]);

        let mut outcomes = sim.write::<CommandOutcomes>();
        let kind = "MapMakeMultipleConnections";
        let invalid = (kind, Rejection::InvalidConnection);
        let invalid_index = (kind, Rejection::InvalidBatchIndex);
        assert_eq!(sim.map().roads().len(), road_count + 1);
        assert_eq!(outcomes.accepted, [0, 3]);
        assert_eq!(outcomes.last, [invalid, invalid, invalid_index]);
        assert_eq!(outcomes.rejections_for(1), [invalid, invalid]);
        assert_eq!(outcomes.rejections_for(2), [invalid_index]);
        assert_eq!(
            outcomes.link_outcomes(1),
            [(0, Err(invalid)), (1, Ok(())), (2, Err(invalid))]
        );
        for index in [0, 2, 3, 4] {
            assert!(outcomes.link_outcomes(index).is_empty());
        }

        let sequence = outcomes.batch_seq();
        outcomes.clear();
        assert_eq!(outcomes.batch_seq(), sequence);
        assert!(outcomes.accepted.is_empty());
        assert!(outcomes.last.is_empty());
        assert!(outcomes.rejections_for(1).is_empty());
        assert!(outcomes.link_outcomes(1).is_empty());
    }
}

impl AsRef<[WorldCommand]> for WorldCommands {
    fn as_ref(&self) -> &[WorldCommand] {
        &self.commands
    }
}

impl WorldCommands {
    pub fn push(&mut self, cmd: WorldCommand) {
        self.commands.push(cmd);
    }

    pub fn extend(&mut self, cmds: impl IntoIterator<Item = WorldCommand>) {
        self.commands.extend(cmds);
    }

    pub fn merge(&mut self, src: &WorldCommands) {
        self.commands.extend_from_slice(&src.commands);
    }

    pub fn iter(&self) -> impl Iterator<Item = &WorldCommand> {
        self.commands.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn map_load_paris(&mut self) {
        self.commands.push(MapLoadParis)
    }

    pub fn map_load_testfield(&mut self, pos: Vec2, size: u32, spacing: f32) {
        self.commands.push(MapLoadTestField { pos, size, spacing })
    }

    pub fn set_game_time(&mut self, gt: GameTime) {
        self.commands.push(SetGameTime(gt))
    }

    pub fn add_train(&mut self, dist: f32, n_wagons: u32, laneid: LaneID) {
        self.commands.push(AddTrain {
            dist,
            n_wagons,
            lane: laneid,
        })
    }

    pub fn map_build_special_building(
        &mut self,
        obb: OBB,
        kind: BuildingKind,
        gen: BuildingGen,
        zone: Option<Zone>,
        connected_road: Option<RoadID>,
    ) {
        self.commands.push(MapBuildSpecialBuilding {
            pos: obb,
            kind,
            gen,
            zone,
            connected_road,
        })
    }

    pub fn map_remove_intersection(&mut self, id: IntersectionID) {
        self.commands.push(MapRemoveIntersection(id))
    }

    pub fn map_remove_road(&mut self, id: RoadID) {
        self.commands.push(MapRemoveRoad(id))
    }

    pub fn map_remove_building(&mut self, id: BuildingID) {
        self.commands.push(MapRemoveBuilding(id))
    }

    pub fn map_build_house(&mut self, id: LotID) {
        self.commands.push(MapBuildHouse(id))
    }

    pub fn map_make_connection(
        &mut self,
        from: MapProject,
        to: MapProject,
        interpoint: Option<Vec2>,
        pat: LanePattern,
    ) {
        self.commands.push(MapMakeConnection {
            from,
            to,
            inter: interpoint,
            pat,
        })
    }

    pub fn map_update_intersection_policy(
        &mut self,
        id: IntersectionID,
        tp: TurnPolicy,
        lp: LightPolicy,
    ) {
        self.commands.push(MapUpdateIntersectionPolicy {
            inter: id,
            turn: tp,
            light: lp,
        })
    }
}

impl WorldCommand {
    /// Returns true if the command can be applied without any systems needed to be run afterward
    pub fn is_instant(&self) -> bool {
        matches!(
            self,
            MapBuildHouse(_)
                | MapUpdateIntersectionPolicy { .. }
                | UpdateZone { .. }
                | SetGameTime(_)
                | ZonePaint { .. }
                | SetTaxRate { .. }
                | TransitAddStop { .. }
                | TransitRemoveStop { .. }
                | TransitCreateLine { .. }
                | TransitEditLine { .. }
                | TransitRemoveLine { .. }
                | TransitSetVehicles { .. }
                | TransitSetFare { .. }
                | SetWarehousePolicy { .. }
                | SetLotLayout(_)
                | EditWater(_)
        )
    }

    pub fn apply(&self, sim: &mut Simulation) {
        sim.apply_commands(std::iter::once(self));
    }

    pub(crate) fn apply_in_batch(
        &self,
        sim: &mut Simulation,
        construction: BudgetCategory,
        water: &mut WaterBatch,
    ) {
        if let EditWater(edit) = self {
            let outcome = if water.reject_surface {
                Err(WaterEditError::Limit)
            } else {
                sim.map_mut().apply_water_edit(
                    edit,
                    &mut sim.write::<ZoneGrid>(),
                    &mut water.remaining,
                )
            };
            match &outcome {
                Ok(result) if result.changed => self.record_for_replay(sim),
                Err(error) => self.reject(sim, Rejection::water_error(error)),
                Ok(_) => {}
            }
            let mut outcomes = sim.write::<CommandOutcomes>();
            let index = outcomes.command_ranges.len();
            outcomes.water_edits.insert(index, outcome);
            return;
        }

        if let Terraform {
            kind,
            center,
            radius,
            amount,
            level,
            slope,
        } = *self
        {
            if water.reject_surface {
                self.reject(sim, Rejection::WaterWorkLimit);
                return;
            }
            if sim.map().environment().has_water_configuration() {
                let tick = sim.read::<GameTime>().tick;
                let result = {
                    let mut map = sim.map_mut();
                    map.environment()
                        .prepare_water_terraform(tick, kind, center, radius, amount, level, slope)
                        .and_then(|prepared| {
                            map.commit_water_edit(
                                prepared,
                                &mut sim.write::<ZoneGrid>(),
                                &mut water.remaining,
                            )
                        })
                };
                match result {
                    Ok(outcome) if outcome.changed => self.record_for_replay(sim),
                    Ok(_) => {}
                    Err(error) => self.reject(
                        sim,
                        match error {
                            WaterEditError::InvalidInput | WaterEditError::OutOfBounds => {
                                Rejection::InvalidTerraform
                            }
                            WaterEditError::Limit => Rejection::WaterWorkLimit,
                            WaterEditError::OccupiedFootprint { .. } => Rejection::WetFootprint,
                            _ => Rejection::WaterTerrainConflict,
                        },
                    ),
                }
                return;
            }
        }

        if water.guard_terraform {
            const MAX_WET_DESCRIPTORS: usize = 65_536;
            let fits = match self {
                MapMakeConnection { pat, .. } => crate::map::wet_lane_count_fits(pat),
                MapMakeMultipleConnections(projects, links) => {
                    let count = projects.len().saturating_add(links.len());
                    count <= MAX_WET_DESCRIPTORS
                        && links
                            .iter()
                            .try_fold(count, |count, (_, _, _, pattern)| {
                                let total = count
                                    .saturating_add(pattern.lanes_forward.len())
                                    .saturating_add(pattern.lanes_backward.len());
                                (total <= MAX_WET_DESCRIPTORS
                                    && crate::map::wet_lane_count_fits(pattern))
                                .then_some(total)
                            })
                            .is_some()
                }
                _ => true,
            };
            if !fits {
                self.reject(sim, Rejection::WaterWorkLimit);
                return;
            }
        }

        if matches!(self, Init(_)) {
            self.reject(sim, Rejection::UnexpectedInit);
            return;
        }

        if let Err(reason) = self.validate_progression(sim) {
            self.reject(sim, reason);
            return;
        }

        let is_transit = transit::is_command(self);
        if is_transit {
            // Record transit commands only after validation and application succeed.
            if let Err(reason) =
                transit::validate(sim, self).and_then(|()| self.apply_one(sim, construction))
            {
                self.reject(sim, reason);
                return;
            }
        }

        if matches!(
            self,
            MapMakeConnection { .. } | MapMakeMultipleConnections(..)
        ) && sim.map().environment().has_water_configuration()
        {
            let result = match self {
                MapMakeMultipleConnections(projects, links) => {
                    Some(self.apply_connections(sim, projects, links, construction))
                }
                MapMakeConnection {
                    from,
                    to,
                    inter,
                    pat,
                } => Some(
                    Self::apply_connection(sim, *from, *to, *inter, pat, construction)
                        .map(|()| true),
                ),
                _ => None,
            };
            if let Some(result) = result {
                match result {
                    Ok(true) => self.record_for_replay(sim),
                    Ok(false) => {}
                    Err(reason) => self.reject(sim, reason),
                }
                return;
            }
        }

        self.record_for_replay(sim);
        if is_transit {
            return;
        }

        let result = match self {
            ZonePaint {
                area,
                kind,
                density,
            } => Self::apply_zone_paint(sim, area, *kind, *density, |_| true),
            MapMakeMultipleConnections(projects, links) => self
                .apply_connections(sim, projects, links, construction)
                .map(|_| ()),
            MapMakeConnection {
                from,
                to,
                inter,
                pat,
            } => Self::apply_connection(sim, *from, *to, *inter, pat, construction),
            _ => self.apply_one(sim, construction),
        };
        if let Err(reason) = result {
            self.reject(sim, reason);
        }
    }

    fn record_for_replay(&self, sim: &Simulation) {
        let mut replay = sim.write::<Replay>();
        if replay.enabled {
            replay.push(sim.read::<GameTime>().tick, self.clone());
        }
    }

    fn reject(&self, sim: &Simulation, reason: Rejection) {
        sim.write::<CommandOutcomes>()
            .last
            .push((self.kind(), reason));
    }

    fn kind(&self) -> &'static str {
        match self {
            Init(_) => "Init",
            MapRemoveIntersection(_) => "MapRemoveIntersection",
            MapRemoveRoad(_) => "MapRemoveRoad",
            MapRemoveBuilding(_) => "MapRemoveBuilding",
            MapBuildHouse(_) => "MapBuildHouse",
            Terraform { .. } => "Terraform",
            SendMessage { .. } => "SendMessage",
            SpawnRandomCars { .. } => "SpawnRandomCars",
            AddTrain { .. } => "AddTrain",
            SpawnTrain { .. } => "SpawnTrain",
            MapMakeConnection { .. } => "MapMakeConnection",
            MapMakeMultipleConnections(..) => "MapMakeMultipleConnections",
            MapUpdateIntersectionPolicy { .. } => "MapUpdateIntersectionPolicy",
            MapBuildSpecialBuilding { .. } => "MapBuildSpecialBuilding",
            MapLoadParis => "MapLoadParis",
            MapLoadTestField { .. } => "MapLoadTestField",
            UpdateZone { .. } => "UpdateZone",
            SetGameTime(_) => "SetGameTime",
            ZonePaint { .. } => "ZonePaint",
            SetTaxRate { .. } => "SetTaxRate",
            TransitAddStop { .. } => "TransitAddStop",
            TransitRemoveStop { .. } => "TransitRemoveStop",
            TransitCreateLine { .. } => "TransitCreateLine",
            TransitEditLine { .. } => "TransitEditLine",
            TransitRemoveLine { .. } => "TransitRemoveLine",
            TransitSetVehicles { .. } => "TransitSetVehicles",
            TransitSetFare { .. } => "TransitSetFare",
            SetWarehousePolicy { .. } => "SetWarehousePolicy",
            SetLotLayout(_) => "SetLotLayout",
            EditWater(_) => "EditWater",
        }
    }

    fn validate_progression(&self, sim: &Simulation) -> Result<(), Rejection> {
        let capability = match self {
            MapBuildSpecialBuilding { kind: BuildingKind::GoodsCompany(id), .. } => {
                BuildCapability::Prototype(*id)
            }
            // Rail freight connectors are available at the start.
            ZonePaint { kind: Some(_), density, .. } if *density != Density::Low => {
                BuildCapability::ZoneDensity(*density)
            }
            TransitCreateLine { .. } | TransitSetVehicles { n: 1.., .. } => {
                BuildCapability::BusService
            }
            _ => return Ok(()),
        };
        can_build(sim, capability).map_err(|lock| Rejection::Locked {
            milestone: lock.milestone,
        })
    }

    fn warehouse_company(sim: &Simulation, building: BuildingID) -> Result<CompanyID, Rejection> {
        let map = sim.map();
        let site = map.get(building).ok_or(Rejection::MissingReference)?;
        let Some(id) = site.kind.as_goods_company() else {
            return Err(Rejection::InvalidWarehouse);
        };
        let Some(SoulID::GoodsCompany(owner)) = sim.read::<BuildingInfos>().owner(building) else {
            return Err(Rejection::InvalidWarehouse);
        };
        let company = sim.world().companies.get(owner).ok_or(Rejection::MissingReference)?;
        if company.comp.building != building
            || company.comp.proto != id
            || try_prototype(id).is_none_or(|proto| proto.storage.is_none())
        {
            return Err(Rejection::InvalidWarehouse);
        }
        Ok(owner)
    }

    fn validate(&self, map: &Map) -> Result<(), Rejection> {
        let exists = match self {
            MapRemoveIntersection(id) | MapUpdateIntersectionPolicy { inter: id, .. } => {
                map.get(*id).is_some()
            }
            MapRemoveRoad(id) => map.get(*id).is_some(),
            MapRemoveBuilding(id) => map.get(*id).is_some(),
            MapLoadParis | MapLoadTestField { .. }
                if map.environment().has_water_configuration() =>
            {
                return Err(Rejection::WaterConfigured);
            }
            MapBuildHouse(id) => {
                let lot = map.get(*id).ok_or(Rejection::MissingReference)?;
                if !Building::dry_footprint(map.environment(), &lot.shape, None) {
                    return Err(Rejection::WetFootprint);
                }
                map.get(lot.parent).is_some()
            }
            Terraform {
                center,
                radius,
                amount,
                level,
                slope,
                ..
            } => {
                const MAX_RADIUS: f32 = 2_000.0;
                const MAX_AMOUNT: f32 = 2_000.0;
                if !center.is_finite()
                    || !((0.0..=MAX_RADIUS).contains(radius) && *radius > 0.0)
                    || !(-MAX_AMOUNT..=MAX_AMOUNT).contains(amount)
                    || !level.is_finite()
                    || slope.is_some_and(|(start, end)| {
                        let length = start.xy().distance2(end.xy());
                        !start.is_finite()
                            || !end.is_finite()
                            || !length.is_finite()
                            || length == 0.0
                    })
                {
                    return Err(Rejection::InvalidTerraform);
                }
                true
            }
            SpawnRandomCars { n_cars } if *n_cars > 1_000 => {
                return Err(Rejection::TooManyRandomCars)
            }
            SpawnTrain { wagons, lane, dist } => {
                let lane = map.get(*lane).ok_or(Rejection::MissingReference)?;
                if lane.kind != LaneKind::Rail
                    || !dist.is_finite()
                    || !(0.0..=lane.points.length()).contains(dist)
                    || wagons.is_empty()
                    || wagons
                        .iter()
                        .try_fold(0u32, |mass, &id| mass.checked_add(try_prototype(id)?.mass))
                        .is_none_or(|mass| mass == 0)
                {
                    return Err(Rejection::InvalidTrain);
                }
                true
            }
            MapBuildSpecialBuilding {
                pos,
                kind,
                gen,
                zone,
                connected_road,
            } => {
                if !Building::valid_footprint(pos, *gen) {
                    return Err(Rejection::InvalidBuilding);
                }
                let zoneable = match *kind {
                    BuildingKind::GoodsCompany(id) => {
                        let proto = try_prototype(id).ok_or(Rejection::MissingReference)?;
                        if id.is_growable() {
                            return Err(Rejection::InvalidBuilding);
                        }
                        if id.is_service() {
                            let road = connected_road
                                .and_then(|road| map.get(road))
                                .ok_or(Rejection::MissingReference)?;
                            let door = Building::door_position(pos, *gen, map.environment());
                            let snap_distance = proto.size.diag() * 0.5 + road.width * 0.5;
                            let nearest = road.points().project(door);
                            if !door.is_close(nearest, snap_distance) {
                                return Err(Rejection::InvalidBuilding);
                            }
                        }
                        proto.zone.is_some()
                    }
                    BuildingKind::RailFreightStation(id) => {
                        try_prototype(id).ok_or(Rejection::MissingReference)?;
                        false
                    }
                    BuildingKind::House
                    | BuildingKind::ExternalTrading
                    | BuildingKind::TrainStation => {
                        return Err(Rejection::InvalidBuilding);
                    }
                };
                if zoneable != zone.is_some()
                    || zone
                        .as_ref()
                        .is_some_and(|zone| zone.validated_area().is_none())
                {
                    return Err(Rejection::InvalidZone);
                }
                if !Building::dry_footprint(map.environment(), pos, zone.as_ref()) {
                    return Err(Rejection::WetFootprint);
                }
                connected_road.is_none_or(|id| map.get(id).is_some())
            }
            UpdateZone { building, zone } => {
                let b = map.get(*building).ok_or(Rejection::MissingReference)?;
                if b.zone.is_none()
                    || b.kind
                        .as_goods_company()
                        .and_then(try_prototype)
                        .is_none_or(|proto| proto.zone.is_none())
                    || zone.validated_area().is_none()
                {
                    return Err(Rejection::InvalidZone);
                }
                if !Building::dry_footprint(map.environment(), &b.obb, Some(zone)) {
                    return Err(Rejection::WetFootprint);
                }
                true
            }
            ZonePaint { area, kind, .. } => {
                ZoneGrid::eligible_paint_cells(map, area, *kind)?;
                true
            }
            SetTaxRate { rate_permille, .. } => {
                if *rate_permille > crate::economy::MAX_TAX_RATE_PERMILLE {
                    return Err(Rejection::InvalidTaxRate);
                }
                true
            }
            _ => true,
        };
        if exists {
            Ok(())
        } else {
            Err(Rejection::MissingReference)
        }
    }

    /// Run the validation and cost checks used before applying a special building.
    pub fn validate_and_price(&self, sim: &Simulation) -> Result<Money, Rejection> {
        self.validate(&sim.map())?;
        Self::affordable_cost(sim, Government::checked_action_cost(self, sim))
    }

    fn affordable_cost(sim: &Simulation, cost: Option<Money>) -> Result<Money, Rejection> {
        let cost = cost.ok_or(Rejection::CostOverflow)?;
        let money = sim.read::<Government>().money;
        if cost > Money::ZERO && money < cost {
            return Err(Rejection::InsufficientFunds);
        }
        money
            .inner()
            .checked_sub(cost.inner())
            .map(|_| cost)
            .ok_or(Rejection::CostOverflow)
    }

    fn charge_construction(sim: &Simulation, cost: Money, category: BudgetCategory) {
        if let Some(delta) = cost.inner().checked_neg() {
            Budget::apply(&sim.resources, category, Money::new_inner(delta));
        } else {
            // A refund of -i64::MIN fits when the balance is negative, but needs two Money deltas.
            Budget::apply(&sim.resources, category, Money::MAX);
            Budget::apply(&sim.resources, category, Money::new_inner(1));
        }
    }

    pub(crate) fn apply_zone_paint(
        sim: &Simulation,
        area: &OBB,
        kind: Option<ZoneKind>,
        density: Density,
        owns: impl FnMut(ZoneCellCoord) -> bool,
    ) -> Result<(), Rejection> {
        sim.write::<ZoneGrid>()
            .paint_on_map_with_land(&sim.map(), area, kind, density, owns)
            .map_err(Rejection::from)
    }

    fn apply_connection(
        sim: &Simulation,
        from: MapProject,
        to: MapProject,
        inter: Option<Vec2>,
        pattern: &LanePattern,
        construction: BudgetCategory,
    ) -> Result<(), Rejection> {
        let mut map = sim.map_mut();
        let mut work = map
            .environment()
            .has_water_configuration()
            .then_some(MAX_WATER_BATCH_WORK);
        let (from, to) = map
            .prepare_connection(from, to, inter, pattern, work.as_mut())
            .map_err(Rejection::water_connection_error)?;
        let cost =
            Self::affordable_cost(sim, Government::connection_cost(from.pos, to.pos, pattern))?;
        map.make_resolved_connection(from, to, inter, pattern, work.as_mut());
        Self::charge_construction(sim, cost, construction);
        Ok(())
    }

    fn apply_connections(
        &self,
        sim: &mut Simulation,
        projects: &[MapProject],
        links: &[(usize, usize, Option<Vec2>, LanePattern)],
        construction: BudgetCategory,
    ) -> Result<bool, Rejection> {
        if links
            .iter()
            .any(|&(from, to, _, _)| from >= projects.len() || to >= projects.len())
        {
            return Err(Rejection::InvalidBatchIndex);
        }

        let mut map = sim.map_mut();
        let mut water_work = map.environment().has_water_configuration().then_some(MAX_WATER_BATCH_WORK);
        let mut endpoints: Vec<_> = projects
            .iter()
            .map(|&project| project.kind.check_valid(&map).then_some(project))
            .collect();
        let mut junctions = BTreeSet::new();
        let mut splits = BTreeMap::new();
        let mut connected_any = false;
        let record_link = |index, result: Result<(), Rejection>| {
            sim.write::<CommandOutcomes>()
                .record_link(index, result.map_err(|reason| (self.kind(), reason)));
        };
        for (index, &(from, to, interpoint, ref pat)) in links.iter().enumerate() {
            let mut resolve = |index: usize| {
                let Some(project) = endpoints[index] else { return Ok(None) };
                if let ProjectKind::Road(id) = project.kind {
                    if splits.contains_key(&id) {
                        return map.reproject_split_endpoint(id, project.pos, &splits, water_work.as_mut());
                    }
                }
                if let Some(work) = &mut water_work {
                    map.resolve_water_connection_endpoint(project, work)
                } else {
                    Ok(map.resolve_connection_endpoint(project))
                }
            };
            let (fromproj, toproj) = match resolve(from).and_then(|start| Ok(start.zip(resolve(to)?))) {
                Ok(Some(pair)) => pair,
                Ok(None) => {
                    record_link(index, Err(Rejection::MissingReference));
                    continue;
                }
                Err(error) => {
                    record_link(index, Err(Rejection::water_connection_error(error)));
                    break;
                }
            };
            if !Map::can_connect(fromproj, toproj, interpoint, pat) {
                record_link(index, Err(Rejection::InvalidConnection));
                continue;
            }
            if let Some(work) = &mut water_work {
                if let Err(error) = map.preflight_water_connection(fromproj, toproj, interpoint, pat, work) {
                    let reason = Rejection::water_connection_error(error);
                    record_link(index, Err(reason));
                    if reason == Rejection::WaterWorkLimit && *work == 0 {
                        break;
                    }
                    continue;
                }
            }
            let cost = match Self::affordable_cost(
                sim,
                Government::connection_cost(fromproj.pos, toproj.pos, pat),
            ) {
                Ok(cost) => cost,
                Err(reason) => {
                    record_link(index, Err(reason));
                    continue;
                }
            };
            let connected = map.connect_projects(fromproj, toproj, interpoint, pat);
            for split in connected.splits.into_iter().flatten() {
                splits.insert(split.original, split);
            }
            for (index, id, pos) in [
                (from, connected.src, fromproj.pos),
                (to, connected.dst, toproj.pos),
            ] {
                endpoints[index] = Some(MapProject {
                    pos,
                    kind: ProjectKind::Intersection(id),
                });
                junctions.insert(id);
            }
            Self::charge_construction(sim, cost, construction);
            connected_any = true;
            record_link(index, Ok(()));
        }
        // Keep junction IDs alive until every link that names them has been processed.
        for junction in junctions {
            if map
                .merge_shallow_junction(junction, water_work.as_mut())
                .is_err()
                && water_work == Some(0)
            {
                break;
            }
        }
        map.check_invariants();
        Ok(connected_any)
    }

    fn apply_one(
        &self,
        sim: &mut Simulation,
        construction: BudgetCategory,
    ) -> Result<(), Rejection> {
        let cost = self.validate_and_price(sim)?;

        match *self {
            MapRemoveIntersection(id) => sim.map_mut().remove_intersection(id),
            MapRemoveRoad(id) => {
                sim.map_mut()
                    .remove_road(id)
                    .ok_or(Rejection::MutationFailed)?;
            }
            MapRemoveBuilding(id) => {
                let was_external_target = sim.map().external_train_stations.first() == Some(&id);
                let building = sim
                    .map_mut()
                    .remove_building(id)
                    .ok_or(Rejection::MutationFailed)?;
                residence::on_building_removed(sim, id);
                crate::souls::lifecycle::remove_building(sim, building, was_external_target);
            }
            MapBuildHouse(id) => {
                let building = sim
                    .map_mut()
                    .build_house(id)
                    .ok_or(Rejection::MutationFailed)?;
                sim.write::<BuildingInfos>().insert(building);
                residence::on_house_built(sim, building);
            }
            MapMakeConnection { .. }
            | MapMakeMultipleConnections(..)
            | EditWater(_)
            | ZonePaint { .. } => {
                unreachable!()
            }
            MapUpdateIntersectionPolicy {
                inter: id,
                turn: tp,
                light: lp,
            } => sim.map_mut().update_intersection(id, move |i| {
                i.light_policy = lp;
                i.turn_policy = tp;
            }),
            MapBuildSpecialBuilding {
                pos: obb,
                kind,
                gen,
                ref zone,
                connected_road,
            } => {
                let id = sim
                    .write::<Map>()
                    .build_special_building(&obb, kind, gen, zone.clone(), connected_road)
                    .ok_or(Rejection::MutationFailed)?;
                sim.write::<BuildingInfos>().insert(id);
            }
            SetGameTime(gt) => {
                let mut time = sim.write::<GameTime>();
                sim.write::<Budget>().shift_clock(time.tick, gt.tick);
                *time = gt;
            }
            SetTaxRate {
                zone,
                rate_permille,
            } => sim.write::<Budget>().set_tax_rate(zone, rate_permille),
            TransitAddStop { .. }
            | TransitRemoveStop { .. }
            | TransitCreateLine { .. }
            | TransitEditLine { .. }
            | TransitRemoveLine { .. }
            | TransitSetVehicles { .. }
            | TransitSetFare { .. } => transit::apply(sim, self)?,
            AddTrain {
                dist: _,
                n_wagons: _,
                lane: _,
            } => {}
            SpawnTrain {
                ref wagons,
                lane,
                dist,
            } => {
                spawn_train(sim, wagons, RailWagonKind::Freight, lane, dist)
                    .ok_or(Rejection::MutationFailed)?;
            }

            MapLoadParis => {
                load_parismap(&mut sim.map_mut()).map_err(|_| Rejection::WaterConfigured)?;
            }
            MapLoadTestField { pos, size, spacing } => {
                load_testfield(&mut sim.map_mut(), pos, size, spacing)
                    .map_err(|_| Rejection::WaterConfigured)?;
            }
            SetWarehousePolicy { building, policy } => {
                let company = Self::warehouse_company(sim, building)?;
                let has_access = {
                    let map = sim.map();
                    map.get(building)
                        .and_then(|site| site.connected_road)
                        .and_then(|road| map.get(road))
                        .is_some_and(|road| {
                            road.lanes_iter()
                                .any(|(_, kind)| kind == crate::map::LaneKind::Driving)
                        })
                };
                if !has_access {
                    return Err(Rejection::InvalidWarehouse);
                }
                CargoBook::set_warehouse_policy(sim, company, policy);
            }
            SetLotLayout(layout) => sim.map_mut().set_lot_layout(layout),
            Init(_) => return Err(Rejection::UnexpectedInit),
            UpdateZone { building, ref zone } => {
                sim.map_mut()
                    .update_zone(building, zone.clone())
                    .ok_or(Rejection::MutationFailed)?;
            }
            SpawnRandomCars { n_cars } => {
                for _ in 0..n_cars {
                    let mut pm = sim.write::<ParkingManagement>();
                    let map = sim.map();
                    let mut rng = sim.write::<RandProvider>();

                    let Some(spot) = pm.reserve_random_free_spot(&map.parking, rng.next_u64())
                    else {
                        continue;
                    };

                    drop((map, pm, rng));

                    let Some(v_id) = spawn_parked_vehicle_with_spot(sim, VehicleKind::Car, spot)
                    else {
                        continue;
                    };
                    unpark(sim, v_id);

                    sim.write::<RandomVehicles>().vehicles.insert(v_id);
                }
            }
            SendMessage { ref message } => {
                sim.write::<MultiplayerState>()
                    .chat
                    .add_message(message.clone());
            }
            Terraform {
                kind,
                amount,
                center,
                radius,
                level,
                slope,
            } => {
                let tick = sim.read::<GameTime>().tick;
                sim.map_mut()
                    .terraform(tick, kind, center, radius, amount, level, slope);
            }
        }
        Self::charge_construction(sim, cost, construction);
        Ok(())
    }

    pub(crate) fn bootstrap(
        sim: &mut Simulation,
        options: SimulationOptions,
    ) -> Result<(), Rejection> {
        Self::bootstrap_world(sim, options, BootstrapKind::Normal)
    }

    pub(crate) fn bootstrap_import(
        sim: &mut Simulation,
        options: SimulationOptions,
    ) -> Result<(), Rejection> {
        Self::bootstrap_world(sim, options, BootstrapKind::Import)
    }

    pub(crate) fn bootstrap_authored(
        sim: &mut Simulation,
        options: SimulationOptions,
    ) -> Result<(), Rejection> {
        Self::bootstrap_world(sim, options, BootstrapKind::AuthoredFlat)
    }

    fn bootstrap_world(
        sim: &mut Simulation,
        options: SimulationOptions,
        kind: BootstrapKind,
    ) -> Result<(), Rejection> {
        let tick = sim.read::<GameTime>().tick;
        if tick.0 != crate::INITIAL_TICK || sim.resources.try_read::<SimulationOptions>().is_ok() {
            return Err(Rejection::UnexpectedInit);
        }
        let valid = match kind {
            BootstrapKind::Normal => options.valid_for_bootstrap(),
            BootstrapKind::Import => {
                !options.save_replay
                    && options.preset != MapPreset::Waters
                    && options.valid_for_bootstrap()
            }
            BootstrapKind::AuthoredFlat => {
                !options.save_replay
                    && options.preset == MapPreset::Flat
                    && (1..=crate::NewGameOptions::MAX_TERRAIN_CHUNKS)
                        .contains(&options.terrain_size)
            }
        };
        if !valid {
            return Err(Rejection::InvalidInitOptions);
        }
        info!("Initializing world with {options:?}");
        if options.terrain_size > 0 {
            generate_terrain(sim, options)?;
            if kind == BootstrapKind::Normal {
                let starter_built = build_starting_external_trading(sim);
                if options.preset == MapPreset::Waters && !starter_built {
                    return Err(Rejection::MutationFailed);
                }
            }
        }
        sim.insert(RandProvider::for_world(options.seed));
        sim.write::<ProgressionState>().mode = options.progression;
        sim.insert(Government {
            money: Money::new_bucks(i64::from(options.starting_money_bucks)),
        });
        {
            let mut replay = sim.write::<Replay>();
            replay.enabled = options.save_replay;
            if replay.enabled {
                replay.push(tick, Init(Box::new(options)));
            }
        }
        sim.resources.insert(options);
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BootstrapKind {
    Normal,
    Import,
    AuthoredFlat,
}

fn generate_terrain(sim: &mut Simulation, options: SimulationOptions) -> Result<(), Rejection> {
    info!("generating terrain..");
    let t = Instant::now();

    let size = options.terrain_size;
    let environment = match options.preset {
        MapPreset::Waters => crate::map::procgen::generate_waters(size, size, options.seed)
            .map_err(|_| Rejection::WaterEditFailed)?,
        _ => Environment::from_options(options),
    };
    if !sim.map_mut().install_environment(environment) {
        return Err(Rejection::UnexpectedInit);
    }
    info!("took {}s", t.elapsed().as_secs_f32());
    Ok(())
}

fn build_starting_external_trading(sim: &mut Simulation) -> bool {
    let c = vec3(3000.0 + 72.2 / 2.0, 200.0 / 2.0 + 1.0, 0.0);
    let obb = OBB::new(c.xy(), -Vec2::X, 72.2, 200.0);

    let [offy, _] = obb.axis().map(|x| x.normalize().z(0.0));

    let pat = LanePatternBuilder::new().rail(true).build();

    let road = {
        let mut map = sim.map_mut();
        let ground = |pos: Vec3| {
            MapProject::ground(
                pos.xy()
                    .z(map.environment().height(pos.xy()).unwrap_or(pos.z)),
            )
        };
        let from = ground(c - offy * 100.0);
        let to = ground(c + offy * 120.0);
        map.make_connection(from, to, None, &pat)
    };

    let external_trading = sim.map_mut().build_special_building(
        &obb,
        BuildingKind::ExternalTrading,
        BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        },
        None,
        None,
    );

    if let Some(id) = external_trading {
        sim.write::<BuildingInfos>().insert(id);
        road.is_some_and(|(_, road)| sim.map().get(road).is_some())
    } else {
        log::error!("failed to build external trading");
        false
    }
}

impl FromIterator<WorldCommands> for WorldCommands {
    fn from_iter<T: IntoIterator<Item = WorldCommands>>(iter: T) -> Self {
        Self {
            commands: iter
                .into_iter()
                .flat_map(|x: WorldCommands| x.commands)
                .collect(),
        }
    }
}

impl From<Vec<WorldCommand>> for WorldCommands {
    fn from(commands: Vec<WorldCommand>) -> Self {
        Self { commands }
    }
}

impl From<&WorldCommands> for Vec<WorldCommand> {
    fn from(x: &WorldCommands) -> Self {
        x.commands.clone()
    }
}
