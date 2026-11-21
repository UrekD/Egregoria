use super::TestCtx;
use crate::map::{
    BuildingID, BuildingKind, LaneKind, LanePatternBuilder, Map, ProjectFilter, RoadID, Zone,
};
use crate::map_dynamic::{
    electricity_flow_system, env_update, utility_flow_update, CoverageGrid, EnvGrid, UtilityFlow,
};
use crate::souls::{goods_company::company_soul, human::spawn_human, residence::Residences};
use crate::world::{CompanyID, HumanID};
use crate::{SoulID, WorldCommand};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, Polygon, Vec2, OBB};
use prototypes::{GameTime, GoodsCompanyID, ResidencePrototypeID, ServiceKind, Tick};

fn road(test: &TestCtx, from: Vec2, to: Vec2, lanes: LanePatternBuilder) -> RoadID {
    let mut map = test.g.map_mut();
    let from = map.project(from.z0(), 0.0, ProjectFilter::INTER);
    let to = map.project(to.z0(), 0.0, ProjectFilter::INTER);
    map.make_connection(from, to, None, &lanes.build())
        .unwrap()
        .1
}

fn place_company(test: &mut TestCtx, name: &str, position: Vec2, road: RoadID) -> CompanyID {
    let proto = GoodsCompanyID::new(name).prototype();
    let kind = BuildingKind::GoodsCompany(proto.id);
    let (position, direction) = if proto.id.is_service() {
        let map = test.g.map();
        let road = &map.roads()[road];
        let nearest = road.points().project_2d(position).xy();
        let outward = (position - nearest).normalize();
        (nearest + outward * (road.width * 0.5 + 10.25), outward)
    } else {
        (position, Vec2::X)
    };
    let zone = proto.zone.as_ref().map(|_| {
        let bounds = OBB::new(position, Vec2::X, 120.0, 120.0);
        Zone::new(Polygon::from(bounds.corners.as_slice()), Vec2::X)
    });
    test.apply(&[WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(position, direction, 20.0, 20.0),
        kind,
        gen: proto.bgen,
        zone,
        connected_road: Some(road),
    }]);
    let building = test
        .g
        .map()
        .buildings()
        .values()
        .find(|b| b.kind == kind && b.obb.center() == position)
        .unwrap_or_else(|| panic!("could not place {name}"))
        .id;
    let Some(SoulID::GoodsCompany(company)) = company_soul(&mut test.g, building, proto.id) else {
        panic!("could not start {name}");
    };
    company
}

fn tick_at(test: &mut TestCtx, tick: u64) {
    *test.g.write::<GameTime>() = GameTime::new(Tick(tick - 1));
    test.tick_fast();
}

#[test]
fn p2abc_rail_bridge_pipes_and_land_value_follow_refresh_offsets() {
    let mut test = TestCtx::new_fast();
    let lanes = LanePatternBuilder::new();
    let south = vec2(330.0, 190.0);
    let north = vec2(330.0, 340.0);
    let supply = road(&test, vec2(50.0, 190.0), south, lanes);
    let bridge = road(&test, south, north, lanes.rail(true));
    let receiving = road(&test, north, vec2(520.0, 340.0), lanes);
    assert!(test.g.map().roads()[bridge]
        .lanes_iter()
        .all(|(_, kind)| kind == LaneKind::Rail));
    let local = test.build_house_near(vec2(220.0, 220.0));
    let remote = test.build_house_near(vec2(425.0, 365.0));
    assert_eq!(test.g.map().buildings()[local].connected_road, Some(supply));
    assert_eq!(
        test.g.map().buildings()[remote].connected_road,
        Some(receiving)
    );

    let pump = place_company(&mut test, "water-pump", vec2(260.0, 160.0), supply);
    let outlet = place_company(&mut test, "sewage-outlet", vec2(300.0, 160.0), supply);
    let clinic = place_company(&mut test, "clinic", vec2(220.0, 160.0), supply);
    place_company(&mut test, "solar-panel", vec2(120.0, 95.0), supply);
    for company in [pump, outlet] {
        let worker = spawn_human(&mut test.g, local).unwrap();
        test.g.world.companies[company].workers.0.push(worker);
    }
    let worker = spawn_human(&mut test.g, remote).unwrap();
    test.g.world.companies[clinic].workers.0.push(worker);

    tick_at(&mut test, 2_999);
    assert!(!test.g.read::<UtilityFlow>().has_water(remote));
    test.tick_fast();
    for home in [local, remote] {
        let flow = test.g.read::<UtilityFlow>();
        assert!(flow.has_water(home) && flow.has_sewage(home));
        assert_eq!(test.g.read::<EnvGrid>().land_value(home), 40);
    }

    let health = |test: &TestCtx, home| {
        test.g
            .read::<CoverageGrid>()
            .coverage(home, ServiceKind::Health)
    };

    tick_at(&mut test, 6_000);
    assert_eq!(
        health(&test, local),
        0.0,
        "coverage shares the utility tick"
    );
    tick_at(&mut test, 6_299);
    assert_eq!(health(&test, local), 0.0);
    test.tick_fast();
    let covered = health(&test, local);
    assert!(covered > 0.0);
    assert_eq!(
        health(&test, remote),
        0.0,
        "coverage crossed a rail-only road"
    );
    assert_eq!(test.g.read::<EnvGrid>().land_value(local), 40);

    tick_at(&mut test, 9_000);
    let env = test.g.read::<EnvGrid>();
    assert_eq!(env.land_value(local), (40.0 + 15.0 * covered / 100.0) as u8);
    assert_eq!(env.land_value(remote), 40);
}

#[test]
fn p2abc_exact_fractional_staffing_meets_utility_demand() {
    let mut test = TestCtx::new_fast();
    prototypes::test_prototypes(
        r#"
        package.path = "../base_mod/?.lua"
        require("data")
        for _, prototype in ipairs(data) do
            if prototype.name == "residence-low-l1" then
                prototype.water_consumption = 70
            elseif prototype.name == "water-pump" then
                prototype.n_workers = 10
                prototype.water_production = 100
                prototype.water_consumption = nil
            elseif prototype.name == "sewage-outlet" then
                prototype.n_workers = 10
                prototype.sewage_capacity = 100
                prototype.water_consumption = nil
            elseif prototype.name == "solar-panel" then
                prototype.water_consumption = 0.00005
            end
        end
        "#,
    );
    let road = road(
        &test,
        vec2(50.0, 190.0),
        vec2(470.0, 190.0),
        LanePatternBuilder::new(),
    );
    let home = test.build_house_near(vec2(220.0, 220.0));
    let pump = place_company(&mut test, "water-pump", vec2(260.0, 160.0), road);
    let outlet = place_company(&mut test, "sewage-outlet", vec2(300.0, 160.0), road);
    place_company(&mut test, "solar-panel", vec2(120.0, 95.0), road);
    for company in [pump, outlet] {
        test.g.world.companies[company].workers.0 = vec![HumanID::default(); 7];
    }

    test.g.write::<GameTime>().tick.0 = 3_000;
    let (world, resources) = test.g.world_res();
    electricity_flow_system(world, resources);
    utility_flow_update(&mut test.g);
    let flow = test.g.read::<UtilityFlow>();
    let network = test.g.map().electricity.net_id(home).unwrap();
    let (_, totals) = flow.networks().find(|&(id, _)| id == network).unwrap();
    assert!(flow.has_water(home) && flow.has_sewage(home), "{totals:?}");
    assert!((totals.water_production - 70.0).abs() < 1e-12);
    assert!((totals.sewage_capacity - 70.0).abs() < 1e-12);
    assert!(totals.water_demand > totals.water_production);
}

#[test]
fn p2abc_company_pollution_scales_with_staffing() {
    let mut test = TestCtx::new_fast();
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
    let road = road(
        &test,
        vec2(80.0, 240.0),
        vec2(440.0, 240.0),
        LanePatternBuilder::new(),
    );
    let center = vec2(208.0, 208.0);
    let emitter = place_company(&mut test, "coal-power-plant", center, road);
    test.g.write::<GameTime>().tick.0 = 3_000;
    env_update(&mut test.g);
    assert_eq!(test.g.read::<EnvGrid>().pollution_at(center), 0.0);

    test.g.world.companies[emitter].workers.0 = vec![HumanID::default(); 5];
    test.g.write::<GameTime>().tick.0 = 6_000;
    env_update(&mut test.g);
    assert!((test.g.read::<EnvGrid>().pollution_at(center) - 18.0).abs() < 0.001);
    let saved = Bincode::encode(&test.g).unwrap();
    let mut restored = Bincode::decode::<crate::Simulation>(&saved).unwrap();
    assert_eq!(test.g.hashes()["env_grid"], restored.hashes()["env_grid"]);

    for sim in [&mut test.g, &mut restored] {
        sim.world.companies[emitter].workers.0.clear();
        sim.write::<GameTime>().tick.0 = 9_000;
        env_update(sim);
        assert!((sim.read::<EnvGrid>().pollution_at(center) - 16.2).abs() < 0.001);
    }
    assert_eq!(test.g.hashes()["env_grid"], restored.hashes()["env_grid"]);
}

#[test]
fn p2abc_environment_skips_missing_residence_prototype() {
    let mut test = TestCtx::new_fast();
    road(
        &test,
        vec2(80.0, 100.0),
        vec2(420.0, 100.0),
        LanePatternBuilder::new(),
    );
    let home = test.build_house_near(vec2(200.0, 125.0));
    test.g
        .write::<Residences>()
        .by_building
        .get_mut(&home)
        .unwrap()
        .prototype = ResidencePrototypeID::new("removed-residence");
    test.g.write::<GameTime>().tick.0 = 3_000;
    env_update(&mut test.g);
    let env = test.g.read::<EnvGrid>();
    assert!(env.cells.is_empty());
    assert_eq!(env.land_value(home), 30);
}

#[test]
fn p2abc_map_load_rejects_a_dangling_road_lane() {
    let test = TestCtx::new_fast();
    let road = road(
        &test,
        vec2(80.0, 100.0),
        vec2(420.0, 100.0),
        LanePatternBuilder::new(),
    );
    let mut map = test.g.map_mut();
    let (lane, _) = map.roads()[road].lanes_iter().next().unwrap();
    map.lanes.remove(lane).unwrap();
    let bytes = Bincode::encode(&*map).unwrap();
    let error = Bincode::decode::<Map>(&bytes)
        .err()
        .expect("dangling lane ID");
    assert!(error.to_string().contains("invalid lane"), "{error}");
}
