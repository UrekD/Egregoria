use super::TestCtx;
use crate::economy::progression::{
    progression_status, progression_update, ObjectiveId, ObjectiveProgress, ProgressionMode,
    ProgressionState,
};
use crate::economy::{
    demand_update, market_update, Budget, BudgetCategory, DailyEconomyMetrics, Market, TripPurpose,
    TICKS_PER_BUDGET_DAY,
};
use crate::map::{
    BuildingID, BuildingKind, Density, LanePatternBuilder, Lot, LotID, ProjectFilter, RoadID,
    ZoneGrid, ZoneKind,
};
use crate::map_dynamic::{
    electricity_flow_system, env_update, utility_flow_update, CoverageGrid, Destination, EnvGrid,
    RoutingStep, UtilityFlow,
};
use crate::souls::{
    goods_company::{company_soul, company_system},
    growth::growth_update,
    human::{spawn_human, HumanDecisionKind},
    residence,
};
use crate::world::CompanyID;
use crate::{NewGameOptions, Simulation, SoulID, WorldCommand, INITIAL_TICK};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, Intersect, Polygon, Vec2, OBB};
use prototypes::{GameTime, GoodsCompanyID, ItemID, Money, ServiceKind, TICKS_PER_HOUR};

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
    let mut test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
    let lot = test.g.map().lots().values().next().unwrap().clone();
    let overlaps = prepare_legacy_growth(&mut test.g, &lot);
    let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    for sim in [&mut test.g, &mut resumed] {
        growth_update(sim);
        assert!(
            sim.map().get(lot.id).is_none(),
            "growth must build over legacy neighbours"
        );
        assert!(overlaps.iter().all(|&id| sim.map().get(id).is_none()));
    }
    assert_eq!(test.g.hashes(), resumed.hashes());
}

fn prepare_legacy_growth(sim: &mut Simulation, lot: &Lot) -> [LotID; 3] {
    let overlaps = {
        let mut map = sim.map_mut();
        let overlaps = [0; 3].map(|_| map.lots.insert_with_key(|id| Lot { id, ..lot.clone() }));
        // Loading indexes these IDs in slot order.
        for id in overlaps.into_iter().rev() {
            let legacy_lot = map.lots[id].clone();
            map.spatial_map.insert(&legacy_lot);
        }
        overlaps
    };
    sim.write::<ZoneGrid>()
        .paint(lot.shape, Some(ZoneKind::Residential), Density::Low);
    sim.write::<GameTime>().tick.0 = 2_999;
    let (world, resources) = sim.world_res();
    demand_update(world, resources);
    sim.write::<GameTime>().tick.0 = 3_000;
    overlaps
}

#[test]
fn p5_core_loop_clear_road_corner_keeps_frontage() {
    let test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
    let mut map = test.g.map_mut();
    let parent = map
        .roads()
        .iter()
        .find(|(_, road)| road.points().first().y == 100.0)
        .unwrap()
        .0;
    let at = vec2(150.0, 160.0);
    let shape = OBB::new(at + Vec2::X * 10.0, Vec2::X, 20.0, 20.0);
    let pat = LanePatternBuilder::new().sidewalks(false).parking(false);
    let offset = Vec2::splat(pat.width() * 0.4);
    let src = vec2(170.0, 170.0) + offset;
    let dst = src + Vec2::X * 100.0;
    let project = |p: Vec2| {
        map.project(
            p.z(map.environment.height(p).unwrap()),
            0.0,
            ProjectFilter::ALL,
        )
    };
    let (src, dst) = (project(src), project(dst));
    let corner_road = map.make_connection(src, dst, None, &pat.build()).unwrap().1;
    let line = map.get(corner_road).unwrap().boldline();
    assert!(line.intersects(&shape));
    assert!(!line.intersects(&Polygon::from(shape.corners.as_slice())));
    let at = at.z(map.environment.height(at).unwrap());
    let lot = Lot::try_make(&mut map, parent, at, Vec2::X, 20.0)
        .expect("clear corner frontage should be generated");
    Lot::remove_intersecting_lots(&mut map, corner_road);
    assert!(map.place_growable(lot, BuildingKind::House).is_some());
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

fn hire(sim: &mut Simulation) {
    let (world, res) = sim.world_res();
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
    hire(&mut test.g);
    assert!(shops
        .into_iter()
        .all(|id| test.g.world().companies[id].workers.0.len() == 12));

    let (_, plant) = place_company(&mut test, "coal-power-plant", vec2(170.0, 135.0), road);
    let (pump_building, pump) = place_company(&mut test, "water-pump", vec2(285.0, 225.0), road);
    let (_, outlet) = place_company(&mut test, "sewage-outlet", vec2(320.0, 225.0), road);
    let (_, school) = place_company(&mut test, "school", vec2(355.0, 225.0), road);
    let (_, clinic) = place_company(&mut test, "clinic", vec2(390.0, 225.0), road);
    hire(&mut test.g);
    assert!(shops
        .into_iter()
        .all(|id| test.g.world().companies[id].workers.0.len() == 12));

    for hour in 1..=8 {
        update_utilities(&mut test, hour * TICKS_PER_HOUR);
        test.g.write::<GameTime>().tick.0 += 1;
        let before = shops.map(|id| test.g.world().companies[id].workers.0.len());
        hire(&mut test.g);
        let after = shops.map(|id| test.g.world().companies[id].workers.0.len());
        for (before, after) in before.into_iter().zip(after) {
            assert!(after >= 6 && before - after <= 1);
        }
        test.g.write::<GameTime>().tick.0 += 1;
        hire(&mut test.g);
        assert_eq!(
            after,
            shops.map(|id| test.g.world().companies[id].workers.0.len())
        );
    }
    for id in [plant, pump, outlet, school, clinic] {
        assert_eq!(test.g.world().companies[id].workers.0.len(), 1);
    }
    assert!(houses.iter().all(|&house| {
        let utilities = test.g.read::<UtilityFlow>();
        utilities.has_water(house) && utilities.has_sewage(house)
    }));

    test.g
        .write::<residence::Residences>()
        .by_building
        .get_mut(&houses[0])
        .unwrap()
        .capacity += 2;
    let worker = spawn_human(&mut test.g, houses[0]).unwrap();
    hire(&mut test.g);
    assert!(shops
        .into_iter()
        .any(|id| test.g.world().companies[id].workers.0.contains(&worker)));

    // Ordinary hiring can fill spare vacancies. A later daily review can return
    // a surplus utility worker when the network has enough reserve capacity.
    let extra = spawn_human(&mut test.g, houses[0]).unwrap();
    let pump_door = test.g.map().get(pump_building).unwrap().door_pos.xy();
    test.g.write::<Market>().buy(
        SoulID::Human(extra),
        pump_door,
        ItemID::new("job-opening"),
        1,
    );
    hire(&mut test.g);
    assert_eq!(test.g.world().companies[pump].workers.0.len(), 2);
    update_utilities(&mut test, TICKS_PER_BUDGET_DAY - TICKS_PER_HOUR);
    test.g.write::<GameTime>().tick.0 += 1;
    hire(&mut test.g);
    assert_eq!(test.g.world().companies[pump].workers.0.len(), 2);
    update_utilities(&mut test, TICKS_PER_BUDGET_DAY);
    test.g.write::<GameTime>().tick.0 += 1;
    hire(&mut test.g);
    assert_eq!(test.g.world().companies[pump].workers.0.len(), 1);
}

#[test]
fn p5_core_loop_priority_hires_unemployed_before_crossing_donor_floor() {
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
    let house = test.build_house_near(vec2(120.0, 285.0));
    let (_, shop) = place_company(&mut test, "commercial-l1", vec2(90.0, 225.0), road);
    test.g
        .write::<residence::Residences>()
        .by_building
        .get_mut(&house)
        .unwrap()
        .capacity = 7;
    for _ in 0..6 {
        spawn_human(&mut test.g, house).unwrap();
    }
    hire(&mut test.g);
    let (_, school) = place_company(&mut test, "school", vec2(355.0, 225.0), road);
    test.g.write::<GameTime>().tick.0 = INITIAL_TICK + TICKS_PER_HOUR + 1;
    hire(&mut test.g);
    assert_eq!(test.g.world().companies[shop].workers.0.len(), 6);
    assert!(test.g.world().companies[school].workers.0.is_empty());

    let applicant = spawn_human(&mut test.g, house).unwrap();
    test.g.write::<GameTime>().tick.0 += TICKS_PER_HOUR;
    hire(&mut test.g);
    assert_eq!(test.g.world().companies[shop].workers.0.len(), 6);
    assert_eq!(test.g.world().companies[school].workers.0, [applicant]);
}

#[test]
fn p5_core_loop_priority_transfer_retargets_work_and_preserves_shopping() {
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
        .write::<residence::Residences>()
        .by_building
        .get_mut(&home)
        .unwrap()
        .capacity = 12;
    let shops = [
        place_company(&mut test, "commercial-l1", vec2(90.0, 225.0), road),
        place_company(&mut test, "commercial-l1", vec2(450.0, 225.0), road),
    ];
    for _ in 0..12 {
        spawn_human(&mut test.g, home).unwrap();
    }
    hire(&mut test.g);
    // A full donor roster ensures an actual hourly transfer.
    let (shop, donor) = shops[0];
    assert_eq!(test.g.world().companies[donor].workers.0.len(), 12);
    let (school, recipient) = place_company(&mut test, "school", vec2(355.0, 225.0), road);
    let old_dest = Destination::Building(shop);
    for &worker in &test.g.world.companies[donor].workers.0 {
        let human = &mut test.g.world.humans[worker];
        human.decision.kind = HumanDecisionKind::GoTo(old_dest);
        human
            .router
            .set_test_route(old_dest, vec![RoutingStep::WalkTo(human.trans.pos)]);
        human.router.trip_purpose = TripPurpose::Work;
        human.household.attended_shift = Some((shop, 0));
    }

    let lot = test.g.map().lots().values().next().unwrap().clone();
    let overlaps = prepare_legacy_growth(&mut test.g, &lot);
    let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    for sim in [&mut test.g, &mut resumed] {
        growth_update(sim);
        assert!(sim.map().get(lot.id).is_none());
        assert!(overlaps.iter().all(|&id| sim.map().get(id).is_none()));
        sim.write::<GameTime>().tick.0 = INITIAL_TICK + TICKS_PER_HOUR + 1;
        hire(sim);
        let workers = &sim.world().companies[recipient].workers.0;
        assert_eq!(workers.len(), 1);
        let human = &sim.world().humans[workers[0]];
        assert_eq!(
            human.router.target_dest,
            Some(Destination::Building(school))
        );
        assert!(
            matches!(human.decision.kind, HumanDecisionKind::GoTo(Destination::Building(id)) if id == school)
        );
        assert_eq!(human.household.attended_shift, None);
    }
    assert_eq!(test.g.hashes(), resumed.hashes());

    let (clinic, recipient) = place_company(&mut test, "clinic", vec2(390.0, 285.0), road);
    for &worker in &test.g.world.companies[donor].workers.0 {
        let human = &mut test.g.world.humans[worker];
        human.decision.kind = HumanDecisionKind::DepositAtHome;
        human.router.set_test_route(
            Destination::Building(home),
            vec![RoutingStep::WalkTo(human.trans.pos)],
        );
        human.router.trip_purpose = TripPurpose::Shopping;
    }
    test.g.write::<GameTime>().tick.0 += TICKS_PER_HOUR;
    hire(&mut test.g);
    let worker = test.g.world().companies[recipient].workers.0[0];
    let human = &test.g.world().humans[worker];
    assert_eq!(human.work.as_ref().unwrap().workplace, clinic);
    assert_eq!(human.router.target_dest, Some(Destination::Building(home)));
    assert!(matches!(
        human.decision.kind,
        HumanDecisionKind::DepositAtHome
    ));
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
