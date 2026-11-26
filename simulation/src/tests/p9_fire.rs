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
    for name in ["service_incidents", "site_remnants"] {
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
    use crate::map::{BuildingKind, LaneKind};
    use crate::souls::service_access::preview_fire_apron;
    use geom::{vec3, Vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID};

    let test = TestCtx::new_fast();
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
    let site = test
        .g
        .map_mut()
        .build_special_building(
            &OBB::new(vec2(220.0, 140.0), Vec2::X, 20.0, 20.0),
            BuildingKind::GoodsCompany(GoodsCompanyID::new("fire-station")),
            BuildingGen::NoWalkway {
                door_pos: vec2(0.0, -9.0),
            },
            None,
            Some(road),
        )
        .unwrap();
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
}
