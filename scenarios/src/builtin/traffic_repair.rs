use crate::builders::{grant_money, place_company, CommandSink};
use crate::traffic_repair::comparison::{check_cancelled, load_snapshot, snapshot_bytes};
use crate::traffic_repair::{
    AbsoluteSchedule, ComparisonArm, ComparisonBatchOutcome, ComparisonError, ComparisonReceipt,
    ComparisonRequest, Observation, Terminal,
};
use crate::ScenarioCtx;
use geom::{vec2, Transform, Vec2, Vec3};
use prototypes::{ItemID, Money, RecTimeInterval};
use serde::Serialize;
use simulation::diagnostics::elapsed_tick;
use simulation::economy::cargo::{reserve_road_freight, CargoBook, ItemQuantity, RoadFreightTracking, ShipmentId};
use simulation::economy::Market;
use simulation::map::road_edit::{preview_road_upgrade, road_edit_stamp, RoadUpgrade};
use simulation::map::{
    BuildingID, LaneID, LaneKind, LanePatternBuilder, LightPolicy, ProjectFilter, RoadID,
    TraverseKind,
};
use simulation::map_dynamic::{BuildingInfos, Destination, Router};
use simulation::souls::desire::{Work, WorkKind};
use simulation::souls::human::{spawn_human, HumanDecision, HumanDecisionKind};
use simulation::transportation::{spawn_parked_vehicle, unpark, Location, VehicleKind};
use simulation::world_command::{CommandOutcomes, PrepareBudget, WorldCommand};
use simulation::{
    HumanID, ReplayAdvance, ReplayBudget, Simulation, SimulationReplayLoader, SnapshotRevision,
    SoulID, VehicleID,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureArm {
    Repair,
    NoLoss,
}

pub struct RepairFixture {
    pub simulation: Simulation,
    pub checkpoint: u64,
    pub repair_at: u64,
    pub primary_cutoff: u64,
    pub tail_cutoff: u64,
    pub road: RoadID,
    pub repair: WorldCommand,
    pub schedule: Vec<ScheduledOffer>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ScheduledOffer {
    pub tag: u32,
    pub tick: u64,
    pub group: &'static str,
    pub offer: FixtureOffer,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "cohort", rename_all = "snake_case")]
pub enum FixtureOffer {
    Person {
        human: HumanID,
        home: BuildingID,
        destination: BuildingID,
        launch: Option<PersonLaunch>,
        arrival_curb: Option<Vec3>,
    },
    Freight {
        sender: BuildingID,
        receiver: BuildingID,
        item: ItemID,
        units: u64,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PersonLaunch {
    lane: LaneID,
    offset_m: f32,
}

/// Builds the checkpoint and freezes offers by semantic corridor and facility.
/// Releases are consumed by the matched runner after both arms stage the checkpoint.
pub fn prepare_repair_fixture(arm: FixtureArm) -> Result<RepairFixture, String> {
    ScenarioCtx::initialize();
    prepare_repair_fixture_initialized(arm)
}

pub(crate) fn prepare_repair_fixture_initialized(arm: FixtureArm) -> Result<RepairFixture, String> {
    const PEOPLE: usize = 120;
    const LOADS: usize = 24;
    const FLEET: usize = LOADS / 2;
    let sim = Simulation::new_authored_flat(4, 6042).map_err(|error| error.to_string())?;
    let mut ctx = ScenarioCtx::from_sim(sim);
    grant_money(&mut ctx, Money::new_bucks(850_000));
    let origin = vec2(400.0, 520.0);
    build_corridor(&mut ctx, origin)?;
    let repair_road = {
        let map = ctx.sim.map();
        let center = origin + vec2(730.0, 230.0);
        map.roads()
            .keys()
            .find(|id| {
                map.roads()[*id]
                    .points()
                    .middle()
                    .xy()
                    .is_close(center, 1.0)
            })
            .ok_or("repair fixture is missing its middle link")?
    };
    let starting_speed = match arm {
        FixtureArm::Repair => 4.0,
        FixtureArm::NoLoss => 38.0,
    };
    let setup = speed_edit(&ctx.sim, repair_road, starting_speed)?;
    ctx.sim.apply_commands([&setup]);
    if ctx.sim.read::<CommandOutcomes>().accepted != [0] {
        return Err("repair fixture could not set the opening speed".into());
    }

    // Each directional freight pair parks on its own side of the straight
    // approaches, so the outbound delivery does not start with a block loop.
    let endpoint = |ctx: &mut ScenarioCtx, at| {
        place_company(ctx, "large-warehouse", origin + at)
            .ok_or_else(|| format!("repair fixture could not place a warehouse near {at:?}"))
    };
    let west_out = endpoint(&mut ctx, vec2(545.0, 172.5))?;
    let east_in = endpoint(&mut ctx, vec2(915.0, 172.5))?;
    let east_out = endpoint(&mut ctx, vec2(915.0, 287.5))?;
    let west_in = endpoint(&mut ctx, vec2(545.0, 287.5))?;
    let mut destinations = [[west_in; 3], [east_in; 3]];
    for (side, index, at) in [
        (0, 0, vec2(440.0, 270.0)),
        (0, 1, vec2(325.0, 270.0)),
        (0, 2, vec2(245.0, 270.0)),
        (1, 0, vec2(1020.0, 190.0)),
        (1, 1, vec2(1135.0, 190.0)),
        (1, 2, vec2(1215.0, 190.0)),
    ] {
        destinations[side][index] = place_company(&mut ctx, "small-depot", origin + at)
            .ok_or_else(|| format!("repair fixture could not place a depot near {at:?}"))?;
    }

    let mut lots_by_side = [Vec::new(), Vec::new()];
    {
        let map = ctx.sim.map();
        for (id, lot) in map.lots() {
            let road = &map.roads()[lot.parent];
            let offset = road.points().middle().xy() - origin;
            if offset.x <= 1.0 || offset.x >= 1459.0 {
                let side = usize::from(offset.x > 730.0);
                lots_by_side[side].push((id, offset.y));
            }
        }
    }
    let residents_per_side = PEOPLE / 2 + FLEET;
    let wanted = residents_per_side / 2;
    let mut commands = Vec::with_capacity(wanted * 2);
    for lots in &mut lots_by_side {
        lots.sort_by(|&(a, ad), &(b, bd)| ad.total_cmp(&bd).then(a.cmp(&b)));
        if lots.len() < wanted {
            return Err(format!(
                "repair fixture needs {wanted} homes on each side, found {}",
                lots.len()
            ));
        }
        commands.extend(
            lots.iter()
                .take(wanted)
                .map(|&(id, _)| WorldCommand::MapBuildHouse(id)),
        );
    }
    let existing: BTreeSet<_> = ctx.sim.map().buildings().keys().collect();
    ctx.submit(&commands);
    ctx.flush();
    let mut houses = [Vec::new(), Vec::new()];
    {
        let map = ctx.sim.map();
        for (id, building) in map.buildings() {
            if !existing.contains(&id) {
                let road = building.connected_road.ok_or("fixture house has no road")?;
                let side = usize::from(map.roads()[road].points().middle().x > origin.x + 730.0);
                houses[side].push(id);
            }
        }
    }
    let mut people = [Vec::new(), Vec::new()];
    for (side, (homes, depot)) in houses.iter_mut().zip([west_out, east_out]).enumerate() {
        if homes.len() != wanted {
            return Err(format!(
                "repair fixture built {} homes on side {side}, expected {wanted}",
                homes.len()
            ));
        }
        homes.sort_unstable();
        let mut driver = 0;
        for &home in homes.iter() {
            let mut residents: Vec<_> = ctx
                .sim
                .read::<BuildingInfos>()
                .get(home)
                .ok_or("fixture house has no building info")?
                .residents
                .iter()
                .filter_map(|soul| match *soul {
                    SoulID::Human(human) => Some(human),
                    _ => None,
                })
                .collect();
            while residents.len() < 2 {
                residents.push(
                    spawn_human(&mut ctx.sim, home)
                        .ok_or("fixture needs a second resident at each home")?,
                );
            }
            for human in residents {
                ctx.sim
                    .world_mut_unchecked()
                    .edit_human(human, |human| human.pedestrian.walking_speed = 1.6);
                if people[side].len() == PEOPLE / 2 {
                    assign_fixture_driver(&mut ctx.sim, human, home, depot, driver)?;
                    driver += 1;
                    continue;
                }
                let distance = 105.0 + people[side].len() as f32 * 6.75;
                let at = if side == 0 {
                    origin + vec2(600.0 - distance, 224.75)
                } else {
                    origin + vec2(860.0 + distance, 235.25)
                };
                let car = spawn_parked_vehicle(&mut ctx.sim, VehicleKind::Car, at.z(10.25))
                    .ok_or("fixture needs one personal car in each departure bay")?;
                ctx.sim
                    .world_mut_unchecked()
                    .edit_human(human, |human| human.router = Router::new(Some(car)));
                people[side].push((human, home));
            }
        }
        if driver != FLEET {
            return Err(format!(
                "repair fixture staged {driver} drivers, expected {FLEET}"
            ));
        }
    }

    // Launch from the front of each departure curb first. Later releases
    // enter behind earlier cars, away from their advancing queue.
    let checkpoint = elapsed_tick(&ctx.sim).0;
    let directed = [
        (0, "west_to_east_near_curb", destinations[1][0]),
        (1, "east_to_west_near_curb", destinations[0][0]),
        (0, "west_to_east_middle_curb", destinations[1][1]),
        (1, "east_to_west_middle_curb", destinations[0][1]),
        (0, "west_to_east_far_curb", destinations[1][2]),
        (1, "east_to_west_far_curb", destinations[0][2]),
    ];
    let departure_lanes = {
        let map = ctx.sim.map();
        let mut directions = [Vec::new(), Vec::new()];
        for (side, x) in [300.0, 1160.0].into_iter().enumerate() {
            let approach = map
                .roads()
                .values()
                .find(|road| {
                    road.points()
                        .middle()
                        .xy()
                        .is_close(origin + vec2(x, 230.0), 1.0)
                })
                .ok_or("repair fixture has no departure approach")?;
            directions[side].extend(approach.lanes_iter().filter_map(|(id, kind)| {
                let lane = &map.lanes()[id];
                (kind == LaneKind::Driving
                    && (lane.points.first().x < lane.points.last().x) == (side == 0))
                    .then_some((id, lane.points.length()))
            }));
            if directions[side].len() != 3 {
                return Err("repair fixture needs three directed launch lanes".into());
            }
        }
        directions
    };
    let arrival_curbs = {
        let map = ctx.sim.map();
        let mut used = BTreeSet::new();
        let mut by_group = Vec::new();
        for &(side, _, destination) in &directed {
            let building = &map.buildings()[destination];
            let mut spots: Vec<_> = map
                .parking
                .all_spots()
                .filter(|(id, spot)| {
                    map.get(spot.parent)
                        .is_some_and(|lane| Some(lane.parent) == building.connected_road)
                        && (spot.trans.dir.x > 0.0) == (side == 0)
                        && !used.contains(id)
                })
                .collect();
            spots.sort_by(|(a_id, a), (b_id, b)| {
                let distance = |position: Vec3| position.distance2(building.door_pos);
                distance(a.trans.pos)
                    .total_cmp(&distance(b.trans.pos))
                    .then(a_id.cmp(b_id))
            });
            if spots.len() < PEOPLE / directed.len() {
                return Err("repair fixture lacks dedicated arrival curb space".into());
            }
            let mut curbs: Vec<_> = spots
                .into_iter()
                .take(PEOPLE / directed.len())
                .map(|(id, spot)| {
                    used.insert(id);
                    spot.trans.pos + spot.trans.dir.cross(Vec3::Z) * 2.0
                })
                .collect();
            // Later offers have less travel time before the common cutoff.
            curbs.sort_by(|a, b| {
                if side == 0 {
                    b.x.total_cmp(&a.x)
                } else {
                    a.x.total_cmp(&b.x)
                }
            });
            by_group.push(curbs);
        }
        by_group
    };
    let mut schedule = Vec::with_capacity(PEOPLE + LOADS);
    for i in 0..PEOPLE {
        let distance_group = 2 - i % directed.len() / 2;
        let route = 2 * distance_group + i % 2;
        let (region, group, destination) = directed[route];
        let (human, home) = people[region][i / 2];
        schedule.push(ScheduledOffer {
            tag: i as u32 + 1,
            tick: checkpoint + 100 + i as u64 * 50,
            group,
            offer: FixtureOffer::Person {
                human,
                home,
                destination,
                arrival_curb: Some(arrival_curbs[route][i / directed.len()]),
                launch: {
                    let (lane, length) = departure_lanes[region][distance_group];
                    let first = [220.0, 125.0, 45.0][distance_group];
                    Some(PersonLaunch {
                        lane,
                        offset_m: length - first - (i / 6) as f32 * 5.5,
                    })
                },
            },
        });
    }
    let pairs = [
        ("west_warehouse_to_east_warehouse", west_out, east_in),
        ("east_warehouse_to_west_warehouse", east_out, west_in),
    ];
    for i in 0..LOADS {
        let (group, sender, receiver) = pairs[i % pairs.len()];
        schedule.push(ScheduledOffer {
            tag: (PEOPLE + i + 1) as u32,
            tick: checkpoint + 100 + i as u64 * 250,
            group,
            offer: FixtureOffer::Freight {
                sender,
                receiver,
                item: ItemID::new("coal"),
                units: 8,
            },
        });
    }
    schedule.sort_by_key(|offer| (offer.tick, offer.tag));
    let repair = speed_edit(&ctx.sim, repair_road, 40.0)?;
    Ok(RepairFixture {
        simulation: ctx.sim,
        checkpoint,
        repair_at: checkpoint + 1,
        primary_cutoff: checkpoint + 10_600,
        tail_cutoff: checkpoint + 144_000,
        road: repair_road,
        repair,
        schedule,
    })
}

fn build_corridor(ctx: &mut ScenarioCtx, origin: Vec2) -> Result<(), String> {
    let west = vec2(600.0, 230.0);
    let east = vec2(860.0, 230.0);
    let left = vec2(0.0, 230.0);
    let right = vec2(1460.0, 230.0);
    let bypass_west = west - vec2(0.0, 155.0);
    let bypass_east = east - vec2(0.0, 155.0);
    let roads = [
        (west, east),
        (west, bypass_west),
        (bypass_west, bypass_east),
        (bypass_east, east),
        (left, west),
        (east, right),
        (left, left + vec2(0.0, 640.0)),
        (left, left - vec2(0.0, 210.0)),
        (right, right + vec2(0.0, 640.0)),
        (right, right - vec2(0.0, 210.0)),
    ];
    for (index, (a, b)) in roads.into_iter().enumerate() {
        let lanes = if matches!(index, 0 | 4 | 5) { 3 } else { 1 };
        let speed = if matches!(index, 4 | 5) { 38.0 } else { 26.0 };
        let pattern = LanePatternBuilder::new()
            .n_lanes(lanes)
            .speed_limit(speed)
            .build();
        let (from, to) = {
            let map = ctx.sim.map();
            let project = |offset| {
                let xy: Vec2 = origin + offset;
                let z = map.environment().height(xy).unwrap_or_default();
                map.project(xy.z(z), 0.0, ProjectFilter::INTER | ProjectFilter::ROAD)
            };
            (project(a), project(b))
        };
        ctx.submit(&[WorldCommand::MapMakeConnection {
            from,
            to,
            inter: None,
            pat: pattern.clone(),
        }]);
        ctx.flush();
        if ctx.sim.read::<CommandOutcomes>().accepted != [0] {
            return Err(format!("repair fixture rejected road {a:?} to {b:?}"));
        }
    }
    let policies: Vec<_> = ctx
        .sim
        .map()
        .intersections()
        .values()
        .map(|inter| WorldCommand::MapUpdateIntersectionPolicy {
            inter: inter.id,
            light: LightPolicy::NoLights,
            turn: inter.turn_policy,
        })
        .collect();
    ctx.sim.apply_commands(&policies);
    if ctx.sim.read::<CommandOutcomes>().accepted.len() != policies.len() {
        return Err("repair fixture could not set its junction policies".into());
    }
    Ok(())
}

fn speed_edit(sim: &Simulation, road: RoadID, speed: f32) -> Result<WorldCommand, String> {
    let mut budget = PrepareBudget::for_preview(4_194_304);
    let expected = road_edit_stamp(sim, road, &mut budget)
        .map_err(|error| format!("road stamp: {error:?}"))?;
    let change = RoadUpgrade::Replace(
        LanePatternBuilder::new()
            .n_lanes(3)
            .speed_limit(speed)
            .build(),
    );
    let preview = preview_road_upgrade(sim, road, expected, &change, &mut budget)
        .map_err(|error| format!("road quote: {error:?}"))?;
    Ok(WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        quote: preview.quote,
    })
}

fn assign_fixture_driver(
    sim: &mut Simulation,
    driver: HumanID,
    home: BuildingID,
    depot: BuildingID,
    truck_index: usize,
) -> Result<(), String> {
    let Some(SoulID::GoodsCompany(company)) = sim.read::<BuildingInfos>().owner(depot) else {
        return Err("depot has no operating company".into());
    };
    let door = sim.map().buildings()[depot].door_pos;
    let truck = if let Some(&vehicle) = sim.world().companies[company].comp.trucks.get(truck_index)
    {
        vehicle
    } else {
        // One stock truck per offered shipment removes fleet turnaround as
        // a competing bottleneck in this authored traffic comparison.
        let vehicle = spawn_parked_vehicle(sim, VehicleKind::Truck, door)
            .ok_or("repair fixture ran out of freight parking")?;
        sim.world_mut_unchecked().companies[company]
            .comp
            .trucks
            .push(vehicle);
        vehicle
    };
    let world = sim.world_mut_unchecked();
    world.companies[company].workers.0.push(driver);
    let mut work = Work::new(
        depot,
        WorkKind::Driver {
            truck,
            deliver_order: None,
            load: None,
        },
        0.0,
    );
    work.work_inter = RecTimeInterval::always();
    world.edit_human(driver, |human| {
        human.location = Location::Building(depot);
        human.trans.pos = door;
        human.work = Some(work);
    });
    let mut infos = sim.write::<BuildingInfos>();
    infos.get_out(home, SoulID::Human(driver));
    infos.get_in(depot, SoulID::Human(driver));
    Ok(())
}

/// Replays the pinned offers on a fresh copy of the checkpoint. Keep movement,
/// routing and cargo on the stock schedule while supplying only authored orders.
pub fn run_no_loss_arm(
    sim: Simulation,
    fixture: &RepairFixture,
    treatment: bool,
) -> Result<Vec<Observation>, String> {
    if elapsed_tick(&sim).0 != fixture.checkpoint {
        return Err("snapshot is at the wrong elapsed tick".into());
    }
    let inputs = AbsoluteSchedule {
        scheduled_offers: &fixture.schedule,
        scoring_offers: &[],
        batches: &[],
        primary_cutoff: fixture.primary_cutoff,
        tail_cutoff: fixture.tail_cutoff,
    };
    let request = ComparisonRequest {
        repair_at: fixture.repair_at,
        repair: &fixture.repair,
        resume_at: None,
        cancelled: &AtomicBool::new(false),
    };
    run_repair_arm(sim, &inputs, request, treatment, true, false)
        .map(|(arm, _)| arm.observations)
        .map_err(|err| err.to_string())
}

pub(crate) fn run_repair_arm(
    mut sim: Simulation,
    inputs: &AbsoluteSchedule<'_>,
    request: ComparisonRequest<'_>,
    treatment: bool,
    authored: bool,
    replay: bool,
) -> Result<(ComparisonArm, Option<ComparisonReceipt>), ComparisonError> {
    let checkpoint = elapsed_tick(&sim).0;
    let mut schedule = Simulation::schedule();
    if authored {
        schedule.retain_systems(|name| {
            !matches!(
                name,
                "company_system" | "market_update" | "residence_update" | "growth_update"
            )
        });
    }
    let mut playback = replay.then(|| {
        let mut batches: Vec<_> = inputs.batches.iter()
            .map(|batch| (batch.tick, batch.advances, batch.commands.to_vec()))
            .collect();
        if treatment {
            if let Some((_, _, commands)) = batches.iter_mut()
                .find(|(tick, advances, _)| *tick == request.repair_at && *advances)
            {
                commands.push(request.repair.clone());
            } else {
                let index = batches.partition_point(|(tick, _, _)| *tick <= request.repair_at);
                batches.insert(index, (request.repair_at, true, vec![request.repair.clone()]));
            }
        }
        SimulationReplayLoader::checkpoint_playback(
            prototypes::Tick(sim.get_tick()), SnapshotRevision::new(1),
            inputs.tail_cutoff - checkpoint,
            batches.iter().map(|(tick, advances, commands)| {
                (tick - checkpoint - 1, *advances, commands.as_slice())
            }),
        ).map_err(|err| ComparisonError::Setup(err.to_string()))
    }).transpose()?;
    let mut replay_budget = ReplayBudget::new((inputs.tail_cutoff - checkpoint) as usize);
    let mut reports: Vec<_> = inputs
        .scheduled_offers
        .iter()
        .map(|entry| Observation {
            tag: entry.tag,
            admitted_at: None,
            terminal: None,
        })
        .collect();
    let mut pending_people = BTreeMap::<usize, (HumanID, BuildingID, Option<Vec3>)>::new();
    let mut freight = BTreeMap::<ShipmentId, (usize, BuildingID, u32)>::new();
    let repaired_road = match request.repair {
        WorldCommand::MapUpgradeRoad { road, .. } => Some(*road),
        _ => None,
    };
    let mut repaired_road_tags = BTreeSet::new();
    let mut next_offer = 0;
    let mut batches = inputs.batches.iter().peekable();
    let mut batch_outcomes = Vec::new();
    let mut receipt = None;
    for tick in (checkpoint + 1)..=inputs.tail_cutoff {
        check_cancelled(request.cancelled)?;
        while batches
            .peek()
            .is_some_and(|batch| batch.tick == tick && !batch.advances)
        {
            let batch = batches.next().unwrap();
            if let Some(playback) = &mut playback {
                if !matches!(playback.advance_one(&mut sim, &mut schedule, &mut replay_budget),
                    ReplayAdvance::Operation { advanced: false, .. })
                {
                    return Err(ComparisonError::InvalidSchedule("expected paused replay batch"));
                }
            } else {
                sim.apply_commands(batch.commands);
            }
            batch_outcomes.push(ComparisonBatchOutcome::read(
                tick,
                false,
                batch.commands.len(),
                &sim,
            ));
        }
        while let Some(entry) = inputs
            .scheduled_offers
            .get(next_offer)
            .filter(|entry| entry.tick == tick)
        {
            match entry.offer {
                FixtureOffer::Person {
                    human,
                    home,
                    destination,
                    launch,
                    arrival_curb,
                } => {
                    let staged_car = if let Some(launch) = launch {
                        if sim
                            .world()
                            .humans
                            .get(human)
                            .is_none_or(|resident| resident.location != Location::Building(home))
                        {
                            return Err(
                                format!("tag {} left home before its release", entry.tag).into()
                            );
                        }
                        let car = sim.world().humans[human]
                            .router
                            .personal_car
                            .ok_or("missing staged personal car")?;
                        let (position, direction) = {
                            let map = sim.map();
                            let lane =
                                map.get(launch.lane).ok_or("missing directed launch lane")?;
                            if lane.kind != LaneKind::Driving
                                || !(0.0..=lane.points.length()).contains(&launch.offset_m)
                            {
                                return Err("invalid directed launch anchor".into());
                            }
                            lane.points.point_dir_along(launch.offset_m)
                        };
                        if !sim
                            .world()
                            .vehicles
                            .get(car)
                            .is_some_and(|vehicle| vehicle.vehicle.state.is_parked())
                        {
                            return Err(
                                format!("tag {} car was not parked at release", entry.tag).into()
                            );
                        }
                        unpark(&mut sim, car);
                        let vehicle = sim
                            .world_mut_unchecked()
                            .vehicles
                            .get_mut(car)
                            .ok_or("missing staged car")?;
                        // Both branches enter at the same directed pose and
                        // speed. The regular router finishes the trip.
                        vehicle.trans = Transform::new_dir(position, direction);
                        vehicle.speed.0 = 20.0;
                        Some((car, position))
                    } else {
                        None
                    };
                    sim.world_mut_unchecked()
                        .edit_human(human, |human| {
                            if human.location != Location::Building(home) {
                                return Err(format!(
                                    "tag {} left home before its release",
                                    entry.tag
                                ));
                            }
                            if let Some((vehicle, position)) = staged_car {
                                human.location = Location::Vehicle(vehicle);
                                human.trans.pos = position;
                            }
                            human.decision = HumanDecision {
                                kind: HumanDecisionKind::GoTo(arrival_curb.map_or(
                                    Destination::Building(destination),
                                    Destination::Outside,
                                )),
                                wait: 0,
                            };
                            Ok(())
                        })
                        .ok_or("missing tagged resident")??;
                    if staged_car.is_some() {
                        sim.write::<BuildingInfos>()
                            .get_out(home, SoulID::Human(human));
                    }
                    pending_people.insert(next_offer, (human, destination, arrival_curb));
                }
                FixtureOffer::Freight {
                    sender,
                    receiver,
                    item,
                    units,
                } => {
                    let units = u32::try_from(units)
                        .ok()
                        .filter(|units| (1..=i32::MAX as u32).contains(units))
                        .ok_or("invalid freight quantity")?;
                    if authored {
                        let source = sim
                            .read::<BuildingInfos>()
                            .owner(sender)
                            .ok_or("missing sender")?;
                        sim.write::<Market>().produce(source, item, units as i32);
                    }
                    let reservation = reserve_road_freight(
                        &mut sim,
                        sender,
                        receiver,
                        ItemQuantity { item, units },
                    );
                    if let Some(id) = reservation {
                        freight.insert(id, (next_offer, receiver, units));
                    } else {
                        reports[next_offer].terminal = Some(Terminal::Rejected { tick });
                        next_offer += 1;
                        continue;
                    }
                }
            }
            reports[next_offer].admitted_at = Some(tick);
            next_offer += 1;
        }

        let commands = match batches.peek() {
            Some(batch) if batch.tick == tick => batches.next().unwrap().commands,
            _ => &[],
        };
        let candidate = (treatment && tick == request.repair_at).then_some(request.repair);
        if let Some(playback) = &mut playback {
            if !matches!(playback.advance_one(&mut sim, &mut schedule, &mut replay_budget),
                ReplayAdvance::Operation { advanced: true, .. })
            {
                return Err(ComparisonError::InvalidSchedule("expected advancing replay step"));
            }
        } else {
            sim.tick(&mut schedule, commands.iter().chain(candidate));
        }
        if let Some(command) = candidate {
            receipt = Some(ComparisonReceipt::read(&sim, command, commands.len())?);
        }
        let command_count = commands.len() + usize::from(candidate.is_some());
        if command_count != 0 {
            batch_outcomes.push(ComparisonBatchOutcome::read(
                tick,
                true,
                command_count,
                &sim,
            ));
        }

        let uses_repaired_road =
            |vehicle| repaired_road.is_some_and(|road| vehicle_on_road(&sim, vehicle, road));
        pending_people.retain(|&index, &mut (human, destination, arrival_curb)| {
            let Some(entity) = sim.world().humans.get(human) else {
                return true;
            };
            if matches!(entity.location, Location::Vehicle(car) if uses_repaired_road(car)) {
                repaired_road_tags.insert(reports[index].tag);
            }
            let arrived = match arrival_curb {
                Some(position) => {
                    entity.location == Location::Outside
                        && entity.trans.pos.is_close(position, 0.15)
                }
                None => entity.location == Location::Building(destination),
            };
            if arrived {
                reports[index].terminal = Some(Terminal::Completed {
                    tick,
                    destination: format!("{destination:?}"),
                    units: 1,
                });
                false
            } else {
                true
            }
        });

        let book = sim.read::<CargoBook>();
        freight.retain(|&id, &mut (index, receiver, units)| {
            let Some(progress) = book.road_shipment_progress(id) else {
                reports[index].terminal = Some(Terminal::Unreconciled { tick });
                return false;
            };
            if progress.active_loads.values().copied().any(uses_repaired_road) {
                repaired_road_tags.insert(reports[index].tag);
            }
            if progress.cancelled {
                reports[index].terminal = Some(Terminal::Cancelled { tick });
            } else if progress.unloaded_units == units {
                reports[index].terminal = Some(Terminal::Completed {
                    tick,
                    destination: format!("{receiver:?}"),
                    units: u64::from(units),
                });
            }
            reports[index].terminal.is_none()
        });
        drop(book);
        if request.resume_at == Some(tick) {
            let mut tracking = RoadFreightTracking::default();
            sim.write::<CargoBook>().swap_road_freight_tracking(&mut tracking);
            sim = load_snapshot(&snapshot_bytes(&sim)?)?;
            sim.write::<CargoBook>().swap_road_freight_tracking(&mut tracking);
        }
        if authored && !replay
            && next_offer == reports.len()
            && batches.peek().is_none()
            && (!treatment || receipt.is_some())
            && request.resume_at.is_none_or(|resume| resume <= tick)
            && pending_people.is_empty()
            && freight.is_empty()
        {
            // All tagged outcomes are terminal at both this point and the tail.
            break;
        }
    }
    if let Some(playback) = &mut playback {
        if !matches!(playback.advance_one(&mut sim, &mut schedule, &mut replay_budget),
            ReplayAdvance::Settled { complete: true })
        {
            return Err(ComparisonError::InvalidSchedule("replay did not reach the scored cutoff"));
        }
    }
    Ok((
        ComparisonArm {
            observations: reports,
            batches: batch_outcomes,
            repaired_road_tags: repaired_road_tags.into_iter().collect(),
        },
        receipt,
    ))
}

fn vehicle_on_road(sim: &Simulation, id: VehicleID, road: RoadID) -> bool {
    let Some(vehicle) = sim.world().vehicles.get(id) else {
        return false;
    };
    let Some(TraverseKind::Lane(lane)) = vehicle.it.get_travers().map(|t| t.kind) else {
        return false;
    };
    sim.map().get(lane).is_some_and(|lane| {
        lane.parent == road
            && lane
                .points
                .project(vehicle.trans.pos)
                .is_close(vehicle.trans.pos, 1.5)
    })
}
