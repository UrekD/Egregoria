use crate::economy::Government;
use crate::map::road_edit::{
    preview_road_edit, road_edit_stamp, RoadEditPreview, RoadEditRequest, RoadEditStatus,
    RoadUpgrade, RoadUpgradePreview,
};
use crate::map::{LaneKind, LanePatternBuilder, MapProject, RoadID};
use crate::map_dynamic::{itinerary_update, Itinerary, Transit};
use crate::transportation::{
    spawn_driving_vehicle, transport_grid_synchronize, ColliderOwnerLookup, TransportGrid,
    TraversalOwner, VehicleKind,
};
use crate::world_command::{CommandOutcomes, PrepareBudget, Rejection, WorldCommand};
use crate::Simulation;
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, Transform};
use prototypes::{Money, Tick};

fn road_fixture() -> (Simulation, RoadID) {
    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 200.0, 0.0)),
        to: MapProject::ground(vec3(800.0, 200.0, 0.0)),
        inter: Some(vec2(440.0, 630.0)),
        pat: LanePatternBuilder::new().build(),
    }]);
    let road = sim.map().roads().keys().next().expect("curved road");
    (sim, road)
}

fn preview_upgrade(
    sim: &Simulation,
    request: &RoadEditRequest,
    budget: &mut PrepareBudget,
) -> Result<RoadUpgradePreview, Rejection> {
    preview_road_edit(sim, request, budget).map(|preview| match preview {
        RoadEditPreview::Upgrade(upgrade) => upgrade,
        RoadEditPreview::JunctionRules(_) => panic!("expected an upgrade preview"),
    })
}

fn upgrade(sim: &Simulation, road: RoadID, speed: f32) -> (WorldCommand, RoadUpgradePreview) {
    let mut budget = PrepareBudget::for_preview(4_194_304);
    let expected = road_edit_stamp(sim, road, &mut budget).unwrap();
    let change = RoadUpgrade::Replace(LanePatternBuilder::new().speed_limit(speed).build());
    let request = RoadEditRequest::Upgrade {
        road,
        expected,
        change: change.clone(),
    };
    let preview = preview_upgrade(sim, &request, &mut budget).unwrap();
    let command = WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        quote: preview.quote,
    };
    (command, preview)
}

#[test]
fn p7_edit_quote_curve_stale_and_paused_reload() {
    let (mut sim, road) = road_fixture();
    let (command, preview) = upgrade(&sim, road, 13.0);
    let original_money = sim.read::<Government>().money;
    let current = sim.map();
    let road_before = current.get(road).unwrap();
    let pattern = road_before.pattern(current.lanes());
    let chord = Government::connection_cost(
        road_before.points().first(),
        road_before.points().last(),
        &pattern,
    )
    .unwrap();
    assert!(preview.cost > chord);
    assert_eq!(
        preview.deck.as_slice(),
        road_before.interfaced_points().as_slice()
    );
    drop(current);

    let elapsed = sim.get_tick();
    sim.apply_commands([&command, &command]);
    let last = sim.read::<CommandOutcomes>();
    let applied = last.road_edit(0).unwrap().as_ref().unwrap().clone();
    assert_eq!(applied.status, RoadEditStatus::Applied);
    assert_eq!(applied.debit, preview.cost);
    assert_eq!(last.road_edit(1), Some(&Err(Rejection::StaleRoadEdit)));
    assert_eq!(last.accepted, [0]);
    drop(last);
    assert_eq!(sim.get_tick(), elapsed);
    assert_eq!(
        sim.read::<Government>().money,
        original_money - preview.cost
    );

    let mut loaded: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    let (repeat, next) = upgrade(&loaded, road, 13.0);
    assert_eq!(next.stamp, applied.stamp);
    assert_eq!(next.retained_lanes, applied.retained_lanes);
    assert_eq!(next.deck.as_slice(), preview.deck.as_slice());
    assert_eq!(next.cost, Money::ZERO);
    let (different, _) = upgrade(&loaded, road, 14.0);
    let WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        ..
    } = different
    else {
        unreachable!()
    };
    let wrong_quote = WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        quote: next.quote,
    };
    loaded.apply_commands([&wrong_quote, &repeat]);
    let last = loaded.read::<CommandOutcomes>();
    assert_eq!(last.road_edit(0), Some(&Err(Rejection::StaleRoadQuote)));
    assert_eq!(
        last.road_edit(1).unwrap().as_ref().unwrap().status,
        RoadEditStatus::NoChange
    );
}

#[test]
fn p7_edit_batch_berth_and_retained_terminal() {
    let (mut sim, road) = road_fixture();
    let (lane, position, stop_pos, dir, itinerary) = {
        let map = sim.map();
        let (lane, _) = map
            .get(road)
            .unwrap()
            .lanes_iter()
            .find(|(_, kind)| *kind == LaneKind::Driving)
            .unwrap();
        let points = &map.get(lane).unwrap().points;
        let position = points.point_along(30.0);
        let stop_pos = points.point_along(100.0);
        let dir = (points.point_along(101.0) - stop_pos).xy().normalize();
        let itinerary =
            Itinerary::route_between_lanes(Tick(0), &map, (lane, 30.0), (lane, 120.0)).unwrap();
        (lane, position, stop_pos, dir, itinerary)
    };
    let vehicle = spawn_driving_vehicle(
        &mut sim,
        VehicleKind::Car,
        Transform::new_dir(position, dir.z0()),
    )
    .unwrap();
    sim.world.vehicles[vehicle].it = itinerary;
    let handle = sim.world.vehicles[vehicle].collider.unwrap().0;
    let traversal = *sim.world.vehicles[vehicle].it.get_travers().unwrap();
    assert_eq!(
        sim.read::<TransportGrid>()
            .collider_owner(sim.world(), handle),
        ColliderOwnerLookup::Deferred
    );
    sim.write::<TransportGrid>().watch_traversal_users();
    let (world, resources) = sim.world_res();
    transport_grid_synchronize(world, resources);
    itinerary_update(world, resources);
    {
        let grid = sim.read::<TransportGrid>();
        assert_eq!(
            grid.collider_owner(sim.world(), handle),
            ColliderOwnerLookup::Owned(TraversalOwner::Vehicle(vehicle))
        );
        assert_eq!(
            grid.traversal_users(&sim.map(), traversal)
                .unwrap()
                .users()
                .collect::<Vec<_>>(),
            [TraversalOwner::Vehicle(vehicle)]
        );
    }
    let before = sim.world().vehicles.get(vehicle).unwrap().it.get_terminal();
    let (command, _) = upgrade(&sim, road, 12.0);
    let add_stop = WorldCommand::TransitAddStop { pos: stop_pos, dir };
    sim.apply_commands([&command, &add_stop]);
    let outcomes = sim.read::<CommandOutcomes>();
    assert_eq!(outcomes.accepted, [0, 1], "{:?}", outcomes.last);
    assert_eq!(sim.map().get(lane).unwrap().speed_limit, 12.0);
    let vehicle = sim.world().vehicles.get(vehicle).unwrap();
    assert_eq!(vehicle.it.get_terminal(), before);
    assert_eq!(vehicle.trans.pos, position);
    assert_eq!(sim.read::<Transit>().stops.len(), 1);
}

#[test]
fn p7_edit_widening_projects_the_final_local_graph_before_the_commit_gate() {
    let (mut sim, road) = road_fixture();
    let before = sim.hashes();
    let mut budget = PrepareBudget::for_preview(4_194_304);
    let expected = road_edit_stamp(&sim, road, &mut budget).unwrap();
    let change = RoadUpgrade::Replace(LanePatternBuilder::new().n_lanes(2).build());
    let request = RoadEditRequest::Upgrade {
        road,
        expected,
        change: change.clone(),
    };
    let preview = preview_upgrade(&sim, &request, &mut budget).unwrap();
    let topology = preview.projected_topology.as_ref().unwrap();
    assert_eq!(topology.affected_roads, [road]);
    assert_eq!(topology.junctions.len(), 2);
    assert!(preview.cost > Money::ZERO);
    assert_eq!(sim.hashes(), before);

    sim.apply_commands([&WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        quote: preview.quote,
    }]);
    assert_eq!(
        sim.read::<CommandOutcomes>().road_edit(0),
        Some(&Err(Rejection::RoadTopologyUnavailable))
    );
    assert_eq!(sim.hashes(), before);
}

#[test]
fn p7_edit_budget_and_insufficient_funds_are_read_only() {
    let (mut sim, road) = road_fixture();
    let (command, preview) = upgrade(&sim, road, 11.0);
    let mut budget = PrepareBudget::for_preview(1);
    assert_eq!(
        road_edit_stamp(&sim, road, &mut budget),
        Err(Rejection::RoadWorkLimit)
    );
    sim.write::<Government>().money = preview.cost - Money::new_inner(1);
    let before = sim.hashes();
    sim.apply_commands([&command]);
    assert_eq!(
        sim.read::<CommandOutcomes>().road_edit(0),
        Some(&Err(Rejection::InsufficientFunds))
    );
    assert_eq!(sim.hashes(), before);
}

#[test]
fn p7_replay_retains_mixed_road_receipt_and_partial_links() {
    use crate::map::{LandMode, LandTileId, TerraformKind, WaterEdit};
    use crate::{MapPreset, NewGameOptions, Replay};

    super::init_test_simulation();
    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        land_mode: LandMode::Tiled,
        ..NewGameOptions::default()
    })
    .unwrap();
    let opening = sim.read::<Replay>().command_count();
    let initial_roads = sim
        .map()
        .roads()
        .keys()
        .collect::<std::collections::BTreeSet<_>>();
    let buy = WorldCommand::BuyTile {
        tile: LandTileId { x: 9, y: 7 },
    };
    let pattern = LanePatternBuilder::new().build();
    let from = |x, y| MapProject::ground(vec3(x, y, 0.0));
    let construction = WorldCommand::MapMakeConnection {
        from: from(4620.0, 3700.0),
        to: from(4800.0, 3700.0),
        inter: None,
        pat: pattern.clone(),
    };
    sim.apply_commands([&buy, &construction]);
    assert!(sim
        .read::<CommandOutcomes>()
        .tile_purchase(0)
        .unwrap()
        .is_ok());
    assert_eq!(sim.map().roads().len(), initial_roads.len() + 1);
    let road = sim
        .map()
        .roads()
        .keys()
        .find(|id| !initial_roads.contains(id))
        .unwrap();
    let (edit, _) = upgrade(&sim, road, 12.0);
    let rejected_wet = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    });
    let raise = WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: vec2(7072.0, 3072.0),
        radius: 48.0,
        amount: 100.0,
        level: 0.0,
        slope: None,
    };
    let links = WorldCommand::MapMakeMultipleConnections(
        vec![from(4620.0, 3880.0), from(4800.0, 3880.0)],
        vec![
            (0, 0, None, pattern.clone()),
            (0, 1, None, pattern.clone()),
            (0, 0, None, pattern),
        ],
    );
    sim.tick(
        &mut Simulation::schedule(),
        [&rejected_wet, &raise, &links, &edit],
    );
    {
        let last = sim.read::<CommandOutcomes>();
        assert_eq!(
            last.rejections_for(1),
            [("Terraform", Rejection::WaterWorkLimit)]
        );
        let indexed = &last.connection_batch(2).unwrap().links;
        assert!(indexed[0].is_err());
        assert_eq!(indexed[1].as_ref().unwrap().len(), 1);
        assert_eq!(indexed[0], indexed[2]);
        assert_eq!(
            last.road_edit(3).unwrap().as_ref().unwrap().status,
            RoadEditStatus::Applied
        );
    }
    let replay = sim.read::<Replay>();
    assert_eq!(
        replay.batch_boundaries(),
        [(0, opening + 2, false), (0, opening + 6, true)]
    );
    let replay = Bincode::decode::<Replay>(&Bincode::encode(&*replay).unwrap()).unwrap();
    let (mut restored, mut cursor) = Simulation::try_from_replay(replay).unwrap();
    assert!(cursor.advance_tick(&mut restored, &mut Simulation::schedule()));
    assert_eq!(sim.hashes(), restored.hashes());
}

#[test]
fn p7_edit_replacement_rejects_rail_and_accepts_both_one_way_directions() {
    let (mut sim, _) = road_fixture();
    let stock = LanePatternBuilder::new();
    let forward = stock.one_way(true).build();
    let mut backward = forward.clone();
    std::mem::swap(&mut backward.lanes_forward, &mut backward.lanes_backward);
    for (row, pattern) in [stock.rail(true).build(), forward, backward]
        .into_iter()
        .enumerate()
    {
        let old = sim
            .map()
            .roads()
            .keys()
            .collect::<std::collections::BTreeSet<_>>();
        let y = 700.0 + row as f32 * 100.0;
        sim.apply_commands([&WorldCommand::MapMakeConnection {
            from: MapProject::ground(vec3(100.0, y, 0.0)),
            to: MapProject::ground(vec3(380.0, y, 0.0)),
            inter: None,
            pat: pattern,
        }]);
        let road = sim
            .map()
            .roads()
            .keys()
            .find(|id| !old.contains(id))
            .unwrap();
        let mut budget = PrepareBudget::for_preview(4_194_304);
        let expected = road_edit_stamp(&sim, road, &mut budget).unwrap();
        let request = RoadEditRequest::Upgrade {
            road,
            expected,
            change: if row == 0 {
                RoadUpgrade::Replace(stock.build())
            } else {
                RoadUpgrade::ReverseOneWay
            },
        };
        let result = preview_upgrade(&sim, &request, &mut budget);
        if row == 0 {
            assert!(
                matches!(result, Err(Rejection::InvalidRoadPattern)),
                "{result:?}"
            );
        } else {
            assert!(result.unwrap().projected_topology.is_some());
        }
    }
}

#[test]
fn p7_life_busy_one_way_reversal_keeps_occupants_then_allocates_new_lane() {
    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 200.0, 0.0)),
        to: MapProject::ground(vec3(400.0, 200.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::new()
            .one_way(true)
            .parking(false)
            .build(),
    }]);
    let (road, lane, src, dst, pos, it, walking) = {
        let map = sim.map();
        let (road_id, road) = map.roads().iter().next().unwrap();
        let lane = road
            .lanes_iter()
            .find(|&(_, kind)| kind == LaneKind::Driving)
            .unwrap()
            .0;
        let driving = map.get(lane).unwrap();
        let it =
            Itinerary::route_between_lanes(Tick(0), &map, (lane, 30.0), (lane, 100.0)).unwrap();
        let walking = road
            .lanes_iter()
            .filter_map(|(id, kind)| (kind == LaneKind::Walking).then_some(id))
            .collect::<Vec<_>>();
        (
            road_id,
            lane,
            driving.src,
            driving.dst,
            driving.points.point_along(30.0),
            it,
            walking,
        )
    };
    let request = || RoadUpgrade::ReverseOneWay;
    let preview = |sim: &Simulation| {
        let mut budget = PrepareBudget::for_preview(4_194_304);
        let expected = road_edit_stamp(sim, road, &mut budget).unwrap();
        preview_upgrade(
            sim,
            &RoadEditRequest::Upgrade {
                road,
                expected,
                change: request(),
            },
            &mut budget,
        )
    };
    let initial = preview(&sim).unwrap();
    let id = spawn_driving_vehicle(
        &mut sim,
        VehicleKind::Car,
        Transform::new_dir(pos, geom::Vec3::X),
    )
    .unwrap();
    sim.world.vehicles[id].it = it;
    let busy = sim.hashes();
    sim.apply_commands([&WorldCommand::MapUpgradeRoad {
        road,
        expected: initial.stamp,
        change: request(),
        quote: initial.quote,
    }]);
    let outcome = sim.read::<CommandOutcomes>();
    assert!(matches!(
        outcome.road_edit(0),
        Some(Err(Rejection::BusyConflict))
    ));
    drop(outcome);
    assert_eq!(sim.hashes(), busy);

    sim.read::<crate::ParCommandBuffer<crate::VehicleEnt>>()
        .kill(id);
    sim.apply_commands(std::iter::empty());
    let after = preview(&sim).unwrap();
    sim.apply_commands([&WorldCommand::MapUpgradeRoad {
        road,
        expected: after.stamp,
        change: request(),
        quote: after.quote,
    }]);
    let outcomes = sim.read::<CommandOutcomes>();
    assert_eq!(
        outcomes.road_edit(0).unwrap().as_ref().unwrap().status,
        RoadEditStatus::Applied
    );
    let map = sim.map();
    assert!(map.get(lane).is_none());
    let road = map.get(road).unwrap();
    let next = road
        .lanes_iter()
        .find(|&(_, kind)| kind == LaneKind::Driving)
        .unwrap()
        .0;
    assert_eq!(
        (map.get(next).unwrap().src, map.get(next).unwrap().dst),
        (dst, src)
    );
    assert!(walking.iter().all(|&lane| map.get(lane).is_some()));
    assert_eq!(road.interfaced_points().as_slice(), after.deck.as_slice());
}

#[test]
fn p7_life_same_width_replacement_preserves_a_live_directed_terminal() {
    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    let pattern = LanePatternBuilder::new().parking(false);
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 200.0, 0.0)),
        to: MapProject::ground(vec3(400.0, 200.0, 0.0)),
        inter: None,
        pat: pattern.one_way(true).n_lanes(2).build(),
    }]);
    let (road, lane, position, itinerary) = {
        let map = sim.map();
        let (id, road) = map.roads().iter().next().unwrap();
        let lane = road
            .lanes_iter()
            .filter(|&(_, kind)| kind == LaneKind::Driving)
            .map(|(id, _)| map.get(id).unwrap())
            .min_by(|a, b| a.dist_from_bottom.total_cmp(&b.dist_from_bottom))
            .unwrap();
        let it = Itinerary::route_between_lanes(Tick(0), &map, (lane.id, 30.0), (lane.id, 100.0))
            .unwrap();
        (id, lane.id, lane.points.point_along(30.0), it)
    };
    let terminal = itinerary.end_pos().unwrap();
    let target = itinerary.target();
    let id = spawn_driving_vehicle(
        &mut sim,
        VehicleKind::Car,
        Transform::new_dir(position, geom::Vec3::X),
    )
    .unwrap();
    sim.world.vehicles[id].it = itinerary;
    let mut budget = PrepareBudget::for_preview(4_194_304);
    let expected = road_edit_stamp(&sim, road, &mut budget).unwrap();
    let change = RoadUpgrade::Replace(pattern.build());
    let preview = preview_upgrade(
        &sim,
        &RoadEditRequest::Upgrade {
            road,
            expected,
            change: change.clone(),
        },
        &mut budget,
    )
    .unwrap();
    assert!(preview.retained_lanes.contains(&lane));
    sim.apply_commands([&WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        quote: preview.quote,
    }]);
    let report = sim
        .read::<CommandOutcomes>()
        .road_edit(0)
        .unwrap()
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(report.status, RoadEditStatus::Applied);
    assert!(report.retained_lanes.contains(&lane));
    assert_eq!(sim.world.vehicles[id].trans.pos, position);
    assert_eq!(sim.world.vehicles[id].it.end_pos(), Some(terminal));
    assert_eq!(sim.world.vehicles[id].it.target(), target);

    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..900 {
        restored.tick(&mut schedule, &[]);
    }
    assert!(restored.world.vehicles[id]
        .trans
        .pos
        .is_close(terminal, 2.0));
}

#[test]
fn p7_edit_junction_mask_applies_in_batch_and_survives_snapshot() {
    use crate::map::road_edit::{junction_edit_stamp, preview_junction_edit};
    use crate::map::{ApproachRole, JunctionRules, ProjectKind, TurnKind};

    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    let pat = LanePatternBuilder::new().build();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 100.0, 0.0)),
        to: MapProject::ground(vec3(400.0, 100.0, 0.0)),
        inter: None,
        pat: pat.clone(),
    }]);
    let (approach, junction, pos) = {
        let map = sim.map();
        let (id, road) = map.roads().iter().next().unwrap();
        (id, road.dst, map.get(road.dst).unwrap().pos)
    };
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject {
            kind: ProjectKind::Intersection(junction),
            pos,
        },
        to: MapProject::ground(vec3(400.0, 400.0, 0.0)),
        inter: None,
        pat: pat.clone(),
    }]);
    let turn = sim
        .map()
        .get(junction)
        .unwrap()
        .turns()
        .find(|turn| {
            turn.kind == TurnKind::Driving && sim.map().get(turn.id.src).unwrap().parent == approach
        })
        .unwrap()
        .id;
    let make_edit = |sim: &Simulation, rules: JunctionRules| {
        let mut budget = PrepareBudget::for_preview(4_194_304);
        let expected = junction_edit_stamp(sim, junction, &mut budget).unwrap();
        let preview = preview_junction_edit(sim, junction, expected, &rules, &mut budget).unwrap();
        assert_eq!(preview.cost, Money::ZERO);
        WorldCommand::MapSetJunctionRules {
            junction,
            expected,
            rules,
            quote: preview.quote,
        }
    };
    let rules = JunctionRules::new(vec![(approach, ApproachRole::Priority)], vec![turn]).unwrap();
    let command = make_edit(&sim, rules.clone());
    let command = Bincode::decode::<WorldCommand>(&Bincode::encode(&command).unwrap()).unwrap();
    let before = sim.hashes();
    sim.apply_commands([&command, &command]);
    let outcomes = sim.read::<CommandOutcomes>();
    assert_eq!(outcomes.accepted, [0]);
    assert_eq!(
        outcomes.junction_edit(0).unwrap().as_ref().unwrap().status,
        RoadEditStatus::Applied
    );
    assert_eq!(
        outcomes.junction_edit(1),
        Some(&Err(Rejection::StaleRoadEdit))
    );
    drop(outcomes);
    assert!(!sim.map().allows_turn(turn));
    assert_eq!(
        sim.map().get(junction).unwrap().raw_turn(turn).unwrap().id,
        turn
    );
    assert_eq!(sim.hashes()["map"], before["map"]);
    assert_eq!(sim.hashes()["government"], before["government"]);

    let mut loaded: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    assert_eq!(loaded.hashes(), sim.hashes());
    assert_eq!(loaded.map().junction_rules(junction), Some(&rules));
    assert!(!loaded.map().allows_turn(turn));
    let repeat = make_edit(&loaded, rules);
    let clear = make_edit(&loaded, JunctionRules::default());
    loaded.apply_commands([&repeat, &clear]);
    let outcomes = loaded.read::<CommandOutcomes>();
    assert_eq!(outcomes.accepted, [0, 1]);
    assert_eq!(
        outcomes.junction_edit(0).unwrap().as_ref().unwrap().status,
        RoadEditStatus::NoChange
    );
    assert!(loaded.map().allows_turn(turn));
}

#[test]
fn p7_edit_speed_checks_protected_deck_footprint() {
    use crate::map::{
        GatewayRole, LandDenial, LandOwnership, LandTileId, OwnedTileRect, PreparedEffects,
        ProtectedRoot, TileExtent,
    };
    let (mut sim, _) = road_fixture();
    let pattern = LanePatternBuilder::new().build();
    let mut ids = Vec::new();
    for x in [550.0, 552.0] {
        let (_, road) = sim
            .map_mut()
            .make_connection(
                MapProject::ground(vec3(x, 560.0, 0.0)),
                MapProject::ground(vec3(x, 730.0, 0.0)),
                None,
                &pattern,
            )
            .unwrap();
        ids.push(road);
    }
    let [road, reserve] = ids.try_into().unwrap();
    let mut budget = PrepareBudget::for_preview(4_194_304);
    let expected = road_edit_stamp(&sim, road, &mut budget).unwrap();
    sim.map_mut().install_land_ownership(
        LandOwnership::tiled(
            TileExtent {
                width: 4,
                height: 4,
            },
            OwnedTileRect {
                origin: LandTileId { x: 1, y: 1 },
                width: 2,
                height: 2,
            },
            [ProtectedRoot::Road(GatewayRole::Highway, reserve)]
                .into_iter()
                .collect(),
        )
        .unwrap(),
    );
    assert!(!sim.map().land_ownership().is_protected(road.into()));
    let map = sim.map();
    assert!(
        crate::map::land_deck_triangles(&map.roads()[road]).all(|triangle| {
            map.land_ownership()
                .check_polygon(&triangle.map(|point| point.xy()), None)
                .is_ok()
        })
    );
    drop(map);
    assert_eq!(
        sim.map()
            .authorize_land_effects(PreparedEffects::Road(road)),
        Err(LandDenial::ProtectedGateway),
    );
    let request = RoadEditRequest::Upgrade {
        road,
        expected,
        change: RoadUpgrade::Replace(LanePatternBuilder::new().speed_limit(13.0).build()),
    };
    assert!(matches!(
        preview_upgrade(&sim, &request, &mut budget),
        Err(Rejection::LandDenied(LandDenial::ProtectedGateway)),
    ));
}

#[test]
fn p7_traversal_snapshot_reaches_owners_beyond_the_old_prefix() {
    use crate::transportation::{Speed, Vehicle, VehicleState};
    use crate::world::VehicleEnt;
    use geom::Color;

    let (mut sim, road) = road_fixture();
    let (anchor, itinerary) = {
        let map = sim.map();
        let (lane, _) = map
            .get(road)
            .unwrap()
            .lanes_iter()
            .find(|&(_, kind)| kind == LaneKind::Driving)
            .unwrap();
        let anchor = map.get(lane).unwrap().points.point_along(30.0);
        let itinerary =
            Itinerary::route_between_lanes(Tick(0), &map, (lane, 30.0), (lane, 120.0)).unwrap();
        (anchor, itinerary)
    };
    for _ in 0..65_536 {
        sim.world.vehicles.insert(VehicleEnt {
            trans: Transform::new(anchor),
            speed: Speed::default(),
            vehicle: Vehicle {
                ang_velocity: 0.0,
                wait_time: 0.0,
                max_speed_multiplier: 1.0,
                state: VehicleState::Driving,
                kind: VehicleKind::Car,
                tint: Color::WHITE,
                flag: 0,
            },
            it: Itinerary::NONE,
            collider: None,
        });
    }
    let vehicle =
        spawn_driving_vehicle(&mut sim, VehicleKind::Car, Transform::new(anchor)).unwrap();
    sim.world.vehicles[vehicle].it = itinerary;
    let traversal = *sim.world.vehicles[vehicle].it.get_travers().unwrap();
    sim.write::<TransportGrid>().watch_traversal_users();
    for _ in 0..3 {
        let (world, resources) = sim.world_res();
        itinerary_update(world, resources);
    }
    {
        let grid = sim.read::<TransportGrid>();
        let map = sim.map();
        let snapshot = grid.traversal_users(&map, traversal).unwrap();
        assert_eq!(
            snapshot.users().collect::<Vec<_>>(),
            [TraversalOwner::Vehicle(vehicle)]
        );
    }
    sim.world.vehicles[vehicle].it = Itinerary::NONE;
    let (world, resources) = sim.world_res();
    itinerary_update(world, resources);
    let grid = sim.read::<TransportGrid>();
    assert!(grid
        .traversal_users(&sim.map(), traversal)
        .unwrap()
        .users()
        .next()
        .is_none());
}

#[test]
fn p7_replay_mixed_dry_batch_keeps_link_indices_and_refusals() {
    use crate::map::LandMode;
    use crate::{MapPreset, NewGameOptions, Replay};

    super::init_test_simulation();
    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        land_mode: LandMode::Unrestricted,
        ..NewGameOptions::default()
    })
    .unwrap();
    assert!(!sim.map().needs_topology_preparation());
    let ground = |x, y| MapProject::ground(vec3(x, y, 0.0));
    let normal = LanePatternBuilder::new().build();
    let before = sim
        .map()
        .roads()
        .keys()
        .collect::<std::collections::BTreeSet<_>>();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: ground(4450.0, 3530.0),
        to: ground(4730.0, 3530.0),
        inter: None,
        pat: normal.clone(),
    }]);
    let road = sim
        .map()
        .roads()
        .keys()
        .find(|id| !before.contains(id))
        .unwrap();
    let (edit, _) = upgrade(&sim, road, 13.0);
    let wide = crate::map::LanePattern {
        lanes_forward: vec![(LaneKind::Driving, 10.0); 17],
        lanes_backward: vec![(LaneKind::Driving, 10.0); 17],
    };
    let single = WorldCommand::MapMakeConnection {
        from: ground(4450.0, 3750.0),
        to: ground(4730.0, 3750.0),
        inter: None,
        pat: wide.clone(),
    };
    let indexed = WorldCommand::MapMakeMultipleConnections(
        vec![
            ground(4450.0, 3980.0),
            ground(4730.0, 3980.0),
            ground(4450.0, 4200.0),
            ground(4730.0, 4200.0),
        ],
        vec![
            (0, 1, None, wide.clone()),
            (2, 3, None, normal),
            (0, 1, None, wide),
        ],
    );
    sim.tick(&mut Simulation::schedule(), [&single, &indexed, &edit]);
    let assert_outcomes = |sim: &Simulation| {
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(
            outcomes.rejections_for(0),
            [("MapMakeConnection", Rejection::WaterWorkLimit)]
        );
        let links = &outcomes.connection_batch(1).unwrap().links;
        assert_eq!(links[0], Err(Rejection::WaterWorkLimit));
        assert!(
            links[1].as_ref().is_ok_and(|ids| ids.len() == 1),
            "{links:?}"
        );
        assert_eq!(links[2], links[0]);
        assert_eq!(
            outcomes.road_edit(2).unwrap().as_ref().unwrap().status,
            RoadEditStatus::Applied
        );
    };
    assert_outcomes(&sim);
    let replay = sim.read::<Replay>();
    assert_eq!(replay.batch_boundaries().len(), 2);
    let (mut restored, mut cursor) = Simulation::try_from_replay(replay.clone()).unwrap();
    assert!(cursor.advance_tick(&mut restored, &mut Simulation::schedule()));
    assert_outcomes(&restored);
    assert_eq!(sim.hashes(), restored.hashes());
}

#[test]
fn p7_reassigning_frontage_updates_replacement_work() {
    use crate::map::{Environment, Map, ProjectKind, WaterTopologyWork};

    let mut environment = Environment::new(1, 1);
    environment.terrain_apply(environment.bounds(), |_| 4.0);
    environment
        .carve_river(&[vec2(256.0, 16.0), vec2(256.0, 496.0)], 48.0, 6.0)
        .unwrap();
    let mut map = Map::empty();
    assert!(map.install_environment(environment));
    let pattern = LanePatternBuilder::default().build();
    let mut connect = |x| {
        map.make_connection(
            MapProject::ground(vec3(x, 64.0, 4.0)),
            MapProject::ground(vec3(x, 448.0, 4.0)),
            None,
            &pattern,
        )
        .unwrap()
        .1
    };
    let affected = connect(80.0);
    let unrelated = connect(432.0);
    let from = MapProject {
        pos: vec3(80.0, 256.0, 4.3),
        kind: ProjectKind::Road(affected),
    };
    let to = MapProject::ground(vec3(176.0, 256.0, 4.0));
    let available = |map: &Map| {
        let mut work = WaterTopologyWork::new();
        map.prepare_connection(from, to, None, &pattern, Some(&mut work))
            .unwrap();
        work.remaining
    };
    let before = available(&map);
    let lots = map.lots.iter().filter_map(|(id, lot)| (lot.parent == affected).then_some(id))
        .collect::<Vec<_>>();
    assert!(!lots.is_empty());
    for lot in lots {
        map.reparent_lot(lot, unrelated);
    }
    assert!(before < available(&map));
}

#[test]
fn p7_junction_fix_paused_reversal_invalidates_read_revisions() {
    use crate::map::{Traversable, TraverseDirection, TraverseKind};
    use crate::transportation::queue_diagnostics::{
        explain_queue, QueueCoverage, QueueLimits, QueueRevision,
    };

    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 200.0, 0.0)),
        to: MapProject::ground(vec3(400.0, 200.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::new()
            .one_way(true)
            .parking(false)
            .build(),
    }]);
    let (road, lane, revision) = {
        let map = sim.map();
        let road = map.roads().values().next().unwrap();
        let lane = road
            .lanes_iter()
            .find(|(_, kind)| kind.vehicles())
            .unwrap()
            .0;
        (road.id, lane, QueueRevision::current(&map))
    };
    let traverse = Traversable::new(TraverseKind::Lane(lane), TraverseDirection::Forward);
    sim.write::<TransportGrid>().watch_traversal_users();
    let (world, resources) = sim.world_res();
    itinerary_update(world, resources);
    assert!(sim
        .read::<TransportGrid>()
        .traversal_users(&sim.map(), traverse)
        .is_some());
    let mut budget = PrepareBudget::for_preview(280_000);
    let expected = road_edit_stamp(&sim, road, &mut budget).unwrap();
    let preview = preview_upgrade(
        &sim,
        &RoadEditRequest::Upgrade {
            road,
            expected,
            change: RoadUpgrade::ReverseOneWay,
        },
        &mut budget,
    )
    .unwrap();
    sim.apply_commands([&WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change: RoadUpgrade::ReverseOneWay,
        quote: preview.quote,
    }]);
    assert_eq!(
        sim.read::<CommandOutcomes>()
            .road_edit(0)
            .unwrap()
            .as_ref()
            .unwrap()
            .status,
        RoadEditStatus::Applied
    );
    assert_ne!(revision, QueueRevision::current(&sim.map()));
    assert!(sim
        .read::<TransportGrid>()
        .traversal_users(&sim.map(), traverse)
        .is_none());
    assert_eq!(
        explain_queue(&sim, lane, revision, QueueLimits::default()).coverage,
        QueueCoverage::StaleRevision
    );
}

#[test]
fn p7_junction_fix_reversal_waits_for_bus_rear_on_neighbor_lane() {
    use crate::map::ProjectKind;

    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 6042).unwrap();
    let a = vec3(100.0, 200.0, 0.0);
    let b = vec3(400.0, 200.0, 0.0);
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(a),
        to: MapProject::ground(b),
        inter: None,
        pat: LanePatternBuilder::new()
            .one_way(true)
            .parking(false)
            .build(),
    }]);
    let (road, junction) = {
        let map = sim.map();
        let (id, road) = map.roads().iter().next().unwrap();
        (id, road.dst)
    };
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject {
            pos: b,
            kind: ProjectKind::Intersection(junction),
        },
        to: MapProject::ground(vec3(510.0, 400.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::new().build(),
    }]);
    let (outgoing, pos, clear, direction, route) = {
        let map = sim.map();
        let lane = map
            .lanes()
            .values()
            .find(|lane| {
                lane.parent != road && lane.src == junction && lane.kind == LaneKind::Driving
            })
            .unwrap();
        let route =
            Itinerary::route_between_lanes(Tick(0), &map, (lane.id, 1.0), (lane.id, 50.0)).unwrap();
        (
            lane.id,
            lane.points.point_along(1.0),
            lane.points.point_along(20.0),
            lane.points.first_dir().unwrap(),
            route,
        )
    };
    assert!(sim
        .map()
        .get(junction)
        .unwrap()
        .turns()
        .any(|turn| turn.id.dst == outgoing));
    let bus = spawn_driving_vehicle(
        &mut sim,
        VehicleKind::Bus,
        Transform::new_dir(pos, direction),
    )
    .unwrap();
    sim.world.vehicles[bus].it = route;
    let preview = |sim: &Simulation| {
        let mut budget = PrepareBudget::for_preview(4_194_304);
        let expected = road_edit_stamp(sim, road, &mut budget).unwrap();
        preview_upgrade(
            sim,
            &RoadEditRequest::Upgrade {
                road,
                expected,
                change: RoadUpgrade::ReverseOneWay,
            },
            &mut budget,
        )
    };
    assert!(matches!(preview(&sim), Err(Rejection::BusyConflict)));
    sim.world.vehicles[bus].trans.pos = clear;
    let edit = preview(&sim).unwrap();
    sim.apply_commands([&WorldCommand::MapUpgradeRoad {
        road,
        expected: edit.stamp,
        change: RoadUpgrade::ReverseOneWay,
        quote: edit.quote,
    }]);
    assert_eq!(
        sim.read::<CommandOutcomes>()
            .road_edit(0)
            .unwrap()
            .as_ref()
            .unwrap()
            .status,
        RoadEditStatus::Applied
    );
}
