use super::TestCtx;
use crate::economy::progression::{
    progression_status, progression_update, ObjectiveId, ObjectiveProgress, ProgressionMode,
    ProgressionState,
};
use crate::economy::{market_update, Budget, BudgetCategory, DailyEconomyMetrics};
use crate::map::{
    BuildingID, BuildingKind, Density, LanePatternBuilder, Lot, ProjectFilter, RoadID, ZoneKind,
};
use crate::map_dynamic::{
    electricity_flow_system, env_update, utility_flow_update, CoverageGrid, EnvGrid, UtilityFlow,
};
use crate::souls::{
    goods_company::{company_soul, company_system},
    human::spawn_human,
    residence,
};
use crate::world::CompanyID;
use crate::{NewGameOptions, Simulation, SoulID, WorldCommand, INITIAL_TICK};
use geom::{vec2, vec3, Polygon, Vec2, OBB};
use prototypes::{GameTime, GoodsCompanyID, Money, ServiceKind, TICKS_PER_HOUR};

#[test]
fn p5_core_loop_starter_residential_lots_are_all_buildable() {
    let mut test = TestCtx::new_fast();
    test.g = Simulation::new_game(&NewGameOptions::default()).unwrap();
    let origin = vec2(3630.0, 6120.0);
    let street = LanePatternBuilder::new().build();
    let mut connect = |a: Vec2, b: Vec2| {
        let (from, to) = {
            let map = test.g.map();
            let project = |point: Vec2| {
                let z = map.environment.height(point).unwrap();
                map.project(point.z(z), 0.0, ProjectFilter::INTER | ProjectFilter::ROAD)
            };
            (project(origin + a * 100.0), project(origin + b * 100.0))
        };
        test.apply(&[WorldCommand::MapMakeConnection {
            from,
            to,
            inter: None,
            pat: street.clone(),
        }]);
    };
    // The E3 starter closes each block before extending the next.
    for y in 0..3 {
        for x in 0..3 {
            let p = vec2(x as f32, y as f32);
            if y == 0 {
                connect(p, p + Vec2::X);
            }
            connect(p + Vec2::X, p + Vec2::X + Vec2::Y);
            connect(p + Vec2::X + Vec2::Y, p + Vec2::Y);
            if x == 0 {
                connect(p + Vec2::Y, p);
            }
        }
    }
    for y in 0..3 {
        for x in 0..2 {
            test.apply(&[WorldCommand::ZonePaint {
                area: OBB::new(
                    origin + vec2(x as f32 + 0.5, y as f32 + 0.5) * 100.0,
                    Vec2::X,
                    100.0,
                    100.0,
                ),
                kind: Some(ZoneKind::Residential),
                density: Density::Low,
            }]);
        }
    }
    let initial: Vec<_> = {
        let map = test.g.map();
        let zones = test.g.read::<crate::map::ZoneGrid>();
        map.lots()
            .values()
            .filter(|lot| zones.lot_zone(&lot.shape).is_some())
            .map(|lot| lot.id)
            .collect()
    };
    assert!(initial.len() >= 12, "only {} zoned lots", initial.len());
    let successes = initial
        .iter()
        .filter(|&&lot| {
            test.g
                .map_mut()
                .place_growable(lot, BuildingKind::House)
                .is_some()
        })
        .count();
    assert_eq!(
        successes,
        initial.len(),
        "zoned lot placement must not deadlock"
    );
}

#[test]
fn p5_core_loop_growable_consumes_overlapping_legacy_lots() {
    let test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
    let mut map = test.g.map_mut();
    let lot = map.lots().values().next().unwrap().clone();
    let legacy = map.lots.insert_with_key(|id| Lot { id, ..lot.clone() });
    let legacy_lot = map.lots[legacy].clone();
    map.spatial_map.insert(&legacy_lot);
    assert!(map.place_growable(lot.id, BuildingKind::House).is_some());
    assert!(map.get(lot.id).is_none() && map.get(legacy).is_none());
}

#[test]
fn p5_core_loop_level_two_accepts_alternative_amenities() {
    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
    let house = test.build_house_near(vec2(150.0, 125.0));
    let requirement = |test: &TestCtx| {
        residence::requirements(&test.g, house)
            .into_iter()
            .find(|req| req.name.contains("School") || req.name.contains("school"))
            .unwrap()
    };
    assert!(!requirement(&test).met);
    for kind in [ServiceKind::School, ServiceKind::Health, ServiceKind::Park] {
        test.g.write::<CoverageGrid>().by_kind.clear();
        for score in [29.9, 30.0] {
            test.g
                .write::<CoverageGrid>()
                .by_kind
                .entry(kind)
                .or_default()
                .insert(house, score);
            assert_eq!(requirement(&test).met, score >= 30.0, "{kind:?} at {score}");
        }
    }
    {
        let mut coverage = test.g.write::<CoverageGrid>();
        for kind in [ServiceKind::School, ServiceKind::Health, ServiceKind::Park] {
            coverage
                .by_kind
                .entry(kind)
                .or_default()
                .insert(house, 15.0);
        }
        coverage
            .by_kind
            .entry(ServiceKind::Police)
            .or_default()
            .insert(house, 100.0);
    }
    test.g.write::<GameTime>().tick.0 = 3_002;
    env_update(&mut test.g);
    assert_eq!(test.g.read::<EnvGrid>().land_value(house), 51);
    assert!(
        requirement(&test).met,
        "land value should qualify independently"
    );
}

fn place_company(
    test: &mut TestCtx,
    name: &str,
    center: Vec2,
    road: RoadID,
) -> (BuildingID, CompanyID) {
    let proto = GoodsCompanyID::new(name).prototype();
    let kind = BuildingKind::GoodsCompany(proto.id);
    let (center, axis) = if proto.id.is_service() {
        let map = test.g.map();
        let road = map.get(road).unwrap();
        let nearest = road.points().project_2d(center).xy();
        let outward = (center - nearest).normalize();
        (nearest + outward * (road.width * 0.5 + 10.25), outward)
    } else {
        (center, Vec2::Y)
    };
    let pos = OBB::new(center, axis, 20.0, 20.0);
    let zone = proto
        .zone
        .as_ref()
        .map(|_| crate::map::Zone::new(Polygon::from(pos.corners.as_slice()), Vec2::X));
    let building = if proto.id.is_growable() {
        let lot = test
            .g
            .map()
            .lots()
            .values()
            .min_by_key(|lot| lot.shape.center().distance2(center) as u32)
            .unwrap()
            .id;
        test.g.map_mut().place_growable(lot, kind).unwrap()
    } else {
        test.apply(&[WorldCommand::MapBuildSpecialBuilding {
            pos,
            kind,
            gen: proto.bgen,
            zone,
            connected_road: Some(road),
        }]);
        test.g
            .map()
            .buildings()
            .values()
            .find(|b| b.kind == kind && b.obb.center().distance2(center) < 1.0)
            .expect(name)
            .id
    };
    let SoulID::GoodsCompany(company) = company_soul(&mut test.g, building, proto.id).unwrap()
    else {
        unreachable!();
    };
    (building, company)
}

fn hire(test: &mut TestCtx) {
    let (world, res) = test.g.world_res();
    market_update(world, res);
    company_system(world, res);
    electricity_flow_system(world, res);
}

fn update_utilities(test: &mut TestCtx, elapsed: u64) {
    test.g.write::<GameTime>().tick.0 = INITIAL_TICK + elapsed;
    utility_flow_update(&mut test.g);
}

#[test]
fn p5_core_loop_late_utilities_and_services_hire_when_shops_are_staffed() {
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
    let houses: Vec<_> = [120.0, 210.0, 305.0, 400.0]
        .into_iter()
        .map(|x| test.build_house_near(vec2(x, 285.0)))
        .collect();
    let shops = [
        place_company(&mut test, "commercial-l1", vec2(90.0, 225.0), road).1,
        place_company(&mut test, "commercial-l1", vec2(450.0, 225.0), road).1,
    ];
    for &house in &houses {
        test.g
            .write::<residence::Residences>()
            .by_building
            .get_mut(&house)
            .unwrap()
            .capacity = 6;
        for _ in 0..6 {
            spawn_human(&mut test.g, house).unwrap();
        }
    }
    hire(&mut test);
    assert!(shops
        .into_iter()
        .all(|id| test.g.world().companies[id].workers.0.len() == 12));

    let (_, plant) = place_company(&mut test, "coal-power-plant", vec2(170.0, 135.0), road);
    let (_, pump) = place_company(&mut test, "water-pump", vec2(285.0, 225.0), road);
    let (_, outlet) = place_company(&mut test, "sewage-outlet", vec2(320.0, 225.0), road);
    let (_, school) = place_company(&mut test, "school", vec2(355.0, 225.0), road);
    let (_, clinic) = place_company(&mut test, "clinic", vec2(390.0, 225.0), road);
    hire(&mut test);
    for (id, workers) in [(plant, 10), (pump, 4), (outlet, 4)] {
        assert_eq!(
            test.g.world().companies[id].workers.0.len(),
            workers,
            "{}",
            test.g.world().companies[id].comp.proto.prototype().name
        );
    }
    assert!(
        test.g.world().companies[school].workers.0.len()
            + test.g.world().companies[clinic].workers.0.len()
            > 0
    );

    for hour in 1..=12 {
        update_utilities(&mut test, hour * TICKS_PER_HOUR - 3_000);
        assert!(houses
            .iter()
            .all(|&house| test.g.read::<UtilityFlow>().has_water(house)));
        test.g.write::<GameTime>().tick.0 = hour * TICKS_PER_HOUR - 2;
        residence::residence_update(&mut test.g);
    }
    assert!(test.g.world().humans.len() > 0);
}

#[test]
fn p5_core_loop_positive_balance_is_in_progress_on_day_one() {
    let test = TestCtx::new_fast();
    test.g.write::<ProgressionState>().mode = ProgressionMode::Guided;
    assert!(progression_status(&test.g).objectives.contains(&(
        ObjectiveId::PositiveBalance,
        ObjectiveProgress::InProgress {
            completed: 0,
            required: 2
        },
    )));
}

#[test]
fn p5_core_loop_utility_objective_measures_the_day() {
    let mut test = TestCtx::new_fast();
    test.g.write::<ProgressionState>().mode = ProgressionMode::Guided;
    test.build_roads(&[vec3(50.0, 250.0, 0.0), vec3(480.0, 250.0, 0.0)]);
    let road = test
        .g
        .map()
        .roads()
        .iter()
        .find(|(_, road)| road.points().first().y == 250.0)
        .unwrap()
        .0;
    let house = test.build_house_near(vec2(155.0, 285.0));
    let (_, plant) = place_company(&mut test, "coal-power-plant", vec2(150.0, 135.0), road);
    let (_, pump) = place_company(&mut test, "water-pump", vec2(285.0, 225.0), road);
    let (_, outlet) = place_company(&mut test, "sewage-outlet", vec2(320.0, 225.0), road);
    let worker = spawn_human(&mut test.g, house).unwrap();
    test.g.world.companies[plant].workers.0.push(worker);
    test.g.world.companies[outlet].workers.0.push(worker);

    for day in 0..2 {
        let start = day * 24 * TICKS_PER_HOUR;
        test.g.write::<GameTime>().tick.0 = INITIAL_TICK + start + 1;
        let (_, res) = test.g.world_res();
        Budget::apply(res, BudgetCategory::TradeExport, Money::new_bucks(1000));
        for hour in 0..24 {
            let served = day == 0 || hour >= 2;
            test.g.world.companies[pump].workers.0 = if served { vec![worker] } else { vec![] };
            let (world, res) = test.g.world_res();
            electricity_flow_system(world, res);
            let sample = start + hour * TICKS_PER_HOUR + TICKS_PER_HOUR / 2;
            update_utilities(&mut test, sample);
            assert_eq!(test.g.read::<UtilityFlow>().has_water(house), served);
            progression_update(&mut test.g);
        }
        // The first day ends with an outage, the second with restored service.
        test.g.world.companies[pump].workers.0 = if day == 0 { vec![] } else { vec![worker] };
        update_utilities(&mut test, start + 24 * TICKS_PER_HOUR);
        progression_update(&mut test.g);
        let metrics = test.g.read::<DailyEconomyMetrics>();
        let sample = &metrics.days[&day];
        let ratio = sample.utility_served_demand.unwrap() / sample.utility_total_demand.unwrap();
        assert!(
            (ratio - if day == 0 { 1.0 } else { 22.0 / 24.0 }).abs() < 1e-6,
            "day {day}: {ratio}"
        );
        assert_eq!(
            test.g.read::<ProgressionState>().streaks[&ObjectiveId::PositiveBalance],
            if day == 0 { 1 } else { 0 }
        );
    }
}
