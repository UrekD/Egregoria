use geom::vec2;
use prototypes::{Tick, TICKS_PER_MINUTE};
use simulation::economy::Government;
use simulation::map::BuildingKind;
use simulation::souls::demolition::{demolition_quote, demolition_status, SiteRemnantCause};
use simulation::souls::events::{fire_incident, offer_fixture_fire, ServiceIncidents};
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SimulationOptions, SimulationReplayLoader, SnapshotRevision};

use crate::builders::{fill_houses, grant_money, grid_city};
use crate::{Scenario, ScenarioCtx};

const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 1,
    save_replay: false,
    ..SimulationOptions::LEGACY
};

pub const RUINS: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_ruins",
        run: run_ruins,
    },
    OPTIONS,
);

pub const RESUME: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_fire_resume",
        run: run_resume,
    },
    OPTIONS,
);

fn run_ruins(ctx: &mut ScenarioCtx) -> Result<(), String> {
    grid_city(ctx, vec2(150.0, 150.0), 2, 2, 160.0);
    let house = *fill_houses(ctx, 1).first().ok_or("missing fixture house")?;
    let center = ctx.sim.map().get(house).unwrap().obb.center();
    let lot_count = ctx.sim.map().lots().len();
    if !offer_fixture_fire(&ctx.sim, house) {
        return Err("fixture fire rejected".into());
    }
    ctx.run_ticks(85 * TICKS_PER_MINUTE + 1);
    let city = &ctx.sim;
    if city.map().get(house).map(|building| building.kind) != Some(BuildingKind::SiteRemnant)
        || demolition_status(city, house).map(|status| status.cause) != Some(SiteRemnantCause::Fire)
        || city.map().lots().len() != lot_count
        || city.map().ruin_land_value_penalty(center) <= 0.0
    {
        return Err(
            "an untreated incident must leave an occupied ruin and lower land value".into(),
        );
    }
    let quote = demolition_quote(city, house)
        .map_err(|e| format!("{e:?}"))?
        .quote;
    let command = WorldCommand::MapStartDemolition { site: house, quote };
    let balance = city.read::<Government>().money;
    ctx.sim.apply_commands([&command, &command]);
    if ctx.sim.read::<Government>().money != balance - quote.price {
        return Err("a repeated demolition command should charge once".into());
    }
    ctx.run_ticks(u64::from(quote.minutes) * TICKS_PER_MINUTE - 1);
    if ctx.sim.map().get(house).is_none() {
        return Err("site was cleared before paid work finished".into());
    }
    ctx.run_ticks(1);
    if ctx.sim.map().get(house).is_some()
        || ctx.sim.map().ruin_land_value_penalty(center) != 0.0
        || ctx.sim.map().lots().len() != lot_count + 1
    {
        return Err("paid clearance did not restore available land and frontage".into());
    }
    ctx.sample("cleared");
    Ok(())
}

fn run_resume(ctx: &mut ScenarioCtx) -> Result<(), String> {
    grid_city(ctx, vec2(150.0, 150.0), 2, 2, 160.0);
    let site = *fill_houses(ctx, 1).first().ok_or("missing fixture house")?;
    if !offer_fixture_fire(&ctx.sim, site)
        || ctx.sim.read::<ServiceIncidents>().pending_count() != 1
    {
        return Err("fixture must save a pending fire".into());
    }
    let mut pending = ScenarioCtx::from_sim(round_trip(&ctx.sim)?);
    pending.run_ticks(1);
    ctx.run_ticks(1);
    if pending.sim.hashes() != ctx.sim.hashes() || fire_incident(&ctx.sim, site).is_none() {
        return Err("pending offer changed across resume".into());
    }

    let quote = demolition_quote(&ctx.sim, site)
        .map_err(|e| format!("{e:?}"))?
        .quote;
    let commands = [WorldCommand::MapStartDemolition { site, quote }];
    let mut replayed = round_trip(&ctx.sim)?;
    let mut resumed = ScenarioCtx::from_sim(round_trip(&ctx.sim)?);
    let origin = Tick(ctx.sim.get_tick());
    let steps = (90 + u64::from(quote.minutes)) * TICKS_PER_MINUTE;
    let mut replay = SimulationReplayLoader::checkpoint_playback(
        origin,
        SnapshotRevision::new(1),
        steps,
        [(0, false, &commands[..])],
    )
    .map_err(|e| e.to_string())?;
    ctx.sim.apply_commands(&commands);
    resumed.sim.apply_commands(&commands);
    ctx.run_ticks(steps);
    resumed.run_ticks(steps);
    let mut schedule = Simulation::schedule();
    while !replay.advance_tick(&mut replayed, &mut schedule) {}
    let expected = ctx.sim.hashes();
    for (arm, city) in [("resume", &resumed.sim), ("replay", &replayed)] {
        let actual = city.hashes();
        if let Some(resource) = expected.keys().find(|&name| expected[name] != actual[name]) {
            return Err(format!(
                "{arm} changed {resource} at tick {} (expected tick {})",
                city.get_tick(),
                ctx.sim.get_tick()
            ));
        }
    }
    if ctx.sim.map().get(site).is_some() {
        return Err("clearance never finished".into());
    }
    ctx.sample("resumed_and_replayed");
    resume_response()?;
    Ok(())
}

fn round_trip(sim: &Simulation) -> Result<Simulation, String> {
    let bytes = sim
        .encode_slot(SnapshotRevision::new(1))
        .map_err(|e| e.to_string())?
        .into_snapshot_bytes();
    Simulation::load_compressed_snapshot(&bytes, 256 * 1024 * 1024).map_err(|e| e.to_string())
}

pub const RESPONSE: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_fire_response",
        run: run_response,
    },
    SimulationOptions {
        terrain_size: 2,
        ..OPTIONS
    },
);

pub const REPAIR: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_fire_repair",
        run: run_repair,
    },
    RESPONSE.1,
);

struct ResponseDistrict {
    station: simulation::map::BuildingID,
    target: simulation::map::BuildingID,
    spare_target: simulation::map::BuildingID,
    staff: Vec<simulation::HumanID>,
    return_road: simulation::map::RoadID,
    return_endpoints: [geom::Vec3; 2],
    pattern: simulation::map::LanePattern,
}

fn response_district(
    ctx: &mut ScenarioCtx,
    origin: geom::Vec2,
    station_x: f32,
    one_way: bool,
) -> Result<ResponseDistrict, String> {
    use geom::{vec3, Vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID};
    use simulation::map::{LanePatternBuilder, ProjectFilter};
    use simulation::souls::desire::{Work, WorkKind};
    use simulation::souls::goods_company::company_soul;
    use simulation::souls::human::spawn_human;
    use simulation::{SoulID, World};

    let pattern = LanePatternBuilder::default()
        .speed_limit(12.5)
        .one_way(one_way)
        .build();
    let corners = [
        vec3(540.0, 95.0, 0.0),
        vec3(30.0, 95.0, 0.0),
        vec3(30.0, 255.0, 0.0),
        vec3(540.0, 255.0, 0.0),
        vec3(540.0, 95.0, 0.0),
    ]
    .map(|point| point + origin.z0());
    let roads = {
        let mut map = ctx.sim.write::<simulation::map::Map>();
        let mut roads = Vec::new();
        for pair in corners.windows(2) {
            let from = map.project(pair[0], 0.0, ProjectFilter::ALL);
            let to = map.project(pair[1], 0.0, ProjectFilter::ALL);
            let outcome = map.try_connect(from, to, None, &pattern).map_err(|error| {
                format!(
                    "response circuit {origin:?}: {:?} to {:?}: {error}",
                    pair[0], pair[1]
                )
            })?;
            roads.push(outcome.road);
        }
        roads
    };
    let proto = GoodsCompanyID::new("fire-station");
    let door = BuildingGen::NoWalkway {
        door_pos: vec2(0.0, -9.0),
    };
    let offset = if one_way { 27.0 } else { 32.0 };
    let station_pos = OBB::new(origin + vec2(station_x, 95.0 + offset), Vec2::Y, 20.0, 20.0);
    ctx.sim
        .apply_commands([&WorldCommand::MapBuildSpecialBuilding {
            pos: station_pos,
            kind: BuildingKind::GoodsCompany(proto),
            gen: door,
            zone: None,
            connected_road: Some(roads[0]),
        }]);
    let station = ctx
        .sim
        .map()
        .buildings()
        .values()
        .find(|site| {
            site.kind == BuildingKind::GoodsCompany(proto)
                && site.obb.corners == station_pos.corners
        })
        .map(|site| site.id)
        .ok_or_else(|| {
            format!(
                "station placement rejected: {:?}",
                ctx.sim
                    .read::<simulation::world_command::CommandOutcomes>()
                    .last
            )
        })?;
    let Some(SoulID::GoodsCompany(operator)) = company_soul(&mut ctx.sim, station, proto) else {
        return Err("station has no service operator".into());
    };
    let solar = GoodsCompanyID::new("solar-panel");
    let generator = ctx
        .sim
        .write::<simulation::map::Map>()
        .build_special_building(
            &OBB::new(origin + vec2(574.0, 180.0), Vec2::Y, 20.0, 20.0),
            BuildingKind::GoodsCompany(solar),
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            None,
            Some(roads[3]),
        )
        .ok_or("could not place the response district power supply")?;
    company_soul(&mut ctx.sim, generator, solar).ok_or("no power operator")?;

    let mut staff = Vec::new();
    for index in 0..8 {
        let home = ctx
            .sim
            .write::<simulation::map::Map>()
            .build_special_building(
                &OBB::new(
                    origin + vec2(100.0 + index as f32 * 48.0, 65.0),
                    Vec2::Y,
                    20.0,
                    20.0,
                ),
                BuildingKind::House,
                door,
                None,
                Some(roads[0]),
            )
            .ok_or("could not build a crew home")?;
        ctx.sim
            .write::<simulation::map_dynamic::BuildingInfos>()
            .insert(home);
        simulation::souls::residence::on_house_built(&ctx.sim, home);
        let id = spawn_human(&mut ctx.sim, home).ok_or("could not create a crew member")?;
        ctx.sim.world_mut_unchecked().edit_human(id, |human| {
            human.work = Some(Work::new(station, WorkKind::Worker, 0.0));
            human.router = simulation::map_dynamic::Router::new(None);
        });
        staff.push(id);
    }
    let world: &mut World = ctx.sim.world_mut_unchecked();
    world.companies[operator].workers.0.clone_from(&staff);
    let site = |x| {
        ctx.sim
            .write::<simulation::map::Map>()
            .build_special_building(
                &OBB::new(origin + vec2(x, 127.0), Vec2::Y, 20.0, 20.0),
                BuildingKind::House,
                door,
                None,
                Some(roads[0]),
            )
            .ok_or_else(|| format!("could not place incident site at {x}"))
    };
    let target = site(126.0)?;
    let spare_target = if one_way { site(197.0)? } else { target };
    Ok(ResponseDistrict {
        station,
        target,
        spare_target,
        staff,
        return_road: roads[2],
        return_endpoints: [corners[2], corners[3]],
        pattern,
    })
}

fn commission_shift(city: &mut Simulation, district: &ResponseDistrict, minutes: u64) {
    use prototypes::GameTime;

    city.apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(
        ((minutes + 16 * 60) % (24 * 60)) * TICKS_PER_MINUTE,
    )))]);
    seat_fixture_crew(city, district);
}

fn seat_fixture_crew(city: &mut Simulation, district: &ResponseDistrict) {
    use simulation::map_dynamic::{Itinerary, ParkingManagement};
    use simulation::transportation::{Location, TransportGrid};

    let mut parking = std::mem::take(&mut *city.write::<ParkingManagement>());
    for &id in &district.staff {
        let collider = city.get(id).and_then(|human| human.collider);
        if let Some(collider) = collider {
            city.write::<TransportGrid>().remove_maintain(collider.0);
        }
        city.world_mut_unchecked().edit_human(id, |human| {
            human.router.cancel_route(&mut parking);
            human.collider = None;
            human.location = Location::Building(district.station);
            human.speed.0 = 0.0;
            human.it = Itinerary::NONE;
            human.decision = Default::default();
            human.decision.wait = 100;
        });
    }
    *city.write::<ParkingManagement>() = parking;
}

fn response_once(
    ctx: &mut ScenarioCtx,
    district: &ResponseDistrict,
    site: simulation::map::BuildingID,
) -> Result<(u64, bool), String> {
    use simulation::souls::service_dispatch::{
        fire_response_blocker, service_station, FireFleet, FireResponseEventKind,
    };

    let before = ctx.sim.read::<FireFleet>().events().count();
    let offered = simulation::diagnostics::elapsed_tick(&ctx.sim).0;
    if !offer_fixture_fire(&ctx.sim, site) {
        return Err("design-load offer rejected".into());
    }
    let mut arrived = None;
    for _ in 0..28_000 {
        ctx.run_ticks(1);
        if let Some(event) = ctx
            .sim
            .read::<FireFleet>()
            .events()
            .skip(before)
            .find(|event| event.site == site && event.kind == FireResponseEventKind::Returned)
        {
            arrived = Some(event.incident);
            break;
        }
    }
    let fleet = ctx.sim.read::<FireFleet>();
    let events: Vec<_> = fleet
        .events()
        .skip(before)
        .filter(|event| event.site == site)
        .collect();
    let Some(id) = arrived else {
        let station = service_station(&ctx.sim, district.station);
        let truck = station.and_then(|station| ctx.sim.get(station.truck));
        return Err(format!(
            "response did not return: {station:?}; truck={:?}; {:?}; {events:?}",
            truck.map(|truck| (truck.trans.pos, truck.speed.0, &truck.it)),
            fire_response_blocker(&ctx.sim, site)
        ));
    };
    let tick = |kind| {
        events
            .iter()
            .find(|event| event.incident == id && event.kind == kind)
            .map(|event| event.tick.0)
    };
    tick(FireResponseEventKind::Assigned).ok_or("missing assignment")?;
    let arrived = tick(FireResponseEventKind::Arrived).ok_or("missing complete-crew arrival")?;
    if tick(FireResponseEventKind::Boarded).is_none() {
        return Err("response skipped physical boarding".into());
    }
    Ok((
        arrived - offered,
        tick(FireResponseEventKind::Suppressed).is_some(),
    ))
}

fn run_response(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let mut arrival = Vec::with_capacity(40);
    let mut extinguished = 0;
    let mut final_city = None;
    for (index, x) in [342.0, 405.0].into_iter().enumerate() {
        let mut base = ScenarioCtx::from_sim(
            Simulation::new_authored_flat(2, 0).map_err(|error| error.to_string())?,
        );
        let district = response_district(&mut base, geom::Vec2::ZERO, x, false)?;
        // Register independent offers against the same commissioned opening.
        // Each call has the whole bay and its own shift and congestion window.
        let mut last = None;
        for minutes in [1, 479, 480, 610, 959, 960, 1150, 1439]
            .into_iter()
            .cycle()
            .take(20)
        {
            let mut city = ScenarioCtx::from_sim(round_trip(&base.sim)?);
            commission_shift(&mut city.sim, &district, minutes);
            let (ticks, suppressed) = response_once(&mut city, &district, district.target)?;
            arrival.push(ticks);
            extinguished += usize::from(suppressed);
            last = Some(city);
        }
        let mut last = last.expect("the fixture registered twenty calls");
        last.sample(if index == 0 {
            "short_layout"
        } else {
            "long_layout"
        });
        ctx.samples.extend(last.samples);
        final_city = Some(last.sim);
    }
    ctx.sim = final_city.expect("the fixture registered two layouts");
    arrival.sort_unstable();
    let p95 = arrival[37];
    log_response(&arrival, extinguished);
    if p95 > 25 * TICKS_PER_MINUTE || extinguished < 38 {
        return Err(format!(
            "design-load gate failed with p95 {p95} ticks and {extinguished}/40 extinguished"
        ));
    }
    Ok(())
}

fn log_response(arrival: &[u64], extinguished: usize) {
    eprintln!("P9 design load: {} complete round trips, {extinguished} extinguished, arrival ticks {arrival:?}", arrival.len());
}

fn run_repair(ctx: &mut ScenarioCtx) -> Result<(), String> {
    use simulation::souls::service_dispatch::fire_response_blocker;

    ctx.sim = Simulation::new_authored_flat(2, 0).map_err(|error| error.to_string())?;
    let district = response_district(ctx, geom::Vec2::ZERO, 362.0, true)?;
    commission_shift(&mut ctx.sim, &district, 10 * 60);
    response_once(ctx, &district, district.target)?;
    ctx.sim
        .apply_commands([&WorldCommand::MapRemoveRoad(district.return_road)]);
    if ctx.sim.map().get(district.return_road).is_some() {
        return Err("return circuit preflight refused an unoccupied edit".into());
    }
    commission_shift(&mut ctx.sim, &district, 10 * 60);
    offer_fixture_fire(&ctx.sim, district.target);
    ctx.run_ticks(2 * TICKS_PER_MINUTE);
    if fire_response_blocker(&ctx.sim, district.target).is_none() {
        return Err("the disconnected return circuit must show a response blocker".into());
    }
    ctx.run_ticks(84 * TICKS_PER_MINUTE);
    if ctx
        .sim
        .map()
        .get(district.target)
        .is_none_or(|site| site.kind != BuildingKind::SiteRemnant)
    {
        return Err("the unserved alarm must leave a ruin".into());
    }
    let repaired = repair_return_road(&ctx.sim, &district)?;
    commission_shift(&mut ctx.sim, &district, 10 * 60);
    response_once(ctx, &district, district.spare_target)?;
    ctx.sample("repaired_and_returned");
    recover_held_response(ctx, &district, repaired)?;
    Ok(())
}

fn repair_return_road(
    city: &Simulation,
    district: &ResponseDistrict,
) -> Result<simulation::map::RoadID, String> {
    use simulation::map::ProjectFilter;
    let mut map = city.write::<simulation::map::Map>();
    let [from, to] = district
        .return_endpoints
        .map(|point| map.project(point, 0.0, ProjectFilter::ALL));
    map.make_connection(from, to, None, &district.pattern)
        .map(|(_, road)| road)
        .ok_or_else(|| "could not repair return circuit".to_string())
}

fn recover_held_response(
    ctx: &mut ScenarioCtx,
    district: &ResponseDistrict,
    road: simulation::map::RoadID,
) -> Result<(), String> {
    use simulation::map::TraverseKind;
    use simulation::souls::service_dispatch::{
        service_station, FireResponseBlocker, ServiceTripPhase,
    };

    commission_shift(&mut ctx.sim, district, 10 * 60);
    if !offer_fixture_fire(&ctx.sim, district.spare_target) {
        return Err("could not offer recovery drill".into());
    }
    let center = (district.return_endpoints[0] + district.return_endpoints[1]) * 0.5;
    let mut cut = false;
    for _ in 0..28_000 {
        ctx.run_ticks(1);
        let station = service_station(&ctx.sim, district.station).ok_or("lost recovery station")?;
        let vehicle = ctx.sim.get(station.truck).ok_or("lost response truck")?;
        let on_road = vehicle.it.get_travers().is_some_and(|traversal| {
            matches!(traversal.kind, TraverseKind::Lane(lane) if
                ctx.sim.map().get(lane).is_some_and(|lane| lane.parent == road))
        });
        if station.phase == Some(ServiceTripPhase::Returning)
            && on_road
            && vehicle.trans.pos.is_close(center, 80.0)
        {
            ctx.sim.apply_commands([&WorldCommand::MapRemoveRoad(road)]);
            cut = ctx.sim.map().get(road).is_none();
            break;
        }
    }
    if !cut {
        return Err("the recovery drill could not cut the truck's current route".into());
    }
    ctx.run_ticks(31 * TICKS_PER_MINUTE);
    let held = service_station(&ctx.sim, district.station).ok_or("lost held station")?;
    if held.blocker != Some(FireResponseBlocker::Held)
        || ctx
            .sim
            .get(held.truck)
            .is_none_or(|truck| truck.collider.is_none())
    {
        return Err(format!(
            "the disconnected truck did not keep a physical hold: {held:?}"
        ));
    }
    let mut reopened = round_trip(&ctx.sim)?;
    repair_return_road(&ctx.sim, district)?;
    repair_return_road(&reopened, district)?;
    let mut schedule = Simulation::schedule();
    for _ in 0..28_000 {
        ctx.run_ticks(1);
        reopened.tick(&mut schedule, []);
        if service_station(&ctx.sim, district.station).is_some_and(|station| station.recovered == 1)
        {
            break;
        }
    }
    let station = service_station(&ctx.sim, district.station).ok_or("lost recovered station")?;
    if station.recovered != 1 || station.phase.is_some() || ctx.sim.hashes() != reopened.hashes() {
        return Err(format!(
            "truck recovery or checkpoint continuation failed: {station:?}"
        ));
    }
    ctx.sample("held_then_recovered");
    Ok(())
}

fn resume_response() -> Result<(), String> {
    use simulation::souls::service_dispatch::{service_station, ServiceTripPhase};

    let mut ctx = ScenarioCtx::from_sim(
        Simulation::new_authored_flat(2, 0).map_err(|error| error.to_string())?,
    );
    let district = response_district(&mut ctx, geom::Vec2::ZERO, 342.0, false)?;
    commission_shift(&mut ctx.sim, &district, 610);
    if !offer_fixture_fire(&ctx.sim, district.target) {
        return Err("could not start the response checkpoint fixture".into());
    }
    let cuts = [
        ServiceTripPhase::Boarding,
        ServiceTripPhase::Working,
        ServiceTripPhase::Returning,
    ];
    let mut saved = Vec::new();
    for _ in 0..28_000 {
        ctx.run_ticks(1);
        let view = service_station(&ctx.sim, district.station).ok_or("lost response station")?;
        if let Some(&phase) = cuts.get(saved.len()) {
            if view.phase == Some(phase) {
                saved.push((phase, round_trip(&ctx.sim)?));
            }
        }
        if view.returned == 1 {
            break;
        }
    }
    if saved.len() != cuts.len()
        || service_station(&ctx.sim, district.station).is_none_or(|station| station.returned != 1)
    {
        return Err("response did not produce all boarding, work and return checkpoints".into());
    }
    let end = ctx.sim.get_tick();
    let expected = ctx.sim.hashes();
    for (phase, mut replayed) in saved {
        let origin = replayed.get_tick();
        let steps = end - origin;
        let mut resumed = ScenarioCtx::from_sim(round_trip(&replayed)?);
        resumed.run_ticks(steps);
        let mut schedule = Simulation::schedule();
        let mut replay = SimulationReplayLoader::checkpoint_playback(
            Tick(origin),
            SnapshotRevision::new(1),
            steps,
            [],
        )
        .map_err(|error| error.to_string())?;
        while !replay.advance_tick(&mut replayed, &mut schedule) {}
        for (branch, city) in [("resume", &resumed.sim), ("replay", &replayed)] {
            let actual = city.hashes();
            if let Some(resource) = expected.keys().find(|&key| expected[key] != actual[key]) {
                return Err(format!(
                    "{phase:?} {branch} diverged in {resource} by tick {end}"
                ));
            }
        }
        eprintln!("P9 response {phase:?} at tick {origin}: physical continuation and replay matched at {end}");
    }
    Ok(())
}

pub const SCALE: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_fire_scale",
        run: run_scale,
    },
    SimulationOptions {
        terrain_size: 50,
        preset: simulation::MapPreset::Flat,
        starting_money_bucks: 2_000_000,
        ..RESPONSE.1
    },
);

fn run_scale(ctx: &mut ScenarioCtx) -> Result<(), String> {
    use simulation::souls::service_dispatch::{service_station, FireFleet, FireResponseEventKind};
    use std::time::Instant;

    let districts = prepare_scale_city(ctx)?;
    commission_shift(&mut ctx.sim, &districts[0], 610);
    for district in districts.iter().skip(1) {
        seat_fixture_crew(&mut ctx.sim, district);
    }

    ctx.sample("opening_64_aprons");
    let normal: Vec<_> = districts.iter().take(4).collect();
    let burst: Vec<_> = districts.iter().skip(4).take(40).collect();
    offer_scale_incidents(&ctx.sim, &normal)?;
    let mut measurements = Vec::new();
    let mut cutoff = 0.0_f64;
    measurements.push(ctx.measure_fire_window("normal", 18_000));
    if normal.iter().any(|district| {
        service_station(&ctx.sim, district.station).is_none_or(|station| station.returned == 0)
    }) {
        return Err("the scale normal cohort did not return physically".into());
    }

    let start = Instant::now();
    let edit = WorldCommand::MapRemoveRoad(districts[63].return_road);
    ctx.sim.apply_commands([&edit]);
    cutoff = cutoff.max(start.elapsed().as_secs_f64() * 1000.0);
    if ctx.sim.map().get(districts[63].return_road).is_some() {
        return Err("scale fixture edit did not change the network".into());
    }
    measurements.push(ctx.measure_fire_window("first_tick_after_edit", 1));
    for district in &burst {
        seat_fixture_crew(&mut ctx.sim, district);
    }
    offer_scale_incidents(&ctx.sim, &burst)?;
    let start = Instant::now();
    let bytes = ctx
        .sim
        .encode_slot(SnapshotRevision::new(1))
        .map_err(|error| error.to_string())?
        .into_snapshot_bytes();
    let save_ms = start.elapsed().as_secs_f64() * 1000.0;
    let start = Instant::now();
    ctx.sim = Simulation::load_compressed_snapshot(&bytes, 512 * 1024 * 1024)
        .map_err(|error| error.to_string())?;
    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    measurements.push(ctx.measure_fire_window("first_tick_after_reload", 1));
    measurements.push(ctx.measure_fire_window("burst_and_drain", 24_000));

    let fleet = ctx.sim.read::<FireFleet>();
    let site_returns = burst
        .iter()
        .filter(|district| {
            fleet.events().any(|event| {
                event.station == district.station
                    && event.site == district.target
                    && event.kind == FireResponseEventKind::Returned
            })
        })
        .count();
    if site_returns != 40
        || districts.iter().any(|district| {
            service_station(&ctx.sim, district.station)
                .is_none_or(|station| station.phase.is_some())
        })
    {
        return Err(format!(
            "scale burst drained with {site_returns}/40 physical returns or outstanding claims"
        ));
    }
    let counters = ctx.sim.read::<ServiceIncidents>().counters;
    let peak_delay = fleet
        .events()
        .filter(|event| event.kind == FireResponseEventKind::Assigned)
        .filter_map(|event| {
            ctx.sim
                .read::<simulation::souls::events::Fires>()
                .events_since(0)
                .find(|alarm| {
                    alarm.building == event.site
                        && alarm.kind == simulation::souls::events::FireEventKind::Ignited
                })
                .map(|alarm| event.tick.0.saturating_sub(alarm.tick.0))
        })
        .max()
        .unwrap_or(0);
    drop(fleet);
    ctx.sample("drained");
    ctx.samples.last_mut().unwrap().extra.extend([
        ("commissioned_aprons".into(), 64.0),
        ("burst_physical_returns".into(), site_returns as f64),
        ("overflow".into(), counters.overflow as f64),
        ("expired".into(), counters.expired as f64),
        ("peak_dispatch_delay_ticks".into(), peak_delay as f64),
        ("encode_ms".into(), save_ms),
        ("cold_load_ms".into(), load_ms),
        ("road_edit_ms".into(), cutoff),
    ]);
    if counters.overflow != 0
        || peak_delay > TICKS_PER_MINUTE
        || measurements
            .into_iter()
            .any(|(p99, peak)| p99 > 1.0 || peak > 5.0)
    {
        return Err("P9 scale exceeded work, delay or overflow gates".into());
    }
    Ok(())
}

fn prepare_scale_city(ctx: &mut ScenarioCtx) -> Result<Vec<ResponseDistrict>, String> {
    ctx.sim = Simulation::new_authored_flat(42, 0).map_err(|error| error.to_string())?;
    grant_money(ctx, prototypes::Money::new_bucks(3_000_000));
    // Keep the housing workload on a separate grid so its streets cannot
    // overlap the fixed response circuits or provide bypasses around a cut.
    let housing = grid_city(ctx, vec2(9000.0, 8000.0), 32, 32, 160.0);
    if housing.roads.len() != 2 * 32 * 31 {
        return Err(format!(
            "scale housing grid built {} of 1984 roads",
            housing.roads.len()
        ));
    }
    let houses = fill_houses(ctx, 10_000);
    if houses.len() != 10_000 {
        return Err(format!(
            "scale built {} of 10,000 eligible houses",
            houses.len()
        ));
    }
    let mut districts = Vec::new();
    for i in 0..64 {
        let origin = vec2(
            160.0 + (i % 8) as f32 * 750.0,
            160.0 + (i / 8) as f32 * 475.0,
        );
        districts.push(response_district(ctx, origin, 342.0, false)?);
    }
    Ok(districts)
}

fn offer_scale_incidents(sim: &Simulation, districts: &[&ResponseDistrict]) -> Result<(), String> {
    for district in districts {
        if !offer_fixture_fire(sim, district.target) {
            return Err(format!("scale offer refused at {:?}", district.target));
        }
    }
    Ok(())
}

pub const SOAK: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_fire_soak",
        run: run_soak,
    },
    SCALE.1,
);

fn run_soak(ctx: &mut ScenarioCtx) -> Result<(), String> {
    use simulation::souls::service_dispatch::{fire_apron_quote, service_station};

    let root = std::env::var_os("EGR_P9_LESCE_ROOT")
        .map(std::path::PathBuf::from)
        .ok_or("set EGR_P9_LESCE_ROOT to the packaged Lesce slot root")?;
    let slot = simulation::SlotId::new("lesce_osm").map_err(|error| error.to_string())?;
    let mut reference = ScenarioCtx::from_sim(
        Simulation::load_from_slot(&root, &slot).map_err(|error| error.to_string())?,
    );
    if reference.sim.get_tick() != 1048 || reference.sim.world().humans.len() != 2903 {
        return Err("the release soak requires the packaged 0.12.0 Lesce fixture".into());
    }
    let stations: Vec<_> = reference
        .sim
        .map()
        .buildings()
        .values()
        .filter(|building| {
            building.kind.as_goods_company().is_some_and(|id| {
                prototypes::try_prototype(prototypes::ServiceID::from(id))
                    .is_some_and(|proto| proto.service_kind == prototypes::ServiceKind::Fire)
            })
        })
        .map(|building| building.id)
        .collect();
    let mut retrofitted = 0;
    for site in stations {
        if let Ok(quote) = fire_apron_quote(&reference.sim, site) {
            reference
                .sim
                .apply_commands([&WorldCommand::MapRetrofitFireApron { site, quote }]);
            retrofitted += usize::from(service_station(&reference.sim, site).is_some());
        }
    }
    let reference_rate = natural_week(&mut reference, "reference_week")?;
    reference
        .samples
        .last_mut()
        .unwrap()
        .extra
        .insert("retrofitted".into(), retrofitted as f64);
    ctx.samples.extend(reference.samples);

    prepare_scale_city(ctx)?;
    natural_week(ctx, "scale_week")?;
    if !(0.5..=2.0).contains(&reference_rate) {
        return Err(format!(
            "Lesce natural ignition rate {reference_rate:.3} per 1,000 site-days exceeded its band"
        ));
    }
    Ok(())
}

fn natural_week(ctx: &mut ScenarioCtx, label: &str) -> Result<f64, String> {
    use simulation::souls::events::{FireEventKind, Fires};
    use simulation::souls::service_dispatch::{FireFleet, FireResponseEventKind};

    const WEEK: u64 = 7 * prototypes::TICKS_PER_DAY;
    const SUFFIX: u64 = 2_000;
    ctx.run_ticks(WEEK - SUFFIX);
    let mut replayed = round_trip(&ctx.sim)?;
    let mut resumed = ScenarioCtx::from_sim(round_trip(&ctx.sim)?);
    let origin = Tick(ctx.sim.get_tick());
    ctx.run_ticks(SUFFIX);
    resumed.run_ticks(SUFFIX);
    let mut schedule = Simulation::schedule();
    let mut replay =
        SimulationReplayLoader::checkpoint_playback(origin, SnapshotRevision::new(1), SUFFIX, [])
            .map_err(|error| error.to_string())?;
    while !replay.advance_tick(&mut replayed, &mut schedule) {}
    let expected = ctx.sim.hashes();
    if expected != resumed.sim.hashes() || expected != replayed.hashes() {
        return Err(format!(
            "{label} resume or replay diverged at the end of the natural week"
        ));
    }

    let counters = ctx.sim.read::<ServiceIncidents>().counters;
    let rate = counters.admitted as f64 * 24_000.0 / counters.site_hours.max(1) as f64;
    let retained_losses = ctx
        .sim
        .read::<Fires>()
        .events_since(0)
        .filter(|event| event.kind == FireEventKind::Collapsed)
        .count();
    let retained_returns = ctx
        .sim
        .read::<FireFleet>()
        .events()
        .filter(|event| event.kind == FireResponseEventKind::Returned)
        .count();
    ctx.sample(label);
    ctx.samples.last_mut().unwrap().extra.extend([
        ("site_hours".into(), counters.site_hours as f64),
        ("spontaneous_offers".into(), counters.spontaneous as f64),
        ("admitted".into(), counters.admitted as f64),
        ("expired".into(), counters.expired as f64),
        ("overflow".into(), counters.overflow as f64),
        ("admissions_per_1000_site_days".into(), rate),
        ("retained_losses".into(), retained_losses as f64),
        ("retained_returns".into(), retained_returns as f64),
    ]);
    eprintln!(
        "P9 {label}: natural week checkpoint resume and replay matched through tick {}",
        ctx.sim.get_tick()
    );
    Ok(rate)
}
