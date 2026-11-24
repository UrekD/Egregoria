use super::TestCtx;
use crate::economy::{Budget, BudgetCategory, Government, Market, Trade, TradeTarget};
use crate::map::{
    BuildingID, BuildingKind, LaneKind, LanePatternBuilder, MapProject, PathKind, ProjectKind,
    TerraformKind, Zone,
};
use crate::map_dynamic::{
    BuildingInfos, Destination, DispatchID, DispatchKind, DispatchQueryTarget, Dispatcher,
    Itinerary,
};
use crate::souls::desire::{Work, WorkKind};
use crate::souls::freight_station::FreightTrainState;
use crate::souls::human::HumanDecisionKind;
use crate::transportation::testing_vehicles::RandomVehicles;
use crate::transportation::{spawn_parked_vehicle, unpark, Location, VehicleKind, VehicleState};
use crate::utils::replay::ReplayJson;
use crate::world_command::{CommandOutcomes, Rejection};
use crate::{
    NewGameOptions, Replay, SaveHeader, SaveLoadError, Simulation, SimulationOptions,
    SimulationSer, SoulID, WorldCommand, START_COMMANDS, VERSION,
};
use common::saveload::{Bincode, CompressedBincode, Encoder, JSON};
use geom::{vec2, vec3, Polygon, OBB};
use prototypes::{BuildingGen, GameTime, GoodsCompanyID, ItemID, Money, RollingStockID, Tick};

#[path = "p4a_new_game.rs"]
mod p4a_new_game;
#[path = "p4a_slots.rs"]
mod p4a_slots;

fn street(test: &TestCtx) {
    test.build_roads(&[
        vec3(100.0, 100.0, 0.0),
        vec3(300.0, 100.0, 0.0),
        vec3(300.0, 300.0, 0.0),
        vec3(100.0, 300.0, 0.0),
    ]);
}

fn resident(test: &TestCtx, house: BuildingID) -> crate::HumanID {
    let Some(SoulID::Human(id)) = test.g.read::<BuildingInfos>().owner(house) else {
        panic!("house has no resident");
    };
    id
}

fn run_systems(test: &mut TestCtx) {
    for _ in 0..50 {
        test.tick();
    }
}

#[test]
fn p4_init_is_rejected_in_ordinary_batches_even_at_the_initial_tick() {
    let mut test = TestCtx::new();
    test.g = Simulation::new_with_options(SimulationOptions {
        terrain_size: 0,
        save_replay: true,
        ..Default::default()
    });
    assert!(test.g.read::<Replay>().enabled);
    let init = WorldCommand::Init(Box::new(SimulationOptions::LEGACY));
    assert_eq!(test.g.get_tick(), crate::INITIAL_TICK);
    for tick in [crate::INITIAL_TICK, crate::INITIAL_TICK + 1] {
        assert_eq!(test.g.get_tick(), tick);
        let before = test.g.hashes();
        let replay = JSON::encode(&*test.g.read::<crate::utils::replay::Replay>()).unwrap();
        init.apply(&mut test.g);
        assert_eq!(test.g.read::<CommandOutcomes>().last[0].0, "Init");
        assert!(test.g.read::<CommandOutcomes>().accepted.is_empty());
        assert_eq!(test.g.hashes(), before);
        assert_eq!(
            JSON::encode(&*test.g.read::<crate::utils::replay::Replay>()).unwrap(),
            replay
        );
        test.tick();
    }
}

fn recorded_city() -> Simulation {
    drop(TestCtx::new());
    Simulation::new_with_options(SimulationOptions {
        terrain_size: NewGameOptions::MIN_TERRAIN_CHUNKS,
        ..SimulationOptions::LEGACY
    })
}

fn load_recording(sim: &Simulation) -> (Simulation, crate::SimulationReplayLoader) {
    let bytes = ReplayJson::encode(&*sim.read::<Replay>()).unwrap();
    Simulation::try_from_replay(ReplayJson::decode(&bytes).unwrap()).unwrap()
}

fn assert_replay_matches(sim: &Simulation) {
    let (mut replayed, mut loader) = load_recording(sim);
    let mut schedule = Simulation::schedule();
    for _ in 0..10 {
        if loader.advance_tick(&mut replayed, &mut schedule) {
            assert_eq!(replayed.get_tick(), sim.get_tick());
            assert_eq!(replayed.hashes(), sim.hashes());
            return;
        }
    }
    panic!("replay did not finish");
}

#[test]
fn p4_replay_bootstraps_without_stepping_systems() {
    let sim = recorded_city();
    let (mut replayed, mut loader) = load_recording(&sim);
    assert_eq!(replayed.get_tick(), crate::INITIAL_TICK);
    assert_eq!(replayed.hashes(), sim.hashes());
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(replayed.get_tick(), crate::INITIAL_TICK);
    assert_eq!(replayed.hashes(), sim.hashes());
}

#[test]
fn p4_replay_preserves_paused_edits_and_clock_changes() {
    let mut sim = recorded_city();
    let mut schedule = Simulation::schedule();
    let edit = WorldCommand::SetTaxRate {
        zone: crate::map::ZoneKind::Residential,
        rate_permille: 125,
    };
    sim.apply_commands([&edit]);
    assert_replay_matches(&sim);

    sim.apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(90_000)))]);
    assert_replay_matches(&sim);

    sim.tick(&mut schedule, &[]);
    sim.tick(
        &mut schedule,
        [&WorldCommand::SetGameTime(GameTime::new(Tick(50))), &edit],
    );
    sim.apply_commands([&WorldCommand::SetTaxRate {
        zone: crate::map::ZoneKind::Residential,
        rate_permille: 150,
    }]);
    assert_replay_matches(&sim);
}

#[test]
fn p4_replay_recognizes_the_constructor_batch_in_legacy_recordings() {
    let sim = recorded_city();
    let options = *sim.read::<SimulationOptions>();
    let mut replay = Replay::default();
    replay.enabled = true;
    let initial_tick = Tick(crate::INITIAL_TICK);
    replay.push(initial_tick, WorldCommand::Init(Box::new(options)));
    let starters: Vec<(u32, WorldCommand)> = JSON::decode(START_COMMANDS.as_bytes()).unwrap();
    for (_, command) in starters {
        replay.push(initial_tick, command);
    }
    *sim.write::<Replay>() = replay;
    assert_replay_matches(&sim);
}

#[test]
fn p4_replay_rejects_small_bootstrap_but_legacy_snapshot_loads() {
    drop(TestCtx::new());
    let terrain_size = NewGameOptions::MIN_TERRAIN_CHUNKS - 1;
    assert!(!NewGameOptions {
        terrain_chunks: terrain_size,
        ..NewGameOptions::default()
    }
    .is_valid());
    let sim = Simulation::new_with_options(SimulationOptions {
        terrain_size,
        save_replay: false,
        ..SimulationOptions::LEGACY
    });
    let name = save_fixture("p4-small-snapshot");
    CompressedBincode::save(&sim, &name).unwrap();
    let loaded = Simulation::load_from_disk(&name);
    std::fs::remove_file(CompressedBincode::filename(&name)).unwrap();
    assert_eq!(loaded.unwrap().hashes(), sim.hashes());

    let mut replay = Replay::default();
    replay.enabled = true;
    replay.push(
        Tick(crate::INITIAL_TICK),
        WorldCommand::Init(Box::new(SimulationOptions {
            terrain_size,
            ..SimulationOptions::LEGACY
        })),
    );
    let err = Simulation::try_from_replay(replay).err().unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn p1bce_identical_rejections_have_distinct_batch_sequences() {
    let mut test = TestCtx::new();
    let invalid = WorldCommand::MapBuildHouse(crate::map::LotID::default());
    test.g.apply_commands([&invalid]);
    let outcomes = test.g.read::<CommandOutcomes>();
    assert!(!outcomes.last.is_empty());
    let first = outcomes.sequence;
    let rejections = outcomes.last.clone();
    drop(outcomes);
    test.g.apply_commands([]);
    assert_eq!(test.g.read::<CommandOutcomes>().sequence, first);
    test.g.apply_commands([&invalid]);
    let outcomes = test.g.read::<CommandOutcomes>();
    assert!(outcomes.sequence > first);
    assert_eq!(outcomes.last, rejections);
}

fn assert_no_market_refs(sim: &Simulation, soul: SoulID) {
    for (_, market) in sim.read::<Market>().iter() {
        assert!(market.buy_order(soul).is_none());
        assert!(market.sell_order(soul).is_none());
        assert!(market.capital(soul).is_none());
    }
}

#[test]
fn removing_a_house_drains_the_resident_and_driving_car() {
    let mut test = TestCtx::new();
    street(&test);
    let house = test.build_house_near(vec2(150.0, 125.0));
    let neighbor = test.build_house_near(vec2(250.0, 125.0));
    test.set_residential_demand(40.0);
    run_systems(&mut test);

    let human = resident(&test, house);
    let passenger = resident(&test, neighbor);
    let car = test.g.get(human).unwrap().router.personal_car.unwrap();
    unpark(&mut test.g, car);
    let car_pos = test.g.get(car).unwrap().trans.pos;
    test.g
        .write::<BuildingInfos>()
        .get_out(house, SoulID::Human(human));
    test.g
        .write::<BuildingInfos>()
        .get_out(neighbor, SoulID::Human(passenger));
    for id in [human, passenger] {
        let h = test.g.world_mut_unchecked().humans.get_mut(id).unwrap();
        h.location = Location::Vehicle(car);
        h.router.use_vehicle(Some(car));
        h.router.target_dest = Some(Destination::Building(house));
    }
    test.g.write::<RandomVehicles>().vehicles.insert(car);

    test.apply(&[WorldCommand::MapRemoveBuilding(house)]);

    assert!(test.g.get(human).is_none(), "resident survives the command");
    assert!(
        test.g.get(car).is_none(),
        "personal car survives the command"
    );
    assert_eq!(test.g.get(passenger).unwrap().location, Location::Outside);
    assert_eq!(test.g.get(passenger).unwrap().trans.pos, car_pos);
    assert!(test.g.get(passenger).unwrap().router.target_dest.is_none());
    assert!(!test.g.read::<RandomVehicles>().vehicles.contains(&car));
    let infos = test.g.read::<BuildingInfos>();
    assert!(infos.get(house).is_none());
    assert!(infos.building_owned_by(SoulID::Human(human)).is_none());
    drop(infos);
    assert_no_market_refs(&test.g, SoulID::Human(human));

    run_systems(&mut test);
    assert!(test.g.get(passenger).is_some());
}

fn factory(test: &mut TestCtx) -> BuildingID {
    let proto = GoodsCompanyID::new("flour-factory").prototype();
    test.apply(&[WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(vec2(209.0, 200.0), vec2(1.0, 0.0), 20.0, 20.0),
        kind: BuildingKind::GoodsCompany(proto.id),
        gen: proto.bgen,
        zone: None,
        connected_road: None,
    }]);
    test.g
        .map()
        .buildings()
        .iter()
        .find_map(|(id, b)| (b.kind == BuildingKind::GoodsCompany(proto.id)).then_some(id))
        .unwrap()
}

#[test]
fn removing_a_company_frees_trucks_and_workers_and_discards_pending_trades() {
    let mut test = TestCtx::new();
    street(&test);
    let house = test.build_house_near(vec2(150.0, 125.0));
    let other_house = test.build_house_near(vec2(250.0, 125.0));
    let building = factory(&mut test);
    test.set_residential_demand(40.0);
    run_systems(&mut test);

    let Some(SoulID::GoodsCompany(company)) = test.g.read::<BuildingInfos>().owner(building) else {
        panic!("factory has no company");
    };
    let worker = resident(&test, house);
    let customer = resident(&test, other_house);
    let trucks = test.g.get(company).unwrap().comp.trucks.clone();
    let truck = trucks[0];
    let door = test.g.map().get(building).unwrap().door_pos;
    let kind = ItemID::new("bread");
    let trade = Trade {
        buyer: TradeTarget(SoulID::Human(customer)),
        seller: TradeTarget(SoulID::GoodsCompany(company)),
        qty: 1,
        kind,
        money_delta: Money::ZERO,
    };

    test.g
        .write::<BuildingInfos>()
        .get_out(house, SoulID::Human(worker));
    test.g
        .write::<BuildingInfos>()
        .get_in(building, SoulID::Human(worker));
    let world = test.g.world_mut_unchecked();
    let human = world.humans.get_mut(worker).unwrap();
    human.work = Some(Work::new(
        building,
        WorkKind::Driver {
            truck,
            deliver_order: Some(other_house),
            load: None,
        },
        0.0,
    ));
    human.location = Location::Building(building);
    human.router.use_vehicle(Some(truck));
    human.decision.kind = HumanDecisionKind::DeliverAtBuilding(building);
    let human = world.humans.get_mut(customer).unwrap();
    human.bought.0.entry(kind).or_default().push(trade);
    human.decision.kind = HumanDecisionKind::MultiStack(vec![
        HumanDecisionKind::DeliverAtBuilding(building),
        HumanDecisionKind::GoTo(Destination::Building(building)),
    ]);

    test.apply(&[WorldCommand::MapRemoveBuilding(building)]);

    assert!(
        test.g.get(company).is_none(),
        "company survives the command"
    );
    assert!(trucks.into_iter().all(|id| test.g.get(id).is_none()));
    let human = test.g.get(worker).unwrap();
    assert!(human.work.is_none());
    assert_eq!(human.location, Location::Outside);
    assert_eq!(human.trans.pos, door);
    assert!(human.collider.is_some());
    assert!(matches!(human.decision.kind, HumanDecisionKind::Yield));
    let human = test.g.get(customer).unwrap();
    assert!(human.bought.0.get(&kind).is_none_or(Vec::is_empty));
    assert!(matches!(human.decision.kind, HumanDecisionKind::Yield));
    assert_no_market_refs(&test.g, SoulID::GoodsCompany(company));
    assert!(test.g.read::<BuildingInfos>().get(building).is_none());

    run_systems(&mut test);
    assert!(test.g.get(worker).unwrap().work.is_none());
}

#[test]
fn insufficient_money_leaves_a_valid_lot_unbuilt() {
    let mut test = TestCtx::new();
    street(&test);
    let lot = test.g.map().lots().keys().next().unwrap();
    let buildings = test.g.map().buildings().len();
    let money = Money::new_bucks(99);
    test.g.write::<Government>().money = money;

    test.apply(&[WorldCommand::MapBuildHouse(lot)]);

    assert!(
        test.g.map().get(lot).is_some(),
        "unaffordable house consumed the lot"
    );
    assert_eq!(test.g.map().buildings().len(), buildings);
    assert_eq!(test.g.read::<Government>().money, money);
    assert_eq!(
        test.g.read::<CommandOutcomes>().last,
        [("MapBuildHouse", Rejection::InsufficientFunds)]
    );
}

#[test]
fn failed_house_build_leaves_money_unchanged() {
    let mut test = TestCtx::new();
    street(&test);
    let lot = test.g.map().lots().keys().next().unwrap();
    let command = WorldCommand::MapBuildHouse(lot);
    test.apply(std::slice::from_ref(&command));
    assert!(test.g.map().get(lot).is_none());
    let before = test.g.hashes();

    test.apply(&[command]);

    assert_eq!(test.g.hashes(), before);
}

#[test]
fn failed_connection_between_points_on_one_road_leaves_the_map_intact() {
    let mut test = TestCtx::new();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let map = test.g.map();
    let road = map
        .roads()
        .values()
        .find(|r| r.points.first().x == 100.0)
        .unwrap();
    let from = MapProject {
        pos: road.points.point_dir_along(70.0).0,
        kind: ProjectKind::Road(road.id),
    };
    let to = MapProject {
        pos: road.points.point_dir_along(130.0).0,
        kind: from.kind,
    };
    assert!(map.lots().values().any(|l| l.parent == road.id));
    drop(map);
    let before = test.g.hashes();

    test.apply(&[WorldCommand::MapMakeConnection {
        from,
        to,
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }]);

    assert_eq!(test.g.hashes(), before);
    test.g.map().check_invariants();
}

#[test]
fn batch_charges_successful_links_only_and_is_deterministic_for_equal_costs() {
    let mut test = TestCtx::new();
    let options = *test.g.read::<crate::SimulationOptions>();
    let projects = vec![
        MapProject::ground(vec3(100.0, 100.0, 0.0)),
        MapProject::ground(vec3(300.0, 100.0, 0.0)),
        MapProject::ground(vec3(100.0, 300.0, 0.0)),
        MapProject::ground(vec3(100.5, 100.0, 0.0)),
    ];
    let pat = LanePatternBuilder::default().build();
    let one_link = WorldCommand::MapMakeConnection {
        from: projects[0],
        to: projects[1],
        inter: None,
        pat: pat.clone(),
    };
    let cmd = WorldCommand::MapMakeMultipleConnections(
        projects,
        vec![
            (0, 1, None, pat.clone()),
            (0, 3, None, pat.clone()),
            (0, 2, None, pat),
        ],
    );
    let cost = Government::action_cost(&one_link, &test.g);
    let mut hashes = None;
    for _ in 0..2 {
        let money = test.g.read::<Government>().money;
        let roads = test.g.map().roads().len();
        test.apply(std::slice::from_ref(&cmd));
        assert_eq!(test.g.map().roads().len(), roads + 2);
        assert_eq!(test.g.read::<Government>().money, money - cost - cost);
        run_systems(&mut test);
        let current = test.g.hashes();
        if let Some(first) = hashes.replace(current.clone()) {
            assert_eq!(current, first);
        }
        test.g = Simulation::new_with_options(options);
    }
}

fn stale_connection_uses_the_canonical_endpoints(batch: bool) {
    let mut test = TestCtx::new();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let from = {
        let map = test.g.map();
        let intersection = map
            .intersections()
            .values()
            .find(|i| i.pos.x == 100.0)
            .unwrap();
        MapProject {
            pos: intersection.pos,
            kind: ProjectKind::Intersection(intersection.id),
        }
    };
    let to = MapProject::ground(vec3(100.0, 300.0, 0.0));
    let pat = LanePatternBuilder::default().build();
    let elbow = Some(vec2(175.0, 200.0));
    let command = |from| {
        if batch {
            WorldCommand::MapMakeMultipleConnections(
                vec![from, to],
                vec![(0, 1, elbow, pat.clone())],
            )
        } else {
            WorldCommand::MapMakeConnection {
                from,
                to,
                inter: elbow,
                pat: pat.clone(),
            }
        }
    };
    let baseline = common::saveload::Bincode::encode(&test.g).unwrap();
    let opening = test.g.read::<Government>().money;
    let construction = test.g.read::<Budget>().total(BudgetCategory::Construction);
    let cost = Government::action_cost(&command(from), &test.g);
    test.apply(&[command(from)]);
    assert_eq!(test.g.read::<Government>().money, opening - cost);
    assert_eq!(
        test.g.read::<Budget>().total(BudgetCategory::Construction),
        construction - i128::from(cost.inner())
    );
    let expected = test.g.hashes();
    test.g = common::saveload::Bincode::decode(&baseline).unwrap();
    let stale = MapProject {
        pos: vec3(900.0, 800.0, 50.0),
        ..from
    };
    assert_eq!(Government::action_cost(&command(stale), &test.g), cost);
    test.apply(&[command(stale)]);
    assert_eq!(test.g.hashes(), expected);
}

#[test]
fn stale_single_connection_uses_the_canonical_endpoints() {
    stale_connection_uses_the_canonical_endpoints(false);
}

#[test]
fn stale_batch_connection_uses_the_canonical_endpoints() {
    stale_connection_uses_the_canonical_endpoints(true);
}

#[test]
fn split_batch_keeps_links_on_the_original_road_at_a_grade_separated_crossing() {
    let mut test = TestCtx::new();
    let pat = LanePatternBuilder::default().build();
    let ground = |x, y, z| MapProject::ground(vec3(x, y, z));
    let (road, elevated) = {
        let mut map = test.g.map_mut();
        let (_, road) = map
            .make_connection(
                ground(80.0, 100.0, 0.0),
                ground(350.0, 100.0, 0.0),
                None,
                &pat,
            )
            .unwrap();
        let (elevated, _) = map
            .make_connection(
                ground(240.0, 40.0, 40.0),
                ground(240.0, 100.0, 40.0),
                None,
                &pat,
            )
            .unwrap();
        (road, elevated)
    };
    let on_road = |x| MapProject {
        pos: vec3(x, 100.0, crate::map::ROAD_Z_OFFSET),
        kind: ProjectKind::Road(road),
    };
    test.apply(&[WorldCommand::MapMakeMultipleConnections(
        vec![
            on_road(150.0),
            ground(150.0, 200.0, 0.0),
            on_road(240.0),
            ground(240.0, 200.0, 0.0),
        ],
        vec![(0, 1, None, pat.clone()), (2, 3, None, pat)],
    )]);

    assert!(test.g.read::<CommandOutcomes>().last.is_empty());
    let map = test.g.map();
    assert_eq!(map.get(elevated).unwrap().roads.len(), 1);
    assert!(map
        .intersections()
        .values()
        .any(|i| { i.pos.distance(vec3(240.0, 100.0, 0.0)) < 0.01 && i.roads.len() == 3 }));
}

#[test]
fn zone_fill_direction_is_normalized_when_stored() {
    let mut test = TestCtx::new();
    let proto = GoodsCompanyID::new("cereal-farm").prototype();
    let build = |filldir| WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(vec2(204.0, 204.0), vec2(1.0, 0.0), 4.0, 4.0),
        kind: BuildingKind::GoodsCompany(proto.id),
        gen: proto.bgen,
        zone: Some(Zone {
            poly: Polygon(vec![
                vec2(200.0, 200.0),
                vec2(212.0, 200.0),
                vec2(212.0, 212.0),
                vec2(200.0, 212.0),
            ]),
            area: 1.0,
            filldir,
        }),
        connected_road: None,
    };
    for direction in [geom::Vec2::ZERO, vec2(f32::INFINITY, 1.0)] {
        test.apply(&[build(direction)]);
        assert_eq!(
            test.g.read::<CommandOutcomes>().last,
            [("MapBuildSpecialBuilding", Rejection::InvalidZone)]
        );
    }
    test.apply(&[build(vec2(3.0e30, 4.0e30))]);
    let map = test.g.map();
    let zone = map
        .buildings()
        .values()
        .find_map(|b| b.zone.as_ref())
        .unwrap();
    assert!(zone.filldir.distance(vec2(0.6, 0.8)) < 0.001);
}

fn staffed_factory(
    test: &mut TestCtx,
) -> (
    crate::CompanyID,
    crate::HumanID,
    crate::HumanID,
    crate::VehicleID,
    BuildingID,
) {
    street(test);
    let house = test.build_house_near(vec2(150.0, 125.0));
    let customer = test.build_house_near(vec2(250.0, 125.0));
    let factory = factory(test);
    test.set_residential_demand(40.0);
    run_systems(test);
    let Some(SoulID::GoodsCompany(company)) = test.g.read::<BuildingInfos>().owner(factory) else {
        panic!("factory has no company");
    };
    let driver = resident(test, house);
    let worker = resident(test, customer);
    let truck = test.g.get(company).unwrap().comp.trucks[0];
    let world = test.g.world_mut_unchecked();
    let entity = world.companies.get_mut(company).unwrap();
    entity.comp.driver = Some(driver);
    entity.workers.0 = vec![worker, driver];
    world.humans.get_mut(driver).unwrap().work = Some(Work::new(
        factory,
        WorkKind::Driver {
            truck,
            deliver_order: None,
            load: None,
        },
        0.0,
    ));
    world.humans.get_mut(worker).unwrap().work = Some(Work::new(factory, WorkKind::Worker, 0.0));
    (company, driver, worker, truck, customer)
}

#[test]
fn deleted_active_truck_is_replaced_for_pending_deliveries() {
    let mut test = TestCtx::new();
    let (company, driver, worker, truck, customer) = staffed_factory(&mut test);
    let door = test.g.get(company).unwrap().trans.pos;
    let replacement = spawn_parked_vehicle(&mut test.g, VehicleKind::Truck, door).unwrap();
    let company_has_two_trucks = test
        .g
        .world_mut_unchecked()
        .companies
        .get_mut(company)
        .unwrap();
    company_has_two_trucks.comp.trucks = vec![truck, replacement];

    let station = {
        let map = test.g.map();
        map.buildings()
            .values()
            .find(|b| matches!(b.kind, BuildingKind::RailFreightStation(_)))
            .unwrap()
            .id
    };
    let Some(SoulID::FreightStation(station_soul)) = test.g.read::<BuildingInfos>().owner(station)
    else {
        panic!("expected a freight station");
    };
    let buyer = resident(&test, customer);
    let entity = test
        .g
        .world_mut_unchecked()
        .companies
        .get_mut(company)
        .unwrap();
    for buyer in [SoulID::Human(buyer), SoulID::FreightStation(station_soul)] {
        entity.sold.0.push(Trade {
            buyer: TradeTarget(buyer),
            seller: TradeTarget(SoulID::GoodsCompany(company)),
            qty: 1,
            kind: ItemID::new("bread"),
            money_delta: Money::ZERO,
        });
    }
    test.g
        .read::<crate::ParCommandBuffer<crate::VehicleEnt>>()
        .kill(truck);
    for _ in 0..5 {
        test.tick_fast();
    }
    assert_eq!(
        test.g.get(company).unwrap().comp.driver,
        Some(driver.min(worker))
    );
    let driver = driver.min(worker);
    let human = test.g.world_mut_unchecked().humans.get_mut(driver).unwrap();
    assert!(matches!(human.work.as_ref().unwrap().kind,
        WorkKind::Driver { truck, deliver_order: Some(target), .. } if truck == replacement && target == station));
    // Exercise delivery completion after the replacement driver reaches the target.
    human.decision.kind = HumanDecisionKind::DeliverAtBuilding(station);
    human.decision.wait = 0;
    test.tick_fast();
    assert_eq!(test.g.get(station_soul).unwrap().f.waiting_cargo, 1);
    let work = test.g.get(driver).unwrap().work.as_ref().unwrap();
    assert!(matches!(work.kind,
        WorkKind::Driver { truck, deliver_order: Some(target), .. } if truck == replacement && target == customer));
}

#[test]
fn removing_a_driver_mid_drive_clears_the_truck_from_the_lane() {
    let mut test = TestCtx::new();
    let (company, driver, worker, truck, _) = staffed_factory(&mut test);
    let (start, direction, destination) = {
        let map = test.g.map();
        let VehicleState::Parked(spot) = &test.g.get(truck).unwrap().vehicle.state else {
            panic!("expected a parked truck");
        };
        let depot = spot.park_pos(&map).unwrap();
        let lane = map
            .get(map.nearest_lane(depot, LaneKind::Driving, None).unwrap())
            .unwrap();
        let (start, direction) = lane.points.point_dir_along(10.0);
        (
            start,
            direction,
            lane.points.point_along(lane.points.length() * 0.75),
        )
    };
    let vehicle = test
        .g
        .world_mut_unchecked()
        .vehicles
        .get_mut(truck)
        .unwrap();
    vehicle.trans.pos = start;
    vehicle.trans.dir = direction;
    vehicle.it = Itinerary::simple(vec![destination]);
    unpark(&mut test.g, truck);
    for _ in 0..50 {
        test.tick_fast();
    }
    let vehicle = test.g.get(truck).unwrap();
    assert!(vehicle.speed.0 > 0.0 && vehicle.collider.is_some());

    crate::souls::lifecycle::despawn_human(&mut test.g, driver);
    for _ in 0..7000 {
        test.tick_fast();
        if test
            .g
            .get(truck)
            .is_none_or(|vehicle| vehicle.vehicle.state.is_parked())
        {
            break;
        }
    }

    let company = test.g.get(company).unwrap();
    assert_eq!(company.comp.driver, Some(worker));
    assert!(!company.comp.trucks.is_empty());
    for &id in &company.comp.trucks {
        let vehicle = test.g.get(id).unwrap();
        assert!(
            matches!(vehicle.vehicle.state, VehicleState::Parked(_)),
            "truck {id:?} at {:?}, speed {}, state {:?}, destination {:?}, next {:?}",
            vehicle.trans.pos,
            vehicle.speed.0,
            vehicle.vehicle.state,
            vehicle.it.end_pos(),
            vehicle.it.get_point()
        );
        assert!(vehicle.collider.is_none());
    }
}

#[test]
fn a_truck_without_a_return_route_is_replaced_at_its_depot() {
    let mut test = TestCtx::new();
    let (company, driver, worker, truck, _) = staffed_factory(&mut test);
    unpark(&mut test.g, truck);
    let roads: Vec<_> = test
        .g
        .map()
        .roads()
        .values()
        .filter(|r| r.points.first().x < 400.0)
        .map(|r| WorldCommand::MapRemoveRoad(r.id))
        .collect();
    test.apply(&roads);

    crate::souls::lifecycle::despawn_human(&mut test.g, driver);
    for _ in 0..5 {
        test.tick_fast();
    }

    assert!(test.g.get(truck).is_none());
    let company = test.g.get(company).unwrap();
    let [replacement] = company.comp.trucks.as_slice() else {
        panic!("expected one replacement truck");
    };
    let vehicle = test.g.get(*replacement).unwrap();
    assert!(matches!(vehicle.vehicle.state, VehicleState::ParkedAtDepot));
    assert!(vehicle.collider.is_none());
    assert_eq!(
        vehicle.trans.pos,
        test.g.map().get(company.comp.building).unwrap().door_pos
    );
    assert_eq!(company.comp.driver, Some(worker));
}

#[test]
fn validating_commands_records_only_accepted_indices() {
    let mut test = TestCtx::new();
    let before = test.g.hashes();
    let terraform = |radius, amount| WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: vec2(100.0, 100.0),
        radius,
        amount,
        level: 0.0,
        slope: None,
    };
    let company = GoodsCompanyID::new("cereal-farm").prototype();
    let building = |kind, gen| WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(vec2(50.0, 50.0), geom::Vec2::X, 10.0, 10.0),
        kind,
        gen,
        zone: None,
        connected_road: None,
    };

    test.g.apply_commands(&[
        terraform(f32::NAN, 100.0),
        terraform(100.0, f32::INFINITY),
        terraform(1.0e8, 100.0),
        terraform(100.0, 1.0e8),
        WorldCommand::SpawnRandomCars { n_cars: 0 },
        WorldCommand::SpawnRandomCars { n_cars: 1_001 },
        building(BuildingKind::GoodsCompany(company.id), company.bgen),
        building(BuildingKind::House, BuildingGen::House),
        building(
            BuildingKind::ExternalTrading,
            BuildingGen::NoWalkway {
                door_pos: geom::Vec2::ZERO,
            },
        ),
        building(BuildingKind::TrainStation, BuildingGen::House),
    ]);

    let outcomes = test.g.read::<CommandOutcomes>();
    assert_eq!(outcomes.accepted, [4]);
    assert_eq!(
        outcomes
            .last
            .iter()
            .map(|&(_, reason)| reason)
            .collect::<Vec<_>>(),
        [
            Rejection::InvalidTerraform,
            Rejection::InvalidTerraform,
            Rejection::InvalidTerraform,
            Rejection::InvalidTerraform,
            Rejection::TooManyRandomCars,
            Rejection::InvalidZone,
            Rejection::InvalidBuilding,
            Rejection::InvalidBuilding,
            Rejection::InvalidBuilding,
        ]
    );
    assert_eq!(test.g.hashes(), before);
}

#[test]
fn out_of_range_batch_index_rejects_the_batch() {
    let mut test = TestCtx::new();
    let before = test.g.hashes();
    let points = vec![
        MapProject::ground(vec3(100.0, 100.0, 0.0)),
        MapProject::ground(vec3(300.0, 100.0, 0.0)),
    ];
    let pat = LanePatternBuilder::default().build();
    test.apply(&[WorldCommand::MapMakeMultipleConnections(
        points,
        vec![(0, 1, None, pat.clone()), (0, 2, None, pat)],
    )]);
    assert_eq!(before, test.g.hashes());
}

#[test]
fn zone_area_and_perimeter_are_checked_against_the_polygon() {
    let mut test = TestCtx::new();
    let proto = GoodsCompanyID::new("cereal-farm").prototype();
    let before = test.g.hashes();
    for (width, height) in [(500.0, 500.0), (1600.0, 2.0)] {
        let poly = Polygon(vec![
            vec2(10.0, 10.0),
            vec2(10.0 + width, 10.0),
            vec2(10.0 + width, 10.0 + height),
            vec2(10.0, 10.0 + height),
        ]);
        test.apply(&[WorldCommand::MapBuildSpecialBuilding {
            pos: OBB::new(vec2(11.0, 11.0), vec2(1.0, 0.0), 1.0, 1.0),
            kind: BuildingKind::GoodsCompany(proto.id),
            gen: proto.bgen,
            zone: Some(Zone {
                poly,
                area: 1.0,
                filldir: vec2(1.0, 0.0),
            }),
            connected_road: None,
        }]);
        assert_eq!(before, test.g.hashes());
        assert_eq!(
            test.g.read::<CommandOutcomes>().last,
            [("MapBuildSpecialBuilding", Rejection::InvalidZone)]
        );
    }
}

#[test]
fn overflow_in_connection_cost_rejects_before_building() {
    let mut test = TestCtx::new();
    let before = test.g.hashes();
    test.apply(&[WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(0.0, 100.0, 0.0)),
        to: MapProject::ground(vec3(1.0e18, 100.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }]);
    assert_eq!(test.g.hashes(), before);
    assert_eq!(
        test.g.read::<CommandOutcomes>().last,
        [("MapMakeConnection", Rejection::CostOverflow)]
    );
}

#[test]
fn removing_a_road_orphans_the_house_without_despawning_the_resident() {
    let mut test = TestCtx::new();
    street(&test);
    let house = test.build_house_near(vec2(150.0, 125.0));
    test.set_residential_demand(40.0);
    run_systems(&mut test);
    let human = resident(&test, house);
    let road = test.g.map().get(house).unwrap().connected_road.unwrap();
    test.apply(&[WorldCommand::MapRemoveRoad(road)]);
    assert!(test.g.map().get(house).unwrap().is_orphaned());
    assert_eq!(resident(&test, house), human);
    run_systems(&mut test);
    assert!(test.g.get(human).is_some());
}

#[test]
fn spawn_train_charges_for_the_consist_and_rejects_a_driving_lane() {
    let mut test = TestCtx::new();
    street(&test);
    let map = test.g.map();
    let (rail, lane) = map
        .lanes()
        .iter()
        .find(|(_, lane)| lane.kind == LaneKind::Rail)
        .unwrap();
    let dist = lane.points.length() / 2.0;
    let driving = map
        .lanes()
        .iter()
        .find(|(_, lane)| lane.kind == LaneKind::Driving)
        .unwrap()
        .0;
    drop(map);
    let money = test.g.read::<Government>().money;
    let wagons = vec![
        RollingStockID::new("locomotive"),
        RollingStockID::new("freight-wagon"),
    ];

    test.apply(&[WorldCommand::SpawnTrain {
        wagons: wagons.clone(),
        lane: rail,
        dist,
    }]);
    assert_eq!(test.g.world().trains.len(), 1);
    assert_eq!(
        test.g.read::<Government>().money,
        money - Money::new_bucks(1_200)
    );

    test.apply(&[WorldCommand::SpawnTrain {
        wagons,
        lane: driving,
        dist: 50.0,
    }]);
    assert_eq!(test.g.world().trains.len(), 1);
    assert_eq!(
        test.g.read::<Government>().money,
        money - Money::new_bucks(1_200)
    );
}

#[test]
fn removing_freight_buildings_releases_assigned_trains() {
    let mut test = TestCtx::new();
    test.tick();
    let (external, depot) = {
        let map = test.g.map();
        let external = *map.external_train_stations.first().unwrap();
        let depot = map
            .buildings()
            .iter()
            .find_map(|(id, building)| {
                matches!(building.kind, BuildingKind::RailFreightStation(_)).then_some(id)
            })
            .unwrap();
        (external, depot)
    };
    let Some(SoulID::FreightStation(station)) = test.g.read::<BuildingInfos>().owner(depot) else {
        panic!("freight depot has no station");
    };
    let (lane, dist) = {
        let map = test.g.map();
        let (id, lane) = map
            .lanes()
            .iter()
            .find(|(_, lane)| lane.kind == LaneKind::Rail)
            .unwrap();
        (id, lane.points.length() * 0.5)
    };
    test.apply(&[WorldCommand::SpawnTrain {
        wagons: vec![RollingStockID::new("locomotive")],
        lane,
        dist,
    }]);
    let train = test.g.world().trains.keys().next().unwrap();
    let reserve = |sim: &Simulation| {
        let map = sim.map();
        let mut dispatcher = sim.write::<Dispatcher>();
        dispatcher.update(&map, sim.world());
        assert_eq!(
            dispatcher.query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Pos(sim.get(train).unwrap().trans.pos)
            ),
            Some(DispatchID::FreightTrain(train)),
        );
    };

    for (building, state) in [
        (external, FreightTrainState::Moving),
        (depot, FreightTrainState::Loading),
    ] {
        reserve(&test.g);
        let world = test.g.world_mut_unchecked();
        world
            .freight_stations
            .get_mut(station)
            .unwrap()
            .f
            .trains
            .push((train, state));
        world.trains.get_mut(train).unwrap().it = Itinerary::wait_until(f64::MAX);
        test.apply(&[WorldCommand::MapRemoveBuilding(building)]);

        assert!(test.g.read::<BuildingInfos>().get(building).is_none());
        assert!(test.g.get(train).unwrap().it.has_ended(0.0));
        if building == external {
            assert!(test.g.map().external_train_stations.is_empty());
            assert!(test.g.get(station).unwrap().f.trains.is_empty());
        } else {
            assert!(test.g.get(station).is_none());
        }
    }
    reserve(&test.g);
    test.g
        .read::<crate::ParCommandBuffer<crate::TrainEnt>>()
        .kill(train);
    test.tick();
    assert!(test.g.get(train).is_none());
    assert!(test.g.world().wagons.is_empty());
}

#[test]
fn startup_money_matches_the_price_of_start_commands() {
    let test = TestCtx::new();
    let commands: Vec<(u32, WorldCommand)> = JSON::decode(START_COMMANDS.as_bytes()).unwrap();
    let mut total: Money = Money::ZERO;
    for (_, command) in &commands {
        let cost = Government::action_cost(command, &test.g);
        if matches!(command, WorldCommand::AddTrain { .. }) {
            assert_eq!(cost, Money::ZERO, "legacy no-op train command has a price");
        }
        total += cost;
    }
    assert_eq!(
        test.g.read::<Government>().money,
        Money::new_bucks(150_000) - total
    );
}

#[test]
fn startup_freight_station_and_external_trading_are_connected_both_ways() {
    let test = TestCtx::new();
    let map = test.g.map();
    let station = map
        .buildings()
        .values()
        .find(|b| matches!(b.kind, BuildingKind::RailFreightStation(_)))
        .unwrap();
    let external = map.get(map.external_train_stations[0]).unwrap();
    let depot = station.obb.center().z0();
    let outside = external.obb.center().z0();
    let tick = test.g.read::<prototypes::GameTime>().tick;
    for (start, end) in [(depot, outside), (outside, depot)] {
        assert!(Itinerary::route(tick, start, end, &map, PathKind::Rail).is_some());
    }
}

fn save_fixture(name: &str) -> String {
    format!("p0e-lifecycle-{}-{name}", std::process::id())
}

#[test]
fn p1_spine_save_rejects_wrong_version_before_world_decode() {
    #[derive(serde::Serialize)]
    struct HeaderOnly<'a> {
        version: &'a str,
    }

    for version in ["0.0.0", VERSION] {
        let encoded = CompressedBincode::encode(&HeaderOnly { version }).unwrap();
        let error = CompressedBincode::decode::<Simulation>(&encoded)
            .err()
            .expect("save has no World payload")
            .to_string();
        assert_eq!(
            error.contains("Save has version"),
            version != VERSION,
            "{error}"
        );
    }
}

#[test]
fn disk_load_classifies_a_header_before_decoding_the_world() {
    let name = save_fixture("header-only");
    for version in ["0.7.0", VERSION] {
        let header = SaveHeader {
            version: version.to_owned(),
        };
        for compressed in [true, false] {
            let path = if compressed {
                CompressedBincode::save(&header, &name).unwrap();
                CompressedBincode::filename(&name)
            } else {
                Bincode::save(&header, &name).unwrap();
                Bincode::filename(&name)
            };
            let result = Simulation::load_from_disk(&name);
            std::fs::remove_file(path).unwrap();
            match result {
                Err(SaveLoadError::Incompatible { version: actual }) if version != VERSION => {
                    assert_eq!(actual, version);
                }
                Err(SaveLoadError::Corrupt {
                    version: actual, ..
                }) if version == VERSION => {
                    assert_eq!(actual.as_deref(), Some(VERSION));
                }
                other => panic!("unexpected load result for {version}: {:?}", other.err()),
            }
        }
    }
}

#[test]
fn disk_load_rejects_a_truncated_save_version() {
    let name = save_fixture("truncated-version");
    // Bincode prefixes strings with their length. The fixture omits the bytes.
    Bincode::save(&u64::MAX, &name).unwrap();
    let result = Simulation::load_from_disk(&name);
    std::fs::remove_file(Bincode::filename(&name)).unwrap();
    assert!(matches!(
        result,
        Err(SaveLoadError::Corrupt { version: None, .. })
    ));
}

#[test]
fn p1_spine_replay_rejects_wrong_version_before_command_decode() {
    for version in ["0.0.0", VERSION] {
        let json = format!(
            r#"{{"commands":[[1,"UnknownCommand"]],"enabled":true,"last_tick_recorded":1,"version":{version:?}}}"#
        );
        let error = ReplayJson::decode::<crate::Replay>(json.as_bytes())
            .unwrap_err()
            .to_string();
        assert_eq!(
            error.contains("Replay has version"),
            version != VERSION,
            "{error}"
        );
    }
}

#[test]
fn disk_load_rejects_wrong_version_missing_resources_and_bad_resource_bytes() {
    let test = TestCtx::new();
    for variant in ["current", "version", "patch", "missing", "corrupt"] {
        let mut resources = common::FastMap::default();
        for handler in unsafe { &*std::ptr::addr_of!(crate::SAVELOAD_FUNCS) } {
            resources.insert(handler.name.to_string(), (handler.save)(&test.g));
        }
        let version = match variant {
            "version" => "0.0".to_owned(),
            "patch" => format!("{}.99", VERSION.rsplit_once('.').unwrap().0),
            _ => VERSION.to_owned(),
        };
        if variant == "missing" {
            resources.remove("government");
        }
        if variant == "corrupt" {
            resources.insert("government".to_string(), Vec::new());
        }
        let name = save_fixture(variant);
        CompressedBincode::save(
            &SimulationSer {
                header: SaveHeader { version },
                world: test.g.world(),
                res: resources,
            },
            &name,
        )
        .unwrap();
        let loaded = Simulation::load_from_disk(&name);
        std::fs::remove_file(CompressedBincode::filename(&name)).unwrap();
        match (variant, loaded) {
            ("current", Ok(_)) => {}
            ("version" | "patch", Err(SaveLoadError::Incompatible { version })) => {
                assert_ne!(version, VERSION);
            }
            ("missing" | "corrupt", Err(SaveLoadError::Corrupt { version, .. })) => {
                assert_eq!(version.as_deref(), Some(VERSION));
            }
            (variant, result) => panic!("unexpected result for {variant}: {:?}", result.err()),
        }
        assert!(matches!(
            Simulation::load_from_disk(&name),
            Err(SaveLoadError::NotFound)
        ));
    }
}

#[test]
fn replay_load_rejects_missing_or_mismatched_version() {
    for (variant, bytes) in [
        (
            "legacy",
            r#"{"enabled":true,"commands":[],"last_tick_recorded":0}"#.to_owned(),
        ),
        (
            "mismatch",
            r#"{"version":"0.0","enabled":true,"commands":[],"last_tick_recorded":0}"#.to_owned(),
        ),
        (
            "current",
            format!(
                r#"{{"version":{VERSION:?},"enabled":true,"commands":[],"last_tick_recorded":0}}"#
            ),
        ),
    ] {
        let name = save_fixture(variant);
        std::fs::create_dir_all("world").unwrap();
        let path = JSON::filename(&format!("{name}_replay"));
        std::fs::write(&path, bytes).unwrap();
        let loaded = Simulation::load_replay_from_disk(&name);
        std::fs::remove_file(path).unwrap();
        assert_eq!(loaded.is_some(), variant == "current", "{variant}");
    }
}

#[test]
fn pf_d_abandonment_batch_cleans_departing_workers_and_trade_references() {
    use crate::map_dynamic::{ParkingManagement, RoutingStep};
    use crate::souls::{human::spawn_human, residence};
    use prototypes::{GameTime, Tick, TICKS_PER_HOUR};

    let mut test = TestCtx::new_fast();
    street(&test);
    let departing: Vec<_> = [vec2(150.0, 125.0), vec2(250.0, 125.0)]
        .into_iter()
        .map(|pos| {
            let home = test.build_house_near(pos);
            spawn_human(&mut test.g, home).unwrap()
        })
        .collect();
    for hour in 1..12 {
        *test.g.write::<GameTime>() = GameTime::new(Tick(hour * TICKS_PER_HOUR - 2));
        residence::residence_update(&mut test.g);
        test.g.apply_commands([]);
    }
    let third = test.build_house_near(vec2(275.0, 225.0));
    let survivor = spawn_human(&mut test.g, third).unwrap();
    let passenger = spawn_human(&mut test.g, third).unwrap();
    let cars: Vec<_> = departing
        .iter()
        .map(|&id| {
            let human = test.g.get(id).unwrap();
            if let Some(car) = human.router.personal_car {
                return car;
            }
            let pos = human.trans.pos;
            let car = spawn_parked_vehicle(&mut test.g, VehicleKind::Car, pos).unwrap();
            let router = &mut test.g.world.humans[id].router;
            router.personal_car = Some(car);
            router.use_vehicle(Some(car));
            car
        })
        .collect();
    let car_position = test.g.get(cars[0]).unwrap().trans.pos;
    test.g.world.humans[passenger].location = Location::Vehicle(cars[0]);
    for &id in &departing {
        test.g
            .write::<BuildingInfos>()
            .get_in(third, SoulID::Human(id));
    }
    let route_spot = test
        .g
        .write::<ParkingManagement>()
        .reserve_near(car_position, &test.g.map())
        .unwrap();
    let saved_spot = Bincode::encode(&route_spot).unwrap();
    let destination = Destination::Outside(car_position);
    test.g.world.humans[survivor].router.set_test_route(
        destination,
        vec![
            RoutingStep::WalkTo(car_position),
            RoutingStep::Park(cars[1], Some(route_spot)),
        ],
    );
    let building = factory(&mut test);
    let proto = GoodsCompanyID::new("flour-factory");
    let Some(SoulID::GoodsCompany(company)) =
        crate::souls::goods_company::company_soul(&mut test.g, building, proto)
    else {
        panic!("unable to create employer");
    };
    let bread = ItemID::new("bread");
    let trade = |buyer, seller| Trade {
        buyer: TradeTarget(buyer),
        seller: TradeTarget(seller),
        qty: 1,
        kind: bread,
        money_delta: Money::ZERO,
    };
    test.g.world.humans[survivor].bought.0.insert(
        bread,
        vec![
            trade(SoulID::Human(survivor), SoulID::Human(departing[0])),
            trade(SoulID::Human(survivor), SoulID::GoodsCompany(company)),
        ],
    );
    let employer = &mut test.g.world.companies[company];
    employer.workers.0.extend(departing.iter().copied());
    employer.comp.driver = Some(departing[1]);
    employer.sold.0.push(trade(
        SoulID::Human(departing[1]),
        SoulID::GoodsCompany(company),
    ));

    *test.g.write::<GameTime>() = GameTime::new(Tick(12 * TICKS_PER_HOUR - 2));
    residence::residence_update(&mut test.g);
    test.g.apply_commands([]);

    for id in departing {
        assert!(test.g.get(id).is_none());
        assert_no_market_refs(&test.g, SoulID::Human(id));
        assert!(!test
            .g
            .read::<BuildingInfos>()
            .get(third)
            .unwrap()
            .inside
            .contains(&SoulID::Human(id)));
    }
    assert!(cars.into_iter().all(|car| test.g.get(car).is_none()));
    let passenger = test.g.get(passenger).unwrap();
    assert_eq!(passenger.location, Location::Outside);
    assert_eq!(passenger.trans.pos, car_position);
    assert!(test.g.get(survivor).unwrap().router.target_dest.is_none());
    assert!(test
        .g
        .read::<ParkingManagement>()
        .is_free(Bincode::decode(&saved_spot).unwrap()));
    let trades = &test.g.get(survivor).unwrap().bought.0[&bread];
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].seller, TradeTarget(SoulID::GoodsCompany(company)));
    let employer = test.g.get(company).unwrap();
    assert!(employer.workers.0.is_empty());
    assert_eq!(employer.comp.driver, None);
    assert!(employer.sold.0.is_empty());
    assert!(test.g.world.departing.is_none());
}

#[test]
fn l1_activity_skips_idle_slots_and_keeps_reused_ids_in_storage_order() {
    use crate::souls::human::spawn_human;
    use crate::world::HumanWork;
    use slotmapd::Key;

    let mut test = TestCtx::new();
    street(&test);
    let first_home = test.build_house_near(vec2(150.0, 125.0));
    let second_home = test.build_house_near(vec2(250.0, 125.0));
    let first = spawn_human(&mut test.g, first_home).unwrap();
    let second = spawn_human(&mut test.g, second_home).unwrap();

    let mut ids = Vec::new();
    test.g.world.collect_active(HumanWork::Outside, &mut ids);
    assert!(ids.is_empty(), "residents inside buildings must be skipped");

    for id in [first, second] {
        test.g
            .world
            .edit_human(id, |h| h.location = Location::Outside);
    }
    test.g.world.collect_active(HumanWork::Outside, &mut ids);
    assert_eq!(ids, [first, second]);
    crate::souls::lifecycle::despawn_human(&mut test.g, first);
    test.g.apply_commands([]);
    assert!(test.g.get(first).is_none());
    test.g.world.collect_active(HumanWork::Outside, &mut ids);
    assert_eq!(ids, [second]);
    let replacement = spawn_human(&mut test.g, first_home).unwrap();
    assert_ne!(replacement, first);
    assert_eq!(
        replacement.data().as_ffi() as u32,
        first.data().as_ffi() as u32
    );
    assert!(test.g.world.edit_human(first, |_| ()).is_none());
    test.g.world.collect_active(HumanWork::Outside, &mut ids);
    assert_eq!(ids, [second]);
    test.g
        .world
        .edit_human(replacement, |h| h.location = Location::Outside);
    test.g.world.collect_active(HumanWork::Outside, &mut ids);
    assert_eq!(ids, [replacement, second]);
}

#[test]
fn l1_activity_split_edit_refreshes_after_callback_forgets_its_argument() {
    use crate::souls::human::spawn_human;
    use crate::HumanWork;

    let mut test = TestCtx::new();
    street(&test);
    let home = test.build_house_near(vec2(150.0, 125.0));
    let resident = spawn_human(&mut test.g, home).unwrap();
    let mut ids = Vec::new();
    test.g.world.collect_active(HumanWork::Outside, &mut ids);
    assert!(ids.is_empty());

    #[allow(unused_mut, forgetting_references)]
    test.g.world.humans.edit_each_split(|_, mut human| {
        human.location = Location::Outside;
        std::mem::forget(human);
    });

    test.g.world.collect_active(HumanWork::Outside, &mut ids);
    assert_eq!(ids, [resident]);
}

#[test]
fn l1_activity_ordered_roster_basket_and_route_boundaries() {
    use crate::economy::BasketRequest;
    use crate::economy::{market_update, TripPurpose};
    use crate::map_dynamic::{itinerary_update, routing_changed_system, routing_update_system};
    use crate::souls::goods_company::{company_soul, company_system};
    use crate::souls::household::BasketId;
    use crate::souls::human::{spawn_human, update_decision_system};
    use crate::souls::residence::Residences;
    use crate::utils::scheduler::drain_commands;
    use crate::{HumanEnt, HumanWork, ParCommandBuffer, INITIAL_TICK};
    use prototypes::TICKS_PER_HOUR;

    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(60.0, 250.0, 0.0), vec3(490.0, 250.0, 0.0)]);
    let road = test
        .g
        .map()
        .roads()
        .iter()
        .find(|(_, road)| road.points().first().y == 250.0)
        .unwrap()
        .0;
    let home = test.build_house_near(vec2(120.0, 275.0));
    test.g
        .write::<Residences>()
        .by_building
        .get_mut(&home)
        .unwrap()
        .capacity = 12;
    let place = |sim: &mut Simulation, name, at| {
        let proto = GoodsCompanyID::new(name);
        let building = sim
            .map_mut()
            .build_special_building(
                &OBB::new(at, geom::Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto),
                proto.prototype().bgen,
                None,
                Some(road),
            )
            .unwrap();
        sim.write::<BuildingInfos>().insert(building);
        let SoulID::GoodsCompany(company) = company_soul(sim, building, proto).unwrap() else {
            unreachable!();
        };
        (building, company)
    };
    let (shop, donor) = place(&mut test.g, "commercial-l1", vec2(90.0, 225.0));
    let workers: Vec<_> = (0..12)
        .map(|_| spawn_human(&mut test.g, home).unwrap())
        .collect();
    let sim = &mut test.g;
    let (world, resources) = sim.world_res();
    market_update(world, resources);
    company_system(world, resources);
    assert_eq!(sim.world.companies[donor].workers.0, workers);
    drain_commands(sim);

    let (school, recipient) = place(sim, "school", vec2(355.0, 225.0));
    for &id in &workers {
        let human = &mut sim.world.humans[id];
        let destination = Destination::Building(shop);
        human.decision.kind = HumanDecisionKind::GoTo(destination);
        human.decision.wait = 100;
        human.router.go_to_for(destination, TripPurpose::Work);
    }
    *sim.write::<GameTime>() = GameTime::new(Tick(INITIAL_TICK + TICKS_PER_HOUR + 1));
    let mut trace = Vec::new();
    let mut record = |phase, sim: &Simulation| trace.push((phase, common::hash_u64(sim.hashes())));
    let (world, resources) = sim.world_res();
    market_update(world, resources);
    let moved = sim.world.companies[recipient].workers.0[0];
    assert_eq!(moved, workers[0]);
    let remaining: Vec<_> = workers
        .iter()
        .copied()
        .filter(|&worker| worker != moved)
        .collect();
    assert_eq!(sim.world.companies[donor].workers.0, remaining);
    assert_eq!(
        sim.get(moved).unwrap().router.target_dest,
        Some(Destination::Building(school))
    );
    record("market and roster transfer", sim);

    // Two purchases are ready for collection during the same decision pass.
    // Leave their route planning and the subsequent building exits to the real systems.
    let buyers = [remaining[0], remaining[1]];
    let seller = SoulID::GoodsCompany(donor);
    let item = ItemID::new("retail-goods");
    let pos = sim.map().get(shop).unwrap().door_pos.xy();
    {
        let mut market = sim.write::<Market>();
        market.produce(seller, item, 5);
        market.sell_all(seller, pos, item, 0);
    }
    let now = crate::map_dynamic::transit::elapsed_ticks(&sim.resources).0;
    let mut expected_pickups = Vec::new();
    for ((id, units), reserved_at) in buyers.into_iter().zip([2, 3]).zip([now - 1, now]) {
        let request = BasketRequest {
            fallback: units,
            ..Default::default()
        };
        let (basket, _) = sim
            .write::<Market>()
            .reserve_basket(id, pos, 600.0, request, reserved_at, |_| Some(shop))
            .unwrap();
        let basket_id = basket.id;
        expected_pickups.push(basket_id);
        let (world, resources) = sim.world_res();
        let human = &mut world.humans[id];
        resources
            .write::<BuildingInfos>()
            .get_out(home, SoulID::Human(id));
        resources
            .write::<BuildingInfos>()
            .get_in(shop, SoulID::Human(id));
        human.location = Location::Building(shop);
        human.decision.kind = HumanDecisionKind::PickupBasket(basket_id);
        human.decision.wait = 0;
        assert!(human.household.reserve(basket, resources));
    }
    sim.insert(Vec::<BasketId>::new());
    let (world, resources) = sim.world_res();
    update_decision_system(world, resources);
    assert_eq!(*sim.read::<Vec<BasketId>>(), expected_pickups);
    for (id, units) in buyers.into_iter().zip([2, 3]) {
        assert_eq!(sim.get(id).unwrap().household.carried[&item], units);
        sim.world.humans[id].decision.wait = 0;
    }
    record("basket collected in decision pass", sim);
    let (world, resources) = sim.world_res();
    update_decision_system(world, resources);
    let (world, resources) = sim.world_res();
    let mut candidates = Vec::new();
    world.collect_active(HumanWork::RouteChanged, &mut candidates);
    assert_eq!(candidates, workers);
    routing_changed_system(world, resources);
    record("routes planned", sim);
    let (world, resources) = sim.world_res();
    world.collect_active(HumanWork::RouteStep, &mut candidates);
    assert_eq!(
        candidates
            .iter()
            .copied()
            .filter(|id| buyers.contains(id))
            .collect::<Vec<_>>(),
        buyers
    );
    routing_update_system(world, resources);
    record("route step before callbacks", sim);
    for id in buyers {
        let human = sim.get(id).unwrap();
        assert_eq!(human.location, Location::Outside);
        assert!(human.collider.is_none());
    }
    assert!(ParCommandBuffer::<HumanEnt>::apply(sim));
    assert!(buyers
        .into_iter()
        .all(|id| sim.get(id).unwrap().collider.is_some()));
    record("ordered walking callbacks", sim);
    drain_commands(sim);
    let (world, resources) = sim.world_res();
    itinerary_update(world, resources);
    record("itinerary phase", sim);

    // Captured by running this fixture with the serial phase implementations
    // from the base on which L1 was cherry-picked.
    assert_eq!(
        trace,
        [
            ("market and roster transfer", 15932243669541147862),
            ("basket collected in decision pass", 4499298847199328628),
            ("routes planned", 15333327391016120918),
            ("route step before callbacks", 7653428565931336012),
            ("ordered walking callbacks", 397648402682092739),
            ("itinerary phase", 397648402682092739),
        ]
    );
}

#[test]
fn l1_activity_cancelled_driver_wakes_when_only_the_car_changes() {
    use crate::map_dynamic::{routing_changed_system, Router};
    use crate::souls::human::spawn_human;
    use crate::HumanWork;

    let mut test = TestCtx::new_fast();
    street(&test);
    let home = test.build_house_near(vec2(150.0, 125.0));
    let driver = spawn_human(&mut test.g, home).unwrap();
    let car = spawn_parked_vehicle(&mut test.g, VehicleKind::Car, vec3(175.0, 100.0, 0.0)).unwrap();
    let (world, resources) = test.g.world_res();
    world.humans[driver].router = Router::new(Some(car));
    world.humans[driver].location = Location::Vehicle(car);
    resources
        .write::<BuildingInfos>()
        .get_out(home, SoulID::Human(driver));
    *resources.write::<GameTime>() = GameTime::new(Tick(crate::INITIAL_TICK + 1));
    let mut candidates = Vec::new();
    world.collect_active(HumanWork::RouteChanged, &mut candidates);
    assert_eq!(candidates, [driver]);
    routing_changed_system(world, resources);
    let before = Bincode::encode(test.g.get(driver).unwrap()).unwrap();

    unpark(&mut test.g, car);
    assert_eq!(
        Bincode::encode(test.g.get(driver).unwrap()).unwrap(),
        before
    );
    let (world, resources) = test.g.world_res();
    world.collect_active(HumanWork::RouteChanged, &mut candidates);
    assert_eq!(candidates, [driver]);
    routing_changed_system(world, resources);
    world.collect_active(HumanWork::RouteStep, &mut candidates);
    assert_eq!(candidates, [driver]);
    assert_ne!(
        Bincode::encode(test.g.get(driver).unwrap()).unwrap(),
        before
    );
}

fn l1_assert_phase_hashes(indexed: &mut Simulation, full_scan: &mut Simulation) {
    use crate::utils::scheduler::SeqSchedule;
    assert_eq!(indexed.hashes(), full_scan.hashes(), "paused boundary");

    // Go through the normal command/clock entry point, then run the registered
    // schedule with observations immediately after each callback drain.
    for sim in [&mut *indexed, &mut *full_scan] {
        sim.tick(&mut SeqSchedule::default(), []);
    }
    let schedule = Simulation::schedule();
    let mut legacy = std::collections::VecDeque::new();
    schedule.execute_observed(full_scan, |phase, sim| {
        legacy.push_back((phase, sim.hashes()));
    });
    schedule.execute_observed(indexed, |phase, sim| {
        let (name, expected) = legacy.pop_front().unwrap();
        assert_eq!(phase, name);
        assert_eq!(sim.hashes(), expected, "after {phase}");
    });
    assert!(legacy.is_empty());
}

#[test]
fn l1_activity_transit_removal_clock_jumps_and_dirty_mid_wait_resume() {
    use super::p3_spine::loop_fixture;
    use crate::map_dynamic::transit::elapsed_ticks;
    use crate::map_dynamic::{BusRun, Router, RoutingStep, Transit};
    use crate::souls::human::spawn_human;
    use crate::souls::residence::Residences;
    use crate::transportation::{spawn_driving_vehicle, TransportGrid};
    use crate::HumanWork;
    use geom::Transform;

    let (mut test, line) = loop_fixture();
    let home = *test
        .g
        .read::<Residences>()
        .by_building
        .keys()
        .next()
        .unwrap();
    test.g
        .write::<Residences>()
        .by_building
        .get_mut(&home)
        .unwrap()
        .capacity = 4;
    let rider = spawn_human(&mut test.g, home).unwrap();
    let waiter = spawn_human(&mut test.g, home).unwrap();
    let idle = spawn_human(&mut test.g, home).unwrap();
    let (first, second, first_pos, second_pos, berth, dir) = {
        let transit = test.g.read::<Transit>();
        let first = transit.lines[line].stops[0];
        let second = transit.lines[line].stops[1];
        let stop = &transit.stops[first];
        (
            first,
            second,
            stop.walk_pos,
            transit.stops[second].walk_pos,
            stop.berth,
            stop.dir.z0(),
        )
    };
    let bus = spawn_driving_vehicle(&mut test.g, VehicleKind::Bus, Transform { pos: berth, dir })
        .unwrap();
    let now = elapsed_ticks(&test.g.resources);
    {
        let mut transit = test.g.write::<Transit>();
        transit.lines[line].target_vehicles = 1;
        transit.runs.insert(
            bus,
            BusRun {
                line,
                next_stop_idx: 0,
                dwell_until: Some(Tick(now.0 + 150)),
                leg_started: now,
                failures: 0,
            },
        );
    }

    let mut candidates = Vec::new();
    test.g
        .world
        .collect_active(HumanWork::Outside, &mut candidates);
    assert!(candidates.is_empty());

    // Save without giving a consumer the opportunity to rebuild these dirty sets.
    let (world, resources) = test.g.world_res();
    for id in [rider, waiter] {
        resources
            .write::<BuildingInfos>()
            .get_out(home, SoulID::Human(id));
        let human = &mut world.humans[id];
        human.location = Location::Outside;
        human.trans.pos = second_pos;
        human.router.set_test_route(
            Destination::Outside(first_pos),
            vec![
                RoutingStep::WaitAtStop {
                    stop: second,
                    line,
                    since: now,
                },
                RoutingStep::RideTransit {
                    line,
                    alight: second,
                },
            ],
        );
        human.decision.wait = 2;
    }
    // The rider is bound for the second stop. The waiter stays at that stop.
    assert!(Router::board(&mut world.humans[rider], bus, resources));
    world.humans[idle].decision.wait = 2;
    let paused = Bincode::encode(&test.g).unwrap();
    let mut resumed: Simulation = Bincode::decode(&paused).unwrap();
    assert_eq!(Bincode::encode(&resumed).unwrap(), paused);
    resumed
        .world
        .collect_active(HumanWork::Outside, &mut candidates);
    assert_eq!(candidates, [rider, waiter]);
    resumed
        .world
        .collect_active(HumanWork::RouteStep, &mut candidates);
    assert_eq!(candidates, [rider, waiter]);
    let mut full_scan: Simulation = Bincode::decode(&paused).unwrap();
    full_scan.world.use_legacy_human_scan();

    for wait in [1, 0] {
        l1_assert_phase_hashes(&mut resumed, &mut full_scan);
        assert_eq!(resumed.get(idle).unwrap().decision.wait, wait);
        assert_eq!(resumed.get(rider).unwrap().location, Location::Transit(bus));
    }
    l1_assert_phase_hashes(&mut resumed, &mut full_scan);
    assert!(resumed.get(idle).unwrap().decision.wait > 0);

    for sim in [&mut resumed, &mut full_scan] {
        sim.apply_commands([&WorldCommand::TransitRemoveLine { line }]);
        for id in [rider, waiter] {
            let human = sim.get(id).unwrap();
            assert_eq!(human.location, Location::Outside);
            assert!(human.collider.is_some());
        }
        assert!(sim.get(bus).is_none());
    }
    resumed
        .world
        .collect_active(HumanWork::RouteChanged, &mut candidates);
    assert!(candidates.contains(&rider) && candidates.contains(&waiter));

    for to in [crate::INITIAL_TICK + 5000, crate::INITIAL_TICK + 50] {
        for sim in [&mut resumed, &mut full_scan] {
            sim.apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(to)))]);
        }
        l1_assert_phase_hashes(&mut resumed, &mut full_scan);
    }
    assert_eq!(
        resumed.read::<TransportGrid>().len(),
        full_scan.read::<TransportGrid>().len()
    );
}

#[test]
fn l1_activity_rehome_and_freight_abort_refresh_before_the_next_phase() {
    use crate::economy::cargo::LoadId;
    use crate::souls::lifecycle::abort_freight_trip;
    use crate::HumanWork;

    let mut test = TestCtx::new();
    let (_, driver, _, truck, _) = staffed_factory(&mut test);
    let old_home = test.g.get(driver).unwrap().home.house;
    let new_home = test.build_house_near(vec2(285.0, 260.0));
    let pos = test.g.get(truck).unwrap().trans.pos;
    let dest = Destination::Outside(pos);
    test.g.world.edit_human(driver, |human| {
        human.location = Location::Vehicle(truck);
        human.router.use_vehicle(Some(truck));
        human.router.go_to(dest);
        human.it = Itinerary::simple(vec![pos]);
    });
    let bytes = Bincode::encode(&test.g).unwrap();
    let mut indexed: Simulation = Bincode::decode(&bytes).unwrap();
    let mut full_scan: Simulation = Bincode::decode(&bytes).unwrap();
    full_scan.world.use_legacy_human_scan();

    for sim in [&mut indexed, &mut full_scan] {
        abort_freight_trip(sim, driver, truck, LoadId(1), false);
        sim.apply_commands([]);
        let human = sim.get(driver).unwrap();
        assert_eq!(human.location, Location::Outside);
        assert!(human.router.target_dest.is_none());
        assert!(human.it.is_inert());
    }
    let mut candidates = Vec::new();
    indexed
        .world
        .collect_active(HumanWork::Itinerary, &mut candidates);
    assert!(!candidates.contains(&driver));
    indexed
        .world
        .collect_active(HumanWork::Outside, &mut candidates);
    assert!(candidates.contains(&driver));
    l1_assert_phase_hashes(&mut indexed, &mut full_scan);

    for sim in [&mut indexed, &mut full_scan] {
        sim.apply_commands([&WorldCommand::MapRemoveBuilding(old_home)]);
        assert_eq!(sim.get(driver).unwrap().home.house, new_home);
    }
    l1_assert_phase_hashes(&mut indexed, &mut full_scan);
}
