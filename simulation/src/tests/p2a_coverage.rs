use super::{coverage_update, CoverageGrid};
use crate::map::{BuildingID, BuildingKind, LanePatternBuilder, ProjectFilter, RoadID};
use crate::map_dynamic::{electricity_flow_system, BuildingInfos, ElectricityFlow};
use crate::souls::goods_company::company_soul;
use crate::souls::residence::on_house_built;
use crate::tests::TestCtx;
use crate::world::HumanID;
use crate::{Simulation, SoulID};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, Vec2, OBB};
use prototypes::{BuildingGen, GameTime, GoodsCompanyID, ServiceKind, Tick};
use slotmapd::KeyData;

fn road(sim: &Simulation, from: Vec2, to: Vec2, lanes: LanePatternBuilder) -> RoadID {
    let mut map = sim.map_mut();
    let from = map.project(from.z0(), 0.0, ProjectFilter::INTER);
    let to = map.project(to.z0(), 0.0, ProjectFilter::INTER);
    map.make_connection(from, to, None, &lanes.build())
        .unwrap()
        .1
}

fn building(sim: &Simulation, road: RoadID, at: Vec2, kind: BuildingKind) -> BuildingID {
    let id = sim
        .map_mut()
        .build_special_building(
            &OBB::new(at, Vec2::X, 1.0, 1.0),
            kind,
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            None,
            Some(road),
        )
        .unwrap();
    sim.write::<BuildingInfos>().insert(id);
    id
}

// Coverage reads the resident and worker rosters. Synthetic IDs let these fixtures
// control occupancy and staffing without depending on immigration or job matching.
fn people(first: u32, count: u32) -> impl Iterator<Item = HumanID> {
    (first..first + count).map(|id| KeyData::from_ffi((1_u64 << 32) | u64::from(id)).into())
}

fn house(sim: &Simulation, road: RoadID, at: Vec2, first: u32, occupied: u32) -> BuildingID {
    let id = building(sim, road, at, BuildingKind::House);
    on_house_built(sim, id);
    let mut infos = sim.write::<BuildingInfos>();
    for human in people(first, occupied) {
        infos.add_resident(id, SoulID::Human(human));
    }
    id
}

fn company(sim: &mut Simulation, road: RoadID, at: Vec2, name: &str, staff: u32) -> BuildingID {
    let proto = GoodsCompanyID::new(name);
    let id = building(sim, road, at, BuildingKind::GoodsCompany(proto));
    company_soul(sim, id, proto).unwrap();
    set_staff(sim, id, staff);
    id
}

fn set_staff(sim: &mut Simulation, id: BuildingID, staff: u32) {
    let Some(SoulID::GoodsCompany(company)) = sim.read::<BuildingInfos>().owner(id) else {
        panic!("expected a company on {id:?}");
    };
    sim.world.companies[company].workers.0 = people(1, staff).collect();
}

fn refresh_power(sim: &mut Simulation) {
    let (world, resources) = sim.world_res();
    electricity_flow_system(world, resources);
}

fn set_tick(sim: &Simulation, tick: u64) {
    *sim.write::<GameTime>() = GameTime::new(Tick(tick));
}

fn update(sim: &mut Simulation, tick: u64) {
    set_tick(sim, tick);
    coverage_update(sim);
}

fn coverage(sim: &Simulation, building: BuildingID, kind: ServiceKind) -> f32 {
    sim.read::<CoverageGrid>().coverage(building, kind)
}

#[test]
fn p2a_coverage_respects_one_way_lanes_and_missing_sidewalks() {
    let mut sim = TestCtx::new_fast().g;
    let both = LanePatternBuilder::new().parking(false);
    let one_way = road(
        &sim,
        vec2(200.0, 400.0),
        vec2(600.0, 400.0),
        both.one_way(true),
    );
    let up = road(&sim, vec2(200.0, 400.0), vec2(200.0, 650.0), both);
    let down = road(&sim, vec2(600.0, 400.0), vec2(600.0, 650.0), both);
    let no_walk = road(
        &sim,
        vec2(600.0, 400.0),
        vec2(900.0, 400.0),
        both.sidewalks(false),
    );
    let beyond = road(&sim, vec2(900.0, 400.0), vec2(900.0, 650.0), both);
    let upstream = house(&sim, up, vec2(220.0, 500.0), 1, 1);
    let downstream = house(&sim, down, vec2(620.0, 500.0), 2, 1);
    let same_road_upstream = house(&sim, one_way, vec2(300.0, 380.0), 3, 1);
    let missing_sidewalk = house(&sim, no_walk, vec2(750.0, 420.0), 4, 1);
    let behind_missing_sidewalk = house(&sim, beyond, vec2(920.0, 500.0), 5, 1);
    company(&mut sim, one_way, vec2(400.0, 415.0), "fire-station", 8);
    company(&mut sim, one_way, vec2(420.0, 415.0), "school", 6);
    company(&mut sim, up, vec2(220.0, 575.0), "solar-panel", 0);
    refresh_power(&mut sim);

    update(&mut sim, 6600);
    assert!(coverage(&sim, downstream, ServiceKind::Fire) > 0.0);
    assert!(coverage(&sim, behind_missing_sidewalk, ServiceKind::Fire) > 0.0);
    assert_eq!(coverage(&sim, upstream, ServiceKind::Fire), 0.0);
    assert_eq!(coverage(&sim, same_road_upstream, ServiceKind::Fire), 0.0);

    update(&mut sim, 7800);
    assert!(coverage(&sim, downstream, ServiceKind::School) > 0.0);
    assert!(coverage(&sim, upstream, ServiceKind::School) > 0.0);
    assert!(coverage(&sim, same_road_upstream, ServiceKind::School) > 0.0);
    assert_eq!(coverage(&sim, missing_sidewalk, ServiceKind::School), 0.0);
    assert_eq!(
        coverage(&sim, behind_missing_sidewalk, ServiceKind::School),
        0.0
    );
}

#[test]
fn p2a_overlapping_providers_allocate_capacity_deterministically() {
    let mut sim = TestCtx::new_fast().g;
    let road = road(
        &sim,
        vec2(100.0, 400.0),
        vec2(600.0, 400.0),
        LanePatternBuilder::new(),
    );
    let first = house(&sim, road, vec2(300.0, 420.0), 1, 40);
    let second = house(&sim, road, vec2(300.0, 380.0), 41, 40);
    let third = house(&sim, road, vec2(440.0, 420.0), 81, 30);
    let near = company(&mut sim, road, vec2(300.0, 440.0), "fire-station", 1);
    let far = company(&mut sim, road, vec2(120.0, 415.0), "fire-station", 1);
    company(&mut sim, road, vec2(420.0, 440.0), "solar-panel", 0);
    assert!(first < second && near < far);

    // Put both receivers at the same road station and the same distance from the centerline.
    let (radius, capacity) = (1800.0, 50.0);
    {
        let mut map = sim.map_mut();
        for id in [first, second, near, far] {
            let door = map.buildings[id].door_pos;
            let z = map.get(road).unwrap().points().project_2d(door.xy()).z;
            map.buildings[id].door_pos.z = z;
        }
        // Reverse traversal order to ensure ties are resolved by BuildingID.
        map.roads[road].connected_buildings.reverse();
    }
    let near_score = 100.0 * (1.0 - 60.0 / radius);
    let far_score = 100.0 * (1.0 - 215.0 / radius);
    refresh_power(&mut sim);
    update(&mut sim, 6600);
    assert!((coverage(&sim, first, ServiceKind::Fire) - near_score).abs() < 0.001);
    let partial = near_score * (capacity - 40.0) / 40.0;
    assert!((coverage(&sim, second, ServiceKind::Fire) - partial).abs() < 0.001);
    assert_eq!(coverage(&sim, third, ServiceKind::Fire), 0.0);

    set_staff(&mut sim, far, 2);
    update(&mut sim, 12600);
    assert!((coverage(&sim, first, ServiceKind::Fire) - near_score).abs() < 0.001);
    assert!((coverage(&sim, second, ServiceKind::Fire) - far_score).abs() < 0.001);
}

#[test]
fn p2a_coverage_requires_staff_and_power_and_serves_growables() {
    let mut sim = TestCtx::new_fast().g;
    let road = road(
        &sim,
        vec2(100.0, 400.0),
        vec2(600.0, 400.0),
        LanePatternBuilder::new(),
    );
    let home = house(&sim, road, vec2(310.0, 420.0), 1, 1);
    let empty = house(&sim, road, vec2(320.0, 420.0), 2, 0);
    let park = company(&mut sim, road, vec2(200.0, 420.0), "park", 0);
    let shop = company(&mut sim, road, vec2(330.0, 420.0), "commercial-l1", 1);
    let power = company(&mut sim, road, vec2(410.0, 420.0), "coal-power-plant", 10);
    let network = sim.map().electricity.net_id(park).unwrap();

    refresh_power(&mut sim);
    assert!(!sim.read::<ElectricityFlow>().blackout(network));
    update(&mut sim, 9000);
    assert_eq!(coverage(&sim, home, ServiceKind::Park), 0.0);

    set_staff(&mut sim, park, 1);
    refresh_power(&mut sim);
    update(&mut sim, 15000);
    assert!(coverage(&sim, home, ServiceKind::Park) > 0.0);
    assert!(coverage(&sim, shop, ServiceKind::Park) > 0.0);
    assert_eq!(coverage(&sim, empty, ServiceKind::Park), 0.0);
    assert_eq!(coverage(&sim, power, ServiceKind::Park), 0.0);

    set_staff(&mut sim, power, 0);
    refresh_power(&mut sim);
    assert!(sim.read::<ElectricityFlow>().blackout(network));
    update(&mut sim, 21000);
    assert_eq!(coverage(&sim, home, ServiceKind::Park), 0.0);
    assert_eq!(coverage(&sim, shop, ServiceKind::Park), 0.0);
}

#[test]
fn p2a_coverage_survives_save_load() {
    let mut sim = TestCtx::new_fast().g;
    let road = road(
        &sim,
        vec2(100.0, 400.0),
        vec2(600.0, 400.0),
        LanePatternBuilder::new(),
    );
    let home = house(&sim, road, vec2(330.0, 420.0), 1, 2);
    let school = company(&mut sim, road, vec2(310.0, 420.0), "school", 6);
    company(&mut sim, road, vec2(290.0, 420.0), "fire-station", 8);
    company(&mut sim, road, vec2(210.0, 420.0), "solar-panel", 0);
    let mut schedule = Simulation::schedule();

    set_tick(&sim, 6599);
    sim.tick(&mut schedule, &[]);
    let fire = coverage(&sim, home, ServiceKind::Fire);
    assert!(fire > 0.0);
    assert_eq!(coverage(&sim, home, ServiceKind::School), 0.0);
    set_tick(&sim, 7799);
    sim.tick(&mut schedule, &[]);
    let value = coverage(&sim, home, ServiceKind::School);
    assert!(value > 0.0);

    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    assert_eq!(
        restored.read::<CoverageGrid>().by_kind,
        sim.read::<CoverageGrid>().by_kind
    );
    for run in [&mut sim, &mut restored] {
        let mut schedule = Simulation::schedule();
        set_staff(run, school, 0);
        set_tick(run, 13798);
        run.tick(&mut schedule, &[]);
        assert_eq!(coverage(run, home, ServiceKind::School), value);
        run.tick(&mut schedule, &[]);
        assert_eq!(coverage(run, home, ServiceKind::School), 0.0);
        assert_eq!(coverage(run, home, ServiceKind::Fire), fire);
    }
    assert_eq!(
        restored.hashes()["coverage_grid"],
        sim.hashes()["coverage_grid"]
    );
}

#[test]
#[ignore = "release coverage benchmark"]
fn p2a_perf_services() {
    let mut sim = TestCtx::new_fast().g;
    let old_log_level = log::max_level();
    log::set_max_level(log::LevelFilter::Warn);
    let kinds = [
        (ServiceKind::Health, "clinic", 8),
        (ServiceKind::Fire, "fire-station", 8),
        (ServiceKind::Police, "police-station", 8),
        (ServiceKind::School, "school", 6),
        (ServiceKind::Garbage, "garbage-depot", 6),
        (ServiceKind::Park, "park", 2),
    ];
    let lanes = LanePatternBuilder::new().parking(false);
    for row in 0..20 {
        let y = 100.0 + 60.0 * row as f32;
        let branch = road(&sim, vec2(100.0, y), vec2(1300.0, y), lanes);
        if row > 0 {
            road(&sim, vec2(100.0, y - 60.0), vec2(100.0, y), lanes);
        }
        for col in 0..500 {
            let x = 150.0 + 2.2 * col as f32;
            house(
                &sim,
                branch,
                vec2(x, y + 16.0),
                1 + 2 * (500 * row + col),
                2,
            );
        }
        for (index, &(_, name, staff)) in kinds.iter().enumerate() {
            for place in 0..2 {
                let x = 590.0 + 15.0 * (2 * index + place) as f32;
                company(&mut sim, branch, vec2(x, y - 16.0), name, staff);
            }
        }
        if row == 0 {
            company(
                &mut sim,
                branch,
                vec2(260.0, y - 16.0),
                "coal-power-plant",
                10,
            );
        }
    }
    set_tick(&sim, 5998);
    sim.tick(&mut Simulation::schedule(), &[]);

    let mut peak_tick_ms = 0.0_f64;
    for (index, &(kind, _, _)) in kinds.iter().enumerate() {
        let mut peak_ms = 0.0_f32;
        let mut total_ms = 0.0_f32;
        for cycle in 1..=10 {
            // Avoid hourly simulation work at tick 18000 multiples.
            let tick = 6000 * (cycle + (cycle - 1) / 2) + 600 * index as u64;
            set_tick(&sim, tick - 1);
            let mut schedule = Simulation::schedule();
            let elapsed = sim.tick(&mut schedule, &[]);
            peak_tick_ms = peak_tick_ms.max(elapsed.as_secs_f64() * 1000.0);
            let (_, recompute_ms) = schedule
                .times()
                .into_iter()
                .find(|(name, _)| name == "coverage_update")
                .unwrap();
            peak_ms = peak_ms.max(recompute_ms);
            total_ms += recompute_ms;
        }
        assert!(sim
            .read::<CoverageGrid>()
            .by_kind
            .get(&kind)
            .is_some_and(|covered| !covered.is_empty()));
        println!(
            "{kind:?} recompute mean {:.3} ms, peak {peak_ms:.3} ms",
            total_ms / 10.0
        );
    }
    println!("Peak simulation tick {peak_tick_ms:.3} ms");
    log::set_max_level(old_log_level);
}
