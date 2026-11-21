use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use prototypes::{try_prototype, Money, RollingStockID};
use serde::{Deserialize, Serialize};

use geom::{vec3, Vec2, Vec3, OBB};
use prototypes::BuildingGen;
use prototypes::GameTime;
use WorldCommand::*;

use crate::economy::{Budget, BudgetCategory, Government};
use crate::map::procgen::{load_parismap, load_testfield};
use crate::map::{
    Building, BuildingID, BuildingKind, Density, Environment, IntersectionID, LaneID, LaneKind,
    LanePattern, LanePatternBuilder, LightPolicy, LotID, Map, MapProject, ProjectKind, RoadID,
    TerraformKind, TurnPolicy, Zone, ZoneGrid, ZoneKind,
};
use crate::map_dynamic::{BuildingInfos, ParkingManagement};
use crate::multiplayer::chat::Message;
use crate::multiplayer::MultiplayerState;
use crate::souls::residence;
use crate::transportation::testing_vehicles::RandomVehicles;
use crate::transportation::train::{spawn_train, RailWagonKind};
use crate::transportation::{spawn_parked_vehicle_with_spot, unpark, VehicleKind};
use crate::utils::rand_provider::RandProvider;
use crate::{Replay, Simulation, SimulationOptions};

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
}

#[derive(Default)]
pub struct CommandOutcomes {
    /// Process-wide sequence of the most recent nonempty batch, including across world loads.
    pub sequence: u64,
    /// Rejections from the most recent nonempty batch, in command order.
    pub last: Vec<(&'static str, Rejection)>,
    /// Input indices of commands that completed without any rejection.
    pub accepted: Vec<usize>,
}

impl CommandOutcomes {
    pub(crate) fn begin_batch(&mut self) {
        static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
        self.sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.last.clear();
        self.accepted.clear();
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
        )
    }

    pub fn apply(&self, sim: &mut Simulation) {
        sim.apply_commands(std::iter::once(self));
    }

    pub(crate) fn apply_in_batch(&self, sim: &mut Simulation) {
        {
            let mut replay = sim.write::<Replay>();
            if replay.enabled {
                replay.push(sim.read::<GameTime>().tick, self.clone());
            }
        }

        let result = match self {
            MapMakeMultipleConnections(projects, links) => {
                self.apply_connections(sim, projects, links)
            }
            MapMakeConnection {
                from,
                to,
                inter,
                pat,
            } => Self::apply_connection(sim, *from, *to, *inter, pat),
            _ => self.apply_one(sim),
        };
        if let Err(reason) = result {
            self.reject(sim, reason);
        }
    }

    fn reject(&self, sim: &Simulation, reason: Rejection) {
        let kind = match self {
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
        };
        sim.write::<CommandOutcomes>().last.push((kind, reason));
    }

    fn validate(&self, map: &Map) -> Result<(), Rejection> {
        let exists = match self {
            MapRemoveIntersection(id) | MapUpdateIntersectionPolicy { inter: id, .. } => {
                map.get(*id).is_some()
            }
            MapRemoveRoad(id) => map.get(*id).is_some(),
            MapRemoveBuilding(id) => map.get(*id).is_some(),
            MapBuildHouse(id) => map
                .get(*id)
                .is_some_and(|lot| map.get(lot.parent).is_some()),
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
                        if id.is_service() && connected_road.is_none() {
                            return Err(Rejection::MissingReference);
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
                true
            }
            ZonePaint { area, .. } => {
                if !ZoneGrid::valid_paint(area, map.environment.bounds()) {
                    return Err(Rejection::InvalidZone);
                }
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

    fn charge_construction(sim: &Simulation, cost: Money) {
        if let Some(delta) = cost.inner().checked_neg() {
            Budget::apply(
                &sim.resources,
                BudgetCategory::Construction,
                Money::new_inner(delta),
            );
        } else {
            // A refund of -i64::MIN fits when the balance is negative, but needs two Money deltas.
            Budget::apply(&sim.resources, BudgetCategory::Construction, Money::MAX);
            Budget::apply(
                &sim.resources,
                BudgetCategory::Construction,
                Money::new_inner(1),
            );
        }
    }

    fn apply_connection(
        sim: &Simulation,
        from: MapProject,
        to: MapProject,
        inter: Option<Vec2>,
        pattern: &LanePattern,
    ) -> Result<(), Rejection> {
        let mut map = sim.map_mut();
        let from = map
            .resolve_connection_endpoint(from)
            .ok_or(Rejection::InvalidConnection)?;
        let to = map
            .resolve_connection_endpoint(to)
            .ok_or(Rejection::InvalidConnection)?;
        if !Map::can_connect(from, to, inter, pattern) {
            return Err(Rejection::InvalidConnection);
        }
        let cost =
            Self::affordable_cost(sim, Government::connection_cost(from.pos, to.pos, pattern))?;
        map.make_resolved_connection(from, to, inter, pattern);
        Self::charge_construction(sim, cost);
        Ok(())
    }

    fn apply_connections(
        &self,
        sim: &mut Simulation,
        projects: &[MapProject],
        links: &[(usize, usize, Option<Vec2>, LanePattern)],
    ) -> Result<(), Rejection> {
        if links
            .iter()
            .any(|&(from, to, _, _)| from >= projects.len() || to >= projects.len())
        {
            return Err(Rejection::InvalidBatchIndex);
        }

        let mut map = sim.map_mut();
        let mut endpoints: Vec<_> = projects
            .iter()
            .map(|&project| project.kind.check_valid(&map).then_some(project))
            .collect();
        let mut junctions = BTreeSet::new();
        let mut splits = BTreeMap::new();
        for &(from, to, interpoint, ref pat) in links {
            let resolve = |index: usize| {
                let project = endpoints[index]?;
                if let ProjectKind::Road(id) = project.kind {
                    if splits.contains_key(&id) {
                        return map.reproject_split_endpoint(id, project.pos, &splits);
                    }
                }
                map.resolve_connection_endpoint(project)
            };
            let Some((fromproj, toproj)) = resolve(from).zip(resolve(to)) else {
                self.reject(sim, Rejection::MissingReference);
                continue;
            };
            if !Map::can_connect(fromproj, toproj, interpoint, pat) {
                self.reject(sim, Rejection::InvalidConnection);
                continue;
            }
            let cost = match Self::affordable_cost(
                sim,
                Government::connection_cost(fromproj.pos, toproj.pos, pat),
            ) {
                Ok(cost) => cost,
                Err(reason) => {
                    self.reject(sim, reason);
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
            Self::charge_construction(sim, cost);
        }
        // Keep junction IDs alive until every link that names them has been processed.
        for junction in junctions {
            map.merge_shallow_junction(junction);
        }
        map.check_invariants();
        Ok(())
    }

    fn apply_one(&self, sim: &mut Simulation) -> Result<(), Rejection> {
        self.validate(&sim.map())?;
        let cost = Self::affordable_cost(sim, Government::checked_action_cost(self, sim))?;

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
            MapMakeConnection { .. } | MapMakeMultipleConnections(..) => unreachable!(),
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
            ZonePaint {
                area,
                kind,
                density,
            } => sim.write::<ZoneGrid>().paint(area, kind, density),
            SetTaxRate {
                zone,
                rate_permille,
            } => sim.write::<Budget>().set_tax_rate(zone, rate_permille),
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

            MapLoadParis => load_parismap(&mut sim.map_mut()),
            MapLoadTestField { pos, size, spacing } => {
                load_testfield(&mut sim.map_mut(), pos, size, spacing)
            }
            Init(ref opts) => {
                if opts.save_replay {
                    let mut rep = sim.resources.write::<Replay>();
                    rep.enabled = true;
                    let tick = sim.read::<GameTime>().tick;
                    rep.push(tick, Init(opts.clone()));
                }

                if opts.terrain_size > 0 {
                    generate_terrain(sim, opts.terrain_size);
                }

                sim.resources
                    .insert::<SimulationOptions>(SimulationOptions::clone(opts));
            }
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
        Self::charge_construction(sim, cost);
        Ok(())
    }
}

fn generate_terrain(sim: &mut Simulation, size: u16) {
    info!("generating terrain..");
    let t = Instant::now();

    sim.map_mut().environment = Environment::new(size, size);
    info!("took {}s", t.elapsed().as_secs_f32());

    let c = vec3(3000.0 + 72.2 / 2.0, 200.0 / 2.0 + 1.0, 0.0);
    let obb = OBB::new(c.xy(), -Vec2::X, 72.2, 200.0);

    let [offy, _] = obb.axis().map(|x| x.normalize().z(0.0));

    let pat = LanePatternBuilder::new().rail(true).build();

    sim.map_mut().make_connection(
        MapProject::ground(c - offy * 100.0),
        MapProject::ground(c + offy * 120.0),
        None,
        &pat,
    );

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
    } else {
        log::error!("failed to build external trading");
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
