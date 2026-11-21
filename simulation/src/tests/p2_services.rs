use super::TestCtx;
use crate::economy::{Budget, BudgetCategory, Government, Market};
use crate::map::{BuildingID, BuildingKind, RoadID, Zone};
use crate::map_dynamic::{
    electricity_flow_system, utility_flow_update, BuildingInfos, CoverageGrid, ElectricityFlow,
    UtilityFlow,
};
use crate::souls::desire::{Work, WorkKind};
use crate::souls::{goods_company::company_soul, human::spawn_human, residence};
use crate::world::CompanyID;
use crate::world_command::{CommandOutcomes, Rejection};
use crate::{Simulation, SoulID, WorldCommand};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, Polygon, Vec2, OBB};
use prototypes::{
    Density, GameTime, GoodsCompanyID, ItemID, Money, ServiceKind, Tick, DELTA, TICKS_PER_HOUR,
};

const SPINE_RESOURCES: [&str; 5] = [
    "coverage_grid",
    "utility_flow",
    "env_grid",
    "happiness",
    "fires",
];

#[test]
fn p2_spine_service_placement_requires_a_live_road() {
    let mut test = TestCtx::new_fast();
    let proto = GoodsCompanyID::new("school").prototype();
    let kind = BuildingKind::GoodsCompany(proto.id);
    let position = OBB::new(vec2(200.0, 125.0), Vec2::Y, 20.0, 20.0);
    test.g.write::<Government>().money = Money::new_bucks(100_000);
    let before = test.g.read::<Government>().money;
    let spent = test.g.read::<Budget>().total(BudgetCategory::Construction);

    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let road = test.g.map().roads().keys().next().unwrap();
    test.apply(&[WorldCommand::MapRemoveRoad(road)]);
    assert!(test.g.map().get(road).is_none());

    let place = |pos, connected_road| WorldCommand::MapBuildSpecialBuilding {
        pos,
        kind,
        gen: proto.bgen,
        zone: None,
        connected_road,
    };
    for invalid in [None, Some(road)] {
        test.g.write::<CommandOutcomes>().last.clear();
        test.apply(&[place(position, invalid)]);
        assert_eq!(
            test.g.read::<CommandOutcomes>().last.as_slice(),
            &[("MapBuildSpecialBuilding", Rejection::MissingReference)]
        );
        assert!(test.g.map().buildings().values().all(|b| b.kind != kind));
        assert_eq!(test.g.read::<Government>().money, before);
        assert_eq!(
            test.g.read::<Budget>().total(BudgetCategory::Construction),
            spent
        );
    }

    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let live_road = test.g.map().roads().keys().next().unwrap();
    test.g.write::<CommandOutcomes>().last.clear();
    let distant = OBB::new(vec2(200.0, 400.0), Vec2::Y, 20.0, 20.0);
    test.apply(&[place(distant, Some(live_road))]);
    assert_eq!(
        test.g.read::<CommandOutcomes>().last.as_slice(),
        &[("MapBuildSpecialBuilding", Rejection::InvalidBuilding)]
    );
    assert!(test.g.map().buildings().values().all(|b| b.kind != kind));
    assert_eq!(test.g.read::<Government>().money, before);
    assert_eq!(
        test.g.read::<Budget>().total(BudgetCategory::Construction),
        spent
    );

    test.g.write::<CommandOutcomes>().last.clear();
    test.apply(&[place(position, Some(live_road))]);
    assert!(test.g.read::<CommandOutcomes>().last.is_empty());
    assert!(test.g.read::<Government>().money < before);
    assert!(test.g.read::<Budget>().total(BudgetCategory::Construction) < spent);
    assert!(test
        .g
        .map()
        .buildings()
        .values()
        .any(|b| b.kind == kind && b.connected_road == Some(live_road)));
}

#[test]
fn p2_spine_resources_round_trip() {
    let test = TestCtx::new_fast();
    let restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    let original = test.g.hashes();
    let loaded = restored.hashes();
    for name in SPINE_RESOURCES {
        assert!(original.contains_key(name), "missing {name} resource");
        assert_eq!(original[name], loaded[name], "{name}");
    }
}

#[test]
fn p2_spine_save_missing_a_required_resource_is_rejected() {
    let test = TestCtx::new_fast();
    let current = Bincode::encode(&test.g).unwrap();
    for name in SPINE_RESOURCES {
        let mut payload: crate::SimulationDeser = Bincode::decode(&current).unwrap();
        assert!(
            payload.res.remove(name).is_some(),
            "missing {name} resource"
        );
        let incomplete = Bincode::encode(&crate::SimulationSer {
            header: payload._header,
            world: &payload.world,
            res: payload.res,
        })
        .unwrap();
        let error = Bincode::decode::<Simulation>(&incomplete)
            .err()
            .unwrap_or_else(|| panic!("loaded a save missing {name}"));
        assert!(error
            .to_string()
            .contains(&format!("Missing resource {name}")));
    }
}

#[test]
fn p2_spine_school_alone_does_not_advance_residence() {
    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let road = test.g.map().roads().keys().next().unwrap();
    let house = test.build_house_near(vec2(150.0, 125.0));
    let school = GoodsCompanyID::new("school").prototype();
    let school_building = test
        .g
        .map_mut()
        .build_special_building(
            &OBB::new(vec2(200.0, 80.0), -Vec2::Y, 20.0, 20.0),
            BuildingKind::GoodsCompany(school.id),
            school.bgen,
            None,
            Some(road),
        )
        .unwrap();

    for _ in 0..2 {
        let resident = spawn_human(&mut test.g, house).unwrap();
        test.g.world.humans[resident].work =
            Some(Work::new(school_building, WorkKind::Worker, 0.0));
    }
    for hour in 1..=6 {
        *test.g.write::<GameTime>() = GameTime::new(Tick(hour * TICKS_PER_HOUR));
        residence::residence_update(&mut test.g);
    }
    assert_eq!(
        test.g.read::<residence::Residences>().by_building[&house].level,
        1
    );
}

#[test]
fn p2_spine_power_cut_stops_batch_about_to_finish() {
    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
    let road = test.g.map().roads().keys().next().unwrap();
    let house = test.build_house_near(vec2(150.0, 125.0));
    let (plant, generator) =
        p2b_place_company(&mut test, "coal-power-plant", vec2(240.0, 70.0), road);
    p2b_staff(&mut test, generator, house);

    let proto = GoodsCompanyID::new("industrial-l1").prototype();
    let factory = test
        .g
        .map_mut()
        .build_special_building(
            &OBB::new(vec2(320.0, 70.0), Vec2::X, 20.0, 20.0),
            BuildingKind::GoodsCompany(proto.id),
            proto.bgen,
            None,
            Some(road),
        )
        .unwrap();
    test.g.write::<BuildingInfos>().insert(factory);
    let soul = company_soul(&mut test.g, factory, proto.id).unwrap();
    let SoulID::GoodsCompany(company) = soul else {
        unreachable!()
    };
    p2b_staff(&mut test, company, house);
    test.tick_fast();

    let network = test.g.map().electricity.net_id(factory).unwrap();
    assert!(!test.g.read::<ElectricityFlow>().blackout(network));
    let recipe = proto.recipe.as_ref().unwrap();
    let productivity = test.g.world.companies[company].raw_productivity(proto, None);
    let progress = 1.0 - 0.5 * productivity * DELTA / recipe.duration.seconds() as f32;
    assert!(progress < 1.0);
    test.g.world.companies[company].comp.progress = progress;
    let goods = ItemID::new("goods");
    assert_eq!(test.g.read::<Market>().capital(soul, goods), 0);

    test.g
        .tick(&mut test.sched, &[WorldCommand::MapRemoveBuilding(plant)]);
    assert!(test.g.map().get(plant).is_none());
    let network = test.g.map().electricity.net_id(factory).unwrap();
    assert!(test.g.read::<ElectricityFlow>().blackout(network));
    assert_eq!(test.g.read::<Market>().capital(soul, goods), 0);
    assert_eq!(test.g.world.companies[company].comp.progress, progress);
}

fn p2b_road_at(test: &TestCtx, position: Vec2) -> RoadID {
    test.g
        .map()
        .roads()
        .values()
        .find(|road| road.points.project_2d(position).xy().distance2(position) < 1.0)
        .unwrap()
        .id
}

fn p2b_place_company(
    test: &mut TestCtx,
    name: &str,
    position: Vec2,
    road: RoadID,
) -> (BuildingID, CompanyID) {
    let proto = GoodsCompanyID::new(name).prototype();
    let kind = BuildingKind::GoodsCompany(proto.id);
    let (position, direction) = if proto.id.is_service() {
        let map = test.g.map();
        let road = map.get(road).unwrap();
        let nearest = road.points().project_2d(position).xy();
        let outward = (position - nearest).normalize();
        (nearest + outward * (road.width * 0.5 + 10.25), outward)
    } else {
        (position, Vec2::X)
    };
    let zone = proto.zone.as_ref().map(|_| {
        let bounds = OBB::new(position, Vec2::X, 80.0, 80.0);
        Zone::new(Polygon::from(bounds.corners.as_slice()), Vec2::X)
    });
    test.apply(&[WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(position, direction, 20.0, 20.0),
        kind,
        gen: proto.bgen,
        zone,
        connected_road: Some(road),
    }]);
    {
        let outcomes = &test.g.read::<CommandOutcomes>().last;
        assert!(outcomes.is_empty(), "could not place {name}: {outcomes:?}");
    }
    let building = test
        .g
        .map()
        .buildings()
        .values()
        .find(|building| building.kind == kind && building.obb.center() == position)
        .unwrap()
        .id;
    let Some(SoulID::GoodsCompany(company)) = company_soul(&mut test.g, building, proto.id) else {
        panic!("could not start {name}");
    };
    (building, company)
}

fn p2b_staff(test: &mut TestCtx, company: CompanyID, house: BuildingID) {
    let worker = spawn_human(&mut test.g, house).unwrap();
    test.g.world.companies[company].workers.0.push(worker);
}

fn p2b_update(sim: &mut Simulation, tick: u64) {
    *sim.write::<GameTime>() = GameTime::new(Tick(tick));
    let (world, resources) = sim.world_res();
    electricity_flow_system(world, resources);
    utility_flow_update(sim);
}

fn p2b_single_network() -> (TestCtx, BuildingID, RoadID) {
    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
    let road = p2b_road_at(&test, vec2(260.0, 100.0));
    let house = test.build_house_near(vec2(140.0, 125.0));
    let (_, pump) = p2b_place_company(&mut test, "water-pump", vec2(210.0, 70.0), road);
    let (_, outlet) = p2b_place_company(&mut test, "sewage-outlet", vec2(270.0, 70.0), road);
    p2b_staff(&mut test, pump, house);
    p2b_staff(&mut test, outlet, house);
    (test, house, road)
}

#[test]
fn p2b_road_removal_splits_pipe_supply() {
    let mut test = TestCtx::new_fast();
    test.build_roads(&[
        vec3(80.0, 100.0, 0.0),
        vec3(270.0, 100.0, 0.0),
        vec3(270.0, 320.0, 0.0),
        vec3(460.0, 320.0, 0.0),
    ]);
    let supply_road = p2b_road_at(&test, vec2(170.0, 100.0));
    let bridge = p2b_road_at(&test, vec2(270.0, 210.0));
    let supplied = test.build_house_near(vec2(140.0, 125.0));
    let orphan = test.build_house_near(vec2(245.0, 210.0));
    let separated = test.build_house_near(vec2(400.0, 345.0));
    let (_, pump) = p2b_place_company(&mut test, "water-pump", vec2(140.0, 70.0), supply_road);
    let (_, outlet) = p2b_place_company(&mut test, "sewage-outlet", vec2(185.0, 70.0), supply_road);
    p2b_place_company(&mut test, "solar-panel", vec2(230.0, 70.0), supply_road);
    let (_, plant) = p2b_place_company(
        &mut test,
        "coal-power-plant",
        vec2(205.0, 125.0),
        supply_road,
    );
    p2b_staff(&mut test, pump, supplied);
    p2b_staff(&mut test, outlet, supplied);
    p2b_staff(&mut test, plant, separated);

    *test.g.write::<GameTime>() = GameTime::new(Tick(2_998));
    test.tick_fast();
    assert!(!test.g.read::<UtilityFlow>().has_water(supplied));
    test.tick_fast();
    for building in [supplied, orphan, separated] {
        let flow = test.g.read::<UtilityFlow>();
        assert!(flow.has_water(building), "connected house needs water");
        assert!(flow.has_sewage(building), "connected house needs sewage");
    }

    test.apply(&[WorldCommand::MapRemoveRoad(bridge)]);
    assert!(test.g.map().get(orphan).unwrap().is_orphaned());
    assert!(!test.g.map().get(separated).unwrap().is_orphaned());
    assert_ne!(
        test.g.map().electricity.net_id(supplied),
        test.g.map().electricity.net_id(separated)
    );
    p2b_update(&mut test.g, 5_999);
    assert!(test.g.read::<UtilityFlow>().has_water(separated));
    p2b_update(&mut test.g, 6_000);

    let flow = test.g.read::<UtilityFlow>();
    assert!(flow.has_water(supplied));
    assert!(flow.has_sewage(supplied));
    for building in [orphan, separated, BuildingID::default()] {
        assert!(!flow.has_water(building));
        assert!(!flow.has_sewage(building));
    }
}

#[test]
fn p2b_pump_in_blackout_cannot_supply_water() {
    let (mut test, house, road) = p2b_single_network();
    let (power, _) = p2b_place_company(&mut test, "solar-panel", vec2(340.0, 70.0), road);
    let network = test.g.map().electricity.net_id(house).unwrap();
    p2b_update(&mut test.g, 3_000);
    assert!(!test.g.read::<ElectricityFlow>().blackout(network));
    assert!(test.g.read::<UtilityFlow>().has_water(house));

    test.apply(&[WorldCommand::MapRemoveBuilding(power)]);
    p2b_update(&mut test.g, 6_000);
    assert!(test.g.read::<ElectricityFlow>().blackout(network));
    assert!(!test.g.read::<UtilityFlow>().has_water(house));
}

#[test]
fn p2b_outlet_in_blackout_still_serves() {
    let (mut test, house, _) = p2b_single_network();
    p2b_update(&mut test.g, 3_000);
    let network = test.g.map().electricity.net_id(house).unwrap();
    assert!(test.g.read::<ElectricityFlow>().blackout(network));
    let flow = test.g.read::<UtilityFlow>();
    assert!(!flow.has_water(house));
    assert!(flow.has_sewage(house), "staffed outlet works in a blackout");
    let (_, totals) = flow.networks().find(|&(id, _)| id == network).unwrap();
    assert_eq!(totals.water_production, 0.0);
    assert_eq!(totals.sewage_capacity, 500.0);
    assert_eq!(totals.sewage_demand, 0.0);
}

#[test]
fn p2b_shortage_leaves_whole_network_unserved() {
    let mut test = TestCtx::new_fast();
    test.build_roads(&[
        vec3(60.0, 60.0, 0.0),
        vec3(445.0, 60.0, 0.0),
        vec3(445.0, 180.0, 0.0),
        vec3(60.0, 180.0, 0.0),
        vec3(60.0, 300.0, 0.0),
        vec3(445.0, 300.0, 0.0),
        vec3(445.0, 420.0, 0.0),
        vec3(60.0, 420.0, 0.0),
    ]);
    let road = p2b_road_at(&test, vec2(250.0, 60.0));
    let (_, pump) = p2b_place_company(&mut test, "water-pump", vec2(160.0, 25.0), road);
    let (_, outlet) = p2b_place_company(&mut test, "sewage-outlet", vec2(210.0, 25.0), road);
    let (_, power) = p2b_place_company(&mut test, "coal-power-plant", vec2(260.0, 25.0), road);
    let houses: Vec<_> = (0..43)
        .map(|_| {
            let house = test.build_house_near(vec2(250.0, 140.0));
            residence::register(
                &test.g,
                house,
                residence::prototype(Density::High, 3).unwrap(),
            );
            house
        })
        .collect();
    p2b_staff(&mut test, pump, houses[0]);
    p2b_staff(&mut test, outlet, houses[0]);
    p2b_staff(&mut test, power, houses[0]);
    let network = test.g.map().electricity.net_id(road).unwrap();

    // 500 units of water cannot cover 43 homes at 12 units each, plus company demand.
    p2b_update(&mut test.g, 3_000);
    assert!(!test.g.read::<ElectricityFlow>().blackout(network));
    for &house in &houses {
        let flow = test.g.read::<UtilityFlow>();
        assert!(!flow.has_water(house));
        assert!(
            flow.has_sewage(house),
            "no served water means no sewage demand"
        );
    }

    p2b_staff(&mut test, pump, houses[0]);
    p2b_update(&mut test.g, 6_000);
    for &house in &houses {
        let flow = test.g.read::<UtilityFlow>();
        assert!(flow.has_water(house));
        assert!(
            !flow.has_sewage(house),
            "500 units of outlet capacity falls short"
        );
    }
}

#[test]
fn p2b_utility_supply_is_equal_after_reload() {
    let (mut test, house, road) = p2b_single_network();
    let (solar, _) = p2b_place_company(&mut test, "solar-panel", vec2(340.0, 70.0), road);
    let network = test.g.map().electricity.net_id(house).unwrap();
    p2b_update(&mut test.g, 3_000);
    let flow = test.g.read::<UtilityFlow>();
    assert!(flow.has_water(house) && flow.has_sewage(house));
    assert!(flow.has_water(solar) && flow.has_sewage(solar));
    let (_, totals) = flow.networks().find(|&(id, _)| id == network).unwrap();
    assert_eq!(totals.water_production, 500.0);
    assert_eq!(totals.water_demand, 3.0);
    assert_eq!(totals.sewage_capacity, 500.0);
    assert_eq!(totals.sewage_demand, 3.0);
    drop(flow);

    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    let saved_hash = test.g.hashes()["utility_flow"];
    assert_eq!(restored.hashes()["utility_flow"], saved_hash);
    p2b_update(&mut test.g, 6_000);
    p2b_update(&mut restored, 6_000);
    assert_eq!(
        restored.hashes()["utility_flow"],
        test.g.hashes()["utility_flow"]
    );
    assert_eq!(restored.hashes()["utility_flow"], saved_hash);

    let empty_spine_snapshot: UtilityFlow = Bincode::decode(&[0, 0]).unwrap();
    assert!(!empty_spine_snapshot.has_water(house));
    assert!(!empty_spine_snapshot.has_sewage(house));
    assert!(empty_spine_snapshot.networks().next().is_none());
}

#[test]
fn p2c_emitter_adds_pollution_with_linear_falloff() {
    use crate::map_dynamic::EnvGrid;
    use prototypes::GameTime;

    let mut test = TestCtx::new_fast();
    let center = vec2(208.0, 208.0);
    p2c_place_polluter(&mut test, center);

    test.g.write::<GameTime>().tick.0 = 2998;
    test.tick_fast();
    assert!(test.g.read::<EnvGrid>().cells.is_empty());
    test.tick_fast();

    let env = test.g.read::<EnvGrid>();
    assert!((env.pollution_at(center) - 36.0).abs() < 0.001);
    assert!((env.pollution_at(center + vec2(64.0, 0.0)) - 21.6).abs() < 0.001);
    assert_eq!(env.pollution_at(center + vec2(160.0, 0.0)), 0.0);
    assert_eq!(env.pollution_at(center + vec2(128.0, 128.0)), 0.0);
}

#[test]
fn p2c_pollution_decays_to_zero_after_emitter_removal() {
    use crate::map_dynamic::EnvGrid;

    let mut test = TestCtx::new_fast();
    let center = vec2(208.0, 208.0);
    let emitter = p2c_place_polluter(&mut test, center);
    p2c_update_at(&mut test.g, 3000);
    let initial = test.g.read::<EnvGrid>().pollution_at(center);
    assert!(initial > 1.0, "the emitter must pollute before removal");

    test.apply(&[WorldCommand::MapRemoveBuilding(emitter)]);
    assert!(test.g.map().get(emitter).is_none());
    p2c_update_at(&mut test.g, 6000);
    assert_eq!(test.g.read::<EnvGrid>().pollution_at(center), initial * 0.9);

    for pass in 3..=40 {
        p2c_update_at(&mut test.g, pass * 3000);
    }
    let env = test.g.read::<EnvGrid>();
    assert_eq!(env.pollution_at(center), 0.0);
    assert!(env.cells.is_empty());
}

#[test]
fn p2c_pollution_lowers_nearby_land_value() {
    use crate::map_dynamic::EnvGrid;

    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(520.0, 100.0, 0.0)]);
    let near = test.build_house_near(vec2(208.0, 125.0));
    let far = test.build_house_near(vec2(480.0, 125.0));
    p2c_update_at(&mut test.g, 3000);
    let clean = test.g.read::<EnvGrid>().land_value(near);
    assert!(clean > 0, "the residence must receive a land value");
    assert_eq!(test.g.read::<EnvGrid>().land_value(far), clean);

    let emitter = p2c_place_polluter(&mut test, vec2(208.0, 176.0));
    p2c_update_at(&mut test.g, 6000);
    let near_center = test.g.map().buildings()[near].obb.center();
    let env = test.g.read::<EnvGrid>();
    let pollution = env.pollution_at(near_center);
    assert!(pollution > 2.0);
    assert_eq!(
        env.land_value(near),
        (f32::from(clean) - pollution / 2.0) as u8
    );
    assert_eq!(env.land_value(far), clean);
    assert_eq!(env.land_value(emitter), 0);
}

#[test]
fn p2c_environment_survives_save_load() {
    use crate::map_dynamic::EnvGrid;

    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(320.0, 100.0, 0.0)]);
    let home = test.build_house_near(vec2(208.0, 125.0));
    let center = vec2(208.0, 176.0);
    p2c_place_polluter(&mut test, center);
    p2c_update_at(&mut test.g, 3000);

    let save = Bincode::encode(&test.g).unwrap();
    let mut restored: Simulation = Bincode::decode(&save).unwrap();
    {
        let original = test.g.read::<EnvGrid>();
        let loaded = restored.read::<EnvGrid>();
        assert!(
            original.pollution_at(center) > 0.0,
            "save must contain pollution"
        );
        assert!(
            original.land_value(home) > 0,
            "save must contain land value"
        );
        assert_eq!(original.pollution_at(center), loaded.pollution_at(center));
        assert_eq!(original.land_value(home), loaded.land_value(home));
    }
    assert_eq!(test.g.hashes()["env_grid"], restored.hashes()["env_grid"]);

    p2c_update_at(&mut test.g, 6000);
    p2c_update_at(&mut restored, 6000);
    assert_eq!(test.g.hashes()["env_grid"], restored.hashes()["env_grid"]);
}

#[test]
fn p2c_loads_empty_spine_environment() {
    let test = TestCtx::new_fast();
    let mut payload: crate::SimulationDeser =
        Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    let empty_spine_grid = std::collections::BTreeMap::<(i32, i32), (f32, f32)>::new();
    payload.res.insert(
        "env_grid".to_owned(),
        Bincode::encode(&empty_spine_grid).unwrap(),
    );
    let save = Bincode::encode(&crate::SimulationSer {
        header: payload._header,
        world: &payload.world,
        res: payload.res,
    })
    .unwrap();
    let restored: Simulation = Bincode::decode(&save).expect("load save from the P2 spine");
    assert_eq!(test.g.hashes()["env_grid"], restored.hashes()["env_grid"]);
}

fn p2c_place_polluter(test: &mut TestCtx, center: geom::Vec2) -> crate::map::BuildingID {
    // Unit tests run serially because their fixtures replace the global prototype registry.
    prototypes::test_prototypes(
        r#"
        package.path = "../base_mod/?.lua"
        require("data")
        for _, prototype in ipairs(data) do
            if prototype.name == "coal-power-plant" then
                prototype.pollution = 40
            end
        end
        "#,
    );

    let company = GoodsCompanyID::new("coal-power-plant");
    let kind = BuildingKind::GoodsCompany(company);
    test.apply(&[WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(center, geom::Vec2::X, 20.0, 20.0),
        kind,
        gen: company.prototype().bgen,
        zone: None,
        connected_road: None,
    }]);
    test.g
        .map()
        .buildings()
        .values()
        .find(|building| building.kind == kind)
        .expect("place polluter")
        .id
}

fn p2c_update_at(sim: &mut Simulation, tick: u64) {
    sim.write::<prototypes::GameTime>().tick.0 = tick;
    crate::map_dynamic::env_update(sim);
}

#[test]
#[ignore = "P2d progression"]
fn p2d_l2_requires_power_water_sewage_and_school() {
    todo!("Vary power, water, sewage, and school coverage independently and check L2 eligibility");
}

#[test]
#[ignore = "P2d progression"]
fn p2d_unmet_need_resets_progression_counter() {
    todo!("Lose a need after a successful check and verify the streak resets");
}

#[test]
#[ignore = "P2d fire"]
fn p2d_covered_forced_fire_is_extinguished() {
    todo!("Force a fire at a covered building and verify it is extinguished within one hour");
}

#[test]
#[ignore = "P2d fire"]
fn p2d_uncovered_forced_fire_collapses_building_and_restores_lot() {
    todo!("Force a fire at an uncovered building and check collapse and lot restoration");
}

#[test]
#[ignore = "P2d fire"]
fn p2d_removing_a_building_mid_fire_is_safe() {
    todo!("Remove a burning building and verify the next hourly fire update prunes its state");
}

#[test]
#[ignore = "P2d fire"]
fn p2d_fire_rng_is_independent_of_unrelated_spawns() {
    todo!("Compare the same building's fire draws with and without unrelated entity spawns");
}
