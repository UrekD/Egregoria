use super::{active_providers, coverage_update, recompute, CoverageGrid, COVERAGE_KINDS};
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
use slotmapd::{Key, KeyData};

const PERF_KINDS: [(ServiceKind, &str, u32); 6] = [
    (ServiceKind::Health, "clinic", 8),
    (ServiceKind::Fire, "fire-station", 8),
    (ServiceKind::Police, "police-station", 8),
    (ServiceKind::School, "school", 6),
    (ServiceKind::Garbage, "garbage-depot", 6),
    (ServiceKind::Park, "park", 2),
];

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
fn p2a_coverage_starts_promptly_after_staffing() {
    let mut sim = TestCtx::new_fast().g;
    let road = road(
        &sim,
        vec2(100.0, 400.0),
        vec2(600.0, 400.0),
        LanePatternBuilder::new(),
    );
    let home = house(&sim, road, vec2(330.0, 420.0), 1, 2);
    let school = company(&mut sim, road, vec2(310.0, 420.0), "school", 0);
    company(&mut sim, road, vec2(210.0, 420.0), "solar-panel", 0);
    refresh_power(&mut sim);
    update(&mut sim, 8100);
    assert_eq!(coverage(&sim, home, ServiceKind::School), 0.0);
    let revision = sim.read::<CoverageGrid>().revision(ServiceKind::School);

    set_staff(&mut sim, school, 1);
    for tick in 8101..=8400 {
        update(&mut sim, tick);
    }
    assert!(coverage(&sim, home, ServiceKind::School) > 0.0);
    assert_ne!(
        sim.read::<CoverageGrid>().revision(ServiceKind::School),
        revision
    );
}

#[test]
fn p2a_coverage_starts_promptly_for_an_additional_provider() {
    let mut sim = TestCtx::new_fast().g;
    let road = road(
        &sim,
        vec2(100.0, 400.0),
        vec2(600.0, 400.0),
        LanePatternBuilder::new(),
    );
    let near = house(&sim, road, vec2(330.0, 420.0), 1, 60);
    let far = house(&sim, road, vec2(540.0, 420.0), 61, 2);
    company(&mut sim, road, vec2(310.0, 420.0), "school", 1);
    company(&mut sim, road, vec2(210.0, 420.0), "solar-panel", 0);
    refresh_power(&mut sim);
    update(&mut sim, 8100);
    assert!(coverage(&sim, near, ServiceKind::School) > 0.0);
    assert_eq!(coverage(&sim, far, ServiceKind::School), 0.0);

    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    for run in [&mut sim, &mut restored] {
        let revision = run.read::<CoverageGrid>().revision(ServiceKind::School);
        update(run, 8275);
        assert_eq!(
            run.read::<CoverageGrid>().revision(ServiceKind::School),
            revision
        );

        company(run, road, vec2(520.0, 420.0), "school", 1);
        refresh_power(run);
        for tick in 8276..=8575 {
            update(run, tick);
        }
        assert!(coverage(run, far, ServiceKind::School) > 0.0);
    }
    assert_eq!(
        restored.hashes()["coverage_grid"],
        sim.hashes()["coverage_grid"]
    );
}

#[test]
fn p2a_coverage_loads_the_legacy_grid_format() {
    use std::collections::BTreeMap;

    #[derive(serde::Serialize)]
    struct LegacyGrid {
        by_kind: BTreeMap<ServiceKind, BTreeMap<BuildingID, f32>>,
    }

    let by_kind = COVERAGE_KINDS
        .into_iter()
        .chain([ServiceKind::WaterPump, ServiceKind::SewageOutlet])
        .map(|kind| (kind, [(BuildingID::null(), 42.0)].into()))
        .collect();
    let legacy = LegacyGrid { by_kind };
    let old = Bincode::encode(&legacy).unwrap();
    let grid: CoverageGrid = Bincode::decode(&old).unwrap();
    assert_eq!(grid.by_kind, legacy.by_kind);
    assert_eq!(Bincode::encode(&grid).unwrap(), old);
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

    update(&mut sim, 6900);
    assert!(coverage(&sim, downstream, ServiceKind::Fire) > 0.0);
    assert!(coverage(&sim, behind_missing_sidewalk, ServiceKind::Fire) > 0.0);
    assert_eq!(coverage(&sim, upstream, ServiceKind::Fire), 0.0);
    assert_eq!(coverage(&sim, same_road_upstream, ServiceKind::Fire), 0.0);

    update(&mut sim, 8100);
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
    let adjacent = company(&mut sim, road, vec2(300.0, 455.0), "fire-station", 0);
    company(&mut sim, road, vec2(420.0, 440.0), "solar-panel", 0);
    assert!(first < second && near < adjacent);

    // Put both receivers at the same road station and the same distance from the centerline.
    let (radius, capacity) = (1800.0, 50.0);
    {
        let mut map = sim.map_mut();
        for id in [first, second, third, near, adjacent] {
            let door = map.buildings[id].door_pos;
            let z = map.get(road).unwrap().points().project_2d(door.xy()).z;
            map.buildings[id].door_pos.z = z;
        }
        // Reverse traversal order to ensure ties are resolved by BuildingID.
        map.roads[road].connected_buildings.reverse();
    }
    let near_score = 100.0 * (1.0 - 60.0 / radius);
    refresh_power(&mut sim);
    let covered = recompute(
        &sim,
        ServiceKind::Fire,
        &mut active_providers(&sim, ServiceKind::Fire),
    );
    assert!((covered[&first] - near_score).abs() < 0.001);
    let partial = near_score * (capacity - 40.0) / 40.0;
    assert!((covered[&second] - partial).abs() < 0.001);
    assert!(!covered.contains_key(&third));

    set_staff(&mut sim, adjacent, 1);
    let covered = recompute(
        &sim,
        ServiceKind::Fire,
        &mut active_providers(&sim, ServiceKind::Fire),
    );
    assert!(
        covered.contains_key(&third),
        "second provider must extend coverage"
    );
    assert!((covered[&first] - near_score).abs() < 0.001);
    let adjacent_score = 100.0 * (1.0 - 75.0 / radius);
    let second_score = adjacent_score * 30.0 / 40.0;
    assert!((covered[&second] - second_score).abs() < 0.001);
    let third_score = 100.0 * (1.0 - 215.0 / radius) * (20.0 / 30.0);
    assert!((covered[&third] - third_score).abs() < 0.001);
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
    update(&mut sim, 9300);
    assert_eq!(coverage(&sim, home, ServiceKind::Park), 0.0);

    set_staff(&mut sim, park, 1);
    refresh_power(&mut sim);
    update(&mut sim, 15300);
    assert!(coverage(&sim, home, ServiceKind::Park) > 0.0);
    assert!(coverage(&sim, shop, ServiceKind::Park) > 0.0);
    assert_eq!(coverage(&sim, empty, ServiceKind::Park), 0.0);
    assert_eq!(coverage(&sim, power, ServiceKind::Park), 0.0);

    set_staff(&mut sim, power, 0);
    refresh_power(&mut sim);
    assert!(sim.read::<ElectricityFlow>().blackout(network));
    update(&mut sim, 21300);
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

    set_tick(&sim, 5999);
    sim.tick(&mut schedule, &[]);
    set_tick(&sim, 6899);
    sim.tick(&mut schedule, &[]);
    let fire = coverage(&sim, home, ServiceKind::Fire);
    assert!(fire > 0.0);
    assert_eq!(coverage(&sim, home, ServiceKind::School), 0.0);
    set_tick(&sim, 8099);
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
        set_tick(run, 14098);
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
    let sim = TestCtx::new_fast().g;
    let old_log_level = log::max_level();
    log::set_max_level(log::LevelFilter::Warn);
    measure_services("39 service roads", perf_branches(sim));
    measure_services(
        "1,984-road grid with sparse slots",
        perf_grid(TestCtx::new_fast().g),
    );
    log::set_max_level(old_log_level);
}

fn perf_branches(mut sim: Simulation) -> Simulation {
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
        for (index, &(_, name, staff)) in PERF_KINDS.iter().enumerate() {
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
    sim
}

fn perf_grid(mut sim: Simulation) -> Simulation {
    const SIDE: u32 = 32;
    const SPACING: f32 = 45.0;
    const VACANT_SLOTS: usize = 16_384;

    let (initial_roads, initial_intersections) = {
        let map = sim.map();
        (map.roads().len(), map.intersections().len())
    };
    let lanes = LanePatternBuilder::new().parking(false);
    let seed = road(&sim, vec2(100.0, 100.0), vec2(145.0, 100.0), lanes);
    let pattern = lanes.build();
    let mut segments = vec![(seed, vec2(100.0, 100.0), Vec2::X)];
    let mut providers = Vec::new();
    {
        let mut map = sim.map_mut();
        // Reserve low slot IDs to simulate deleted roads and intersections.
        // The placeholders never enter the spatial index or electricity graph.
        let seed_road = map.roads[seed].clone();
        let seed_intersection = map.intersections[seed_road.src].clone();
        let road_slots: Vec<_> = (0..VACANT_SLOTS)
            .map(|_| map.roads.insert(seed_road.clone()))
            .collect();
        let intersection_slots: Vec<_> = (0..VACANT_SLOTS)
            .map(|_| map.intersections.insert(seed_intersection.clone()))
            .collect();
        for row in 0..SIDE {
            for col in 0..SIDE {
                let at = vec2(100.0 + SPACING * col as f32, 100.0 + SPACING * row as f32);
                for (direction, fits) in [
                    (Vec2::X, col + 1 < SIDE && (row != 0 || col != 0)),
                    (Vec2::Y, row + 1 < SIDE),
                ] {
                    if !fits {
                        continue;
                    }
                    let from = map.project(at.z0(), 0.0, ProjectFilter::INTER);
                    let to =
                        map.project((at + direction * SPACING).z0(), 0.0, ProjectFilter::INTER);
                    let from = map.resolve_connection_endpoint(from).unwrap();
                    let to = map.resolve_connection_endpoint(to).unwrap();
                    // Both straight segments at a grid boundary must keep their own IDs.
                    let id = map.connect_projects(from, to, None, &pattern).road;
                    segments.push((id, at, direction));
                    if direction == Vec2::X && (6..26).contains(&row) && matches!(col, 14 | 16) {
                        providers.push((id, at));
                    }
                }
            }
        }
        for id in road_slots {
            map.roads.remove(id).unwrap();
        }
        for id in intersection_slots {
            map.intersections.remove(id).unwrap();
        }
        map.check_invariants();
        assert_eq!(map.roads().len() - initial_roads, 1_984);
        assert_eq!(map.intersections().len() - initial_intersections, 1_024);
        assert!(segments.last().unwrap().0.data().as_ffi() as u32 > VACANT_SLOTS as u32);
        assert!(map
            .intersections()
            .keys()
            .any(|id| id.as_ffi() as u32 > VACANT_SLOTS as u32));
    }

    let mut people_start = 1;
    for (index, &(id, at, direction)) in segments.iter().enumerate() {
        let houses = if index < 80 { 6 } else { 5 };
        for i in 0..houses {
            let door = at + direction * (13.0 + 4.0 * i as f32) - direction.perpendicular() * 10.0;
            house(&sim, id, door, people_start, 2);
            people_start += 2;
        }
    }
    assert_eq!(people_start, 20_001);
    assert_eq!(providers.len(), 40);
    for (id, at) in providers {
        for (index, &(_, name, staff)) in PERF_KINDS.iter().enumerate() {
            let door = at + vec2(13.0 + 4.0 * index as f32, -10.0);
            company(&mut sim, id, door, name, staff);
        }
    }
    company(&mut sim, seed, vec2(120.0, 85.0), "coal-power-plant", 10);
    sim
}

fn measure_services(name: &str, mut sim: Simulation) {
    const CYCLES: u64 = 10;
    let mut peaks = [0.0_f32; PERF_KINDS.len()];
    let mut totals = [0.0_f32; PERF_KINDS.len()];
    let mut peak_tick_ms = 0.0_f64;
    let mut hourly_peak_ms = 0.0_f64;
    for cycle in 1..=CYCLES {
        let start = 6_000 * cycle;
        if cycle == 1 || start % 18_000 == 0 {
            set_tick(&sim, start - 1);
            let elapsed = sim.tick(&mut Simulation::schedule(), &[]);
            let tick_ms = elapsed.as_secs_f64() * 1000.0;
            peak_tick_ms = peak_tick_ms.max(tick_ms);
            if start % 18_000 == 0 {
                hourly_peak_ms = hourly_peak_ms.max(tick_ms);
            }
        }
        for (index, _) in PERF_KINDS.iter().enumerate() {
            let tick = start + 600 * index as u64 + 300;
            set_tick(&sim, tick - 1);
            let mut schedule = Simulation::schedule();
            let elapsed = sim.tick(&mut schedule, &[]);
            peak_tick_ms = peak_tick_ms.max(elapsed.as_secs_f64() * 1000.0);
            let (_, recompute_ms) = schedule
                .times()
                .into_iter()
                .find(|(name, _)| name == "coverage_update")
                .unwrap();
            peaks[index] = peaks[index].max(recompute_ms);
            totals[index] += recompute_ms;
        }
    }
    println!("{name}");
    for (index, &(kind, _, _)) in PERF_KINDS.iter().enumerate() {
        assert!(sim
            .read::<CoverageGrid>()
            .by_kind
            .get(&kind)
            .is_some_and(|covered| !covered.is_empty()));
        println!(
            "{kind:?} recompute mean {:.3} ms, peak {:.3} ms",
            totals[index] / CYCLES as f32,
            peaks[index]
        );
    }
    println!("Peak simulation tick {peak_tick_ms:.3} ms (hourly peak {hourly_peak_ms:.3} ms)");
}
