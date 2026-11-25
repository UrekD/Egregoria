use crate::economy::Government;
use crate::map::road_edit::{
    preview_road_edit, road_edit_stamp, RoadEditPreview, RoadEditRequest, RoadEditStatus,
    RoadUpgrade,
};
use crate::map::{LaneKind, LanePatternBuilder, MapProject, RoadID};
use crate::map_dynamic::{Itinerary, Transit};
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

fn upgrade(sim: &Simulation, road: RoadID, speed: f32) -> (WorldCommand, RoadEditPreview) {
    let mut budget = PrepareBudget::for_preview(4_194_304);
    let expected = road_edit_stamp(sim, road, &mut budget).unwrap();
    let change = RoadUpgrade::Replace(LanePatternBuilder::new().speed_limit(speed).build());
    let request = RoadEditRequest::Upgrade {
        road,
        expected,
        change: change.clone(),
    };
    let preview = preview_road_edit(sim, &request, &mut budget).unwrap();
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
    let preview = preview_road_edit(&sim, &request, &mut budget).unwrap();
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
