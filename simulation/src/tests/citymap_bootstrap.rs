use super::init_test_simulation;
use crate::economy::progression::{ProgressionMode, ProgressionState};
use crate::economy::{Budget, BudgetCategory, Government};
use crate::map::{
    AuthoredTownOptions, BuildingKind, LaneKind, LanePatternBuilder, LotLayout, MapProject,
    RailMarketSite,
};
use crate::map_dynamic::BuildingInfos;
use crate::souls::freight_station::train_stop;
use crate::world_command::{CommandOutcomes, WorldCommand};
use crate::{MapPreset, Replay, Simulation, INITIAL_TICK};
use geom::{vec2, vec3, Transform, Vec2, AABB, OBB};
use prototypes::{BuildingGen, Money};

#[test]
fn citymap_bootstrap_starts_empty_on_the_compact_frame() {
    init_test_simulation();
    let options = AuthoredTownOptions {
        seed: 42,
        starting_money_bucks: 345_000,
        ..AuthoredTownOptions::default()
    };
    let sim = Simulation::new_authored_town(options).unwrap();
    let map = sim.map();
    assert_eq!(map.environment().size(), (6, 6));
    let elevation = map.environment().height(vec2(200.0, 200.0)).unwrap();
    assert!((elevation - 10.0).abs() < 0.016);
    assert!(map.is_empty());
    assert!(map.buildings().is_empty());
    assert!(map.lots().is_empty());
    assert!(map.external_train_stations.is_empty());
    assert_eq!(map.lot_layout(), LotLayout::Uniform);
    assert_eq!(sim.get_tick(), INITIAL_TICK);
    assert_eq!(sim.read::<Government>().money, Money::new_bucks(345_000));
    assert_eq!(
        sim.read::<Budget>()
            .total(BudgetCategory::StarterInfrastructure),
        0
    );
    assert_eq!(
        sim.read::<ProgressionState>().mode,
        ProgressionMode::FreeBuild
    );
    assert!(!sim.read::<Replay>().enabled);
    assert_eq!(sim.read::<Replay>().command_count(), 0);
    assert!(sim.read::<CommandOutcomes>().accepted.is_empty());
    let same = Simulation::new_authored_town(options).unwrap();
    let other = Simulation::new_authored_town(AuthoredTownOptions {
        seed: 43,
        ..options
    })
    .unwrap();
    assert_eq!(sim.hashes(), same.hashes());
    assert_ne!(sim.hashes(), other.hashes());
}

#[test]
fn citymap_bootstrap_validates_extents_and_options() {
    init_test_simulation();
    let compact = AuthoredTownOptions::default();
    for chunks in [7, 8] {
        let margin = f32::from(chunks - 7) * 512.0;
        let min = Vec2::splat(margin);
        let owned_rect = AABB::new_ll_ur(min, Vec2::splat(f32::from(chunks) * 512.0) - min);
        let sim = Simulation::new_authored_town(AuthoredTownOptions {
            terrain_chunks: chunks,
            owned_rect,
            ..compact
        })
        .unwrap();
        assert_eq!(sim.map().environment().size(), (chunks, chunks));
    }
    let expanded = |min: Vec2| AuthoredTownOptions {
        terrain_chunks: 9,
        owned_rect: AABB::new_ll_size(min, Vec2::splat(3072.0)),
        ..compact
    };
    for min in [vec2(512.0, 512.0), vec2(768.0, 832.0)] {
        let sim = Simulation::new_authored_town(expanded(min)).unwrap();
        assert_eq!(sim.map().environment().size(), (9, 9));
        let map = sim.map();
        let elevation = map.environment().height(vec2(100.0, 4400.0)).unwrap();
        assert!((elevation - 10.0).abs() < 0.016);
        assert!(map.is_empty());
    }
    for invalid in [
        AuthoredTownOptions {
            terrain_chunks: 5,
            ..compact
        },
        AuthoredTownOptions {
            terrain_chunks: 81,
            ..compact
        },
        AuthoredTownOptions {
            owned_rect: AABB::zero(),
            ..compact
        },
        AuthoredTownOptions {
            starting_money_bucks: 9_999,
            ..compact
        },
        AuthoredTownOptions {
            preset: MapPreset::Procedural,
            ..compact
        },
        AuthoredTownOptions {
            progression: ProgressionMode::Guided,
            ..compact
        },
        AuthoredTownOptions {
            lot_layout: LotLayout::Varied,
            ..compact
        },
        AuthoredTownOptions {
            save_replay: true,
            ..compact
        },
        expanded(vec2(511.0, 512.0)),
    ] {
        assert!(
            Simulation::new_authored_town(invalid).is_err(),
            "{invalid:?}"
        );
    }
}

#[test]
fn citymap_bootstrap_places_only_a_clear_routable_rail_market() {
    init_test_simulation();
    let mut sim = Simulation::new_authored_town(AuthoredTownOptions::default()).unwrap();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 700.0, 10.0)),
        to: MapProject::ground(vec3(1000.0, 700.0, 10.0)),
        inter: None,
        pat: LanePatternBuilder::new().rail(true).build(),
    }]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    let (lane, offset, market, stop) = {
        let map = sim.map();
        let lane = map
            .lanes()
            .values()
            .find(|lane| lane.kind == LaneKind::Rail)
            .unwrap();
        let offset = 200.0;
        let (pos, dir) = lane.points.point_dir_along(offset);
        let platform = lane.points.point_along(600.0);
        let station = Transform::new_dir(platform - dir * 75.0 + dir.perp_up() * 40.0, dir);
        let stop = train_stop(&station);
        (
            lane.id,
            offset,
            OBB::new(pos.xy(), dir.xy(), 100.0, 24.0),
            stop,
        )
    };
    let site = RailMarketSite {
        footprint: market,
        service_lane: lane,
        service_offset: offset,
        platform_stop: stop,
        consist_length_m: 100.0,
    };
    sim.apply_commands([&WorldCommand::MapBuildSpecialBuilding {
        pos: market,
        kind: BuildingKind::ExternalTrading,
        gen: BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        },
        zone: None,
        connected_road: None,
    }]);
    assert!(sim.read::<CommandOutcomes>().accepted.is_empty());

    let before = sim.hashes();
    for invalid in [
        RailMarketSite {
            consist_length_m: 250.0,
            ..site
        },
        RailMarketSite {
            platform_stop: vec3(750.0, 1000.0, 10.0),
            ..site
        },
        RailMarketSite {
            service_offset: offset + 20.0,
            ..site
        },
        RailMarketSite {
            footprint: OBB::new(vec2(2.0, 2.0), Vec2::X, 100.0, 24.0),
            ..site
        },
    ] {
        assert!(sim.place_authored_rail_market(invalid).is_err());
        assert_eq!(sim.hashes(), before);
    }
    let id = sim.place_authored_rail_market(site).unwrap();
    assert_eq!(sim.map().external_train_stations, [id]);
    assert!(sim.read::<BuildingInfos>().get(id).is_some());
    assert!(sim.place_authored_rail_market(site).is_err());
}

#[test]
fn citymap_bootstrap_directed_trip_arrives_and_releases_its_collider() {
    use crate::map::{PathKind, Pathfinder};
    use crate::map_dynamic::Itinerary;
    use crate::transportation::spawn_driving_vehicle;
    use crate::transportation::{TransportGrid, VehicleKind};
    use crate::{ParCommandBuffer, VehicleEnt};
    use prototypes::{Tick, TICKS_PER_REALTIME_SECOND};

    init_test_simulation();
    let mut sim = Simulation::new_authored_town(AuthoredTownOptions::default()).unwrap();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(250.0, 600.0, 10.0)),
        to: MapProject::ground(vec3(550.0, 600.0, 10.0)),
        inter: None,
        pat: LanePatternBuilder::new().build(),
    }]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    let (pose, target, itinerary) = {
        let map = sim.map();
        let lane = map
            .lanes()
            .values()
            .find(|lane| lane.kind == LaneKind::Driving)
            .unwrap();
        let (pos, dir) = lane.points.point_dir_along(30.0);
        let target = lane.points.point_along(110.0);
        assert_eq!(PathKind::Vehicle.nearest_lane(&map, pos), Some(lane.id));
        let itinerary =
            Itinerary::route_to_lane(Tick(sim.get_tick()), pos, &map, lane.id, 110.0).unwrap();
        assert_eq!(itinerary.get_travers().unwrap().destination_lane(), lane.id);
        (Transform::new_dir(pos, dir), target, itinerary)
    };
    let car = spawn_driving_vehicle(&mut sim, VehicleKind::Car, pose).unwrap();
    let vehicle = sim.world_mut_unchecked().vehicles.get_mut(car).unwrap();
    vehicle.it = itinerary;
    let collider = vehicle.collider.unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..30 * TICKS_PER_REALTIME_SECOND {
        if sim.get(car).unwrap().it.has_ended(0.0) {
            break;
        }
        sim.tick(&mut schedule, []);
    }
    let vehicle = sim.get(car).unwrap();
    assert!(
        vehicle.it.has_ended(0.0),
        "car stopped at {:?}",
        vehicle.trans
    );
    assert!(
        vehicle.trans.pos.distance(target) < 3.0,
        "{:?}",
        vehicle.trans
    );
    sim.read::<ParCommandBuffer<VehicleEnt>>().kill(car);
    sim.apply_commands([]);
    assert!(sim.get(car).is_none());
    assert!(sim.read::<TransportGrid>().get(collider.0).is_none());
}
