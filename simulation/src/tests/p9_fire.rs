use common::saveload::{Bincode, Encoder};
use geom::vec2;
use prototypes::{Tick, TICKS_PER_MINUTE};

use super::TestCtx;
use crate::souls::events::{fire_incident, Fires, ServiceIncidents};
use crate::{
    Replay, SaveLoadError, Simulation, SimulationDeser, SimulationSer, PRE_FIRE_VERSION,
    STOCK_PRE_FIRE_REGISTRY,
};

fn pre_fire_snapshot(test: &TestCtx, replay: &str) -> SimulationDeser {
    let mut snapshot: SimulationDeser =
        Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    snapshot._header.version = PRE_FIRE_VERSION.to_owned();
    snapshot.res.remove("service_incidents").unwrap();
    snapshot.res.remove("site_remnants").unwrap();
    snapshot.res.remove("fire_fleet").unwrap();
    snapshot.res.insert(
        "prototype_registry".into(),
        Bincode::encode(&STOCK_PRE_FIRE_REGISTRY).unwrap(),
    );
    snapshot
        .res
        .insert("replay".into(), replay.as_bytes().to_vec());
    snapshot
}

#[test]
fn p9_requires_both_authority_resources_in_current_snapshots() {
    let test = TestCtx::new_fast();
    let encoded = Bincode::encode(&test.g).unwrap();
    for name in ["service_incidents", "site_remnants", "fire_fleet"] {
        let mut snapshot: SimulationDeser = Bincode::decode(&encoded).unwrap();
        snapshot.res.remove(name).unwrap();
        assert!(
            matches!(snapshot.into_sim(), Err(SaveLoadError::Corrupt { reason, .. }) if reason.contains(name))
        );
    }
}

#[test]
fn p9_migrates_recorded_city_and_preserves_the_elapsed_fire_age() {
    let test = TestCtx::new_fast();
    test.build_roads(&[geom::vec3(65.0, 115.0, 0.0), geom::vec3(325.0, 115.0, 0.0)]);
    let site = test.build_house_near(vec2(120.0, 90.0));
    test.g.write::<prototypes::GameTime>().tick.0 = 10_001;
    test.g.write::<Fires>().by_building.insert(site, Tick(500));
    let replay = format!(
        r#"{{"version":"0.12.0+commands.2","enabled":true,"registry":{},"commands":[[1,{{"UnsupportedOldCommand":[]}}]]}}"#,
        STOCK_PRE_FIRE_REGISTRY
    );
    let snapshot = pre_fire_snapshot(&test, &replay);
    let bytes = Bincode::encode(&SimulationSer {
        header: snapshot._header,
        world: &snapshot.world,
        res: snapshot.res,
    })
    .unwrap();
    assert!(
        Bincode::decode::<Simulation>(&bytes).is_err(),
        "network decode must use the current schema"
    );
    let mut restored = Simulation::decode_snapshot(&bytes).unwrap();
    let incident = fire_incident(&restored, site).unwrap();
    assert_eq!(incident.alarm_tick, Tick(500));
    assert_eq!(restored.read::<ServiceIncidents>().active().count(), 1);
    assert!(restored.read::<Replay>().migrated_city());
    assert!(restored.read::<Replay>().obsolete_recording_discarded());

    let saved = Bincode::encode(&restored).unwrap();
    let current: Simulation = Bincode::decode(&saved).unwrap();
    assert!(current.read::<Replay>().migrated_city());
    assert!(!current.read::<Replay>().enabled);
    assert_eq!(current.hashes(), restored.hashes());

    restored.write::<prototypes::GameTime>().tick.0 += 15 * TICKS_PER_MINUTE - 1;
    crate::souls::events::events_update(&mut restored);
    assert!(fire_incident(&restored, site).is_some());
    restored.write::<prototypes::GameTime>().tick.0 += 1;
    crate::souls::events::events_update(&mut restored);
    assert!(restored.map().is_ruined(site));
}

#[test]
fn p9_rejects_wrong_legacy_replay_protocol_and_registry() {
    let test = TestCtx::new_fast();
    let wrong_protocol = r#"{"version":"0.12.0+commands.1","enabled":false,"commands":[]}"#;
    assert!(matches!(
        pre_fire_snapshot(&test, wrong_protocol).into_sim(),
        Err(SaveLoadError::IncompatibleReplay { .. })
    ));
    let wrong_registry =
        r#"{"version":"0.12.0+commands.2","enabled":false,"registry":7,"commands":[]}"#;
    assert!(matches!(
        pre_fire_snapshot(&test, wrong_registry).into_sim(),
        Err(SaveLoadError::PrototypeRegistry)
    ));
}

#[test]
#[ignore = "requires the packaged release snapshots under EGR_P9_FIXTURE_ROOT"]
fn p9_reopens_packaged_legacy_cities_and_preserves_original_files() {
    use common::saveload::{SlotId, SnapshotRevision};
    use std::fs;
    use std::path::PathBuf;

    super::init_test_simulation();
    let root = PathBuf::from(std::env::var_os("EGR_P9_FIXTURE_ROOT").unwrap());
    let source = SlotId::new("lesce_osm").unwrap();
    let original = fs::read(root.join("lesce_osm.zip")).unwrap();
    let restored = Simulation::load_from_slot(&root, &source).unwrap();
    assert_eq!(restored.get_tick(), 1048);
    assert_eq!(restored.world().humans.len(), 2903);
    assert!(restored.read::<Replay>().migrated_city());
    assert!(restored
        .save_to_slot(&root, &source, SnapshotRevision::new(9))
        .is_err());
    assert_eq!(fs::read(root.join("lesce_osm.zip")).unwrap(), original);

    let dest = SlotId::new("lesce_p9").unwrap();
    for n in [10, 11] {
        let revision = SnapshotRevision::new(n);
        restored.save_to_slot(&root, &dest, revision).unwrap();
        let saved = Simulation::load_from_slot(&root, &dest).unwrap();
        assert!(saved.read::<Replay>().migrated_city());
        assert!(!saved.read::<Replay>().enabled);
    }
    assert_eq!(fs::read(root.join("lesce_osm.zip")).unwrap(), original);
}

#[test]
fn p9_checks_truck_apron_on_a_stock_parking_road() {
    let (test, site) = fire_station_on_parking_loop();
    assert!(crate::souls::service_access::preview_fire_apron(&test.g, site).is_ok());
}

#[test]
fn p9_placement_commissions_a_physical_stock_truck() {
    let (test, site) = fire_station_on_parking_loop();
    let apron = crate::souls::service_access::preview_fire_apron(&test.g, site).unwrap();
    assert!(
        test.g.world.vehicles.values().any(|truck| {
            matches!(
                truck.vehicle.kind,
                crate::transportation::VehicleKind::Truck
            ) && truck.trans.pos == apron.bay.pos
                && truck.vehicle.state.is_parked()
        }),
        "placing the station must commission its truck at the checked bay"
    );
}

fn fire_station_on_parking_loop() -> (TestCtx, crate::map::BuildingID) {
    use crate::map::{BuildingKind, LaneKind};
    use crate::souls::service_access::preview_fire_apron;
    use geom::{vec3, Vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID};

    let mut test = TestCtx::new_fast();
    test.g = Simulation::new_authored_flat(1, 0).unwrap();
    test.build_roads(&[
        vec3(70.0, 95.0, 0.0),
        vec3(420.0, 95.0, 0.0),
        vec3(420.0, 410.0, 0.0),
        vec3(70.0, 410.0, 0.0),
        vec3(70.0, 95.0, 0.0),
    ]);
    let road = test
        .g
        .map()
        .roads()
        .values()
        .find(|road| road.points().first().y == 95.0 && road.points().last().y == 95.0)
        .unwrap()
        .id;
    test.g.apply_commands([
        &crate::world_command::WorldCommand::MapBuildSpecialBuilding {
            pos: OBB::new(vec2(220.0, 127.0), Vec2::Y, 20.0, 20.0),
            kind: BuildingKind::GoodsCompany(GoodsCompanyID::new("fire-station")),
            gen: BuildingGen::NoWalkway {
                door_pos: vec2(0.0, -9.0),
            },
            zone: None,
            connected_road: Some(road),
        },
    ]);
    let site = test
        .g
        .map()
        .buildings()
        .values()
        .find(|b| b.kind == BuildingKind::GoodsCompany(GoodsCompanyID::new("fire-station")))
        .unwrap_or_else(|| {
            panic!(
                "fixture station placement: {:?}",
                test.g.read::<crate::world_command::CommandOutcomes>().last
            )
        })
        .id;
    let apron = preview_fire_apron(&test.g, site).unwrap();
    let map = test.g.map();
    assert_eq!(apron.road, road);
    assert_eq!(
        map.get(apron.departure.lane).unwrap().kind,
        LaneKind::Driving
    );
    assert_eq!(apron.arrival.lane, apron.departure.lane);
    assert!(apron.departure.offset_cm > apron.arrival.offset_cm);
    assert!(apron.return_drive_ms > 0);
    assert!(!apron.parking_displacements.is_empty());
    assert_eq!(apron.arrival_sweep.last(), Some(&apron.bay.pos));
    assert_eq!(apron.departure_sweep.first(), Some(&apron.bay.pos));
    drop(map);
    (test, site)
}

fn staffed_fire_at_boarding() -> (TestCtx, crate::map::BuildingID, crate::map::BuildingID, Vec<crate::HumanID>) {
    use crate::souls::desire::{Work, WorkKind};
    use crate::souls::events::offer_fixture_fire;
    use crate::souls::goods_company::company_soul;
    use crate::souls::human::spawn_human;
    use crate::souls::service_dispatch::{
        service_station, ServiceTripPhase,
    };
    use crate::transportation::Location;
    use crate::{BuildingKind, SoulID};
    use geom::{Vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID};

    let (mut test, station) = fire_station_on_parking_loop();
    let road = test.g.map().get(station).unwrap().connected_road.unwrap();
    let workshop = GoodsCompanyID::new("fire-station");
    let Some(SoulID::GoodsCompany(company)) = company_soul(&mut test.g, station, workshop) else {
        panic!("fixture station must hire");
    };
    let power = GoodsCompanyID::new("solar-panel");
    let (_, road_end) = {
        let map = test.g.map();
        let road = map.get(road).unwrap();
        (road.src, road.dst)
    };
    let power_road = test
        .g
        .map()
        .get(road_end)
        .unwrap()
        .roads
        .iter()
        .copied()
        .find(|id| *id != road)
        .unwrap();
    let generator = test
        .g
        .map_mut()
        .build_special_building(
            &OBB::new(vec2(370.0, 245.0), Vec2::Y, 20.0, 20.0),
            BuildingKind::GoodsCompany(power),
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            None,
            Some(power_road),
        )
        .unwrap();
    company_soul(&mut test.g, generator, power).unwrap();
    let mut crew = Vec::new();
    for x in [185.0, 255.0] {
        let home = test.build_house_near(vec2(x, 435.0));
        let worker = spawn_human(&mut test.g, home).unwrap();
        test.g.world.edit_human(worker, |human| {
            human.work = Some(Work::new(station, WorkKind::Worker, 0.0));
            human.location = Location::Building(station);
            human.decision.wait = 100;
        });
        crew.push(worker);
    }
    test.g.world.companies[company].workers.0 = crew.clone();
    let target = test
        .g
        .map_mut()
        .build_special_building(
            &OBB::new(vec2(121.0, 129.0), Vec2::Y, 20.0, 20.0),
            BuildingKind::House,
            BuildingGen::NoWalkway {
                door_pos: vec2(0.0, -9.0),
            },
            None,
            Some(road),
        )
        .unwrap();
    test.g
        .apply_commands([&crate::world_command::WorldCommand::SetGameTime(
            prototypes::GameTime::new(Tick(16 * prototypes::TICKS_PER_HOUR + 1)),
        )]);
    assert!(offer_fixture_fire(&test.g, target));
    test.tick_fast();
    let phase = service_station(&test.g, station).unwrap().phase;
    assert_eq!(
        phase,
        Some(ServiceTripPhase::Boarding),
        "incident has blocker {:?}",
        crate::souls::service_dispatch::fire_response_blocker(&test.g, target)
    );

    (test, station, target, crew)
}

#[test]
fn p9_station_closure_waits_for_the_crew_to_return_to_the_door() {
    use crate::souls::demolition::{demolition_quote, demolition_status, start_demolition};
    use crate::souls::service_dispatch::{service_station, ServiceTripPhase};

    for phase in [ServiceTripPhase::WalkingToTruck, ServiceTripPhase::LeavingTruck] {
        let (mut test, station, _, _) = staffed_fire_at_boarding();
        for _ in 0..28_000 {
            if service_station(&test.g, station).unwrap().phase == Some(phase) {
                break;
            }
            test.tick_fast();
        }
        assert_eq!(service_station(&test.g, station).unwrap().phase, Some(phase));
        let quote = demolition_quote(&test.g, station).unwrap().quote;
        let mut work = crate::map::MAX_WATER_BATCH_WORK;
        start_demolition(&mut test.g, station, quote, &mut work).unwrap();
        let mut restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
        let mut schedule = Simulation::schedule();
        for _ in 0..28_000 {
            if let Some(view) = service_station(&test.g, station) {
                if view.phase.is_some() {
                    assert_eq!(demolition_status(&test.g, station).unwrap().progress_percent, 0);
                }
            } else {
                break;
            }
            test.tick_fast();
            restored.tick(&mut schedule, []);
        }
        assert!(test.g.map().get(station).is_none(), "clearance stuck in {phase:?}");
        assert_eq!(test.g.hashes(), restored.hashes());
    }
}

#[test]
fn p9_working_requires_both_responders_at_the_door() {
    use crate::souls::service_dispatch::{service_station, ServiceTripPhase};

    let (mut test, station, target, crew) = staffed_fire_at_boarding();
    for _ in 0..15_000 {
        if service_station(&test.g, station).unwrap().phase == Some(ServiceTripPhase::Working) {
            break;
        }
        test.tick_fast();
    }
    assert_eq!(service_station(&test.g, station).unwrap().phase, Some(ServiceTripPhase::Working));
    test.g.world.edit_human(crew[0], |human| human.trans.pos.y += 5.0);
    assert!(Bincode::decode::<Simulation>(&Bincode::encode(&test.g).unwrap()).is_err());
    for _ in 0..(7 * TICKS_PER_MINUTE) {
        test.tick_fast();
    }
    assert!(!test.g.read::<Fires>().events_since(0).any(|event|
        event.building == target && event.kind == crate::souls::events::FireEventKind::Extinguished
    ));
}

#[test]
fn p9_staffed_truck_suppresses_and_returns_after_saved_boarding() {
    use crate::souls::events::FireEventKind;
    use crate::souls::service_dispatch::{
        service_station, FireFleet, FireResponseEventKind, ServiceTripPhase,
    };
    use crate::transportation::Location;
    use geom::vec3;

    let (mut test, station, target, crew) = staffed_fire_at_boarding();
    let road = test.g.map().get(station).unwrap().connected_road.unwrap();
    let bytes = Bincode::encode(&test.g).unwrap();
    let mut restored: Simulation = Bincode::decode(&bytes).unwrap();
    assert_eq!(test.g.hashes(), restored.hashes());
    assert!(
        test.g.map_mut().remove_road(road).is_none(),
        "an occupied apron anchors the road"
    );
    assert!(
        restored.map_mut().remove_road(road).is_none(),
        "a reopened apron anchors the road"
    );

    let mut schedule = Simulation::schedule();
    let mut checked_priority = false;
    for _ in 0..28_000 {
        test.tick_fast();
        restored.tick(&mut schedule, []);
        let station_state = service_station(&test.g, station).unwrap();
        if !checked_priority && station_state.phase == Some(ServiceTripPhase::Outbound) {
            let probe: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
            let eligible = |sim: &Simulation| {
                sim.read::<FireFleet>().is_live_outbound(
                    sim.world(),
                    &sim.read::<ServiceIncidents>(),
                    station_state.truck,
                )
            };
            assert!(
                eligible(&probe),
                "both boarded responders serve the live generation"
            );
            crate::souls::events::remove_incident(&probe, target);
            assert!(
                !eligible(&probe),
                "the retired generation cannot get priority"
            );
            checked_priority = true;
        }
        if station_state.returned == 1 {
            break;
        }
    }
    assert!(
        checked_priority,
        "the fixture must exercise outbound junction eligibility"
    );
    assert_eq!(test.g.hashes(), restored.hashes());
    let station_view = service_station(&test.g, station).unwrap();
    let history = test.g.read::<FireFleet>();
    let events: Vec<_> = history
        .events()
        .filter(|event| event.site == target)
        .copied()
        .collect();
    assert_eq!(
        station_view.returned,
        1,
        "phase {station_view:?}, truck {:?}, events {events:?}",
        test.g
            .get(station_view.truck)
            .map(|truck| (truck.trans.pos, truck.speed.0, &truck.it))
    );
    let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        [
            FireResponseEventKind::Assigned,
            FireResponseEventKind::Boarded,
            FireResponseEventKind::Arrived,
            FireResponseEventKind::Suppressed,
            FireResponseEventKind::Returned,
        ]
    );
    assert!(events[2].tick.0 - events[0].tick.0 <= 25 * TICKS_PER_MINUTE);
    assert!(events[3].tick.0 - events[2].tick.0 >= 6 * TICKS_PER_MINUTE);
    assert!(test
        .g
        .read::<Fires>()
        .events_since(0)
        .any(|event| event.building == target && event.kind == FireEventKind::Extinguished));
    assert!(test.g.map().get(target).is_some());
    assert!(crew
        .iter()
        .all(|&id| test.g.get(id).unwrap().location == Location::Building(station)));
    assert_ne!(
        test.g.get(station_view.truck).unwrap().trans.pos,
        vec3(0.0, 0.0, 0.0)
    );
}

#[test]
fn p9_wet_clearance_keeps_the_paid_site_across_reload() {
    use crate::souls::demolition::{
        demolition_quote, demolition_status, demolition_update, ClearanceBlocker, DemolitionPhase,
    };
    use crate::world_command::WorldCommand;
    use geom::{vec3, Polygon};

    let (mut test, _) = fire_station_on_parking_loop();
    let house = test.build_house_near(vec2(180.0, 440.0));
    let quote = demolition_quote(&test.g, house).unwrap().quote;
    test.g
        .apply_commands([&WorldCommand::MapStartDemolition { site: house, quote }]);
    let shape = test.g.map().get(house).unwrap().obb;
    let center = shape.center();
    let basin = Polygon(vec![
        center + vec2(-7.0, -7.0),
        center + vec2(7.0, -7.0),
        center + vec2(7.0, 7.0),
        center + vec2(-7.0, 7.0),
    ]);
    // Inject the wet state through the terrain authoring path. The normal
    // WATER command rejects flooding an already occupied footprint.
    assert!(!test.g.map().environment().is_water(center));
    let lake = test
        .g
        .map_mut()
        .environment_mut_for_test()
        .fill_lake(&basin, 0, 2.0)
        .unwrap();
    assert!(
        test.g.map().environment().is_water(center),
        "lake covers {}",
        vec3(center.x, center.y, 0.0)
    );
    test.g.write::<prototypes::GameTime>().tick.0 += u64::from(quote.minutes) * TICKS_PER_MINUTE;
    demolition_update(&mut test.g);
    let blocked = demolition_status(&test.g, house).unwrap();
    assert_eq!(blocked.phase, Some(DemolitionPhase::ClearanceBlocked));
    assert_eq!(blocked.blocker, Some(ClearanceBlocker::Water));
    assert_eq!(blocked.progress_percent, 100);
    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    assert_eq!(demolition_status(&restored, house), Some(blocked));
    restored
        .map_mut()
        .environment_mut_for_test()
        .remove_freshwater(lake)
        .unwrap();
    restored.write::<prototypes::GameTime>().tick.0 += 5 * TICKS_PER_MINUTE;
    demolition_update(&mut restored);
    assert!(restored.map().get(house).is_none());
}

#[test]
fn p9_retrofit_rejects_a_changed_quote_before_reserving_the_curb() {
    use crate::economy::Government;
    use crate::map::BuildingKind;
    use crate::souls::service_dispatch::{fire_apron_quote, service_station};
    use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
    use geom::{Vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID};

    let (mut test, original) = fire_station_on_parking_loop();
    let road = test.g.map().get(original).unwrap().connected_road.unwrap();
    let retrofit = test
        .g
        .map_mut()
        .build_special_building(
            &OBB::new(vec2(338.0, 127.0), Vec2::Y, 20.0, 20.0),
            BuildingKind::GoodsCompany(GoodsCompanyID::new("fire-station")),
            BuildingGen::NoWalkway {
                door_pos: vec2(0.0, -9.0),
            },
            None,
            Some(road),
        )
        .unwrap();
    let valid = fire_apron_quote(&test.g, retrofit).unwrap();
    let mut invalid = valid;
    invalid.signature ^= 1;
    let balance = test.g.read::<Government>().money;
    test.g.apply_commands([&WorldCommand::MapRetrofitFireApron {
        site: retrofit,
        quote: invalid,
    }]);
    assert_eq!(
        test.g.read::<CommandOutcomes>().last[0].1,
        Rejection::StaleFireApronQuote
    );
    assert!(service_station(&test.g, retrofit).is_none());
    assert_eq!(test.g.read::<Government>().money, balance);
    let command = WorldCommand::MapRetrofitFireApron {
        site: retrofit,
        quote: valid,
    };
    test.g.apply_commands([&command, &command]);
    assert_eq!(test.g.read::<Government>().money, balance - valid.price);
    assert!(service_station(&test.g, retrofit).is_some());
    assert_eq!(test.g.world.vehicles.len(), 2);
}

#[test]
fn p9_paused_junction_edit_invalidates_a_boarding_route_across_reload() {
    use crate::souls::service_dispatch::cached_route_is_current;

    let (test, station, _, _) = staffed_fire_at_boarding();
    let junction = {
        let map = test.g.map();
        let road = map.get(station).unwrap().connected_road.unwrap();
        map.get(road).unwrap().src
    };
    assert!(cached_route_is_current(&test.g, station));
    test.g.map_mut().update_intersection(junction, |inter| {
        inter.turn_policy.left_turns = false;
    });
    assert!(!test.g.map().get(junction).unwrap().turn_policy.left_turns);
    assert!(!cached_route_is_current(&test.g, station));
    let mut expected = test.g;
    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&expected).unwrap()).unwrap();
    assert!(!cached_route_is_current(&restored, station));
    let mut a = Simulation::schedule();
    let mut b = Simulation::schedule();
    for _ in 0..5000 {
        expected.tick(&mut a, []);
        restored.tick(&mut b, []);
    }
    assert_eq!(expected.hashes(), restored.hashes());
}

#[test]
fn p9_idle_station_does_not_lock_a_frontage_removal() {
    use crate::souls::service_dispatch::service_station;
    use crate::world_command::WorldCommand;

    let (mut test, station) = fire_station_on_parking_loop();
    let road = test.g.map().get(station).unwrap().connected_road.unwrap();
    let truck = service_station(&test.g, station).unwrap().truck;
    test.g.apply_commands([&WorldCommand::MapRemoveRoad(road)]);
    assert!(test.g.map().get(road).is_none());
    assert!(test.g.map().get(station).is_some());
    assert!(service_station(&test.g, station).is_none());
    assert!(test.g.get(truck).is_none());
    assert!(Bincode::decode::<Simulation>(&Bincode::encode(&test.g).unwrap()).is_ok());
}

#[test]
fn p9_route_search_resumes_a_metered_frontier_across_save() {
    use crate::souls::service_access::{drive_route, preview_fire_apron, AccessBudget, AccessError, DriveSearch};

    let (test, station) = fire_station_on_parking_loop();
    let apron = preview_fire_apron(&test.g, station).unwrap();
    let map = test.g.map();
    let expected = drive_route(&map, apron.departure, apron.arrival, &mut AccessBudget(4096)).unwrap();
    let mut search = DriveSearch::new(apron.departure, apron.arrival);
    assert_eq!(search.advance(&map, &mut AccessBudget(1)), Err(AccessError::WorkLimit));
    let mut search: DriveSearch = Bincode::decode(&Bincode::encode(&search).unwrap()).unwrap();
    for _ in 0..1024 {
        match search.advance(&map, &mut AccessBudget(1)) {
            Ok(result) => {
                assert_eq!(result, expected);
                return;
            }
            Err(AccessError::WorkLimit) => {}
            Err(error) => panic!("route lost after yielding: {error:?}"),
        }
    }
    panic!("route search stalled");
}

#[test]
fn p9_scenario_fire_rules_are_local_and_survive_reload() {
    use crate::souls::events::FireScenarioRules;
    let ordinary = TestCtx::new_fast();
    let mut benchmark = Simulation::new_with_options(crate::SimulationOptions {
        terrain_size: 1,
        save_replay: false,
        ..crate::SimulationOptions::LEGACY
    });
    let rules = FireScenarioRules {
        suppress_incidental_offers: true,
        emergency_junction_preference: true,
    };
    rules.apply(&mut benchmark).unwrap();
    let restored: Simulation = Bincode::decode(&Bincode::encode(&benchmark).unwrap()).unwrap();
    assert_eq!(*restored.read::<FireScenarioRules>(), rules);
    assert_eq!(*ordinary.g.read::<FireScenarioRules>(), FireScenarioRules::default());
}
