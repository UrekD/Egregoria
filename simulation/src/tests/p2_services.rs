use super::TestCtx;
use crate::economy::{Budget, BudgetCategory, Government};
use crate::map::BuildingKind;
use crate::map_dynamic::CoverageGrid;
use crate::world_command::{CommandOutcomes, Rejection};
use crate::{Simulation, WorldCommand};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, OBB};
use prototypes::{GoodsCompanyID, Money, ServiceKind};

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
    let position = OBB::new(vec2(200.0, 125.0), vec2(1.0, 0.0), 20.0, 20.0);
    test.g.write::<Government>().money = Money::new_bucks(100_000);
    let before = test.g.read::<Government>().money;
    let spent = test.g.read::<Budget>().total(BudgetCategory::Construction);

    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let road = test.g.map().roads().keys().next().unwrap();
    test.apply(&[WorldCommand::MapRemoveRoad(road)]);
    assert!(test.g.map().get(road).is_none());

    let place = |connected_road| WorldCommand::MapBuildSpecialBuilding {
        pos: position,
        kind,
        gen: proto.bgen,
        zone: None,
        connected_road,
    };
    for invalid in [None, Some(road)] {
        test.g.write::<CommandOutcomes>().last.clear();
        test.apply(&[place(invalid)]);
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
    test.apply(&[place(Some(live_road))]);
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
fn p2_spine_previous_save_defaults_new_resources() {
    let test = TestCtx::new_fast();
    let current = Bincode::encode(&test.g).unwrap();
    let mut payload: crate::SimulationDeser = Bincode::decode(&current).unwrap();
    for name in SPINE_RESOURCES {
        assert!(
            payload.res.remove(name).is_some(),
            "missing {name} resource"
        );
    }
    let previous = Bincode::encode(&crate::SimulationSer {
        header: payload._header,
        world: &payload.world,
        res: payload.res,
    })
    .unwrap();

    let restored: Simulation = Bincode::decode(&previous).expect("load pre-spine save");
    assert_eq!(test.g.hashes(), restored.hashes());
}

#[test]
#[ignore = "P2a coverage"]
fn p2a_coverage_respects_one_way_lanes_and_missing_sidewalks() {
    todo!("Compare driving coverage in both directions and walking coverage across a road without sidewalks");
}

#[test]
#[ignore = "P2a coverage"]
fn p2a_overlapping_providers_allocate_capacity_deterministically() {
    todo!("Assert provider and receiver tie order, including a partially served receiver");
}

#[test]
#[ignore = "P2a coverage"]
fn p2a_coverage_survives_save_load() {
    let test = TestCtx::new_fast();
    test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
    let building = test.build_house_near(vec2(200.0, 125.0));
    test.g
        .write::<CoverageGrid>()
        .by_kind
        .entry(ServiceKind::School)
        .or_default()
        .insert(building, 57.5);

    let restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
    assert_eq!(
        restored
            .read::<CoverageGrid>()
            .coverage(building, ServiceKind::School),
        57.5
    );
}

#[test]
#[ignore = "P2b utilities"]
fn p2b_road_removal_splits_pipe_supply() {
    todo!("Remove a road linking the pump to a consumer and check supply on both sides");
}

#[test]
#[ignore = "P2b utilities"]
fn p2b_pump_in_blackout_cannot_supply_water() {
    todo!("Black out a staffed pump and check that its consumers lose water");
}

#[test]
#[ignore = "P2b utilities"]
fn p2b_utility_supply_is_equal_after_reload() {
    todo!("Save and restore a populated supply snapshot and compare recomputed results");
}

#[test]
#[ignore = "P2c environment"]
fn p2c_pollution_decays_to_zero_after_emitter_removal() {
    todo!("Remove a polluting emitter and wait for its cells to decay and be removed");
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
