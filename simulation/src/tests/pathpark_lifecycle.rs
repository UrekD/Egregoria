use super::TestCtx;
use crate::map::{LaneKind, RoadSide};
use crate::map_dynamic::{DirectedEndpoint, DirectedSearch, DirectedSearchResult};
use crate::transportation::{spawn_driving_vehicle, VehicleKind};
use crate::transportation::{
    DrivewayBlocker, DrivewayDirection, DrivewayPhase, DrivewayPortal, DrivewayTraffic, Location,
    TransportGrid,
};
use crate::world_command::{CommandOutcomes, WorldCommand};
use crate::Simulation;
use common::saveload::{Bincode, Encoder};
use geom::{vec3, Transform};

#[test]
fn unruled_portal_resumes_route_and_reloads_with_original_car_and_claim() {
    let mut ctx = TestCtx::new();
    let (first_road, last_road) = {
        use crate::map::{LanePatternBuilder, MapProject, ProjectKind};
        let pattern = LanePatternBuilder::default().parking(false).build();
        let mut map = ctx.g.map_mut();
        let ground = |x| MapProject {
            pos: vec3(x, 100., 0.),
            kind: ProjectKind::Ground,
        };
        let middle = vec3(104., 80., 0.);
        let from = map.resolve_connection_endpoint(ground(24.)).unwrap();
        let joint = map
            .resolve_connection_endpoint(MapProject {
                pos: middle,
                kind: ProjectKind::Ground,
            })
            .unwrap();
        let first = map.connect_projects(from, joint, None, &pattern);
        let joint = map
            .resolve_connection_endpoint(MapProject {
                pos: middle,
                kind: ProjectKind::Intersection(first.dst),
            })
            .unwrap();
        let far = map.resolve_connection_endpoint(ground(224.)).unwrap();
        let last = map.connect_projects(joint, far, None, &pattern);
        (first.road, last.road)
    };
    let (from, at, end, anchor) = {
        let map = ctx.g.map();
        let forward_lane = |id| {
            let road = &map.roads()[id];
            let lane = road
                .outgoing_lanes_from(road.src)
                .iter()
                .find(|(_, kind)| *kind == LaneKind::Driving)
                .unwrap()
                .0;
            &map.lanes()[lane]
        };
        let first = forward_lane(first_road);
        let last = forward_lane(last_road);
        (
            DirectedEndpoint {
                lane: first.id,
                offset_cm: 300,
            },
            DirectedEndpoint {
                lane: last.id,
                offset_cm: 3500,
            },
            DirectedEndpoint {
                lane: last.id,
                offset_cm: 6000,
            },
            map.pick_curb(last.parent, RoadSide::Right, 4_500).unwrap(),
        )
    };
    {
        let map = ctx.g.map();
        let mut cursor = DirectedSearch::new(from, at);
        assert!(matches!(
            cursor.advance(&map, &mut 0),
            DirectedSearchResult::Deferred
        ));
        assert!((0..128).any(|_| matches!(
            cursor.advance(&map, &mut 1),
            DirectedSearchResult::Feasible(_)
        )));
    }
    let portal = DrivewayPortal::place(&mut ctx.g, anchor).unwrap();
    let (sidewalk, away) = {
        let map = ctx.g.map();
        let road = map.get(anchor.road).unwrap();
        let (center, _) = map.path_station(road, anchor.station_cm).unwrap();
        let along = road.src_dir();
        let curb = center + (along.perpendicular() * (road.width * 0.5 - 0.75)).z0();
        (curb, (along * 20.).z0())
    };
    let house = ctx.build_house_near(vec3(40., 120., 0.).xy());
    let walker = crate::souls::human::spawn_human(&mut ctx.g, house).unwrap();
    let collider = crate::transportation::put_pedestrian_in_transport_grid(
        &mut ctx.g.write::<TransportGrid>(),
        sidewalk,
    );
    ctx.g.world.edit_human(walker, |human| {
        human.location = Location::Outside;
        human.trans.pos = sidewalk;
        human.collider = Some(collider);
        human.it = crate::map_dynamic::Itinerary::simple(vec![sidewalk + away]);
    });
    let approach = portal.approach(&ctx.g).unwrap();
    let vehicle = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, approach).unwrap();
    portal.hold(&mut ctx.g, vehicle).unwrap();
    let stored = |sim: &Simulation| sim.read::<DrivewayTraffic>().phase(portal);
    assert_eq!(stored(&ctx.g), Some(DrivewayPhase::Held));
    {
        let (world, resources) = ctx.g.world_res();
        crate::transportation::transport_grid_synchronize(world, resources);
        crate::transportation::road::vehicle_state_update_system(world, resources);
    }
    assert_eq!(
        stored(&ctx.g),
        Some(DrivewayPhase::Held),
        "a live walker occupies the apron"
    );
    ctx.g
        .world
        .edit_human(walker, |human| human.trans.pos += away);
    ctx.tick_fast();
    assert_eq!(stored(&ctx.g), Some(DrivewayPhase::Entering));
    assert_eq!(
        ctx.g
            .read::<DrivewayTraffic>()
            .crossing(portal)
            .unwrap()
            .direction,
        DrivewayDirection::In
    );
    assert!(ctx.g.world.vehicles[vehicle].collider.is_some());
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(stored(&ctx.g), Some(DrivewayPhase::Entering));
    for _ in 0..600 {
        ctx.tick_fast();
        if stored(&ctx.g) == Some(DrivewayPhase::Stored) {
            break;
        }
    }
    assert_eq!(stored(&ctx.g), Some(DrivewayPhase::Stored));
    assert!(ctx.g.world.vehicles[vehicle].collider.is_none());

    let before = ctx.g.hashes();
    ctx.g
        .apply_commands([&WorldCommand::MapRemoveRoad(anchor.road)]);
    assert!(ctx.g.read::<CommandOutcomes>().accepted.is_empty());
    assert_eq!(before, ctx.g.hashes());
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(stored(&ctx.g), Some(DrivewayPhase::Stored));
    let release = portal.release(&ctx.g).unwrap();
    let stopped = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, release).unwrap();
    portal.depart(&mut ctx.g, vehicle, end).unwrap();
    for _ in 0..3 {
        ctx.tick_fast();
    }
    assert_eq!(stored(&ctx.g), Some(DrivewayPhase::Stored));
    assert_eq!(
        ctx.g.read::<DrivewayTraffic>().blocker(portal),
        Some(DrivewayBlocker::ReceivingSpace)
    );
    assert!(ctx.g.world.vehicles[vehicle].collider.is_none());
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(
        ctx.g.read::<DrivewayTraffic>().blocker(portal),
        Some(DrivewayBlocker::ReceivingSpace)
    );
    ctx.g.world.vehicles[stopped].trans.pos += release.dir * 30.0;
    let mut saw_egress = false;
    for _ in 0..600 {
        ctx.tick_fast();
        if stored(&ctx.g) == Some(DrivewayPhase::Exiting) {
            if !saw_egress {
                assert_eq!(
                    ctx.g
                        .read::<DrivewayTraffic>()
                        .crossing(portal)
                        .unwrap()
                        .direction,
                    DrivewayDirection::Out
                );
                ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
                saw_egress = true;
            }
            assert!(ctx.g.world.vehicles[vehicle].collider.is_some());
        }
        if stored(&ctx.g).is_none() {
            break;
        }
    }
    assert!(saw_egress);
    assert_eq!(stored(&ctx.g), None);
    assert!(ctx.g.world.vehicles[vehicle].collider.is_some());
    assert_eq!(
        ctx.g.world.vehicles[vehicle].it.target(),
        Some(crate::map_dynamic::ItineraryTarget::DirectedLane(
            end.lane,
            end.offset()
        )),
    );
    assert_ne!(
        ctx.g.world.vehicles[vehicle].trans,
        Transform::new(vec3(0., 0., 0.))
    );
}

pub(crate) fn road_chain(count: usize) -> (TestCtx, Vec<crate::map::RoadID>, DrivewayPortal) {
    use crate::map::{LanePatternBuilder, MapProject, ProjectKind};
    let mut ctx = TestCtx::new();
    let roads = {
        let mut map = ctx.g.map_mut();
        let pattern = LanePatternBuilder::default().parking(false).build();
        let point = |i: usize| vec3(40. + 88. * i as f32, 350. + 12. * (i % 2) as f32, 0.);
        let mut joint = ProjectKind::Ground;
        (0..count)
            .map(|i| {
                let from = map
                    .resolve_connection_endpoint(MapProject {
                        pos: point(i),
                        kind: joint,
                    })
                    .unwrap();
                let to = map
                    .resolve_connection_endpoint(MapProject::ground(point(i + 1)))
                    .unwrap();
                let made = map.connect_projects(from, to, None, &pattern);
                joint = ProjectKind::Intersection(made.dst);
                made.road
            })
            .collect::<Vec<_>>()
    };
    let anchor = ctx
        .g
        .map()
        .pick_curb(roads[0], RoadSide::Right, 4_500)
        .unwrap();
    let portal = DrivewayPortal::place(&mut ctx.g, anchor).unwrap();
    (ctx, roads, portal)
}

fn store_car(ctx: &mut TestCtx, portal: DrivewayPortal) -> crate::VehicleID {
    let approach = portal.approach(&ctx.g).unwrap();
    let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, approach).unwrap();
    portal.hold(&mut ctx.g, car).unwrap();
    for _ in 0..200 {
        ctx.tick_fast();
        if ctx.g.read::<DrivewayTraffic>().phase(portal) == Some(DrivewayPhase::Stored) {
            return car;
        }
    }
    panic!("car never reached storage");
}

fn road_target(sim: &Simulation, id: crate::map::RoadID) -> DirectedEndpoint {
    let map = sim.map();
    let road = &map.roads()[id];
    let lane = road
        .outgoing_lanes_from(road.src)
        .iter()
        .find(|(_, k)| *k == LaneKind::Driving)
        .unwrap()
        .0;
    DirectedEndpoint {
        lane,
        offset_cm: 6_000,
    }
}

#[test]
fn pathpark_remote_edit_preserves_committed_egress_across_reload() {
    let (mut ctx, roads, portal) = road_chain(4);
    let car = store_car(&mut ctx, portal);
    let target = road_target(&ctx.g, roads[3]);
    portal.depart(&mut ctx.g, car, target).unwrap();
    for _ in 0..64 {
        ctx.tick_fast();
        if ctx.g.read::<DrivewayTraffic>().phase(portal) == Some(DrivewayPhase::Exiting) {
            break;
        }
    }
    assert_eq!(
        ctx.g.read::<DrivewayTraffic>().phase(portal),
        Some(DrivewayPhase::Exiting)
    );
    let cmd = WorldCommand::MapRemoveRoad(roads[3]);
    ctx.g.apply_commands([&cmd]);
    let mut loaded: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(loaded.hashes(), ctx.g.hashes());
    assert!(loaded.map().get(roads[3]).is_some());
    for _ in 0..160 {
        loaded.tick(&mut Simulation::schedule(), &[]);
        if loaded.read::<DrivewayTraffic>().phase(portal).is_none() {
            break;
        }
    }
    assert_eq!(
        loaded.world.vehicles[car].it.target(),
        Some(crate::map_dynamic::ItineraryTarget::DirectedLane(
            target.lane,
            target.offset()
        ))
    );
}

#[test]
fn pathpark_reload_keeps_pending_search_progress() {
    let (mut ctx, roads, portal) = road_chain(5);
    let car = store_car(&mut ctx, portal);
    let target = road_target(&ctx.g, roads[4]);
    portal.depart(&mut ctx.g, car, target).unwrap();
    ctx.tick_fast();
    assert_eq!(
        ctx.g.read::<DrivewayTraffic>().phase(portal),
        Some(DrivewayPhase::Stored)
    );
    let mut loaded: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(
        loaded.hashes()["driveway_traffic"],
        ctx.g.hashes()["driveway_traffic"]
    );
    let mut schedule = Simulation::schedule();
    for _ in 0..64 {
        ctx.tick_fast();
        loaded.tick(&mut schedule, &[]);
        assert_eq!(
            loaded.hashes()["driveway_traffic"],
            ctx.g.hashes()["driveway_traffic"]
        );
        if ctx.g.read::<DrivewayTraffic>().phase(portal) == Some(DrivewayPhase::Exiting) {
            return;
        }
    }
    panic!("search never granted egress");
}

#[test]
fn pathpark_snapshot_requires_driveway_resource() {
    let (mut ctx, _, portal) = road_chain(1);
    store_car(&mut ctx, portal);
    let mut res = ctx.g.snapshot_resources(true, usize::MAX).unwrap();
    res.remove("driveway_traffic").unwrap();
    let bytes = Bincode::encode(&crate::SimulationSer {
        header: crate::SaveHeader::current(),
        world: &ctx.g.world,
        res,
    })
    .unwrap();
    assert!(Bincode::decode::<Simulation>(&bytes).is_err());
}

#[test]
fn pathpark_departure_settles_at_the_64_tick_deadline() {
    let (mut ctx, roads, portal) = road_chain(1);
    let car = store_car(&mut ctx, portal);
    let release = portal.release(&ctx.g).unwrap();
    let blocker = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, release).unwrap();
    let target = road_target(&ctx.g, roads[0]);
    portal.depart(&mut ctx.g, car, target).unwrap();
    for _ in 0..65 {
        ctx.tick_fast();
    }
    assert_eq!(
        ctx.g.read::<DrivewayTraffic>().phase(portal),
        Some(DrivewayPhase::Stored)
    );
    assert_eq!(
        format!(
            "{:?}",
            ctx.g.read::<DrivewayTraffic>().blocker(portal).unwrap()
        ),
        "BudgetExhausted"
    );
    ctx.g.world.vehicles[blocker].trans.pos.x += 100.;
    assert!(portal.depart(&mut ctx.g, car, target).is_ok());
}

#[test]
fn pathpark_empty_portal_only_blocks_water_changes_at_its_footprint() {
    use crate::map::{WaterBasin, WaterEdit};
    use geom::{vec2, Polygon};
    let (mut ctx, _, portal) = road_chain(1);
    let lake = |center| {
        WorldCommand::EditWater(WaterEdit::FillLake {
            basin: WaterBasin::new(&Polygon::centered_rect(center, 16., 16.).0).unwrap(),
            level_dm: 5,
            depth_m: 2.,
        })
    };
    let remote = lake(vec2(450., 260.));
    assert!(
        remote.preview(&ctx.g).is_ok(),
        "{:?}",
        remote.preview(&ctx.g).err()
    );
    ctx.g.apply_commands([&remote]);
    assert!(ctx
        .g
        .read::<CommandOutcomes>()
        .water_edit(0)
        .unwrap()
        .is_ok());
    let local = lake(portal.approach(&ctx.g).unwrap().pos.xy());
    assert!(local.preview(&ctx.g).is_err());
    let before = ctx.g.hashes();
    ctx.g.apply_commands([&local]);
    assert!(matches!(
        ctx.g.read::<CommandOutcomes>().water_edit(0).unwrap(),
        Err(crate::map::WaterEditError::TopologyConflict { .. })
    ));
    assert_eq!(ctx.g.hashes(), before);
}
