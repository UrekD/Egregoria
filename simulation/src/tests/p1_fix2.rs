use super::TestCtx;
use crate::economy::balance::CITY_UPDATE_INTERVAL_TICKS as CITY;
use crate::economy::{market_update, EcoStats, Market};
use crate::map::{
    BuildingID, BuildingKind, Density, Map, ProjectFilter, ProjectKind, RoadID, ZoneGrid, ZoneKind,
};
use crate::map_dynamic::{BuildingInfos, Destination, RouterError};
use crate::souls::desire::PICKUP_STALL_TICKS;
use crate::souls::{desire::BuyGoods, growth, residence};
use crate::transportation::Location;
use crate::{HumanEnt, HumanID, ParCommandBuffer, Simulation, SoulID, WorldCommand};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, Vec2, OBB};
use prototypes::{BuildingGen, GameTime, GoodsCompanyID, ItemID, Tick, TICKS_PER_HOUR};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn p1_fix2_player_house_restores_lot_after_reload() {
    let mut test = street();
    let (lot, original) = {
        let map = test.g.map();
        let (id, lot) = map.lots().iter().next().unwrap();
        (id, lot.clone())
    };
    test.apply(&[WorldCommand::MapBuildHouse(lot)]);
    let house = test.g.map().buildings().keys().last().unwrap();
    assert!(test.g.map().get(lot).is_none());
    test.g = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    assert!(test.g.map().lot_origin(house).is_none());

    let before = Bincode::encode(&*test.g.read::<residence::Residences>()).unwrap();
    for hour in 1..=24 {
        let tick = (hour - 1) * TICKS_PER_HOUR + residence::UPDATE_OFFSET;
        assert!(residence::due(tick));
        test.g.write::<GameTime>().tick.0 = tick;
        residence::residence_update(&mut test.g);
    }
    let after = Bincode::encode(&*test.g.read::<residence::Residences>()).unwrap();
    assert_ne!(after, before, "the hourly checks should record residence state");
    assert!(!test.g.read::<residence::Residences>().is_abandoned(house));
    test.remove_fixture_building(house);
    let map = test.g.map();
    assert!(map.get(house).is_none());
    assert!(
        map.lots().values().any(|lot| {
            lot.parent == original.parent
                && lot
                    .shape
                    .corners
                    .iter()
                    .zip(original.shape.corners)
                    .all(|(actual, expected)| actual.is_close(expected, 0.001))
        }),
        "demolishing a player house should restore the original lot"
    );
}

#[test]
fn p1_fix2_retail_counts_only_arrivals() {
    let (mut test, buyer, store, seller) = shop_fixture();
    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);
    assert_eq!(
        test.g
            .read::<Market>()
            .capital(SoulID::Human(buyer), retail()),
        1
    );
    assert_eq!(recorded_retail(&test.g), 0, "a reservation is not a pickup");

    start_pickup(&mut test.g, buyer, store);
    test.g.world_mut_unchecked().humans[buyer].location = Location::Building(store);
    update_pickup(&mut test.g, buyer);
    update_pickup(&mut test.g, buyer);
    assert_eq!(recorded_retail(&test.g), 1);
    let market = test.g.read::<Market>();
    assert_eq!(market.capital(SoulID::Human(buyer), retail()), 0);
    assert_eq!(market.capital(seller, retail()), 0);
}

#[test]
fn p1_fix2_reentered_pickup_restores_selected_car() {
    let (mut test, buyer, store, _) = shop_fixture();
    let car = test.g.get(buyer).unwrap().router.personal_car.unwrap();
    test.g.world_mut_unchecked().humans[buyer]
        .router
        .use_vehicle(Some(car));
    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);

    start_pickup(&mut test.g, buyer, store);
    start_pickup(&mut test.g, buyer, store);
    test.g.world_mut_unchecked().humans[buyer].location = Location::Building(store);
    update_pickup(&mut test.g, buyer);
    assert_eq!(
        test.g.get(buyer).unwrap().router.selected_vehicle(),
        Some(car)
    );
}

#[test]
fn p1_fix2_progress_keeps_pickup_and_timeout_skips_seller_after_reload() {
    let (mut test, buyer, store, seller) = shop_fixture();
    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);
    test.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(
        4 * TICKS_PER_HOUR,
    )))]);
    start_pickup(&mut test.g, buyer, store);

    let buyer_soul = SoulID::Human(buyer);
    for _ in 0..2 * PICKUP_STALL_TICKS + 5 {
        test.g.write::<GameTime>().tick.0 += 1;
        test.g.world_mut_unchecked().humans[buyer].trans.pos.x += 0.025;
        update_pickup(&mut test.g, buyer);
    }
    assert_eq!(
        test.g.read::<Market>().capital(buyer_soul, retail()),
        1,
        "a moving resident should keep the reservation beyond two stall windows"
    );

    test.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(1)))]);
    for _ in 0..PICKUP_STALL_TICKS - 1 {
        test.g.write::<GameTime>().tick.0 += 1;
        update_pickup(&mut test.g, buyer);
    }
    start_pickup(&mut test.g, buyer, store);
    assert_eq!(test.g.read::<Market>().capital(buyer_soul, retail()), 1);
    test.g = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    update_pickup(&mut test.g, buyer);
    {
        let market = test.g.read::<Market>();
        assert_eq!(
            market.capital(buyer_soul, retail()),
            0,
            "a stall window without position progress should cancel"
        );
        assert_eq!(market.capital(seller, retail()), 1);
        assert_eq!(market.inner()[&retail()].sell_order(seller).unwrap().qty, 1);
    }
    assert_eq!(recorded_retail(&test.g), 0);

    test.g = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);
    assert!(test.g.read::<Market>().inner()[&retail()]
        .buy_order(buyer_soul)
        .is_some());
    let road = test.g.map().get(store).unwrap().connected_road.unwrap();
    let (_, alternate_seller) = shop(&mut test, 340.0, road);
    match_orders(&mut test.g);
    let resident = test.g.get(buyer).unwrap();
    assert_eq!(resident.bought.0[&retail()][0].seller.0, alternate_seller);
}

#[test]
fn p1_fix2_pickup_uses_the_sellers_connected_road() {
    let mut test = street();
    let house = test.build_house_near(vec2(155.0, 122.0));
    let buyer = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
    test.build_roads(&[vec3(100.0, 500.0, 0.0), vec3(500.0, 500.0, 0.0)]);
    let disconnected = road_at(&test.g, vec2(300.0, 500.0));
    shop(&mut test, 240.0, disconnected);

    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);
    assert!(
        test.g.read::<Market>().inner()[&retail()]
            .buy_order(SoulID::Human(buyer))
            .is_some(),
        "a nearer sidewalk on an unrelated road should not make a store reachable"
    );
}

#[test]
fn p1_fix2_cancelled_pickup_refunds_only_if_the_seller_still_owns_the_store() {
    let (mut test, buyer, store, seller) = shop_fixture();
    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);
    start_pickup(&mut test.g, buyer, store);
    test.g
        .write::<BuildingInfos>()
        .set_owner(store, SoulID::Human(buyer));
    test.g.world_mut_unchecked().humans[buyer].router.last_error =
        Some(RouterError::LocatingVehicle);
    update_pickup(&mut test.g, buyer);

    let market = test.g.read::<Market>();
    assert_eq!(market.capital(SoulID::Human(buyer), retail()), 0);
    assert_eq!(market.capital(seller, retail()), 0);
    assert!(market.inner()[&retail()].sell_order(seller).is_none());
    assert_eq!(recorded_retail(&test.g), 0);
}

#[test]
fn p1_fix2_walking_components_follow_road_removal_and_reconnection() {
    let mut test = street();
    let house = test.build_house_near(vec2(155.0, 122.0));
    let buyer = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
    let connection = [vec3(400.0, 100.0, 0.0), vec3(500.0, 200.0, 0.0)];
    test.build_roads(&connection);
    test.build_roads(&[connection[1], vec3(800.0, 200.0, 0.0)]);
    let frontage = road_at(&test.g, vec2(650.0, 200.0));
    let (store, seller) = shop(&mut test, 650.0, frontage);

    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);
    start_pickup(&mut test.g, buyer, store);
    test.g.world_mut_unchecked().humans[buyer].location = Location::Building(store);
    update_pickup(&mut test.g, buyer);
    test.g.world_mut_unchecked().humans[buyer].location = Location::Building(house);

    let connector = road_at(&test.g, vec2(450.0, 150.0));
    test.apply(&[WorldCommand::MapRemoveRoad(connector)]);
    {
        let mut market = test.g.write::<Market>();
        market.produce(seller, retail(), 1);
        market.sell(seller, vec2(650.0, 145.0), retail(), 1, 0);
    }
    post_order(&mut test.g, buyer);
    match_orders(&mut test.g);
    let buyer = SoulID::Human(buyer);
    assert!(test.g.read::<Market>().inner()[&retail()]
        .buy_order(buyer)
        .is_some());

    test.build_roads(&connection);
    match_orders(&mut test.g);
    assert_eq!(test.g.read::<Market>().capital(buyer, retail()), 1);
}

#[test]
fn pfc_growth_uses_the_demand_sample_on_elapsed_ticks() {
    use crate::economy::ZoneDemand;

    for destination in [CITY - 2, CITY - 1] {
        let mut test = street();
        let shape = test.g.map().lots().values().next().unwrap().shape;
        test.g
            .write::<ZoneGrid>()
            .paint(shape, Some(ZoneKind::Residential), Density::Low);
        let houses = |sim: &Simulation| {
            sim.map()
                .buildings()
                .values()
                .filter(|building| building.kind == BuildingKind::House)
                .count()
        };

        test.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(destination)))]);
        for _ in 0..2 {
            test.tick_fast();
        }
        assert_eq!(houses(&test.g), 0, "jump destination {destination}");
        assert!(test
            .g
            .read::<ZoneDemand>()
            .drivers(ZoneKind::Residential)
            .is_none());

        for _ in 2..CITY - 2 {
            test.tick_fast();
        }
        assert!(test
            .g
            .read::<ZoneDemand>()
            .drivers(ZoneKind::Residential)
            .is_some());
        assert_eq!(houses(&test.g), 0);
        let mut restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
        test.tick_fast();
        restored.tick(&mut Simulation::schedule(), []);
        assert_eq!(houses(&test.g), 1);
        assert_eq!(test.g.hashes(), restored.hashes());
    }
}

#[test]
fn pfc_demand_scratch_preserves_equality_after_reload() {
    use crate::utils::scheduler::{RunnableFn, SeqSchedule};

    let mut test = TestCtx::new_fast();
    // Isolate demand from the market's independently lazy walking cache.
    let mut schedule = SeqSchedule::default();
    schedule.add_system(Box::new(RunnableFn {
        f: |sim: &mut Simulation| {
            let (world, resources) = sim.world_res();
            crate::economy::demand_update(world, resources);
        },
        name: "demand_update",
    }));
    while test.g.get_tick() < CITY - 1 {
        test.g.tick(&mut schedule, []);
    }
    assert!(test
        .g
        .read::<crate::economy::ZoneDemand>()
        .drivers(ZoneKind::Residential)
        .is_some());
    let restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    assert_eq!(test.g.hashes(), restored.hashes());
    assert!(
        test.g.is_equal(&restored),
        "demand sampling changed the resource count"
    );
}

#[test]
fn pfc_growth_waits_for_the_first_sample_in_a_pre_staggering_save() {
    let mut test = street();
    let shape = test.g.map().lots().values().next().unwrap().shape;
    test.g
        .write::<ZoneGrid>()
        .paint(shape, Some(ZoneKind::Residential), Density::Low);
    // Older saves at this tick are still waiting for their first demand pass.
    test.g.write::<GameTime>().tick.0 = CITY - 1;
    test.g = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    let has_house = |sim: &Simulation| {
        sim.map()
            .buildings()
            .values()
            .any(|building| building.kind == BuildingKind::House)
    };
    test.tick_fast();
    assert_eq!(test.g.get_tick(), CITY);
    assert!(!has_house(&test.g));
    while test.g.get_tick() < 2 * CITY {
        test.tick_fast();
    }
    assert!(has_house(&test.g));
}

#[test]
fn p1_fix2_sparse_growth_has_bounded_search_time() {
    let (finished, result) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut test = street();
        let shape = {
            let mut map = test.g.map_mut();
            let existing: Vec<_> = map.buildings().keys().collect();
            for id in existing {
                map.remove_building(id).unwrap();
            }
            // Stay within the building index bounds while forcing the distant fallback.
            let distant = Vec2::splat(40_000.0);
            map.build_special_building(
                &OBB::new(distant, Vec2::X, 20.0, 20.0),
                BuildingKind::TrainStation,
                BuildingGen::NoWalkway { door_pos: distant },
                None,
                None,
            )
            .expect("distant building");
            map.lots().values().next().unwrap().shape
        };
        test.g
            .write::<ZoneGrid>()
            .paint(shape, Some(ZoneKind::Residential), Density::Low);
        test.g.write::<GameTime>().tick.0 = growth::GROWTH_INTERVAL - 1;
        let (world, resources) = test.g.world_res();
        crate::economy::demand_update(world, resources);
        test.g.write::<GameTime>().tick.0 = growth::GROWTH_INTERVAL;
        assert!(growth::due(test.g.get_tick()));
        growth::growth_update(&mut test.g);
        finished
            .send(
                test.g
                    .map()
                    .buildings()
                    .values()
                    .any(|building| building.kind == BuildingKind::House),
            )
            .unwrap();
    });
    assert!(result
        .recv_timeout(Duration::from_secs(3))
        .expect("growth should finish in three seconds on a sparse map"));
    worker.join().unwrap();
}

fn street() -> TestCtx {
    let test = TestCtx::new();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
    test
}

fn road_at(sim: &Simulation, pos: Vec2) -> RoadID {
    let ProjectKind::Road(road) = sim.map().project(pos.z0(), 2.0, ProjectFilter::ROAD).kind else {
        panic!("no road at {pos:?}");
    };
    road
}

fn shop_fixture() -> (TestCtx, HumanID, BuildingID, SoulID) {
    let mut test = street();
    let house = test.build_house_near(vec2(155.0, 122.0));
    let buyer = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
    let road = test.g.map().get(house).unwrap().connected_road.unwrap();
    let (store, seller) = shop(&mut test, 245.0, road);
    (test, buyer, store, seller)
}

fn shop(test: &mut TestCtx, x: f32, road: RoadID) -> (BuildingID, SoulID) {
    let proto = GoodsCompanyID::new("commercial-l1").prototype();
    let kind = BuildingKind::GoodsCompany(proto.id);
    let pos = vec2(x, 145.0);
    let store = test
        .g
        .map_mut()
        .build_special_building(
            &OBB::new(pos, Vec2::X, 20.0, 20.0),
            kind,
            proto.bgen,
            None,
            Some(road),
        )
        .unwrap();
    crate::souls::growth::on_growable_placed(&test.g, store, kind);
    crate::souls::add_souls_to_empty_buildings(&mut test.g);
    let seller = test.g.read::<BuildingInfos>().owner(store).unwrap();
    let mut market = test.g.write::<Market>();
    market.produce(seller, retail(), 1);
    market.sell(seller, pos, retail(), 1, 0);
    (store, seller)
}

fn retail() -> ItemID {
    ItemID::new("retail-goods")
}

fn post_order(sim: &mut Simulation, buyer: HumanID) {
    let pos = sim.get(buyer).unwrap().trans.pos.xy();
    sim.write::<Market>()
        .buy(SoulID::Human(buyer), pos, retail(), 1);
}

fn match_orders(sim: &mut Simulation) {
    let (world, resources) = sim.world_res();
    market_update(world, resources);
}

fn start_pickup(sim: &mut Simulation, buyer: HumanID, store: BuildingID) {
    let (world, resources) = sim.world_res();
    let human = &mut world.humans[buyer];
    let decision = human.goods.apply(
        &resources.read::<ParCommandBuffer<HumanEnt>>(),
        &resources.read::<BuildingInfos>(),
        &resources.read::<GameTime>(),
        buyer,
        &human.trans,
        &mut human.router,
        &mut human.bought,
    );
    assert!(
        matches!(decision, crate::souls::human::HumanDecisionKind::GoTo(Destination::Building(id)) if id == store)
    );
}

fn update_pickup(sim: &mut Simulation, buyer: HumanID) {
    let (world, resources) = sim.world_res();
    BuyGoods::update_pickup(
        &mut world.humans[buyer],
        &resources.read::<GameTime>(),
        &resources.read::<Map>(),
        &world.vehicles,
        resources,
    );
}

fn recorded_retail(sim: &Simulation) -> i64 {
    sim.read::<EcoStats>()
        .internal_trade
        .iter_histories(3)
        .find(|(item, _)| *item == retail())
        .unwrap()
        .1
        .past_ring_items
        .iter()
        .sum()
}
