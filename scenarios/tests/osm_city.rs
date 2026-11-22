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
    let bridge = link(19);
    assert!(![road.from, road.to].contains(&bridge.from));
    assert!(![road.from, road.to].contains(&bridge.to));
    assert!(normalized
        .flat_crossings
        .iter()
        .any(|(ways, _)| ways.contains("way/19")));
    assert!(normalized
        .skipped
        .iter()
        .any(|(id, reason)| id == "way/18" && reason.contains("sub-10 m")));
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
    for (input, message) in [
        (
            r#"{"elements":[{"type":"node","id":1,"lat":46,"lon":14}],"remark":null}"#,
            "has a remark",
        ),
        (r#"{"elements":["#, "invalid Overpass JSON"),
        (
            r#"{"elements":[{"type":"way","id":1,"nodes":[1,2],"geometry":[{"lat":46,"lon":14}]}]}"#,
            "node/geometry",
        ),
        (
            r#"{"elements":[{"type":"node","id":1,"lat":91,"lon":14}]}"#,
            "malformed coordinates",
        ),
        (
            r#"{"elements":[{"type":"node","id":1,"lat":46,"lon":14},{"type":"node","id":1,"lat":46,"lon":14}]}"#,
            "duplicate node/1",
        ),
        (
            r#"{"elements":[{"type":"node","id":1,"lat":46,"lon":14},{"type":"way","id":1,"nodes":[1,2],"geometry":[{"lat":47,"lon":14},{"lat":46,"lon":14}]}]}"#,
            "inconsistent coordinates",
        ),
    ] {
        assert!(
            read_overpass(input.as_bytes())
                .unwrap_err()
                .to_string()
                .contains(message),
            "{input}"
        );
    }
    let nodes: Vec<_> = (0..20_001)
        .map(|id| serde_json::json!({"type":"node","id":id,"lat":46,"lon":14}))
        .collect();
    let input = serde_json::to_vec(&serde_json::json!({"elements":nodes})).unwrap();
    assert!(read_overpass(input.as_slice())
        .unwrap_err()
        .to_string()
        .contains("element count"));
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
        lot_layout: simulation::map::LotLayout::Varied,
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

#[test]
fn osm_reports_and_drops_nearly_closed_way() {
    let mut input: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    input["elements"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "type":"way", "id":9001, "nodes":[9001,9002,9003],
            "tags":{"highway":"residential"},
            "geometry":[{"lat":46.35,"lon":14.14}, {"lat":46.3501,"lon":14.14},
                        {"lat":46.35,"lon":14.140006}]
        }));
    let bytes = serde_json::to_vec(&input).unwrap();
    let city = normalize(read_overpass(bytes.as_slice()).unwrap(), LESCE_BOUNDS).unwrap();
    assert!(city
        .skipped
        .iter()
        .any(|(id, reason)| id == "way/9001" && reason.contains("endpoints")));
    assert!(city
        .city
        .links
        .iter()
        .all(|link| !link.source_way_ids.contains(&9001)));
}

#[test]
fn osm_bounds_long_ways_and_link_counts() {
    let make_way = |id: u64, lat: f64, count: usize| {
        let nodes: Vec<_> = (0..count as u64).map(|i| id * 100_000 + i).collect();
        let geometry: Vec<_> = (0..count)
            .map(|i| serde_json::json!({"lat":lat + (i % 2) as f64 * 0.000003,"lon":14.14 + (i as f64 * 0.000004)}))
            .collect();
        serde_json::json!({
            "type":"way", "id":id, "nodes":nodes, "geometry":geometry,
            "tags":{"highway":"residential", "layer":format!("{id}")}
        })
    };
    let too_long =
        serde_json::to_vec(&serde_json::json!({"elements":[make_way(1,46.35,25_000)]})).unwrap();
    assert!(read_overpass(too_long.as_slice())
        .unwrap_err()
        .to_string()
        .contains("point geometry limit"));

    let disjoint = serde_json::to_vec(&serde_json::json!({
        "elements":[make_way(1,46.35,8000),make_way(2,46.36,8000)]
    }))
    .unwrap();
    let disjoint = normalize(read_overpass(disjoint.as_slice()).unwrap(), LESCE_BOUNDS).unwrap();
    assert_eq!(disjoint.city.links.len(), 2);
    assert!(disjoint.flat_crossings.is_empty());

    let ways: Vec<_> = (1..=6001).map(|id| make_way(id, 46.35, 2)).collect();
    let input = serde_json::to_vec(&serde_json::json!({"elements":ways})).unwrap();
    let error = normalize(read_overpass(input.as_slice()).unwrap(), LESCE_BOUNDS).unwrap_err();
    assert!(error.to_string().contains("6000-link import limit"));
}

#[test]
fn osm_wood_multipolygon_respects_holes() {
    let mut input: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    let elements = input["elements"].as_array_mut().unwrap();
    let mut wood = elements.iter().find(|e| e["id"] == 50).unwrap().clone();
    wood["id"] = 51.into();
    wood["tags"] = serde_json::json!({"type":"multipolygon", "natural":"wood"});
    elements.push(wood);
    let bytes = serde_json::to_vec(&input).unwrap();
    let city = normalize(read_overpass(bytes.as_slice()).unwrap(), LESCE_BOUNDS).unwrap();
    assert_eq!(city.forests.len(), 1);
    assert!(city.forests[0].contains(LESCE_BOUNDS.project(46.3585, 14.157)));
    assert!(!city.forests[0].contains(LESCE_BOUNDS.project(46.3585, 14.155)));
}

#[test]
fn osm_bounds_overlapping_amenity_polygon_matches() {
    let mut input: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    let elements = input["elements"].as_array_mut().unwrap();
    // The points are inside each triangular footprint's bounds, across its diagonal.
    for id in 100..112 {
        let mut geometry: Vec<_> = (0..5000)
            .map(|i| serde_json::json!({"lat":46.35 + i as f64 * 0.000001, "lon":14.14}))
            .collect();
        geometry.push(serde_json::json!({"lat":46.35, "lon":14.145}));
        geometry.push(geometry[0].clone());
        let mut nodes: Vec<_> = (0..geometry.len() - 1).map(|i| id * 10_000 + i).collect();
        nodes.push(nodes[0]);
        elements.push(serde_json::json!({
            "type":"way", "id":id, "nodes":nodes, "geometry":geometry,
            "tags":{"amenity":"school"}
        }));
    }
    for id in 10_000..11_000 {
        elements.push(serde_json::json!({
            "type":"node", "id":id, "lat":46.354, "lon":14.144,
            "tags":{"amenity":"school"}
        }));
    }
    let bytes = serde_json::to_vec(&input).unwrap();
    let error = normalize(read_overpass(bytes.as_slice()).unwrap(), LESCE_BOUNDS)
        .err()
        .expect("overlapping amenity matches must exceed the geometry work limit");
    assert!(error.to_string().contains("amenity polygon matching"));
}

#[test]
fn osm_does_not_infer_homes_from_buildings_tagged_as_accessories() {
    let mut input: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    let elements = input["elements"].as_array_mut().unwrap();
    for (id, tags) in [
        (101, serde_json::json!({"building":"yes"})),
        (
            102,
            serde_json::json!({"building":"yes", "building:use":"garage"}),
        ),
        (
            103,
            serde_json::json!({"building":"yes", "building:part":"shed"}),
        ),
        (104, serde_json::json!({"building":"shed"})),
    ] {
        elements.push(serde_json::json!({
            "type":"way","id":id,"nodes":[id * 10, id * 10 + 1, id * 10 + 2, id * 10],
            "tags":tags,
            "geometry":[{"lat":46.35,"lon":14.14},{"lat":46.351,"lon":14.14},
                        {"lat":46.35,"lon":14.141},{"lat":46.35,"lon":14.14}]
        }));
    }
    let bytes = serde_json::to_vec(&input).unwrap();
    let city = normalize(read_overpass(bytes.as_slice()).unwrap(), LESCE_BOUNDS).unwrap();
    assert_eq!(
        city.features
            .iter()
            .filter(|feature| feature.inferred_house)
            .map(|feature| feature.source.as_str())
            .collect::<Vec<_>>(),
        ["way/101"]
    );
}

#[test]
fn osm_junctions_default_to_priority_and_keep_mapped_signals() {
    use simulation::map::TrafficControl;
    use simulation::{MapPreset, NewGameOptions, Simulation};

    scenarios::ensure_repo_root_cwd();
    simulation::init::init();
    let input = include_bytes!("fixtures/osm-junction-control.json");
    let normalized = normalize(read_overpass(input.as_slice()).unwrap(), LESCE_BOUNDS).unwrap();
    let options = NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: 16,
        ..Default::default()
    };
    let (sim, network) = Simulation::from_imported_city(&normalized.city, &options).unwrap();
    let map = sim.map();
    let controls = |lat, lon| {
        let at = LESCE_BOUNDS.project(lat, lon);
        let junction = normalized
            .city
            .junctions
            .iter()
            .find(|junction| junction.position.distance(at) < 1.0)
            .unwrap();
        let id = network.junctions[&junction.id];
        let controls: Vec<TrafficControl> = map.intersections()[id]
            .roads
            .iter()
            .flat_map(|&road| map.roads()[road].incoming_lanes_to(id).iter())
            .filter(|(_, kind)| *kind == LaneKind::Driving)
            .map(|&(lane, _)| map.lanes()[lane].control)
            .collect();
        assert!(controls.len() >= 3, "{lat},{lon}: {controls:?}");
        controls
    };
    // Mapped signals on an approach a few metres from the junction node.
    assert!(controls(46.35, 14.142).iter().all(TrafficControl::is_light));
    // A pedestrian-only signal leaves the crossroads under priority rules.
    assert!(controls(46.35, 14.146)
        .iter()
        .all(TrafficControl::is_always));
    // A tee on an arterial has no all-way stop.
    assert!(controls(46.353, 14.142)
        .iter()
        .all(TrafficControl::is_always));
    // Streets crossing an arterial cannot find gaps in its traffic without signals.
    assert!(controls(46.356, 14.142)
        .iter()
        .all(TrafficControl::is_light));
    // Roundabouts yield on entry, even where arterials meet.
    let roundabout = normalized
        .city
        .junctions
        .iter()
        .find(|junction| junction.roundabout_radius.is_some())
        .unwrap();
    let roundabout = network.junctions[&roundabout.id];
    assert_eq!(map.intersections()[roundabout].roads.len(), 4);
    assert!(map.intersections()[roundabout]
        .roads
        .iter()
        .flat_map(|&road| map.roads()[road].incoming_lanes_to(roundabout).iter())
        .filter(|(_, kind)| *kind == LaneKind::Driving)
        .all(|&(lane, _)| map.lanes()[lane].control.is_always()));
    // Signals away from any road junction are reported, not guessed.
    assert!(normalized
        .skipped
        .iter()
        .any(|(id, reason)| id == "node/5103" && reason.contains("traffic signal")));
}
