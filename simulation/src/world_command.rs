use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use prototypes::{try_prototype, Money, RollingStockID};
use serde::{Deserialize, Serialize};

use geom::{vec3, Polygon, Vec2, Vec3, OBB};
use prototypes::BuildingGen;
use prototypes::GameTime;
use WorldCommand::*;

use crate::economy::cargo::{CargoBook, WarehousePolicy};
use crate::economy::progression::{can_build, BuildCapability, ProgressionState};
use crate::economy::{Budget, BudgetCategory, Government};
use crate::map::procgen::{load_parismap, load_testfield};
use crate::map::road_edit::{
    prepare_junction_edit, prepare_upgrade, EditReceipt, JunctionEditReceipt, JunctionEditStamp,
    PreparedJunctionEdit, PreparedRoadEdit, RoadEditStamp, RoadQuote, RoadUpgrade,
};
use crate::map::{
    Building, BuildingID, BuildingKind, ConnectionError, Density, Environment, IntersectionID,
    JunctionRuleError, JunctionRules, LandDenial, LandTileId, LaneID, LaneKind, LanePattern,
    LanePatternBuilder, LightPolicy, LotID, LotLayout, Map, MapProject, PreparedEffects,
    ProjectKind, RoadID, TerraformKind, TilePurchased, TurnPolicy, WaterEdit, WaterEditError,
    WaterEditOutcome, WaterTopologyWork, Zone, ZoneGrid, ZoneKind, ZonePaintError,
    MAX_WATER_BATCH_WORK,
};
use crate::map_dynamic::{transit, BuildingInfos, LineID, ParkingManagement, StopID};
use crate::multiplayer::chat::Message;
use crate::multiplayer::MultiplayerState;
use crate::souls::residence;
use crate::transportation::testing_vehicles::RandomVehicles;
use crate::transportation::train::{spawn_train, RailWagonKind};
use crate::transportation::{spawn_parked_vehicle_with_spot, unpark, VehicleKind};
use crate::utils::rand_provider::RandProvider;
use crate::{
    CompanyID, MapConstructor, MapOrigin, MapPreset, Replay, Simulation, SimulationOptions, SoulID,
};

pub(crate) const MAX_TILED_COMMANDS: usize = 256;
pub(crate) const MAX_WIRE_COMMANDS: usize = 65_536;
const MAX_WAGONS: usize = 256;
const MAX_LINE_STOPS: usize = 30;
const MAX_ZONE_VERTICES: usize = 256;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorldCommands {
    #[serde(deserialize_with = "bounded_frame_wire")]
    pub(crate) commands: Vec<WorldCommand>,
}

// Legacy construction DTOs retain their separate descriptor capacity.
fn bounded_batch_wire<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    bounded_command_vec::<_, _, MAX_WIRE_COMMANDS>(deserializer)
}

pub(crate) fn bounded_command_vec<'de, D, T, const LIMIT: usize>(
    deserializer: D,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    use serde::de::{DeserializeSeed, Error, SeqAccess, Visitor};
    struct RejectExtra;
    impl<'de> DeserializeSeed<'de> for RejectExtra {
        type Value = ();

        fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
            Err(D::Error::custom("command sequence exceeds its limit"))
        }
    }
    struct Bounded<T, const LIMIT: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for Bounded<T, LIMIT> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a sequence with at most {LIMIT} entries")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
            let hint = input.size_hint().unwrap_or(0);
            if hint > LIMIT {
                return Err(A::Error::invalid_length(hint, &self));
            }
            let mut entries = Vec::with_capacity(hint.min(256));
            while entries.len() < LIMIT {
                let Some(entry) = input.next_element()? else {
                    return Ok(entries);
                };
                entries.push(entry);
            }
            input.next_element_seed(RejectExtra)?;
            Ok(entries)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, LIMIT>(std::marker::PhantomData))
}

fn bounded_frame_wire<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<WorldCommand>, D::Error> {
    use serde::de::{DeserializeSeed, Error, SeqAccess, Visitor};

    struct Frame;
    impl<'de> Visitor<'de> for Frame {
        type Value = Vec<WorldCommand>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a command frame within the wire descriptor budget")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
            if let Some(count) = input
                .size_hint()
                .filter(|&count| count > MAX_TILED_COMMANDS)
            {
                return Err(A::Error::invalid_length(count, &self));
            }
            let mut commands = Vec::new();
            let mut remaining = MAX_WATER_BATCH_WORK;
            while commands.len() < MAX_TILED_COMMANDS && remaining > 0 {
                let Some(command): Option<WorldCommand> = input.next_element()? else {
                    return Ok(commands);
                };
                remaining = remaining
                    .checked_sub(command.wire_descriptors())
                    .ok_or_else(|| {
                        A::Error::custom("command frame exceeds its descriptor budget")
                    })?;
                commands.push(command);
            }
            struct Extra;
            impl<'de> DeserializeSeed<'de> for Extra {
                type Value = ();

                fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
                    Err(D::Error::custom(
                        "command frame exceeds its descriptor budget",
                    ))
                }
            }
            input.next_element_seed(Extra)?;
            Ok(commands)
        }
    }
    deserializer.deserialize_seq(Frame)
}

// Match the serialized Zone fields while bounding the polygon in command input.
#[derive(Deserialize)]
#[serde(rename = "Zone")]
struct CommandZone {
    #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_ZONE_VERTICES>")]
    poly: Vec<Vec2>,
    area: f32,
    #[serde(default = "Zone::default_fill_direction")]
    filldir: Vec2,
}

impl CommandZone {
    fn decode<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Zone, D::Error> {
        Self::deserialize(deserializer).map(Self::into_zone)
    }

    fn decode_optional<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Zone>, D::Error> {
        Option::<Self>::deserialize(deserializer).map(|zone| zone.map(Self::into_zone))
    }

    fn into_zone(self) -> Zone {
        Zone {
            poly: Polygon(self.poly),
            area: self.area,
            filldir: self.filldir,
        }
    }
}

// Keep the variant names, order, and fields shared by the live and replay decoders.
macro_rules! define_world_command {
    (
        $(#[$attr:meta])*
        $vis:vis enum $name:ident;
        zone = $zone:literal,
        optional_zone = $optional_zone:literal,
    ) => {
        $(#[$attr])*
        $vis enum $name {
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
                #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_WAGONS>")]
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
                #[serde(deserialize_with = "bounded_batch_wire")] Vec<MapProject>,
                #[serde(deserialize_with = "bounded_batch_wire")]
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
                #[serde(default, deserialize_with = $optional_zone)]
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
                #[serde(deserialize_with = $zone)]
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
                #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_LINE_STOPS>")]
                stops: Vec<StopID>,
            },
            TransitEditLine {
                line: LineID,
                #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_LINE_STOPS>")]
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
            BuyTile {
                tile: LandTileId,
            },
            MapUpgradeRoad {
                road: RoadID,
                expected: RoadEditStamp,
                change: RoadUpgrade,
                quote: RoadQuote,
            },
            MapSetJunctionRules {
                junction: IntersectionID,
                expected: JunctionEditStamp,
                rules: JunctionRules,
                quote: RoadQuote,
            },
        }
    };
}

define_world_command! {
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum WorldCommand;
    zone = "CommandZone::decode",
    optional_zone = "CommandZone::decode_optional",
}

define_world_command! {
    #[derive(Deserialize)]
    #[serde(remote = "WorldCommand")]
    pub(crate) enum ReplayWorldCommand;
    zone = "Deserialize::deserialize",
    optional_zone = "Deserialize::deserialize",
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    MissingReference,
    InvalidConnection,
    WaterConnection(ConnectionError),
    InvalidBatchIndex,
    InvalidBuilding,
    InvalidZone,
    InvalidTaxRate,
    UnsupportedDensity { kind: ZoneKind, density: Density },
    UnsupportedTax(ZoneKind),
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
    WaterServiceSite(crate::map::WaterSiteError),
    LandDenied(LandDenial),
    LandSetup(crate::map::LandSetupError),
    TileUnavailable,
    TileAlreadyOwned,
    TileNotAdjacent,
    InvalidCommandEncoding,
    BuildingOverlap,
    MixedUseUnavailable,
    InvalidRoadPattern,
    RoadWorkLimit,
    StaleRoadEdit,
    StaleRoadQuote,
    RoadTopologyUnavailable,
    InvalidJunctionRules(JunctionRuleError),
    BusyConflict,
}

impl From<LandDenial> for Rejection {
    fn from(reason: LandDenial) -> Self {
        Self::LandDenied(reason)
    }
}

impl From<ZonePaintError> for Rejection {
    fn from(error: ZonePaintError) -> Self {
        match error {
            ZonePaintError::InvalidArea => Self::InvalidZone,
            ZonePaintError::UnsupportedDensity { kind, density } => {
                Self::UnsupportedDensity { kind, density }
            }
            ZonePaintError::TileLocked(_) => Self::TileLocked,
            ZonePaintError::WetFootprint => Self::WetFootprint,
        }
    }
}

impl Rejection {
    fn water_error(error: &WaterEditError) -> Self {
        match error {
            WaterEditError::Limit => Self::WaterWorkLimit,
            WaterEditError::LandDenied(reason) => Self::LandDenied(*reason),
            WaterEditError::OccupiedFootprint { .. } => Self::WetFootprint,
            _ => Self::WaterEditFailed,
        }
    }

    fn water_connection_error(error: ConnectionError) -> Self {
        match error {
            ConnectionError::WorkLimit => Self::WaterWorkLimit,
            ConnectionError::LandDenied(reason) => Self::LandDenied(reason),
            ConnectionError::InvalidEndpoint => Self::InvalidConnection,
            other => Self::WaterConnection(other),
        }
    }
}

pub struct PrepareBudget {
    guard_terraform: bool,
    reject_surface: bool,
    shared_topology: bool,
    remaining: usize,
}

impl PrepareBudget {
    pub fn for_preview(work_units: usize) -> Self {
        Self {
            guard_terraform: false,
            reject_surface: false,
            shared_topology: false,
            remaining: work_units.min(MAX_WATER_BATCH_WORK),
        }
    }

    pub(crate) fn spend_road_work(&mut self, work: usize) -> Result<(), Rejection> {
        crate::map::spend_water_work(&mut self.remaining, work)
            .map_err(|_| Rejection::RoadWorkLimit)
    }

    pub(crate) fn road_work_remaining(&mut self) -> &mut usize {
        &mut self.remaining
    }

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
            shared_topology: commands.iter().any(|command| {
                matches!(command, MapUpgradeRoad { .. } | MapSetJunctionRules { .. })
            }),
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

#[derive(Clone, Debug)]
pub struct ActionPreview {
    pub cost: Money,
    pub road: Option<crate::map::RoadPreview>,
    pub paint: Option<crate::map::EligiblePaintCells>,
    pub water: Option<crate::map::WaterEditPreview>,
}

impl ActionPreview {
    fn priced(cost: Money) -> Self {
        Self {
            cost,
            road: None,
            paint: None,
            water: None,
        }
    }
}

pub(crate) enum PreparedAction<'a> {
    Purchase(TilePurchased, crate::map::land_frontage::PreparedFrontage),
    Paint(crate::map::EligiblePaintCells),
    Connection {
        from: crate::map::ConnectionEndpoint,
        to: crate::map::ConnectionEndpoint,
        elbow: Option<Vec2>,
        pattern: &'a LanePattern,
        topology: Option<crate::map::PreparedTopologyEdit>,
        work: Option<WaterTopologyWork>,
        cost: Money,
    },
    SpecialBuilding(crate::map::PreparedSpecialBuilding, Money),
    RoadEdit(PreparedRoadEdit),
    JunctionEdit(PreparedJunctionEdit),
    Command(&'a WorldCommand, Money),
}

impl PreparedAction<'_> {
    fn commit(self, sim: &mut Simulation, construction: BudgetCategory) -> Result<(), Rejection> {
        match self {
            Self::Purchase(purchase, frontage) => {
                sim.map_mut().buy_prepared_tile(purchase.tile);
                WorldCommand::charge_construction(sim, purchase.paid, BudgetCategory::LandPurchase);
                sim.map_mut().commit_land_frontage(frontage);
            }
            Self::Paint(cells) => sim.zone_grid_mut().paint_prepared(cells),
            Self::Connection {
                from,
                to,
                elbow,
                pattern,
                topology,
                mut work,
                cost,
            } => {
                sim.map_mut()
                    .make_resolved_connection(from, to, elbow, pattern, topology, work.as_mut())
                    .map_err(Rejection::water_connection_error)?;
                WorldCommand::charge_construction(sim, cost, construction);
            }
            Self::SpecialBuilding(prepared, cost) => {
                let id = sim.map_mut().commit_special_building(prepared);
                sim.write::<BuildingInfos>().insert(id);
                WorldCommand::charge_construction(sim, cost, construction);
            }
            Self::RoadEdit(prepared) => {
                let receipt = prepared.commit(sim)?;
                WorldCommand::charge_construction(sim, receipt.debit, construction);
                let mut outcomes = sim.write::<CommandOutcomes>();
                let index = outcomes.command_ranges.len();
                outcomes.road_edits.insert(index, Ok(receipt));
            }
            Self::JunctionEdit(prepared) => {
                let receipt = prepared.commit(sim)?;
                let mut outcomes = sim.write::<CommandOutcomes>();
                let index = outcomes.command_ranges.len();
                outcomes.junction_edits.insert(index, Ok(receipt));
            }
            Self::Command(command, cost) => command.apply_one(sim, construction, cost)?,
        }
        Ok(())
    }
}

pub type LinkOutcome = (usize, Result<(), (&'static str, Rejection)>);

/// Final live roads for each indexed link, and resolved project junctions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionBatchOutcome {
    pub links: Vec<Result<Vec<RoadID>, Rejection>>,
    pub projects: Vec<Option<IntersectionID>>,
}

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
    batch_rejection: Option<Rejection>,
    connections: BTreeMap<usize, ConnectionBatchOutcome>,
    tile_purchases: BTreeMap<usize, Result<TilePurchased, Rejection>>,
    water_edits: BTreeMap<usize, Result<WaterEditOutcome, WaterEditError>>,
    road_edits: BTreeMap<usize, Result<EditReceipt, Rejection>>,
    junction_edits: BTreeMap<usize, Result<JunctionEditReceipt, Rejection>>,
}

impl CommandOutcomes {
    pub(crate) fn reject_excess_purchase(&mut self, index: usize, reason: Rejection) {
        self.tile_purchases.insert(index, Err(reason));
    }

    pub(crate) fn reject_batch(&mut self, reason: Rejection) {
        self.batch_rejection = Some(reason);
    }

    pub fn batch_rejection(&self) -> Option<Rejection> {
        self.batch_rejection
    }

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

    pub fn connection_batch(&self, index: usize) -> Option<&ConnectionBatchOutcome> {
        self.connections.get(&index)
    }

    /// The result of an EditWater command at the given input index.
    pub fn water_edit(&self, index: usize) -> Option<&Result<WaterEditOutcome, WaterEditError>> {
        self.water_edits.get(&index)
    }

    pub fn tile_purchase(&self, index: usize) -> Option<Result<TilePurchased, Rejection>> {
        self.tile_purchases.get(&index).copied()
    }

    pub fn road_edit(&self, index: usize) -> Option<&Result<EditReceipt, Rejection>> {
        self.road_edits.get(&index)
    }

    pub fn junction_edit(&self, index: usize) -> Option<&Result<JunctionEditReceipt, Rejection>> {
        self.junction_edits.get(&index)
    }

    /// Discards the completed batch's outcomes while preserving its sequence.
    pub fn clear(&mut self) {
        self.last.clear();
        self.accepted.clear();
        self.batch_rejection = None;
        self.command_ranges.clear();
        self.links.clear();
        self.connections.clear();
        self.water_edits.clear();
        self.tile_purchases.clear();
        self.road_edits.clear();
        self.junction_edits.clear();
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

    pub fn append(&mut self, mut src: WorldCommands) {
        self.commands.append(&mut src.commands);
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
                | BuyTile { .. }
                | MapUpgradeRoad { .. }
                | MapSetJunctionRules { .. }
        )
    }

    pub fn apply(&self, sim: &mut Simulation) {
        sim.apply_commands(std::iter::once(self));
    }

    pub(crate) fn apply_in_batch(
        &self,
        sim: &mut Simulation,
        construction: BudgetCategory,
        water: &mut PrepareBudget,
    ) -> Option<Rejection> {
        if let EditWater(edit) = self {
            let outcome = if water.reject_surface {
                Err(WaterEditError::Limit)
            } else {
                sim.map_mut()
                    .apply_water_edit(edit, &mut sim.zone_grid_mut(), &mut water.remaining)
            };
            let rejection = outcome.as_ref().err().map(Rejection::water_error);
            if let Some(reason) = rejection {
                self.reject(sim, reason);
            }
            let mut outcomes = sim.write::<CommandOutcomes>();
            let index = outcomes.command_ranges.len();
            outcomes.water_edits.insert(index, outcome);
            return rejection;
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
                return Some(Rejection::WaterWorkLimit);
            }
            let tick = sim.read::<GameTime>().tick;
            let mut map = sim.map_mut();
            if map.needs_topology_preparation() {
                let result = map
                    .environment()
                    .prepare_water_terraform(tick, kind, center, radius, amount, level, slope)
                    .and_then(|prepared| {
                        map.commit_water_edit(
                            prepared,
                            &mut sim.zone_grid_mut(),
                            &mut water.remaining,
                        )
                    });
                let rejection = result.err().map(Self::terraform_error);
                if let Some(reason) = rejection {
                    self.reject(sim, reason);
                }
                return rejection;
            }
        }

        let result = if let MapMakeMultipleConnections(projects, links) = self {
            self.apply_connections(sim, projects, links, construction, water)
                .map(|_| ())
        } else {
            self.prepare_single(sim, water).and_then(|prepared| {
                let purchase = match &prepared {
                    PreparedAction::Purchase(purchase, _) => Some(*purchase),
                    _ => None,
                };
                prepared.commit(sim, construction)?;
                if let Some(purchase) = purchase {
                    let mut outcomes = sim.write::<CommandOutcomes>();
                    let index = outcomes.command_ranges.len();
                    outcomes.tile_purchases.insert(index, Ok(purchase));
                }
                Ok(())
            })
        };
        let rejection = result.err();
        if let Some(reason) = rejection {
            self.reject_unadmitted(sim, reason);
        }
        rejection
    }

    fn terraform_error(error: WaterEditError) -> Rejection {
        match error {
            WaterEditError::InvalidInput | WaterEditError::OutOfBounds => {
                Rejection::InvalidTerraform
            }
            WaterEditError::Limit => Rejection::WaterWorkLimit,
            WaterEditError::OccupiedFootprint { .. } => Rejection::WetFootprint,
            WaterEditError::LandDenied(reason) => Rejection::LandDenied(reason),
            _ => Rejection::WaterTerrainConflict,
        }
    }

    pub(crate) fn wire_descriptors(&self) -> usize {
        let lanes =
            |pattern: &LanePattern| pattern.lanes_forward.len() + pattern.lanes_backward.len();
        let nested = match self {
            MapMakeConnection { pat, .. } => lanes(pat),
            MapUpgradeRoad {
                change: RoadUpgrade::Replace(pat),
                ..
            } => lanes(pat),
            MapSetJunctionRules { rules, .. } => rules.approaches().len() + rules.forbidden().len(),
            MapMakeMultipleConnections(projects, links) => {
                projects.len()
                    + links.len()
                    + links.iter().map(|link| lanes(&link.3)).sum::<usize>()
            }
            MapBuildSpecialBuilding {
                zone: Some(zone), ..
            }
            | UpdateZone { zone, .. } => zone.poly.len(),
            SpawnTrain { wagons, .. } => wagons.len(),
            TransitCreateLine { stops } | TransitEditLine { stops, .. } => stops.len(),
            EditWater(WaterEdit::CarveRiver { polyline, .. }) => polyline.points().len(),
            EditWater(WaterEdit::FillLake { basin, .. }) => basin.points().len(),
            _ => 0,
        };
        1 + nested
    }

    pub(crate) fn wire_error(&self) -> Option<Result<Rejection, WaterEditError>> {
        if let EditWater(edit) = self {
            return crate::map::validate_water_edit_wire(edit).err().map(Err);
        }
        if matches!(self, MapMakeMultipleConnections(projects, links)
            if projects.len() > MAX_WIRE_COMMANDS || links.len() > MAX_WIRE_COMMANDS)
        {
            return Some(Ok(Rejection::WaterWorkLimit));
        }
        let finite_pattern = |pattern: &LanePattern| {
            pattern.lanes_forward.len() <= 32
                && pattern.lanes_backward.len() <= 32
                && pattern
                    .lanes_forward
                    .iter()
                    .chain(&pattern.lanes_backward)
                    .all(|(_, speed)| speed.is_finite())
        };
        let finite_zone = |zone: &Zone| {
            zone.poly.len() <= MAX_ZONE_VERTICES
                && zone.poly.as_slice().iter().all(|point| point.is_finite())
                && zone.area.is_finite()
        };
        let fits = match self {
            Terraform {
                center,
                radius,
                amount,
                level,
                slope,
                ..
            } => {
                let finite = center.is_finite()
                    && radius.is_finite()
                    && amount.is_finite()
                    && level.is_finite()
                    && slope.is_none_or(|(from, to)| from.is_finite() && to.is_finite());
                return (!finite).then_some(Ok(Rejection::InvalidTerraform));
            }
            MapMakeConnection {
                from,
                to,
                inter,
                pat,
            } => {
                from.pos.is_finite()
                    && to.pos.is_finite()
                    && inter.is_none_or(Vec2::is_finite)
                    && finite_pattern(pat)
            }
            MapUpgradeRoad {
                change: RoadUpgrade::Replace(pattern),
                ..
            } => {
                if pattern
                    .lanes_forward
                    .len()
                    .saturating_add(pattern.lanes_backward.len())
                    > 32
                {
                    return Some(Ok(Rejection::RoadWorkLimit));
                }
                // Finite domain and pattern validation run in the checked planner.
                finite_pattern(pattern)
            }
            MapMakeMultipleConnections(projects, links) => {
                projects.iter().all(|project| project.pos.is_finite())
                    && links.iter().all(|(_, _, elbow, pattern)| {
                        elbow.is_none_or(Vec2::is_finite) && finite_pattern(pattern)
                    })
            }
            MapUpdateIntersectionPolicy { turn, .. } => turn
                .roundabout
                .is_none_or(|policy| policy.radius.is_finite()),
            MapBuildSpecialBuilding {
                zone: Some(zone), ..
            }
            | UpdateZone { zone, .. }
                if !zone.filldir.is_finite() =>
            {
                return Some(Ok(Rejection::InvalidZone));
            }
            MapBuildSpecialBuilding { pos, gen, zone, .. } => {
                let finite_gen = match gen {
                    BuildingGen::House | BuildingGen::Farm => true,
                    BuildingGen::CenteredDoor { vertical_factor } => vertical_factor.is_finite(),
                    BuildingGen::NoWalkway { door_pos } => door_pos.is_finite(),
                };
                finite_gen
                    && pos.corners.iter().all(|point| point.is_finite())
                    && zone.as_ref().is_none_or(finite_zone)
            }
            MapLoadTestField { pos, spacing, .. } => pos.is_finite() && spacing.is_finite(),
            UpdateZone { zone, .. } => finite_zone(zone),
            SetGameTime(time) => time.timestamp.is_finite(),
            ZonePaint {
                area,
                kind,
                density,
            } => {
                let result = ZoneGrid::paint_replacement(*kind, *density)
                    .and_then(|_| ZoneGrid::validate_paint_area(area));
                return result.err().map(|error| Ok(error.into()));
            }
            SetTaxRate {
                zone,
                rate_permille,
            } => {
                return Self::validate_tax_rate(*zone, *rate_permille).err().map(Ok);
            }
            AddTrain { dist, .. } => dist.is_finite(),
            SpawnTrain { dist, wagons, .. } => dist.is_finite() && wagons.len() <= MAX_WAGONS,
            TransitAddStop { pos, dir } => pos.is_finite() && dir.is_finite(),
            TransitCreateLine { stops } | TransitEditLine { stops, .. } => {
                stops.len() <= MAX_LINE_STOPS
            }
            SendMessage { message } => {
                let color = message.color;
                [color.r, color.g, color.b, color.a]
                    .into_iter()
                    .all(f32::is_finite)
            }
            Init(_)
            | MapRemoveIntersection(_)
            | MapRemoveRoad(_)
            | MapRemoveBuilding(_)
            | MapBuildHouse(_)
            | SpawnRandomCars { .. }
            | MapLoadParis
            | TransitRemoveStop { .. }
            | TransitRemoveLine { .. }
            | TransitSetVehicles { .. }
            | TransitSetFare { .. }
            | SetWarehousePolicy { .. }
            | SetLotLayout(_)
            | MapSetJunctionRules { .. }
            | MapUpgradeRoad {
                change: RoadUpgrade::ReverseOneWay,
                ..
            }
            | BuyTile { .. } => true,
            EditWater(_) => unreachable!(),
        };
        (!fits).then_some(Ok(Rejection::InvalidCommandEncoding))
    }

    pub(crate) fn reject_wire(&self, sim: &Simulation, error: Result<Rejection, WaterEditError>) {
        match error {
            Ok(reason) => self.reject_unadmitted(sim, reason),
            Err(error) => {
                self.reject(sim, Rejection::water_error(&error));
                let mut outcomes = sim.write::<CommandOutcomes>();
                let index = outcomes.command_ranges.len();
                outcomes.water_edits.insert(index, Err(error));
            }
        }
    }

    pub(crate) fn admitted(&self, tiled: bool, water: &PrepareBudget) -> Result<(), Rejection> {
        if tiled {
            let fits = match self {
                MapMakeConnection { pat, .. } => crate::map::wet_lane_count_fits(pat),
                MapMakeMultipleConnections(projects, links) => {
                    projects.len() <= 512
                        && links.len() <= 256
                        && links
                            .iter()
                            .all(|(_, _, _, pattern)| crate::map::wet_lane_count_fits(pattern))
                }
                MapBuildSpecialBuilding {
                    zone: Some(zone), ..
                }
                | UpdateZone { zone, .. } => zone.poly.len() <= MAX_ZONE_VERTICES,
                SpawnTrain { wagons, .. } => wagons.len() <= MAX_WAGONS,
                TransitCreateLine { stops } | TransitEditLine { stops, .. } => {
                    stops.len() <= MAX_LINE_STOPS
                }
                _ => true,
            };
            return if fits {
                Ok(())
            } else {
                Err(LandDenial::WorkLimit.into())
            };
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
                return Err(Rejection::WaterWorkLimit);
            }
        }

        Ok(())
    }

    pub(crate) fn reject_unadmitted(&self, sim: &Simulation, reason: Rejection) {
        self.reject(sim, reason);
        if matches!(self, BuyTile { .. }) {
            let mut outcomes = sim.write::<CommandOutcomes>();
            let index = outcomes.command_ranges.len();
            outcomes.tile_purchases.insert(index, Err(reason));
        }
        if matches!(self, MapUpgradeRoad { .. }) {
            let mut outcomes = sim.write::<CommandOutcomes>();
            let index = outcomes.command_ranges.len();
            outcomes.road_edits.insert(index, Err(reason));
        }
        if matches!(self, MapSetJunctionRules { .. }) {
            let mut outcomes = sim.write::<CommandOutcomes>();
            let index = outcomes.command_ranges.len();
            outcomes.junction_edits.insert(index, Err(reason));
        }
        if matches!(self, EditWater(_)) {
            let mut outcomes = sim.write::<CommandOutcomes>();
            let index = outcomes.command_ranges.len();
            outcomes
                .water_edits
                .insert(index, Err(WaterEditError::Limit));
        }
    }

    pub(crate) fn reject(&self, sim: &Simulation, reason: Rejection) {
        sim.write::<CommandOutcomes>()
            .last
            .push((self.kind(), reason));
    }

    pub fn kind(&self) -> &'static str {
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
            BuyTile { .. } => "BuyTile",
            MapUpgradeRoad { .. } => "MapUpgradeRoad",
            MapSetJunctionRules { .. } => "MapSetJunctionRules",
        }
    }

    fn validate_tax_rate(zone: ZoneKind, rate_permille: u16) -> Result<(), Rejection> {
        if !zone.is_taxable() {
            return Err(Rejection::UnsupportedTax(zone));
        }
        if rate_permille > crate::economy::MAX_TAX_RATE_PERMILLE {
            return Err(Rejection::InvalidTaxRate);
        }
        Ok(())
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
            MapUpgradeRoad { road, .. } => map.get(*road).is_some(),
            MapSetJunctionRules { junction, .. } => map.get(*junction).is_some(),
            MapRemoveIntersection(id) | MapUpdateIntersectionPolicy { inter: id, .. } => {
                map.get(*id).is_some()
            }
            MapRemoveRoad(id) => map.get(*id).is_some(),
            MapRemoveBuilding(id) => {
                if map.get(*id).is_none() {
                    return Err(Rejection::MissingReference);
                }
                map.authorize_land_effects(PreparedEffects::Building(*id))?;
                map.authorize_building_removal(*id)?;
                true
            }
            MapLoadParis | MapLoadTestField { .. } | SpawnRandomCars { .. }
                if map.land_ownership().is_tiled() =>
            {
                return Err(LandDenial::ForbiddenOperation.into());
            }
            MapLoadParis | MapLoadTestField { .. } if map.has_junction_rules() => {
                return Err(Rejection::RoadTopologyUnavailable);
            }
            SetLotLayout(layout)
                if map.land_ownership().is_tiled() && *layout != map.lot_layout() =>
            {
                return Err(LandDenial::ForbiddenOperation.into());
            }
            SetWarehousePolicy { building, .. } => {
                if map.get(*building).is_none() {
                    return Err(Rejection::MissingReference);
                }
                map.authorize_land_effects(PreparedEffects::Building(*building))?;
                true
            }
            MapLoadParis | MapLoadTestField { .. }
                if map.environment().has_water_configuration() =>
            {
                return Err(Rejection::WaterConfigured);
            }
            MapBuildHouse(id) => {
                let lot = map.get(*id).ok_or(Rejection::MissingReference)?;
                map.authorize_land_effects(PreparedEffects::Lot(*id))?;
                map.authorize_new_building(&lot.shape, BuildingGen::House, None, Some(lot.parent))?;
                let height = map.environment().height(lot.shape.center()).unwrap_or(0.0);
                map.authorize_building_grade(lot.shape, height)?;
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
                if map.land_ownership().is_tiled() {
                    map.authorize_player_train(wagons, lane, *dist)?;
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
                if kind.is_mixed_growable() || !Building::valid_footprint(pos, *gen) {
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
                true
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
                map.authorize_zone_change(*building, zone)?;
                if !Building::dry_footprint(map.environment(), &b.obb, Some(zone)) {
                    return Err(Rejection::WetFootprint);
                }
                true
            }
            ZonePaint {
                area,
                kind,
                density,
            } => {
                ZoneGrid::eligible_paint_cells(map, area, *kind, *density)?;
                true
            }
            SetTaxRate {
                zone,
                rate_permille,
            } => {
                Self::validate_tax_rate(*zone, *rate_permille)?;
                true
            }
            Init(_)
            | BuyTile { .. }
            | MapMakeConnection { .. }
            | MapMakeMultipleConnections(..)
            | EditWater(_)
            | SendMessage { .. }
            | SpawnRandomCars { .. }
            | AddTrain { .. }
            | MapLoadParis
            | MapLoadTestField { .. }
            | SetGameTime(_)
            | SetLotLayout(_)
            | TransitAddStop { .. }
            | TransitRemoveStop { .. }
            | TransitCreateLine { .. }
            | TransitEditLine { .. }
            | TransitRemoveLine { .. }
            | TransitSetVehicles { .. }
            | TransitSetFare { .. } => true,
        };
        if exists {
            Ok(())
        } else {
            Err(Rejection::MissingReference)
        }
    }

    /// Run the validation and cost checks used before applying a special building.
    pub fn validate_and_price(&self, sim: &Simulation) -> Result<Money, Rejection> {
        Ok(self.preview(sim)?.cost)
    }

    pub fn preview(&self, sim: &Simulation) -> Result<ActionPreview, Rejection> {
        if let Some(error) = self.wire_error() {
            return Err(error.unwrap_or_else(|error| Rejection::water_error(&error)));
        }
        let map = sim.map();
        let mut budget = PrepareBudget::new(&[self], map.environment());
        self.admitted(map.land_ownership().is_tiled(), &budget)?;
        drop(map);
        match self {
            EditWater(edit) => {
                let water = sim
                    .preview_water_edit(edit)
                    .map_err(|error| Rejection::water_error(&error))?;
                Ok(ActionPreview {
                    water: Some(water),
                    ..ActionPreview::priced(Money::ZERO)
                })
            }
            Terraform {
                kind,
                center,
                radius,
                amount,
                level,
                slope,
            } if sim.map().needs_topology_preparation() => {
                let map = sim.map();
                let prepared = map
                    .environment()
                    .prepare_water_terraform(
                        sim.read::<GameTime>().tick,
                        *kind,
                        *center,
                        *radius,
                        *amount,
                        *level,
                        *slope,
                    )
                    .map_err(Self::terraform_error)?;
                let water = map
                    .preview_prepared_water(&prepared, &sim.read::<ZoneGrid>())
                    .map_err(Self::terraform_error)?;
                Ok(ActionPreview {
                    water: Some(water),
                    ..ActionPreview::priced(Money::ZERO)
                })
            }
            MapMakeMultipleConnections(..) => Err(Rejection::InvalidConnection),
            _ => match self.prepare_single(sim, &mut budget)? {
                PreparedAction::Purchase(purchase, _) => Ok(ActionPreview::priced(purchase.paid)),
                PreparedAction::Paint(paint) => Ok(ActionPreview {
                    paint: Some(paint),
                    ..ActionPreview::priced(Money::ZERO)
                }),
                PreparedAction::Connection {
                    topology: Some(topology),
                    cost,
                    ..
                } => Ok(ActionPreview {
                    road: Some(topology.preview),
                    ..ActionPreview::priced(cost)
                }),
                PreparedAction::Connection { cost, .. } => {
                    let MapMakeConnection {
                        from,
                        to,
                        inter,
                        pat,
                    } = self
                    else {
                        unreachable!()
                    };
                    let road = sim
                        .map()
                        .preview_connection(*from, *to, *inter, pat)
                        .map_err(Rejection::water_connection_error)?;
                    Ok(ActionPreview {
                        road: Some(road),
                        ..ActionPreview::priced(cost)
                    })
                }
                PreparedAction::RoadEdit(prepared) => {
                    Ok(ActionPreview::priced(prepared.preview.cost))
                }
                PreparedAction::JunctionEdit(prepared) => {
                    Ok(ActionPreview::priced(prepared.preview.cost))
                }
                PreparedAction::SpecialBuilding(_, cost) | PreparedAction::Command(_, cost) => {
                    Ok(ActionPreview::priced(cost))
                }
            },
        }
    }

    pub(crate) fn prepare_single<'a>(
        &'a self,
        sim: &Simulation,
        budget: &mut PrepareBudget,
    ) -> Result<PreparedAction<'a>, Rejection> {
        if let ZonePaint {
            kind: Some(ZoneKind::MixedUse),
            density,
            ..
        } = self
        {
            if prototypes::prototypes().mixed_use_pair(*density).is_none() {
                return Err(Rejection::MixedUseUnavailable);
            }
        }
        self.validate_progression(sim)?;
        match self {
            Init(_) => return Err(Rejection::UnexpectedInit),
            MapUpgradeRoad {
                road,
                expected,
                change,
                quote,
            } => {
                let prepared = prepare_upgrade(sim, *road, *expected, change, budget)?;
                if prepared.preview.quote != *quote {
                    return Err(Rejection::StaleRoadQuote);
                }
                return Ok(PreparedAction::RoadEdit(prepared));
            }
            MapSetJunctionRules {
                junction,
                expected,
                rules,
                quote,
            } => {
                let prepared = prepare_junction_edit(sim, *junction, *expected, rules, budget)?;
                if prepared.preview.quote != *quote {
                    return Err(Rejection::StaleRoadQuote);
                }
                return Ok(PreparedAction::JunctionEdit(prepared));
            }
            BuyTile { tile } => {
                let purchase = crate::map::prepare_purchase(sim, *tile)?;
                let frontage = sim
                    .map()
                    .prepare_purchase_frontage(*tile, &mut budget.remaining)?;
                return Ok(PreparedAction::Purchase(purchase, frontage));
            }
            ZonePaint {
                area,
                kind,
                density,
            } => {
                let cells = ZoneGrid::eligible_paint_cells(&sim.map(), area, *kind, *density)?;
                return Ok(PreparedAction::Paint(cells));
            }
            MapMakeConnection {
                from,
                to,
                inter,
                pat,
            } => {
                let map = sim.map();
                let shared_topology = map.land_ownership().is_tiled()
                    || budget.shared_topology
                    || map.checks_rail_sweeps(pat);
                let mut work = (shared_topology || map.needs_topology_preparation())
                    .then(WaterTopologyWork::new);
                if let Some(work) = &mut work {
                    if shared_topology {
                        work.remaining = budget.remaining;
                    }
                }
                let prepared = map.prepare_connection(*from, *to, *inter, pat, work.as_mut());
                if let Some(work) = &work {
                    if shared_topology {
                        budget.remaining = work.remaining;
                    }
                }
                let (from, to, topology) = prepared.map_err(Rejection::water_connection_error)?;
                let cost =
                    Self::affordable_cost(sim, Government::connection_cost(from.pos, to.pos, pat))?;
                return Ok(PreparedAction::Connection {
                    from,
                    to,
                    elbow: *inter,
                    pattern: pat,
                    topology,
                    work,
                    cost,
                });
            }
            _ => {}
        }
        let map = sim.map();
        self.validate(&map)?;
        if let MapBuildSpecialBuilding {
            pos,
            kind,
            gen,
            zone,
            connected_road,
        } = self
        {
            let building =
                map.prepare_special_building(*pos, *kind, *gen, zone.clone(), *connected_road)?;
            let cost = Self::affordable_cost(sim, Government::checked_action_cost(self, sim))?;
            return Ok(PreparedAction::SpecialBuilding(building, cost));
        }
        match self {
            MapRemoveRoad(id) => map
                .prepare_remove_road(*id)
                .map_err(Rejection::water_connection_error)?,
            MapRemoveIntersection(id) => map
                .prepare_remove_intersection(*id)
                .map_err(Rejection::water_connection_error)?,
            MapUpdateIntersectionPolicy { inter, turn, light } => map
                .prepare_intersection_policy(*inter, *turn, *light)
                .map_err(Rejection::water_connection_error)?,
            _ if transit::is_command(self) => transit::validate(sim, self)?,
            _ => {}
        }
        let cost = Self::affordable_cost(sim, Government::checked_action_cost(self, sim))?;
        Ok(PreparedAction::Command(self, cost))
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

    #[cfg(test)]
    pub(crate) fn apply_zone_paint(
        sim: &Simulation,
        area: &OBB,
        kind: Option<ZoneKind>,
        density: Density,
        owns: impl FnMut(crate::map::ZoneCellCoord) -> bool,
    ) -> Result<(), Rejection> {
        sim.zone_grid_mut()
            .paint_on_map_with_land(&sim.map(), area, kind, density, owns)
            .map_err(Rejection::from)
    }

    fn apply_connections(
        &self,
        sim: &mut Simulation,
        projects: &[MapProject],
        links: &[(usize, usize, Option<Vec2>, LanePattern)],
        construction: BudgetCategory,
        batch: &mut PrepareBudget,
    ) -> Result<bool, Rejection> {
        let tiled = sim.map().land_ownership().is_tiled();
        if tiled && (projects.len() > 512 || links.len() > 256) {
            return Err(LandDenial::WorkLimit.into());
        }
        if links
            .iter()
            .any(|&(from, to, _, _)| from >= projects.len() || to >= projects.len())
        {
            return Err(Rejection::InvalidBatchIndex);
        }

        let mut map = sim.map_mut();
        let shared_topology = tiled
            || batch.shared_topology
            || links
                .iter()
                .any(|(_, _, _, pattern)| map.checks_rail_sweeps(pattern));
        let mut water_work =
            (shared_topology || map.needs_topology_preparation()).then(WaterTopologyWork::new);
        if shared_topology {
            if let Some(work) = &mut water_work {
                work.remaining = batch.remaining;
            }
        }
        let mut endpoints: Vec<_> = projects
            .iter()
            .map(|&project| project.kind.check_valid(&map).then_some(project))
            .collect();
        let mut junctions = BTreeSet::new();
        let mut splits = BTreeMap::new();
        let mut connected_any = false;
        let mut details = vec![Err(Rejection::WaterWorkLimit); links.len()];
        let mut record_link = |index: usize, result: Result<RoadID, Rejection>| {
            details[index] = result;
            sim.write::<CommandOutcomes>().record_link(
                index,
                result.map(|_| ()).map_err(|reason| (self.kind(), reason)),
            );
        };
        for (index, &(from, to, interpoint, ref pat)) in links.iter().enumerate() {
            let mut resolve = |index: usize| {
                let Some(project) = endpoints[index] else { return Ok(None) };
                if let ProjectKind::Road(id) = project.kind {
                    if splits.contains_key(&id) {
                        return map.reproject_split_endpoint(
                            id,
                            project.pos,
                            &splits,
                            water_work.as_mut().map(|work| &mut work.remaining),
                        );
                    }
                }
                if let Some(work) = &mut water_work {
                    map.resolve_water_connection_endpoint(project, &mut work.remaining)
                } else {
                    Ok(map.resolve_connection_endpoint(project))
                }
            };
            let (fromproj, toproj) =
                match resolve(from).and_then(|start| Ok(start.zip(resolve(to)?))) {
                    Ok(Some(pair)) => pair,
                    Ok(None) => {
                        record_link(index, Err(Rejection::MissingReference));
                        continue;
                    }
                    Err(error) => {
                        record_link(index, Err(Rejection::water_connection_error(error.into())));
                        break;
                    }
                };
            if !Map::can_connect(fromproj, toproj, interpoint, pat) {
                record_link(index, Err(Rejection::InvalidConnection));
                continue;
            }
            let (profile_samples, prepared) = if let Some(work) = &mut water_work {
                match map.prepare_resolved_connection(fromproj, toproj, interpoint, pat, work) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let reason = Rejection::water_connection_error(error);
                        record_link(index, Err(reason));
                        if reason == Rejection::WaterWorkLimit && work.remaining == 0 {
                            break;
                        }
                        continue;
                    }
                }
            } else {
                (0, None)
            };
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
            if let Some(work) = &mut water_work {
                work.spend_profile_samples(profile_samples);
            }
            let connected =
                match map.commit_prepared_connection(fromproj, toproj, interpoint, pat, prepared) {
                    Ok(connected) => connected,
                    Err(error) => {
                        record_link(index, Err(Rejection::water_connection_error(error)));
                        continue;
                    }
                };
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
            record_link(index, Ok(connected.road));
        }
        // Keep junctions alive until every link naming them has been processed.
        let mut merged = BTreeMap::new();
        for junction in junctions {
            match map.merge_shallow_junction(junction, water_work.as_mut()) {
                Ok(Some((old, new))) => merged.extend(old.into_iter().map(|id| (id, new))),
                Err(_) if water_work.as_ref().is_some_and(|work| work.remaining == 0) => break,
                _ => {}
            }
        }
        let mut survivors = BTreeMap::new();
        let mut follow_merge = |mut road| {
            let mut path = Vec::new();
            while let Some(&next) = merged.get(&road) {
                if let Some(&known) = survivors.get(&road) {
                    road = known;
                    break;
                }
                path.push(road);
                road = next;
            }
            for id in path {
                survivors.insert(id, road);
            }
            road
        };
        let links = details
            .into_iter()
            .map(|result| {
                result.map(|id| {
                    let mut pending = vec![id];
                    let mut roads = BTreeSet::new();
                    while let Some(id) = pending.pop() {
                        if let Some(split) = splits.get(&id) {
                            pending.extend(split.roads);
                        } else {
                            let id = follow_merge(id);
                            if map.roads().contains_key(id) {
                                roads.insert(id);
                            }
                        }
                    }
                    roads.into_iter().collect()
                })
            })
            .collect();
        let projects = endpoints
            .into_iter()
            .map(|project| {
                let ProjectKind::Intersection(id) = project?.kind else {
                    return None;
                };
                map.intersections().contains_key(id).then_some(id)
            })
            .collect();
        let mut outcomes = sim.write::<CommandOutcomes>();
        let index = outcomes.command_ranges.len();
        outcomes
            .connections
            .insert(index, ConnectionBatchOutcome { links, projects });
        map.check_invariants();
        if shared_topology {
            if let Some(work) = water_work {
                batch.remaining = work.remaining;
            }
        }
        Ok(connected_any)
    }

    fn apply_one(
        &self,
        sim: &mut Simulation,
        construction: BudgetCategory,
        cost: Money,
    ) -> Result<(), Rejection> {
        match *self {
            MapRemoveIntersection(id) => sim
                .map_mut()
                .try_remove_intersection(id)
                .map_err(Rejection::water_connection_error)?,
            MapRemoveRoad(id) => {
                sim.map_mut()
                    .try_remove_road(id)
                    .map_err(Rejection::water_connection_error)?;
            }
            MapRemoveBuilding(id) => {
                if !crate::souls::lifecycle::remove_site(sim, id) {
                    return Err(Rejection::MutationFailed);
                }
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
            | MapUpgradeRoad { .. }
            | MapSetJunctionRules { .. }
            | EditWater(_)
            | BuyTile { .. }
            | ZonePaint { .. } => {
                unreachable!()
            }
            MapUpdateIntersectionPolicy {
                inter: id,
                turn: tp,
                light: lp,
            } => sim
                .map_mut()
                .try_update_intersection(id, move |i| {
                    i.light_policy = lp;
                    i.turn_policy = tp;
                })
                .map_err(Rejection::water_connection_error)?,
            MapBuildSpecialBuilding { .. } => unreachable!(),
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
        origin: MapOrigin,
    ) -> Result<(), Rejection> {
        let tick = sim.read::<GameTime>().tick;
        if tick.0 != crate::INITIAL_TICK || sim.resources.try_read::<SimulationOptions>().is_ok() {
            return Err(Rejection::UnexpectedInit);
        }
        if origin.validate_options(options).is_err() {
            return Err(Rejection::InvalidInitOptions);
        }
        info!("Initializing world with {options:?}");
        if options.terrain_size > 0 {
            generate_terrain(sim, options)?;
            if let crate::map::LandRecipe::NativeFlatV1 { .. } = options.land_recipe {
                crate::map::procgen::commission_native_land(sim, options)
                    .map_err(Rejection::LandSetup)?;
                sim.map()
                    .validate_commissioned_reserves(options.land_recipe)
                    .map_err(Rejection::LandSetup)?;
            } else if origin.constructor == MapConstructor::NewGame {
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
        if origin.constructor.is_authored() {
            sim.map_mut()
                .set_lot_layout(origin.initial_lot_layout.expect("validated authored layout"));
        }
        sim.resources.insert(options);
        sim.resources.insert(origin);
        Ok(())
    }
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

#[cfg(test)]
#[test]
fn p7_mixed_dry_connections_share_remaining_work_at_each_index() {
    use crate::map::road_edit::road_edit_stamp;
    use common::saveload::{Bincode, Encoder};
    drop(crate::tests::TestCtx::new());
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    let project = |x, y| MapProject::ground(vec3(x, y, 0.0));
    let pattern = LanePatternBuilder::new().build();
    let single = MapMakeConnection {
        from: project(100.0, 200.0),
        to: project(600.0, 200.0),
        inter: None,
        pat: pattern.clone(),
    };
    sim.apply_commands([&single]);
    let road = sim.map().roads().keys().next().unwrap();
    let edit = MapUpgradeRoad {
        road,
        expected: road_edit_stamp(&sim, road, &mut PrepareBudget::for_preview(1_000)).unwrap(),
        change: RoadUpgrade::Replace(pattern.clone()),
        quote: Bincode::decode(&Bincode::encode(&0u64).unwrap()).unwrap(),
    };
    let single = MapMakeConnection {
        from: project(100.0, 400.0),
        to: project(600.0, 400.0),
        inter: None,
        pat: pattern.clone(),
    };
    let projects = [project(100.0, 500.0), project(600.0, 500.0)];
    let links = [(0, 1, None, pattern.clone()), (0, 1, None, pattern)];
    let multiple = MapMakeMultipleConnections(projects.to_vec(), links.to_vec());
    let mut budget = PrepareBudget::new(&[&edit, &single, &multiple], sim.map().environment());
    assert!(!sim.map().needs_topology_preparation());
    budget.remaining = 1;
    let before = sim.hashes();
    let single_result = single.prepare_single(&sim, &mut budget).err();
    sim.write::<CommandOutcomes>().clear();
    multiple
        .apply_connections(
            &mut sim,
            &projects,
            &links,
            BudgetCategory::Construction,
            &mut budget,
        )
        .unwrap();
    assert_eq!(single_result, Some(Rejection::WaterWorkLimit));
    assert_eq!(
        sim.read::<CommandOutcomes>()
            .connection_batch(0)
            .unwrap()
            .links,
        [
            Err(Rejection::WaterWorkLimit),
            Err(Rejection::WaterWorkLimit)
        ]
    );
    assert_eq!(sim.hashes(), before);
}
