use super::{init_test_simulation, TestCtx};
use crate::map::{LaneKind, LanePatternBuilder, Map, MapProject, ProjectKind, Tree};
use crate::transportation::{TransportGrid, TransportState};
use crate::utils::scheduler::SeqSchedule;
use crate::world_command::WorldCommand;
use crate::World;
use crate::{Replay, Simulation, SimulationOptions};
use common::saveload::{Bincode, Encoder, JSONPretty, JSON};
use geom::{vec2, vec3};
use quickcheck::{Arbitrary, Gen, TestResult};

static REPLAY: &[u8] = include_bytes!("world_replay.json");

fn check_coherent(map: &Map, proj: MapProject) {
    match proj.kind {
        ProjectKind::Intersection(i) => {
            assert!(map.intersections.get(i).unwrap().pos.distance(proj.pos) < 5.0)
        }
        ProjectKind::Road(_) => {
            //assert!(map.roads.get(r).unwrap().points.project_dist(proj.pos) < 5.0)
        }
        _ => {}
    }
}

fn check_eq(w1: &World, w2: &World) -> bool {
    for (c1, c2) in w1.entities().zip(w2.entities()) {
        if c1 != c2 {
            println!("{:?} {:?}", c1, c2);
            return false;
        }
    }
    true
}

#[derive(Debug, Copy, Clone)]
struct F3201(f32);

impl Arbitrary for F3201 {
    fn arbitrary(g: &mut Gen) -> Self {
        let v = <u32 as Arbitrary>::arbitrary(g);
        F3201(v as f32 / u32::MAX as f32)
    }
}

#[derive(Debug, Copy, Clone)]
enum MapAction {
    AddInter,
    TwoInter,
    RemoveRoad,
    SplitRoad,
    Serde,
}

impl Arbitrary for MapAction {
    fn arbitrary(g: &mut Gen) -> Self {
        *g.choose(&[
            MapAction::AddInter,
            MapAction::TwoInter,
            MapAction::SplitRoad,
            MapAction::Serde,
        ])
        .unwrap()
    }
}

#[test]
fn quickcheck_map_ser() {
    let mut q = quickcheck::QuickCheck::new().tests(100);
    q.quickcheck(
        (|vals: Vec<(MapAction, u32, F3201, F3201)>| -> TestResult {
            let mut m = Map::empty();
            let mut m2 = Map::empty();

            m.make_connection(
                MapProject {
                    pos: vec3(0.0, 0.0, 0.0),
                    kind: ProjectKind::Ground,
                },
                MapProject {
                    pos: vec3(30.0, 0.0, 0.0),
                    kind: ProjectKind::Ground,
                },
                None,
                &LanePatternBuilder::new().build(),
            );

            m2.make_connection(
                MapProject {
                    pos: vec3(0.0, 0.0, 0.0),
                    kind: ProjectKind::Ground,
                },
                MapProject {
                    pos: vec3(30.0, 0.0, 0.0),
                    kind: ProjectKind::Ground,
                },
                None,
                &LanePatternBuilder::new().build(),
            );

            for (action, r, x, y) in vals {
                match action {
                    MapAction::AddInter => {
                        let i = m
                            .intersections
                            .iter()
                            .nth(r as usize % m.intersections.len())
                            .unwrap()
                            .0;
                        m.make_connection(
                            MapProject {
                                pos: m.intersections[i].pos,
                                kind: ProjectKind::Intersection(i),
                            },
                            MapProject {
                                pos: vec3(x.0 * 500.0, y.0 * 500.0, 0.0),
                                kind: ProjectKind::Ground,
                            },
                            None,
                            &LanePatternBuilder::new().build(),
                        );
                        m2.make_connection(
                            MapProject {
                                pos: m2.intersections[i].pos,
                                kind: ProjectKind::Intersection(i),
                            },
                            MapProject {
                                pos: vec3(x.0 * 500.0, y.0 * 500.0, 0.0),
                                kind: ProjectKind::Ground,
                            },
                            None,
                            &LanePatternBuilder::new().build(),
                        );
                    }
                    MapAction::TwoInter => {
                        let i1 = m
                            .intersections
                            .iter()
                            .nth(r as usize % m.intersections.len())
                            .unwrap()
                            .0;
                        let i2 = m
                            .intersections
                            .iter()
                            .nth(r as usize % m.intersections.len())
                            .unwrap()
                            .0;
                        if i1 != i2 {
                            m.make_connection(
                                MapProject {
                                    pos: m.intersections[i1].pos,
                                    kind: ProjectKind::Intersection(i1),
                                },
                                MapProject {
                                    pos: m.intersections[i2].pos,
                                    kind: ProjectKind::Intersection(i2),
                                },
                                None,
                                &LanePatternBuilder::new().build(),
                            );
                            m2.make_connection(
                                MapProject {
                                    pos: m2.intersections[i1].pos,
                                    kind: ProjectKind::Intersection(i1),
                                },
                                MapProject {
                                    pos: m2.intersections[i2].pos,
                                    kind: ProjectKind::Intersection(i2),
                                },
                                None,
                                &LanePatternBuilder::new().build(),
                            );
                        }
                    }
                    MapAction::RemoveRoad => {
                        if m.roads.len() > 3 {
                            // remove randomly
                            let r = m.roads.iter().nth(r as usize % m.roads.len()).unwrap().0;

                            m.remove_road(r);
                            m2.remove_road(r);
                        }
                    }
                    MapAction::SplitRoad => {
                        let r = m.roads.iter().nth(r as usize % m.roads.len()).unwrap().0;
                        let p = m.roads[r].points.length();
                        m.make_connection(
                            MapProject {
                                pos: m.roads[r].points.point_along(p * 0.5),
                                kind: ProjectKind::Road(r),
                            },
                            MapProject {
                                pos: vec3(x.0 * 500.0, y.0 * 500.0, 0.0),
                                kind: ProjectKind::Ground,
                            },
                            None,
                            &LanePatternBuilder::new().build(),
                        );
                        m2.make_connection(
                            MapProject {
                                pos: m2.roads[r].points.point_along(p * 0.5),
                                kind: ProjectKind::Road(r),
                            },
                            MapProject {
                                pos: vec3(x.0 * 500.0, y.0 * 500.0, 0.0),
                                kind: ProjectKind::Ground,
                            },
                            None,
                            &LanePatternBuilder::new().build(),
                        );
                    }
                    MapAction::Serde => {
                        let v = Bincode::encode(&m).unwrap();
                        let m3: Map = Bincode::decode(&v).unwrap();
                        m = m3;
                    }
                }
            }

            let v = Bincode::encode(&m).unwrap() == Bincode::encode(&m2).unwrap();
            if !v {
                let m_enc = unsafe { String::from_utf8_unchecked(JSONPretty::encode(&m).unwrap()) };
                let m2_enc =
                    unsafe { String::from_utf8_unchecked(JSONPretty::encode(&m2).unwrap()) };
                let diff = diff::lines(&m_enc, &m2_enc);
                let mut diff_str = String::new();
                for line in diff {
                    match line {
                        diff::Result::Left(l) => diff_str.push_str(&format!("- {}\n", l)),
                        diff::Result::Both(l, _) => diff_str.push_str(&format!("  {}\n", l)),
                        diff::Result::Right(r) => diff_str.push_str(&format!("+ {}\n", r)),
                    }
                }

                TestResult::error(diff_str)
            } else {
                TestResult::passed()
            }
        }) as fn(_) -> TestResult,
    );
}

#[test]
fn p1_spine_growth_resources_round_trip() {
    use crate::economy::{Budget, BudgetCategory, ZoneDemand};
    use crate::map::{Density, ZoneGrid, ZoneKind};
    use crate::souls::residence::{Residence, Residences};
    use prototypes::{Money, ResidencePrototypeID};

    let ctx = TestCtx::new_fast();
    ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let house = ctx.build_house_near(vec2(150.0, 125.0));
    let area = ctx.g.map().get(house).unwrap().obb;
    ctx.g
        .write::<ZoneGrid>()
        .paint(area, Some(ZoneKind::Residential), Density::Low);
    assert_eq!(ctx.g.read::<ZoneGrid>().generation, 1);
    ctx.g.write::<Residences>().by_building.insert(
        house,
        Residence {
            prototype: ResidencePrototypeID::new("residence-low-l1"),
            density: Density::Low,
            level: 1,
            capacity: 2,
        },
    );
    ctx.g
        .write::<ZoneDemand>()
        .values
        .insert(ZoneKind::Residential, 40.0);
    Budget::apply(
        &ctx.g.resources,
        BudgetCategory::TradeExport,
        Money::new_cents(1),
    );

    let encoded = Bincode::encode(&ctx.g).unwrap();
    let decoded: Simulation = Bincode::decode(&encoded).unwrap();
    let original = ctx.g.hashes();
    let restored = decoded.hashes();

    for name in ["zone_grid", "residences", "zone_demand", "budget"] {
        let hash = original.get(name).expect(name);
        assert_eq!(restored.get(name), Some(hash), "{name}");
    }
}

#[test]
fn paris_hashes_survive_serde() {
    init_test_simulation();
    let mut sim = Simulation::new_with_options(SimulationOptions {
        terrain_size: 50,
        save_replay: false,
    });
    WorldCommand::MapLoadParis.apply(&mut sim);

    let encoded = Bincode::encode(&sim).unwrap();
    let decoded: Simulation = Bincode::decode(&encoded).unwrap();

    assert_eq!(sim.hashes(), decoded.hashes());
}

#[test]
fn resumed_trajectory_matches_continuous_run() {
    let mut continuous = TestCtx::new_fast();
    let mut resumed = TestCtx::new_fast();
    for ctx in [&continuous, &resumed] {
        // Put a tree in the road's path so the fixture exercises tree removal.
        let tree_pos = vec2(256.0, 128.0);
        ctx.g
            .map_mut()
            .environment
            .trees
            .insert(tree_pos, Tree::new(tree_pos));
        ctx.build_roads(&[vec3(64.0, 128.0, 0.0), vec3(448.0, 128.0, 0.0)]);
        ctx.build_house_near(vec2(200.0, 150.0));
    }

    for _ in 0..1000 {
        continuous.tick_fast();
        resumed.tick_fast();
    }
    assert_eq!(resumed.g.world().humans.len(), 2);
    assert_eq!(continuous.g.hashes(), resumed.g.hashes());
    let encoded = Bincode::encode(&resumed.g).unwrap();
    resumed.g = Bincode::decode(&encoded).unwrap();
    assert_eq!(continuous.g.hashes(), resumed.g.hashes());
    for _ in 0..1000 {
        continuous.tick_fast();
        resumed.tick_fast();
    }

    assert_eq!(continuous.g.hashes(), resumed.g.hashes());
}

#[test]
fn equidistant_lanes_match_after_spatial_index_rebuild() {
    let mut map = Map::empty();
    let pattern = LanePatternBuilder::new()
        .one_way(true)
        .parking(false)
        .sidewalks(false)
        .build();
    let mut road_at = |y| {
        map.make_connection(
            MapProject::ground(vec3(100.0, y, 0.0)),
            MapProject::ground(vec3(300.0, y, 0.0)),
            None,
            &pattern,
        )
        .unwrap()
        .1
    };
    // Removing the first road swaps the last entry forward in the spatial cell.
    let temporary = road_at(127.0);
    road_at(116.0);
    road_at(138.0);
    map.remove_road(temporary).unwrap();

    let encoded = Bincode::encode(&map).unwrap();
    let decoded: Map = Bincode::decode(&encoded).unwrap();
    let mut lanes = map
        .lanes()
        .values()
        .filter(|lane| lane.kind == LaneKind::Driving);
    let first = lanes.next().unwrap();
    let second = lanes.next().unwrap();
    let reference = vec3(200.0, 127.0, 0.0);
    let a = first.points.project(reference);
    let b = second.points.project(reference);
    let pos = (a + b) * 0.5;
    assert_eq!(
        first.points.project_dist2(pos),
        second.points.project_dist2(pos)
    );
    let live = map
        .nearest_lane(pos, LaneKind::Driving, Some(45.0))
        .unwrap();
    let restored = decoded
        .nearest_lane(pos, LaneKind::Driving, Some(45.0))
        .unwrap();
    assert_eq!(live, restored, "lane selection at equal distances");
}

#[test]
fn transport_grid_hash_survives_save_with_sparse_cells() {
    let ctx = TestCtx::new_fast();
    let remaining = {
        let mut grid = ctx.g.write::<TransportGrid>();
        let handles = (0..64)
            .map(|i| grid.insert(vec2(125.0 * i as f32, 128.0), TransportState::default()))
            .collect::<Vec<_>>();
        for &handle in &handles[10..] {
            grid.remove_maintain(handle);
        }
        grid.maintain_deterministic();
        handles[0]
    };

    let encoded = Bincode::encode(&ctx.g).unwrap();
    let restored: Simulation = Bincode::decode(&encoded).unwrap();
    assert_eq!(
        ctx.g.hashes()["transport_grid"],
        restored.hashes()["transport_grid"]
    );
    for sim in [&ctx.g, &restored] {
        let mut grid = sim.write::<TransportGrid>();
        grid.set_position(remaining, vec2(1_000.0, 256.0));
        grid.maintain_deterministic();
        grid.insert(vec2(1_000.0, 256.0), TransportState::default());
    }
    assert_eq!(
        ctx.g.hashes()["transport_grid"],
        restored.hashes()["transport_grid"]
    );
}

#[test]
#[should_panic(expected = "Replay has version \"0.7.0\"")]
fn public_replay_playback_rejects_version_0_7_0() {
    init_test_simulation();
    let replay: Replay = JSON::decode(
        br#"{
        "commands": [[1, {"SetTaxRate": {"zone": "Residential", "rate_permille": 290}}]],
        "enabled": true,
        "last_tick_recorded": 1,
        "version": "0.7.0"
    }"#,
    )
    .unwrap();
    let _ = Simulation::from_replay(replay);
}

#[test]
fn test_world_survives_serde() {
    init_test_simulation();
    //common::logger::MyLog::init();

    let mut replay: Replay = JSON::decode(REPLAY).unwrap();
    replay.version = crate::VERSION.to_owned();
    let mut s = SeqSchedule::default();

    let mut check_size = 1024;
    let mut check_start = 3;

    'main: loop {
        if check_size == 0 {
            break;
        }
        let (mut sim, mut loader) = Simulation::from_replay(replay.clone());
        let (mut sim2, mut loader2) = Simulation::from_replay(replay.clone());

        while !loader.advance_tick(&mut sim, &mut s) {
            loader2.advance_tick(&mut sim2, &mut s);

            let tick = sim.get_tick();
            if tick < check_start || tick % check_size != 0 {
                continue;
            }
            println!(
                "--- tick {} ({}/{})",
                sim.get_tick(),
                loader.pastt.0,
                loader.replay.last_tick_recorded.0
            );

            let ser = common::saveload::Bincode::encode(&sim).unwrap();
            let mut deser: Simulation = common::saveload::Bincode::decode(&ser).unwrap();

            if !sim.is_equal(&sim2) {
                println!("not equal sim+sim2");
                sim.save_to_disk("world");
                sim2.save_to_disk("world2");
                check_start = tick - check_size;
                check_size = check_size / 2;
                continue 'main;
            }
            if !deser.is_equal(&sim) {
                println!("not equal sim");
                deser.save_to_disk("world");
                sim.save_to_disk("world2");
                check_start = tick - check_size;
                check_size = check_size / 2;
                continue 'main;
            }
            if !deser.is_equal(&sim2) {
                println!("not equal sim2");
                deser.save_to_disk("world");
                sim2.save_to_disk("world2");
                check_start = tick - check_size;
                check_size = check_size / 2;
                continue 'main;
            }

            std::mem::swap(&mut deser, &mut sim2);
        }

        break;
    }
}
