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

    let road_center = center + vec2(0.0, 40.0);
    test.build_roads(&[
        (road_center - vec2(100.0, 0.0)).z(0.0),
        (road_center + vec2(100.0, 0.0)).z(0.0),
    ]);
    let road = p2b_road_at(test, road_center);
    let (building, company) = p2b_place_company(test, "coal-power-plant", center, road);
    let house = test.build_house_near(road_center + vec2(0.0, 25.0));
    residence::register(
        &test.g,
        house,
        residence::prototype(Density::High, 3).unwrap(),
    );

    let worker_count = test.g.world.companies[company].comp.max_workers;
    for _ in 0..worker_count {
        spawn_human(&mut test.g, house).expect("house a polluter worker");
    }
    // Fill the job openings before the first environment update. Full staffing
    // gives this unzoned plant raw productivity of 1.0 and its full pollution rate.
    let (world, resources) = test.g.world_res();
    crate::economy::market_update(world, resources);
    assert_eq!(
        world.companies[company].workers.0.len(),
        worker_count as usize
    );
    building
}

fn p2c_update_at(sim: &mut Simulation, tick: u64) {
    sim.write::<prototypes::GameTime>().tick.0 = tick;
    crate::map_dynamic::env_update(sim);
}

struct P2dProgression {
    test: TestCtx,
    house: BuildingID,
    plant: CompanyID,
    pump: CompanyID,
    outlet: CompanyID,
    retailer: BuildingID,
}

fn p2d_house() -> (TestCtx, BuildingID, RoadID) {
    let test = TestCtx::new_fast();
    test.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
    let road = p2b_road_at(&test, vec2(260.0, 100.0));
    let house = test.build_house_near(vec2(140.0, 125.0));
    (test, house, road)
}

impl P2dProgression {
    fn new() -> Self {
        let (mut test, house, road) = p2d_house();
        let (_, plant) = p2b_place_company(&mut test, "coal-power-plant", vec2(200.0, 70.0), road);
        let (_, pump) = p2b_place_company(&mut test, "water-pump", vec2(250.0, 70.0), road);
        let (_, outlet) = p2b_place_company(&mut test, "sewage-outlet", vec2(300.0, 70.0), road);
        let lot = test
            .g
            .map()
            .lots()
            .values()
            .filter(|lot| lot.parent == road)
            .min_by_key(|lot| lot.shape.center().distance2(vec2(350.0, 125.0)) as i32)
            .unwrap()
            .id;
        let kind = BuildingKind::GoodsCompany(GoodsCompanyID::new("commercial-l1"));
        let retailer = test.g.map_mut().place_growable(lot, kind).unwrap();
        crate::souls::growth::on_growable_placed(&test.g, retailer, kind);
        let residents: Vec<_> = (0..2)
            .map(|_| spawn_human(&mut test.g, house).unwrap())
            .collect();
        for &human in &residents {
            test.g.world.humans[human].work = Some(Work::new(retailer, WorkKind::Worker, 0.0));
        }
        for company in [plant, pump, outlet] {
            test.g.world.companies[company].workers.0.push(residents[0]);
        }
        Self {
            test,
            house,
            plant,
            pump,
            outlet,
            retailer,
        }
    }

    fn set_coverage(&self, kind: ServiceKind, score: f32) {
        self.test
            .g
            .write::<CoverageGrid>()
            .by_kind
            .entry(kind)
            .or_default()
            .insert(self.house, score);
    }

    fn check(&mut self, hour: u64) {
        p2b_update(&mut self.test.g, hour * TICKS_PER_HOUR);
        residence::residence_update(&mut self.test.g);
    }

    fn level(&self) -> u8 {
        self.test.g.read::<residence::Residences>().by_building[&self.house].level
    }
}

#[test]
fn p2d_l2_requires_power_water_sewage_and_school() {
    for unmet in ["power", "water", "sewage", "school", "retail", "none"] {
        let mut fixture = P2dProgression::new();
        fixture.set_coverage(
            ServiceKind::School,
            if unmet == "school" { 29.9 } else { 30.0 },
        );
        if unmet == "power" {
            // Model a power cut after the latest utility snapshot.
            p2b_update(&mut fixture.test.g, 3_000);
            let utilities = fixture.test.g.read::<UtilityFlow>();
            assert!(utilities.has_water(fixture.house) && utilities.has_sewage(fixture.house));
        }
        if let Some(company) = match unmet {
            "power" => Some(fixture.plant),
            "water" => Some(fixture.pump),
            "sewage" => Some(fixture.outlet),
            _ => None,
        } {
            fixture.test.g.world.companies[company].workers.0.clear();
        }
        if unmet == "retail" {
            fixture
                .test
                .apply(&[WorldCommand::MapRemoveBuilding(fixture.retailer)]);
            let workplace = fixture.test.g.world.companies[fixture.plant].comp.building;
            for human in fixture.test.g.world.humans.values_mut() {
                human.work = Some(Work::new(workplace, WorkKind::Worker, 0.0));
            }
        }
        for hour in 1..=6 {
            if unmet == "power" {
                let sim = &mut fixture.test.g;
                *sim.write::<GameTime>() = GameTime::new(Tick(hour * TICKS_PER_HOUR));
                let (world, resources) = sim.world_res();
                electricity_flow_system(world, resources);
                residence::residence_update(sim);
            } else {
                fixture.check(hour);
            }
        }
        assert_eq!(
            fixture.level(),
            if unmet == "none" { 2 } else { 1 },
            "{unmet}"
        );
    }
}

#[test]
fn p2d_unmet_need_resets_progression_counter() {
    let mut fixture = P2dProgression::new();
    fixture.set_coverage(ServiceKind::School, 50.0);
    for hour in 1..=5 {
        fixture.check(hour);
    }
    fixture.set_coverage(ServiceKind::School, 0.0);
    fixture.check(6);
    assert_eq!(fixture.level(), 1);

    fixture.set_coverage(ServiceKind::School, 50.0);
    for hour in 7..=11 {
        fixture.check(hour);
    }
    assert_eq!(fixture.level(), 1);
    fixture.check(12);
    assert_eq!(fixture.level(), 2);
}

#[test]
fn p2d_abandonment_requires_twelve_consecutive_unserved_checks() {
    for missing_water in [true, false] {
        let mut fixture = P2dProgression::new();
        let company = if missing_water {
            fixture.pump
        } else {
            fixture.outlet
        };
        let workers = std::mem::take(&mut fixture.test.g.world.companies[company].workers.0);
        let abandoned = |fixture: &P2dProgression| {
            fixture
                .test
                .g
                .read::<residence::Residences>()
                .is_abandoned(fixture.house)
        };
        for hour in 1..=11 {
            fixture.check(hour);
        }
        assert!(!abandoned(&fixture));
        fixture.test.g.world.companies[company].workers.0 = workers;
        fixture.check(12);
        fixture.test.g.world.companies[company].workers.0.clear();
        for hour in 13..=23 {
            fixture.check(hour);
        }
        assert!(!abandoned(&fixture));
        fixture.check(24);
        assert!(abandoned(&fixture));
    }
}

fn p2d_force_ignite(sim: &Simulation, building: BuildingID) {
    *sim.write::<GameTime>() = GameTime::new(Tick(TICKS_PER_HOUR));
    crate::souls::events::force_ignite(sim, building);
}

fn p2d_fire_at(sim: &mut Simulation, hour: u64) {
    *sim.write::<GameTime>() = GameTime::new(Tick(hour * TICKS_PER_HOUR));
    crate::souls::events::events_update(sim);
}

#[test]
fn p2d_covered_forced_fire_is_extinguished() {
    use crate::souls::events::Fires;

    for hour in [2, 4] {
        let (mut test, house, _) = p2d_house();
        test.g
            .write::<CoverageGrid>()
            .by_kind
            .entry(ServiceKind::Fire)
            .or_default()
            .insert(house, 1.0);
        p2d_force_ignite(&test.g, house);
        p2d_fire_at(&mut test.g, hour);
        assert!(!test.g.read::<Fires>().by_building.contains_key(&house));
        assert!(test.g.map().get(house).is_some());
    }
}

#[test]
fn p2d_uncovered_forced_fire_collapses_building_and_restores_lot() {
    use crate::souls::events::Fires;

    let (mut test, house, _) = p2d_house();
    let replacement = test.build_house_near(vec2(310.0, 125.0));
    let resident = spawn_human(&mut test.g, house).unwrap();
    let origin = test.g.map().get(house).unwrap().obb.center();
    let lots_before = test.g.map().lots().len();
    p2d_force_ignite(&test.g, house);
    p2d_fire_at(&mut test.g, 3);
    assert!(test.g.map().get(house).is_some());
    let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    for sim in [&mut test.g, &mut resumed] {
        p2d_fire_at(sim, 4);
        assert!(sim.map().get(house).is_none());
        assert!(!sim.read::<Fires>().by_building.contains_key(&house));
        assert!(!sim
            .read::<residence::Residences>()
            .by_building
            .contains_key(&house));
        assert_eq!(sim.map().lots().len(), lots_before + 1);
        assert!(sim
            .map()
            .lots()
            .values()
            .any(|lot| lot.shape.contains(origin)));
        assert_eq!(sim.world.humans[resident].home.house, replacement);
    }
}

#[test]
fn p2d_removing_a_building_mid_fire_is_safe() {
    use crate::souls::events::Fires;

    let (mut test, house, _) = p2d_house();
    p2d_force_ignite(&test.g, house);
    test.apply(&[WorldCommand::MapRemoveBuilding(house)]);
    p2d_fire_at(&mut test.g, 2);
    assert!(test.g.map().get(house).is_none());
    assert!(!test.g.read::<Fires>().by_building.contains_key(&house));
}

#[test]
fn p2d_fire_rng_is_independent_of_unrelated_spawns() {
    use crate::souls::events::Fires;
    use crate::utils::rand_provider::RandProvider;

    let (mut original, house, _) = p2d_house();
    let mut expanded: Simulation = Bincode::decode(&Bincode::encode(&original.g).unwrap()).unwrap();
    let extra_lot = expanded.map().lots().keys().next().unwrap();
    let extra_house = expanded.map_mut().build_house(extra_lot).unwrap();
    crate::souls::growth::on_growable_placed(&expanded, extra_house, BuildingKind::House);
    spawn_human(&mut expanded, extra_house).unwrap();

    let mut ignitions = 0;
    for hour in 1..=4096 {
        expanded.write::<RandProvider>().next_u32();
        p2d_fire_at(&mut expanded, hour);
        p2d_fire_at(&mut original.g, hour);
        let ignited = original.g.write::<Fires>().by_building.remove(&house);
        assert_eq!(
            expanded.write::<Fires>().by_building.remove(&house),
            ignited
        );
        ignitions += usize::from(ignited.is_some());
    }
    assert!(ignitions > 0, "test must exercise a natural ignition");
}

#[test]
fn p2d_service_upkeep_remainder_is_exact_over_one_day() {
    use crate::economy::budget_update;
    use prototypes::{ServiceID, HOURS_PER_DAY};

    let (mut test, _, road) = p2d_house();
    let before_generic_upkeep = test
        .g
        .map()
        .buildings()
        .values()
        .filter_map(|building| Government::special_building_price(building.kind))
        .map(|price| i128::from(price.inner()))
        .sum::<i128>()
        / 200;
    p2b_place_company(&mut test, "water-pump", vec2(200.0, 70.0), road);
    let initial = i128::from(test.g.read::<Government>().money.inner());
    let target = -i128::from(
        ServiceID::new("water-pump")
            .prototype()
            .upkeep_per_day
            .inner(),
    );
    let advance = |sim: &mut Simulation, hour| {
        *sim.write::<GameTime>() = GameTime::new(Tick(crate::INITIAL_TICK + hour * TICKS_PER_HOUR));
        let (world, resources) = sim.world_res();
        budget_update(world, resources);
    };
    advance(&mut test.g, 1);
    let first_hour = target / i128::from(HOURS_PER_DAY);
    assert_ne!(target % i128::from(HOURS_PER_DAY), 0);
    assert_eq!(
        test.g.read::<Budget>().total(BudgetCategory::ServiceUpkeep),
        first_hour
    );
    let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    for sim in [&mut test.g, &mut resumed] {
        for hour in 2..=HOURS_PER_DAY as u64 {
            advance(sim, hour);
        }
        let budget = sim.read::<Budget>();
        let day = &budget.days()[&0];
        assert_eq!(day[&BudgetCategory::ServiceUpkeep], target);
        assert_eq!(
            budget.total(BudgetCategory::SpecialBuildingUpkeep),
            -before_generic_upkeep
        );
        let operating = target
            + budget.total(BudgetCategory::SpecialBuildingUpkeep)
            + budget.total(BudgetCategory::RoadUpkeep);
        assert_eq!(
            i128::from(sim.read::<Government>().money.inner()),
            initial + operating
        );
    }
}

#[test]
fn p2d_l3_requires_health_and_land_value() {
    for (health, police, expected_level) in [(29.9, 100.0, 2), (30.0, 0.0, 2), (30.0, 100.0, 3)] {
        let mut fixture = P2dProgression::new();
        let l2 = residence::prototype(Density::Low, 2).unwrap();
        fixture
            .test
            .g
            .write::<residence::Residences>()
            .by_building
            .insert(fixture.house, l2.into());
        for _ in 0..2 {
            let human = spawn_human(&mut fixture.test.g, fixture.house).unwrap();
            fixture.test.g.world.humans[human].work =
                Some(Work::new(fixture.retailer, WorkKind::Worker, 0.0));
        }
        fixture.set_coverage(ServiceKind::School, 30.0);
        fixture.set_coverage(ServiceKind::Health, health);
        fixture.set_coverage(ServiceKind::Police, police);
        p2b_update(&mut fixture.test.g, 3_000);
        crate::map_dynamic::env_update(&mut fixture.test.g);
        let land_value = fixture
            .test
            .g
            .read::<crate::map_dynamic::EnvGrid>()
            .land_value(fixture.house);
        assert_eq!(land_value >= 50, police > 0.0);
        for hour in 1..=6 {
            fixture.check(hour);
        }
        assert_eq!(
            fixture.level(),
            expected_level,
            "health={health}, land value={land_value}"
        );
    }
}

#[test]
fn p2d_happiness_changes_residential_demand() {
    use crate::economy::balance::K_HAPPY;
    use crate::economy::{demand_update, ZoneDemand};
    use crate::map::ZoneKind;
    use crate::souls::happiness::{happiness_update, Happiness};

    let mut fixture = P2dProgression::new();
    let house = fixture.house;
    let sample = |sim: &mut Simulation, hour| {
        p2b_update(sim, hour * TICKS_PER_HOUR);
        crate::map_dynamic::env_update(sim);
        happiness_update(sim);
        let (world, resources) = sim.world_res();
        demand_update(world, resources);
        (
            sim.read::<Happiness>().by_building[&house],
            sim.read::<ZoneDemand>()
                .drivers(ZoneKind::Residential)
                .unwrap()
                .base,
        )
    };
    let (neglected, low_demand) = sample(&mut fixture.test.g, 1);
    for kind in [
        ServiceKind::Fire,
        ServiceKind::Health,
        ServiceKind::Police,
        ServiceKind::School,
        ServiceKind::Park,
        ServiceKind::Garbage,
    ] {
        fixture.set_coverage(kind, 90.0);
    }
    let (served, high_demand) = sample(&mut fixture.test.g, 2);
    assert!(neglected <= 45 && served >= 60);
    let expected_change = K_HAPPY * f32::from(served - neglected);
    assert!((high_demand - low_demand - expected_change).abs() < 0.00001);
}
