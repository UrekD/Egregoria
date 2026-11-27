use crate::{Scenario, ScenarioCtx};
use geom::{vec2, Vec2};
use simulation::map::building_parking::{
    parking_entrance_near, BuildingParkingKind, GarageKind, LotFrame, LotMouth, MouthUse,
    SurfaceLotDraft,
};
use simulation::map::{
    BuildingID, Density, LaneKind, LanePatternBuilder, MapProject, ProjectFilter, ZoneCell, ZoneKind,
};
use simulation::map_dynamic::Destination;
use simulation::souls::human::spawn_human;
use simulation::transportation::{Location, VehicleState};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{HumanID, Simulation, SimulationOptions, VehicleID};
use std::collections::BTreeMap;
use std::path::Path;

const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 1,
    save_replay: false,
    ..SimulationOptions::LEGACY
};

pub const GROWABLES: (Scenario, SimulationOptions) = (
    Scenario {
        name: "pathpark_growables",
        run,
    },
    OPTIONS,
);

pub const SCALE: (Scenario, SimulationOptions) = (
    Scenario {
        name: "pathpark_scale",
        run: run_scale,
    },
    SimulationOptions {
        preset: simulation::MapPreset::Flat,
        ..super::perf::OPTIONS
    },
);

pub const BASIC: (Scenario, SimulationOptions) = (
    Scenario {
        name: "pathpark_basic",
        run: run_basic,
    },
    OPTIONS,
);
pub const PRESSURE: (Scenario, SimulationOptions) = (
    Scenario {
        name: "pathpark_pressure",
        run: run_pressure,
    },
    OPTIONS,
);
pub const EDITS: (Scenario, SimulationOptions) = (
    Scenario {
        name: "pathpark_edits",
        run: run_edits,
    },
    OPTIONS,
);

fn run_basic(ctx: &mut ScenarioCtx) -> Result<(), String> {
    use prototypes::Tick;
    use simulation::map::{PathEndpoint, PathKind, RoadID};
    use simulation::map_dynamic::Itinerary;

    let ground = |map: &simulation::map::Map, x, y| {
        map.pick_path_ground(x, y)
            .ok_or("invalid walking fixture ground")
    };
    fn join(sim: &mut Simulation, from: PathEndpoint, to: PathEndpoint) -> Result<RoadID, String> {
        sim.apply_commands([&WorldCommand::MapMakeWalkingPath {
            from,
            to,
            control: None,
        }]);
        sim.read::<CommandOutcomes>()
            .walking_path(0)
            .ok_or("missing walking path receipt".to_owned())?
            .map_err(|reason| format!("{reason:?}"))
    }
    let tip = |map: &simulation::map::Map, road: RoadID, start: bool| {
        let segment = map.get(road).ok_or("missing walking fixture segment")?;
        let joint = if start { segment.src } else { segment.dst };
        let position = map
            .get(joint)
            .ok_or("missing walking fixture junction")?
            .pos;
        map.pick_path_attachment(simulation::map::ProjectKind::Intersection(joint), position)
            .ok_or("invalid walking fixture junction")
    };
    let [a, b, c, d, entry, exit] = {
        let map = ctx.sim.map();
        [
            ground(&map, 20_000, 18_000)?,
            ground(&map, 20_000, 31_000)?,
            ground(&map, 32_000, 31_000)?,
            ground(&map, 32_000, 18_000)?,
            ground(&map, 17_700, 18_000)?,
            ground(&map, 34_300, 18_000)?,
        ]
    };
    let west = join(&mut ctx.sim, a, b)?;
    let b = tip(&ctx.sim.map(), west, false)?;
    let north = join(&mut ctx.sim, b, c)?;
    let c = tip(&ctx.sim.map(), north, false)?;
    let east = join(&mut ctx.sim, c, d)?;
    let entry_end = tip(&ctx.sim.map(), west, true)?;
    let entry = join(&mut ctx.sim, entry, entry_end)?;
    let exit_start = tip(&ctx.sim.map(), east, false)?;
    let exit = join(&mut ctx.sim, exit_start, exit)?;
    let ends = {
        let map = ctx.sim.map();
        (tip(&map, west, true)?, tip(&map, east, false)?)
    };
    let cohorts: Vec<_> = {
        let map = ctx.sim.map();
        (0..20)
            .map(|index| {
                let offset = 2.0 + index as f32 * 0.45;
                let left = map.roads()[entry].points().point_along(offset);
                let right = map.roads()[exit]
                    .points()
                    .point_along(map.roads()[exit].length() - offset);
                (left, right)
            })
            .collect()
    };
    let walk = |sim: &Simulation| {
        let map = sim.map();
        cohorts
            .iter()
            .map(|&(start, end)| {
                let mut travel = Itinerary::route(Tick(0), start, end, &map, PathKind::Pedestrian)?;
                let mut position = start;
                for step in 1..=1_200 {
                    position = travel.update(position, 0.5, Tick(step), 0, &map);
                    if position.distance2(end) < 0.25 {
                        return Some(step);
                    }
                }
                None
            })
            .collect::<Vec<_>>()
    };
    let control = walk(&ctx.sim);
    let bays = ctx.sim.map().parking.all_spots().count();
    let shortcut = join(&mut ctx.sim, ends.0, ends.1)?;
    let candidate = walk(&ctx.sim);
    let cutoff = 400;
    let reached = |steps: &[Option<u64>]| {
        steps
            .iter()
            .filter(|step| step.is_some_and(|n| n <= cutoff))
            .count()
    };
    let total = cohorts.len();
    let before = reached(&control);
    let after = reached(&candidate);
    let median = |mut steps: Vec<u64>| {
        steps.sort_unstable();
        steps.get(steps.len() / 2).copied()
    };
    let matched: Vec<_> = control
        .into_iter()
        .zip(candidate)
        .filter_map(|(a, b)| a.zip(b))
        .collect();
    let control_median = median(matched.iter().map(|&(a, _)| a).collect());
    let shortcut_median = median(matched.iter().map(|&(_, b)| b).collect());
    if before * 100 > total * 85
        || after * 100 < total * 95
        || after.saturating_sub(before) * 100 < total * 15
        || shortcut_median
            .zip(control_median)
            .is_none_or(|(short, long)| short >= long)
    {
        return Err(format!(
            "walking shortcut reached {after}/{total} versus {before}/{total}, median {shortcut_median:?} versus {control_median:?}"
        ));
    }
    if ctx.sim.map().parking.all_spots().count() != bays {
        return Err("walking shortcut changed curb supply".into());
    }
    ctx.sample("shortcut");
    let restored = ctx.round_tripped()?;
    if ctx.sim.hashes() != restored.hashes()
        || restored.map().get(shortcut).is_none_or(|road| {
            !road.is_walking_path() || road.lanes_iter().any(|(_, kind)| kind != LaneKind::Walking)
        })
    {
        return Err("walking shortcut diverged or has a vehicle lane".into());
    }
    println!("PathPark shortcut: {after}/{total} within the cutoff versus {before}/{total}; median {shortcut_median:?} versus {control_median:?}");
    Ok(())
}

fn place_surface_lot(
    sim: &mut Simulation,
    offset: Vec2,
) -> Result<(BuildingID, WorldCommand), String> {
    for (x, y) in [(150., 115.), (180., 115.), (190., 264.), (160., 264.)] {
        let draft = {
            let map = sim.map();
            let at = |x| parking_entrance_near(&map, (offset + vec2(x, y)).z(0.0));
            let Some((center, inward, outward)) = at(x)
                .zip(at(x + 7.0))
                .zip(at(x - 7.0))
                .map(|((center, a), b)| (center, a, b))
            else {
                continue;
            };
            SurfaceLotDraft {
                frame: LotFrame::Curb(center),
                width_blocks: 3,
                mouths: [
                    Some(LotMouth {
                        at: inward,
                        mode: MouthUse::In,
                    }),
                    Some(LotMouth {
                        at: outward,
                        mode: MouthUse::Out,
                    }),
                ],
                foot_gate: Some(simulation::map::PathEndpoint::Sidewalk(center)),
            }
        };
        let Ok((quote, _)) = sim.quote_surface_lot(draft) else {
            continue;
        };
        let command = WorldCommand::MapPlaceParkingLot { draft, quote };
        sim.apply_commands([&command]);
        let id = sim
            .read::<CommandOutcomes>()
            .parking_lot(0)
            .ok_or("missing public lot receipt")?
            .map_err(|reason| format!("surface lot placement: {reason:?}"))?;
        let building = sim
            .map()
            .parking_lot_building(id)
            .ok_or("missing public lot building")?;
        return Ok((building, command));
    }
    Err(format!("no legal public lot near {offset:?}"))
}

fn run_pressure(ctx: &mut ScenarioCtx) -> Result<(), String> {
    run_surface_trip(ctx, false)
}

fn run_edits(ctx: &mut ScenarioCtx) -> Result<(), String> {
    run_surface_trip(ctx, true)
}

fn edit_held_trip_topology(sim: &mut Simulation, resumed: &mut Simulation) -> Result<(), String> {
    use simulation::map::road_edit::{preview_road_upgrade, road_edit_stamp, RoadUpgrade};
    use simulation::map::{ProjectKind, SmallRoadPreset};
    use simulation::world_command::PrepareBudget;

    let apply = |sim: &mut Simulation, resumed: &mut Simulation, command: WorldCommand| {
        let name = command.kind();
        for run in [&mut *sim, &mut *resumed] {
            run.apply_commands([&command]);
            let outcomes = run.read::<CommandOutcomes>();
            if outcomes.accepted != [0] {
                return Err(format!("held-trip {name} failed: {:?}", outcomes.last));
            }
        }
        if sim.hashes() != resumed.hashes() {
            return Err(format!("held-trip {name} diverged after reload"));
        }
        Ok(())
    };
    let ground = |sim: &Simulation, xy: Vec2| {
        sim.map()
            .environment()
            .height(xy)
            .map(|z| MapProject::ground(xy.z(z)))
            .ok_or("topology edit lies outside the terrain")
    };
    let project = |sim: &Simulation, xy: Vec2| {
        ground(sim, xy).map(|at| {
            sim.map()
                .project(at.pos, 0.0, ProjectFilter::ROAD | ProjectFilter::INTER)
        })
    };
    let connect = |from, to, pat| WorldCommand::MapMakeConnection {
        from,
        to,
        inter: None,
        pat,
    };
    let local = SmallRoadPreset::LocalOneWay {
        curb_parking: false,
    }
    .lane_pattern();
    let start = vec2(80.0, 405.0);
    let join = vec2(200.0, 405.0);
    let end = vec2(340.0, 405.0);
    let center = vec2(240.0, 405.0);
    let road_count = sim.map().roads().len();
    let original = connect(ground(sim, start)?, ground(sim, join)?, local.clone());
    apply(sim, resumed, original)?;
    let extension = connect(project(sim, join)?, ground(sim, end)?, local);
    apply(sim, resumed, extension)?;
    if sim.map().roads().len() != road_count + 1 {
        return Err("collinear local road extension did not merge".into());
    }
    let ProjectKind::Road(road) = project(sim, center)?.kind else {
        return Err("merged local road is missing".into());
    };
    let change = RoadUpgrade::ReverseOneWay;
    let mut budget = PrepareBudget::for_preview(1_000_000);
    let expected =
        road_edit_stamp(sim, road, &mut budget).map_err(|reason| format!("{reason:?}"))?;
    let quote = preview_road_upgrade(sim, road, expected, &change, &mut budget)
        .map_err(|reason| format!("held-trip reversal preview: {reason:?}"))?
        .quote;
    apply(
        sim,
        resumed,
        WorldCommand::MapUpgradeRoad {
            road,
            expected,
            change,
            quote,
        },
    )?;

    let branch = connect(
        project(sim, center)?,
        ground(sim, vec2(240.0, 475.0))?,
        LanePatternBuilder::default().build(),
    );
    apply(sim, resumed, branch)?;
    if sim.map().get(road).is_some() || sim.map().roads().len() != road_count + 3 {
        return Err("branch did not split the reversed road".into());
    }
    let path = {
        let map = sim.map();
        let from = map
            .pick_path_ground(8_000, 47_500)
            .ok_or("invalid edit path start")?;
        let to = map
            .pick_path_ground(16_000, 47_500)
            .ok_or("invalid edit path end")?;
        WorldCommand::MapMakeWalkingPath {
            from,
            to,
            control: None,
        }
    };
    apply(sim, resumed, path)
}

fn run_surface_trip(ctx: &mut ScenarioCtx, edits: bool) -> Result<(), String> {
    let sites = setup_at(&mut ctx.sim, true, Vec2::ZERO, edits, false)?;
    ctx.sim
        .apply_commands([&WorldCommand::MapRemoveBuilding(sites[4])]);
    if !edits {
        ctx.sim.apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building: sites[5],
            open: false,
        }]);
    }
    let (lot, placement) = place_surface_lot(&mut ctx.sim, Vec2::ZERO)?;
    let (driver, home, car) = ctx
        .sim
        .world()
        .humans
        .iter()
        .find_map(|(driver, person)| {
            let home = person.home.house;
            let parking = ctx.sim.building_parking(home)?;
            (parking.kind == BuildingParkingKind::Driveway && parking.occupied > 0).then_some((
                driver,
                home,
                person.router.personal_car?,
            ))
        })
        .ok_or("missing tagged driveway driver")?;
    let issue = |run: &mut ScenarioCtx, destination| {
        run.sim.world_mut_unchecked().edit_human(driver, |person| {
            person.router.go_to(destination);
            person.decision.wait = u8::MAX;
        });
    };
    let gate = ctx
        .sim
        .map()
        .get(lot)
        .ok_or("public lot has no gate")?
        .door_pos;
    let destination = if edits {
        Destination::Building(lot)
    } else {
        Destination::Outside(gate)
    };
    let arrived = |sim: &Simulation| {
        let person = &sim.world().humans[driver];
        match destination {
            Destination::Building(building) => person.location == Location::Building(building),
            Destination::Outside(point) => {
                person.location == Location::Outside && person.trans.pos.distance2(point) < 4.0
            }
        }
    };
    issue(ctx, destination);
    let mut cut = false;
    for _ in 0..10_000 {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(driver, |person| person.decision.wait = u8::MAX);
        ctx.run_ticks(1);
        if !cut && ctx.sim.car_property_parking(car).0 == Some(lot) {
            cut = true;
            if edits {
                ctx.sim
                    .apply_commands([&placement, &WorldCommand::MapRemoveBuilding(lot)]);
                let outcomes = ctx.sim.read::<CommandOutcomes>();
                if outcomes.parking_lot(0)
                    != Some(Err(simulation::world_command::Rejection::StaleRoadQuote))
                    || outcomes.rejections_for(1).first().map(|entry| entry.1)
                        != Some(simulation::world_command::Rejection::BusyConflict)
                {
                    return Err(format!(
                        "expected stale and busy denials, got {:?}",
                        outcomes.last
                    ));
                }
            }
            let mut restored = ctx.round_tripped()?;
            if ctx.sim.hashes() != restored.hashes()
                || restored.car_property_parking(car).0 != Some(lot)
            {
                return Err("public-lot hold diverged on reload".into());
            }
            ctx.sample("public_held");
            if edits {
                edit_held_trip_topology(&mut ctx.sim, &mut restored)?;
                if ctx.sim.car_property_parking(car).0 != Some(lot) {
                    return Err("topology edits lost the tagged lot hold".into());
                }
                ctx.sample("held_after_topology_edits");
            }
        }
        if arrived(&ctx.sim) {
            break;
        }
    }
    if !cut || ctx.sim.car_property_parking(car).1 != Some(lot) || !arrived(&ctx.sim) {
        return Err("tagged driver did not park in the public lot and reach the gate".into());
    }
    ctx.sample("public_occupied");
    if edits {
        ctx.sim.apply_commands([
            &WorldCommand::SetBuildingParkingOpen {
                building: lot,
                open: false,
            },
            &WorldCommand::SetBuildingParkingOpen {
                building: lot,
                open: true,
            },
        ]);
        if ctx.sim.read::<CommandOutcomes>().accepted != [0, 1] {
            return Err("public lot did not close and reopen".into());
        }
    } else {
        ctx.sim.apply_commands([
            &WorldCommand::SetBuildingParkingOpen {
                building: sites[5],
                open: true,
            },
            &WorldCommand::SetBuildingParkingOpen {
                building: lot,
                open: false,
            },
        ]);
        if ctx.sim.read::<CommandOutcomes>().accepted != [0, 1] {
            return Err("public parking could not transition to the drain configuration".into());
        }
    }
    issue(ctx, Destination::Building(home));
    for _ in 0..12_000 {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(driver, |person| person.decision.wait = u8::MAX);
        ctx.run_ticks(1);
        if ctx.sim.world().humans[driver].location == Location::Building(home)
            && ctx.sim.car_property_parking(car).1 == Some(home)
        {
            break;
        }
    }
    if ctx.sim.car_property_parking(car) != (None, Some(home))
        || ctx.sim.world().humans[driver].location != Location::Building(home)
    {
        return Err("tagged public-lot driver never cleared the apron and returned home".into());
    }
    if !edits {
        for _ in 0..80 {
            if ctx
                .sim
                .building_parking(lot)
                .is_some_and(|parking| parking.held + parking.occupied == 0)
            {
                break;
            }
            ctx.run_ticks(200);
        }
    }
    ctx.sim
        .apply_commands([&WorldCommand::MapRemoveBuilding(lot)]);
    if ctx.sim.map().get(lot).is_some() {
        return Err(format!(
            "public lot could not be removed after drain: {:?}",
            ctx.sim.building_parking(lot)
        ));
    }
    ctx.sample("public_rear_clear");
    Ok(())
}

fn setup(sim: &mut Simulation, round_trip: bool) -> Result<Vec<BuildingID>, String> {
    setup_at(sim, round_trip, Vec2::ZERO, true, false)
}

fn setup_at(
    sim: &mut Simulation,
    round_trip: bool,
    offset: Vec2,
    curb_parking: bool,
    west_crossing: bool,
) -> Result<Vec<BuildingID>, String> {
    let point = |x, y| (offset + vec2(x, y)).z(0.0);
    let [west, east, far_east, far_west, spur] = {
        let map = sim.map();
        let mut corners = [
            point(75.0, 100.0),
            point(450.0, 100.0),
            point(450.0, 280.0),
            point(75.0, 280.0),
            point(450.0, 360.0),
        ];
        for corner in &mut corners {
            corner.z = map
                .environment()
                .height(corner.xy())
                .ok_or("parking district lies outside the terrain")?;
        }
        corners
    };
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(west),
        to: MapProject::ground(east),
        inter: None,
        pat: LanePatternBuilder::default().parking(curb_parking).build(),
    }]);
    if round_trip {
        // A loop of two-arm corners has neither a crosswalk between its
        // sidewalks nor a connection between its opposing driving directions.
        let links = [
            [east, far_east],
            [far_east, far_west],
            [far_west, west],
            [far_east, spur],
        ];
        let crossing = west_crossing.then_some([west, point(75.0, 42.0)]);
        for pair in links.into_iter().chain(crossing) {
            let [from, to] = {
                let map = sim.map();
                std::array::from_fn(|index| map.project(pair[index], 0.0, ProjectFilter::ALL))
            };
            sim.apply_commands([&WorldCommand::MapMakeConnection {
                from,
                to,
                inter: None,
                pat: LanePatternBuilder::default().parking(curb_parking).build(),
            }]);
            let outcomes = sim.read::<CommandOutcomes>();
            if outcomes.accepted.len() != 1 {
                return Err(format!(
                    "tagged return loop at {pair:?}: {:?}",
                    outcomes.last
                ));
            }
        }
    }
    let mut sites = Vec::new();
    for (point, kind, density) in [
        (vec2(238.0, 135.0), ZoneKind::Residential, Density::Low),
        (vec2(285.0, 135.0), ZoneKind::Residential, Density::Low),
        (vec2(330.0, 135.0), ZoneKind::Residential, Density::High),
        (vec2(265.0, 65.0), ZoneKind::Commercial, Density::Low),
    ] {
        let lot = sim
            .map()
            .lots()
            .values()
            .filter(|lot| {
                kind != ZoneKind::Commercial || {
                    let axes = lot.shape.axis().map(|axis| axis.mag());
                    let center = lot.shape.center() - offset;
                    axes[0] >= 29.5
                        && axes[1] >= 29.5
                        && (35.0..100.0).contains(&center.y)
                        && (175.0..310.0).contains(&center.x)
                }
            })
            .min_by(|a, b| {
                let area_order = if kind == ZoneKind::Commercial {
                    let area = |lot: &simulation::map::Lot| {
                        let [width, depth] = lot.shape.axis().map(|axis| axis.mag());
                        width * depth
                    };
                    area(b).total_cmp(&area(a))
                } else {
                    std::cmp::Ordering::Equal
                };
                area_order
                    .then_with(|| {
                        a.shape
                            .center()
                            .distance2(point + offset)
                            .total_cmp(&b.shape.center().distance2(point + offset))
                    })
                    .then(a.id.cmp(&b.id))
            })
            .ok_or("no parcel")?
            .id;
        let building = sim
            .place_zoned_growable(lot, ZoneCell { kind, density })
            .map_err(|error| format!("build {kind:?} {density:?} at {point:?}: {error:?}"))?
            .building;
        if kind == ZoneKind::Residential {
            spawn_human(sim, building).ok_or("no resident spawned")?;
        }
        sites.push(building);
    }
    for (kind, preferred) in [
        (GarageKind::MultiStorey, point(150., 118.)),
        (GarageKind::Underground, point(364., 81.)),
    ] {
        let map = sim.map();
        let command = [
            preferred,
            point(374., 81.),
            point(400., 81.),
            point(300., 261.),
            point(180., 261.),
            point(300., 298.),
            point(180., 298.),
        ]
        .into_iter()
        .filter_map(|pick| parking_entrance_near(&map, pick))
        .map(|entrance| WorldCommand::MapPlaceParkingGarage { kind, entrance })
        .find(|command| command.validate_and_price(sim).is_ok())
        .ok_or_else(|| format!("no feasible {kind:?} garage entrance"))?;
        drop(map);
        sim.apply_commands([&command]);
        let outcome = sim
            .read::<CommandOutcomes>()
            .parking_garage(0)
            .ok_or("missing garage receipt")?;
        sites.push(outcome.map_err(|error| format!("{kind:?} placement: {error:?}"))?);
    }
    Ok(sites)
}

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let sites = setup(&mut ctx.sim, true)?;
    let summaries: Vec<_> = sites
        .iter()
        .filter_map(|&id| ctx.sim.building_parking(id))
        .collect();
    let house = summaries
        .iter()
        .find(|parking| parking.kind == BuildingParkingKind::Driveway)
        .ok_or("no physical driveway was commissioned")?;
    if house.occupied == 0 {
        return Err("no resident used their driveway".into());
    }
    if !summaries
        .iter()
        .any(|parking| parking.kind == BuildingParkingKind::IntegratedGarage)
    {
        return Err("no integrated garage was commissioned".into());
    }
    if !summaries
        .iter()
        .any(|parking| parking.kind == BuildingParkingKind::RearLot)
    {
        return Err("no shop rear lot was commissioned".into());
    }
    let capacity: u64 = summaries
        .iter()
        .map(|parking| u64::from(parking.capacity))
        .sum();
    let occupied: u64 = summaries
        .iter()
        .map(|parking| u64::from(parking.occupied))
        .sum();
    println!(
        "PathPark property spaces {occupied}/{capacity} occupied across {} facilities",
        summaries.len()
    );
    ctx.sample("property_opening");
    let (driver, home, car) = ctx
        .sim
        .world()
        .humans
        .iter()
        .find_map(|(driver, person)| {
            let Location::Building(home) = person.location else {
                return None;
            };
            let parking = ctx.sim.building_parking(home)?;
            let car = person.router.personal_car?;
            (parking.kind == BuildingParkingKind::Driveway && parking.occupied != 0)
                .then_some((driver, home, car))
        })
        .ok_or("no tagged driver at an occupied driveway")?;
    let garage = *sites.get(4).ok_or("missing tagged public garage")?;
    ctx.sim.world_mut_unchecked().edit_human(driver, |person| {
        person.router.go_to(Destination::Building(garage));
    });
    let mut occupied = false;
    for _ in 0..4_000 {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(driver, |person| person.decision.wait = u8::MAX);
        ctx.run_ticks(1);
        let person = &ctx.sim.world().humans[driver];
        if person.location == Location::Building(garage) && ctx.sim.car_stored_inside_garage(car) {
            occupied = true;
            break;
        }
    }
    if !occupied
        || ctx
            .sim
            .building_parking(home)
            .is_none_or(|parking| parking.occupied != 0)
    {
        return Err(
            "tagged driver did not drive from the driveway to the public garage door".into(),
        );
    }
    ctx.sample("tagged_garage_arrival");
    ctx.submit(&[WorldCommand::SetBuildingParkingOpen {
        building: garage,
        open: false,
    }]);
    let mut drained = false;
    for _ in 0..10_000 {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(driver, |person| person.decision.wait = u8::MAX);
        ctx.run_ticks(1);
        let person = &ctx.sim.world().humans[driver];
        if person.location == Location::Building(home)
            && ctx
                .sim
                .building_parking(home)
                .is_some_and(|parking| parking.occupied == 1)
            && ctx
                .sim
                .building_parking(garage)
                .is_some_and(|parking| parking.occupied == 0)
            && person.router.personal_car == Some(car)
        {
            drained = true;
            break;
        }
    }
    if !drained {
        let person = &ctx.sim.world().humans[driver];
        return Err(format!(
            "tagged return stopped at {:?}, step {:?}, error {:?}, home {:?}, garage {:?}, car {:?}",
            person.location,
            person.router.cur_step(),
            person.router.last_error,
            ctx.sim.building_parking(home),
            ctx.sim.building_parking(garage),
            ctx.sim.world().vehicles.get(car).map(|actor| (&actor.vehicle.state, actor.trans)),
        ));
    }
    ctx.sample("property_live");
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Supply {
    Curb,
    PublicLot,
    BuildingSurface,
    Garage,
}

impl Supply {
    const ALL: [Self; 4] = [
        Self::Curb,
        Self::PublicLot,
        Self::BuildingSurface,
        Self::Garage,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Curb => "curb",
            Self::PublicLot => "public_lot",
            Self::BuildingSurface => "building_surface",
            Self::Garage => "garage",
        }
    }

    fn property(sim: &Simulation, site: BuildingID) -> Option<Self> {
        Some(match sim.building_parking_kind(site)? {
            BuildingParkingKind::SurfaceLot(_) => Self::PublicLot,
            BuildingParkingKind::Driveway | BuildingParkingKind::RearLot => Self::BuildingSurface,
            _ => Self::Garage,
        })
    }
}

#[derive(Default)]
struct TaggedSupply {
    holds: [bool; 4],
    occupancies: [bool; 4],
    departures: [bool; 4],
    previous: Option<Supply>,
}

impl TaggedSupply {
    fn sample(&mut self, sim: &Simulation, car: VehicleID) {
        let (hold, occupied) = sim.car_property_parking(car);
        let Some(vehicle) = sim.world().vehicles.get(car) else {
            return;
        };
        let hold = hold.and_then(|id| Supply::property(sim, id)).or_else(|| {
            matches!(vehicle.vehicle.state, VehicleState::RoadToPark(..)).then_some(Supply::Curb)
        });
        let occupied = occupied
            .and_then(|id| Supply::property(sim, id))
            .or_else(|| {
                matches!(vehicle.vehicle.state, VehicleState::Parked(_)).then_some(Supply::Curb)
            });
        if let Some(source) = hold {
            self.holds[source as usize] = true;
        }
        if let Some(source) = occupied {
            self.occupancies[source as usize] = true;
        }
        if let Some(source) = self.previous.filter(|&source| Some(source) != occupied) {
            self.departures[source as usize] = true;
        }
        self.previous = occupied;
    }
}

struct ScaleTrip {
    driver: HumanID,
    car: VehicleID,
    home: BuildingID,
    destination: Destination,
}

fn run_scale(ctx: &mut ScenarioCtx) -> Result<(), String> {
    const DISTRICTS: usize = 128;
    const POPULATION: usize = 10_000;
    ctx.submit(&[WorldCommand::MapLoadParis]);
    ctx.run_ticks(1);
    super::perf::setup_employers(ctx, POPULATION, 50)?;
    super::perf::setup_houses(ctx, POPULATION - DISTRICTS * 3)?;
    let garage_funds = (GarageKind::MultiStorey.cost() + GarageKind::Underground.cost())
        * DISTRICTS as i64;
    crate::builders::grant_money(ctx, garage_funds + prototypes::Money::new_bucks(1_000_000));
    let mut districts = Vec::new();
    for index in 0..DISTRICTS {
        let offset = vec2((index % 16) as f32 * 550.0, (index / 16) as f32 * 400.0);
        let mut sites = setup_at(&mut ctx.sim, true, offset, true, true)
            .map_err(|error| format!("parking district {index}: {error}"))?;
        if index % 4 == 0 {
            ctx.sim
                .apply_commands([&WorldCommand::MapRemoveBuilding(sites[4])]);
            sites[4] = place_surface_lot(&mut ctx.sim, offset)?.0;
        }
        districts.push(sites);
    }
    let mut offers = Vec::new();
    for (index, sites) in districts.iter().enumerate() {
        for (driver, person) in ctx.sim.world().humans.iter() {
            let Some(car) = person.router.personal_car else {
                continue;
            };
            let home = person.home.house;
            if sites[..3].contains(&home) {
                let destination = if index % 2 == 0 {
                    Destination::Building(sites[4])
                } else {
                    let map = ctx.sim.map();
                    let curbside = map.get(home).unwrap().door_pos - geom::Vec3::X * 65.0;
                    let lane = map
                        .nearest_lane(curbside, LaneKind::Walking, Some(60.0))
                        .and_then(|lane| map.get(lane))
                        .ok_or("curb cohort frontage has no sidewalk")?;
                    Destination::Outside(lane.points.project(curbside))
                };
                offers.push(ScaleTrip {
                    driver,
                    car,
                    home,
                    destination,
                });
            }
        }
    }
    offers.sort_unstable_by_key(|trip| trip.driver);
    if offers.len() * 2 < 500 {
        return Err(format!(
            "scaled opening commissioned only {} tagged drivers",
            offers.len()
        ));
    }
    if ctx.sim.world().humans.len() != POPULATION {
        return Err("mixed-supply opening did not commission 10,000 residents".into());
    }
    println!(
        "PathPark scale opening: {POPULATION} residents, {} tagged drivers in {DISTRICTS} districts",
        offers.len()
    );
    let public_facilities: Vec<_> = districts
        .iter()
        .flat_map(|sites| [sites[4], sites[5]])
        .collect();
    ctx.sample("setup");
    let mut workload = crate::runner::ParkingWorkload::new(&ctx.sim);
    let mut supply: Vec<_> = offers.iter().map(|_| TaggedSupply::default()).collect();
    for trip in &offers {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(trip.driver, |person| {
                person.router.go_to(trip.destination);
            });
    }
    let sample_supply = |sim: &Simulation, supply: &mut [TaggedSupply]| {
        if sim.get_tick() % 4 != 0 {
            return;
        }
        for (trip, supply) in offers.iter().zip(supply) {
            supply.sample(sim, trip.car);
        }
    };
    let advance = |run: &mut ScenarioCtx,
                   ticks,
                   workload: &mut crate::runner::ParkingWorkload,
                   supply: &mut [TaggedSupply]| {
        freeze_scale_drivers(run, &offers);
        run.run_ticks_observed(ticks, |sim| {
            workload.observe(sim);
            sample_supply(sim, supply);
        });
    };
    advance(ctx, 8, &mut workload, &mut supply);
    let holds = public_facilities
        .iter()
        .filter_map(|&garage| ctx.sim.building_parking(garage))
        .map(|parking| usize::from(parking.held))
        .sum::<usize>();
    if holds < 2 {
        return Err(
            "mixed-supply save cut did not contain simultaneous public parking holds".into(),
        );
    }
    let mut extra = workload.finish(&ctx.sim);
    let mut resumed = ScenarioCtx::from_sim(ctx.round_tripped()?);
    freeze_scale_drivers(&mut resumed, &offers);
    resumed.run_ticks(128);
    let mut workload = crate::runner::ParkingWorkload::new(&ctx.sim);
    advance(ctx, 128, &mut workload, &mut supply);
    if ctx.sim.hashes() != resumed.sim.hashes() {
        return Err("mixed-supply resumed parking holds diverged".into());
    }
    drop(resumed);
    // Allow time for a turnaround and the walk from an alternate bay.
    for _ in 0..60 {
        advance(ctx, 200, &mut workload, &mut supply);
    }
    let mut curb = 0;
    let mut garage = 0;
    let mut public_lot = 0;
    let mut curb_miss = None;
    for trip in &offers {
        let human = &ctx.sim.world().humans[trip.driver];
        let vehicle = &ctx.sim.world().vehicles[trip.car];
        match trip.destination {
            Destination::Building(building)
                if human.location == Location::Building(building)
                    && ctx.sim.car_property_parking(trip.car).1 == Some(building) =>
            {
                match Supply::property(&ctx.sim, building) {
                    Some(Supply::PublicLot) => public_lot += 1,
                    Some(Supply::Garage) => garage += 1,
                    _ => {}
                }
            }
            Destination::Outside(door)
                if human.location == Location::Outside
                    && human.trans.pos.distance2(door) < 4.0
                    && matches!(vehicle.vehicle.state, VehicleState::Parked(_)) =>
            {
                curb += 1
            }
            Destination::Outside(destination) => {
                curb_miss.get_or_insert_with(|| {
                    format!(
                        "driver {:?} is {:?} at {:?}, destination {destination:?}, car {:?} at {:?}, routing error {:?}",
                        trip.driver, human.location, human.trans.pos, vehicle.vehicle.state,
                        vehicle.trans.pos, human.router.last_error,
                    )
                });
            }
            _ => {}
        }
    }
    for (key, value) in workload.finish(&ctx.sim) {
        *extra.entry(key).or_default() += value;
    }
    extra.extend(BTreeMap::from([
        ("tagged_outbound_offers".to_owned(), offers.len() as f64),
        ("tagged_garage_arrivals".to_owned(), f64::from(garage)),
        ("tagged_curb_arrivals".to_owned(), f64::from(curb)),
        (
            "tagged_public_lot_arrivals".to_owned(),
            f64::from(public_lot),
        ),
        ("simultaneous_public_holds".to_owned(), holds as f64),
    ]));
    println!(
        "PathPark scale outbound: {curb} curb, {garage} garage and {public_lot} public-lot arrivals among {} offers",
        offers.len()
    );
    ctx.sample_perf_with_extra("measured", POPULATION, extra, true);
    if curb == 0
        || garage == 0
        || public_lot == 0
        || (curb + garage + public_lot) * 100 < offers.len() as i32 * 90
    {
        return Err(format!(
            "mixed-supply departures reached {curb} curb, {garage} garage and {public_lot} public-lot destinations from {} tagged offers; first curb miss {curb_miss:?}",
            offers.len(),
        ));
    }
    // Drain existing routes while holding the authored housing and cohort in
    // place. The test districts have no permanent utilities.
    ctx.retain_systems(|name| !matches!(name, "residence_update" | "update_decision_system"));
    let close: Vec<_> = public_facilities
        .iter()
        .map(|&building| WorldCommand::SetBuildingParkingOpen {
            building,
            open: false,
        })
        .collect();
    ctx.submit(&close);
    for trip in &offers {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(trip.driver, |person| {
                person.router.go_to(Destination::Building(trip.home));
            });
    }
    let parked = |sim: &Simulation, trip: &ScaleTrip| {
        sim.world().humans.get(trip.driver).is_some_and(|human| {
            human.router.personal_car == Some(trip.car)
                && sim
                    .world()
                    .vehicles
                    .get(trip.car)
                    .is_some_and(|car| car.vehicle.state.is_parked())
        })
    };
    let returned = |sim: &Simulation, trip: &ScaleTrip| {
        parked(sim, trip)
            && sim.world().humans[trip.driver].location == Location::Building(trip.home)
    };
    let public_facilities_empty = |sim: &Simulation| {
        public_facilities.iter().all(|&garage| {
            sim.building_parking(garage)
                .is_some_and(|parking| parking.occupied + parking.held == 0)
        })
    };
    // Both ends of each district provide a legal crossing for the return walk.
    const DRAIN_LIMIT: usize = 80_000;
    const DRAIN_BATCH: usize = 200;
    let mut drain_ticks = 0;
    let mut home_followups = 0;
    let mut quiet_batches = 0;
    while drain_ticks < DRAIN_LIMIT {
        ctx.run_ticks_observed(DRAIN_BATCH as u64, |sim| sample_supply(sim, &mut supply));
        drain_ticks += DRAIN_BATCH;
        if public_facilities_empty(&ctx.sim) {
            // Closure can send a driver who already walked home to retrieve
            // a car at the garage door. Finish that driver's scripted trip
            // once closure has moved the car to a legal space.
            for trip in &offers {
                if !ctx.sim.world().vehicles.get(trip.car)
                    .is_some_and(|car| car.vehicle.state.is_parked())
                {
                    continue;
                }
                ctx.sim.world_mut_unchecked().edit_human(trip.driver, |human| {
                    let home = Destination::Building(trip.home);
                    if human.router.personal_car == Some(trip.car)
                        && human.location != Location::Building(trip.home)
                        && human.router.target_dest != Some(home)
                    {
                        human.router.go_to(home);
                        home_followups += 1;
                    }
                });
            }
        }
        let physical_clear =
            public_facilities_empty(&ctx.sim) && offers.iter().all(|trip| parked(&ctx.sim, trip));
        let returned_count = offers
            .iter()
            .filter(|trip| returned(&ctx.sim, trip))
            .count();
        if physical_clear && returned_count == offers.len() {
            break;
        }
        let quiet = physical_clear
            && offers.iter().all(|trip| {
                let human = &ctx.sim.world().humans[trip.driver];
                returned(&ctx.sim, trip)
                    || (human.router.target_dest == Some(Destination::Building(trip.home))
                        && human.router.cur_step().is_none()
                        && human.it.has_ended(0.0))
            });
        quiet_batches = if quiet { quiet_batches + 1 } else { 0 };
        if drain_ticks >= 12_000 && quiet_batches >= 5 && returned_count * 100 >= offers.len() * 90
        {
            break;
        }
    }
    let returned_count = offers
        .iter()
        .filter(|trip| returned(&ctx.sim, trip))
        .count();
    let parked_count = offers.iter().filter(|trip| parked(&ctx.sim, trip)).count();
    if !public_facilities_empty(&ctx.sim)
        || parked_count != offers.len()
        || returned_count * 100 < offers.len() * 90
    {
        return Err(format!(
            "drain at tick {drain_ticks} reached {returned_count}/{} doors with {parked_count} original cars parked and public parking clear = {}",
            offers.len(), public_facilities_empty(&ctx.sim)
        ));
    }
    let unfinished: Vec<_> = offers
        .iter()
        .filter(|trip| !returned(&ctx.sim, trip))
        .map(|trip| (trip.driver, trip.car, trip.home))
        .collect();
    if !unfinished.is_empty() {
        println!("PathPark scale unfinished return walks: {unfinished:?}");
    }
    if home_followups > offers.len() / 20 {
        return Err(format!(
            "mixed-supply drain needed {home_followups} follow-up requests for {} drivers",
            offers.len()
        ));
    }
    let mut drain = BTreeMap::from([
        ("tagged_offers".to_owned(), (offers.len() * 2) as f64),
        ("tagged_returns".to_owned(), returned_count as f64),
        (
            "unfinished_return_walks".to_owned(),
            unfinished.len() as f64,
        ),
        ("drain_ticks".to_owned(), drain_ticks as f64),
        ("home_followups".to_owned(), home_followups as f64),
    ]);
    let mut misses = Vec::new();
    for source in Supply::ALL {
        let index = source as usize;
        let holds = supply.iter().filter(|trip| trip.holds[index]).count();
        let occupied = supply.iter().filter(|trip| trip.occupancies[index]).count();
        let departed = supply.iter().filter(|trip| trip.departures[index]).count();
        for (metric, value) in [
            ("holds", holds),
            ("occupancies", occupied),
            ("departures", departed),
        ] {
            drain.insert(format!("tagged_{}_{metric}", source.label()), value as f64);
        }
        if holds < 50 || occupied < 40 || departed < 30 {
            misses.push(format!(
                "{} has {holds} holds, {occupied} occupancies, {departed} rear-clear departures",
                source.label()
            ));
        }
    }
    println!("PathPark scale drain: {drain_ticks} ticks, {home_followups} follow-up requests, supply {drain:?}");
    ctx.sample_perf_with_extra("drained", POPULATION, drain, true);
    if !misses.is_empty() {
        return Err(format!("mixed-supply floors missed: {}", misses.join(", ")));
    }
    Ok(())
}

fn freeze_scale_drivers(ctx: &mut ScenarioCtx, offers: &[ScaleTrip]) {
    for trip in offers {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(trip.driver, |person| {
                person.decision.wait = u8::MAX;
            });
    }
}

pub fn generate(root: &Path) -> Result<(), String> {
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    let mut ctx = ScenarioCtx::new(OPTIONS);
    setup(&mut ctx.sim, false)?;
    super::demo_cities::publish(&ctx.sim, &root, "pathpark_growables", "PathPark Garages")
}

/// Compare new household parking on the same opening parcels of a published city.
pub fn audit(root: &Path, slot: &simulation::SlotId) -> Result<(), String> {
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    ScenarioCtx::initialize();
    let mut candidate =
        Simulation::load_from_slot(&root, slot).map_err(|error| error.to_string())?;
    let stock_facilities = candidate
        .map()
        .buildings()
        .keys()
        .filter(|&id| candidate.building_parking(id).is_some())
        .count();
    let lots: Vec<_> = candidate.map().lots().keys().collect();
    let zone = ZoneCell {
        kind: ZoneKind::Residential,
        density: Density::Low,
    };
    let mut houses = Vec::new();
    let mut built = Vec::new();
    for lot in lots.into_iter().take(512) {
        if let Ok(site) = candidate.place_zoned_growable(lot, zone) {
            built.push((lot, site.building));
            if candidate.building_parking(site.building).is_some() {
                houses.push(site.building);
                if houses.len() == 16 {
                    break;
                }
            }
        }
    }
    if houses.is_empty() {
        return Err(format!(
            "{slot}: no new driveway fit in the first 512 vacant parcels"
        ));
    }

    let mut control = Simulation::load_from_slot(&root, slot).map_err(|error| error.to_string())?;
    for (lot, expected) in built {
        let building = control
            .place_zoned_growable(lot, zone)
            .map_err(|error| format!("control growth on {lot:?}: {error:?}"))?
            .building;
        if building != expected {
            return Err("opening building IDs differ".into());
        }
    }
    for &building in &houses {
        control.apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building,
            open: false,
        }]);
    }
    let observe = |mut sim: Simulation| -> Result<serde_json::Value, String> {
        let mut drivers = Vec::new();
        for &building in &houses {
            let human = spawn_human(&mut sim, building).ok_or("could not fill a new home")?;
            if let Some(car) = sim.world().humans[human].router.personal_car {
                drivers.push(car);
            }
        }
        let driveway_cars: u16 = houses
            .iter()
            .filter_map(|&building| sim.building_parking(building))
            .map(|parking| parking.occupied)
            .sum();
        let curb_cars = drivers.len() - usize::from(driveway_cars);
        let mut city = ScenarioCtx::from_sim(sim);
        city.run_ticks(500);
        let stranded = drivers
            .iter()
            .filter(|&&car| {
                city.sim.world().vehicles.get(car).is_none_or(|car| {
                    !car.vehicle.state.is_parked() && car.it.is_none_or_wait() && car.speed.0 < 0.05
                })
            })
            .count();
        Ok(serde_json::json!({
            "cars": drivers.len(),
            "opening_driveway_cars": driveway_cars,
            "opening_curb_cars": curb_cars,
            "cars_missing_or_without_route_after_500_ticks": stranded,
        }))
    };
    let with_parking = observe(candidate)?;
    let closed = observe(control)?;
    println!(
        "{}",
        serde_json::json!({
            "slot": slot.as_str(),
            "stock_facilities": stock_facilities,
            "new_houses": houses.len(),
            "property_parking": with_parking,
            "driveways_closed": closed,
        })
    );
    Ok(())
}
