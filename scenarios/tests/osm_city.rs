use scenarios::osm_city::{normalize, read_overpass, LESCE_BOUNDS};
use simulation::map::LaneKind;

const FIXTURE: &[u8] = include_bytes!("fixtures/osm-city-small.json");

#[test]
fn osm_normalizes_shared_nodes_clips_and_reverses() {
    let normalized = normalize(read_overpass(FIXTURE).unwrap(), LESCE_BOUNDS).unwrap();
    let link = |id| {
        normalized
            .city
            .links
            .iter()
            .find(|link| link.source_way_ids.contains(&id))
            .unwrap()
    };
    let road = link(10);
    let rail = link(13);
    assert!(road.points.len() > 2);
    assert_ne!(road.to, rail.to);
    assert_eq!(normalized.city.boundary_stubs.len(), 2);
    let ramp = link(12);
    assert!(ramp.points.first().unwrap().y > ramp.points.last().unwrap().y);
    assert!(ramp
        .pattern
        .lanes_forward
        .iter()
        .any(|lane| lane.0 == LaneKind::Driving));
    assert!(ramp.pattern.lanes_backward.is_empty());
    assert!(normalized
        .city
        .junctions
        .iter()
        .any(|junction| junction.roundabout_radius.is_some()));
    assert!(normalized
        .city
        .links
        .iter()
        .all(|link| !link.source_way_ids.contains(&15)));
    assert_eq!(normalized.sites.len(), 1);
    let feature = &normalized.features[0];
    assert!(!feature.contains(LESCE_BOUNDS.project(46.3585, 14.155)));
    assert!(feature.contains(LESCE_BOUNDS.project(46.3585, 14.157)));

    let mut shuffled: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    shuffled["elements"].as_array_mut().unwrap().reverse();
    let shuffled = serde_json::to_vec(&shuffled).unwrap();
    assert_eq!(
        normalized,
        normalize(read_overpass(shuffled.as_slice()).unwrap(), LESCE_BOUNDS).unwrap()
    );
    let origin = LESCE_BOUNDS.project(46.3605, 14.1605);
    let northeast = LESCE_BOUNDS.project(46.3615, 14.1615) - origin;
    assert!((northeast.x - 76.96).abs() < 0.1 && (northeast.y - 111.16).abs() < 0.1);
}

#[test]
fn osm_rejects_partial_or_malformed_extracts() {
    for input in [
        r#"{"elements":[],"remark":null}"#,
        r#"{"elements":["#,
        r#"{"elements":[{"type":"way","id":1,"nodes":[1,2],"geometry":[{"lat":46,"lon":14}]}]}"#,
        r#"{"elements":[{"type":"node","id":1,"lat":1e999,"lon":14}]}"#,
    ] {
        assert!(read_overpass(input.as_bytes()).is_err(), "{input}");
    }
}

#[test]
fn osm_fixture_builds_routes_and_round_trips_a_slot() {
    use prototypes::{Tick, TICKS_PER_HOUR};
    use simulation::map::{
        Density, PathKind, Pathfinder, Traversable, TraverseDirection, TraverseKind, ZoneGrid,
        ZoneKind,
    };
    use simulation::world_command::WorldCommand;
    use simulation::{MapPreset, NewGameOptions, Simulation, SlotId, SnapshotRevision};

    scenarios::ensure_repo_root_cwd();
    simulation::init::init();
    let normalized = normalize(read_overpass(FIXTURE).unwrap(), LESCE_BOUNDS).unwrap();
    let options = NewGameOptions {
        preset: MapPreset::Flat,
        seed: 42,
        terrain_chunks: 16,
        starting_money_bucks: 1_000_000,
        progression: simulation::economy::progression::ProgressionMode::FreeBuild,
    };
    let build = || {
        let (mut sim, network) =
            Simulation::from_imported_city(&normalized.city, &options).unwrap();
        let lot = {
            let map = sim.map();
            map.check_invariants();
            let stubs = &normalized.city.boundary_stubs;
            let from = &map.roads()[network.roads[&stubs[0].link]];
            let to = &map.roads()[network.roads[&stubs[1].link]];
            let start = from
                .outgoing_lanes_from(network.junctions[&stubs[0].junction])
                .iter()
                .find(|(_, kind)| *kind == LaneKind::Driving)
                .unwrap()
                .0;
            let end = to
                .incoming_lanes_to(network.junctions[&stubs[1].junction])
                .iter()
                .find(|(_, kind)| *kind == LaneKind::Driving)
                .unwrap()
                .0;
            assert!(PathKind::Vehicle
                .path(
                    &map,
                    Tick(0),
                    Traversable::new(TraverseKind::Lane(start), TraverseDirection::Forward),
                    end
                )
                .is_some());
            assert!(map
                .intersections()
                .values()
                .any(|junction| junction.is_roundabout() && junction.turns().len() > 0));
            assert!(map
                .lanes()
                .values()
                .all(|lane| lane.points.iter().all(|p| p.is_finite())));
            let ramp = normalized
                .city
                .links
                .iter()
                .position(|link| link.source_way_ids == [12])
                .unwrap();
            assert!(map
                .lots()
                .values()
                .all(|lot| lot.parent != network.roads[&ramp]));
            map.lots()
                .values()
                .find(|lot| lot.parent == from.id)
                .unwrap()
                .clone()
        };
        sim.apply_commands([&WorldCommand::ZonePaint {
            area: lot.shape,
            kind: Some(ZoneKind::Residential),
            density: Density::Low,
        }]);
        assert!(sim.read::<ZoneGrid>().lot_zone(&lot.shape).is_some());
        sim.apply_commands([&WorldCommand::MapBuildHouse(lot.id)]);
        assert_eq!(sim.map().buildings().len(), 1);
        sim
    };
    let a = build();
    let b = build();
    assert_eq!(a.hashes(), b.hashes());
    let id = SlotId::new("osm_fixture").unwrap();
    let revision = SnapshotRevision::new(65);
    let root = std::env::temp_dir().join(format!("egr-osm-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let other = root.join("again");
    std::fs::create_dir_all(&other).unwrap();
    a.save_to_slot(&root, &id, revision).unwrap();
    b.save_to_slot(&other, &id, revision).unwrap();
    assert_eq!(
        std::fs::read(root.join("osm_fixture.zip")).unwrap(),
        std::fs::read(other.join("osm_fixture.zip")).unwrap()
    );
    let mut loaded = Simulation::load_from_slot(&root, &id).unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..TICKS_PER_HOUR {
        loaded.tick(&mut schedule, &[]);
    }
    assert_eq!(loaded.get_tick(), a.get_tick() + TICKS_PER_HOUR);
    loaded.map().check_invariants();
    std::fs::remove_dir_all(root).unwrap();
}
