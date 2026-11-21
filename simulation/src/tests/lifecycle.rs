use super::TestCtx;
use crate::economy::{Government, Market, Trade, TradeTarget};
use crate::map::{
    BuildingID, BuildingKind, LaneKind, LanePatternBuilder, MapProject, ProjectKind, Zone,
};
use crate::map_dynamic::{
    BuildingInfos, Destination, DispatchID, DispatchKind, DispatchQueryTarget, Dispatcher,
    Itinerary,
};
use crate::souls::desire::{Work, WorkKind};
use crate::souls::freight_station::FreightTrainState;
use crate::souls::human::HumanDecisionKind;
use crate::transportation::testing_vehicles::RandomVehicles;
use crate::transportation::{unpark, Location};
use crate::utils::replay::ReplayJson;
use crate::world_command::{CommandOutcomes, Rejection};
use crate::{SaveHeader, Simulation, SimulationSer, SoulID, WorldCommand, START_COMMANDS, VERSION};
use common::saveload::{CompressedBincode, Encoder, JSON};
use geom::{vec2, vec3, Polygon, OBB};
use prototypes::{BuildingGen, GoodsCompanyID, ItemID, Money, RollingStockID};

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
        assert_eq!(loaded.is_some(), variant == "current", "{variant}");
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
