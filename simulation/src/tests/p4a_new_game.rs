use crate::economy::{Budget, BudgetCategory, Government};
use crate::map::{BuildingKind, PathKind};
use crate::map_dynamic::Itinerary;
use crate::utils::rand_provider::RandProvider;
use crate::{
    MapPreset, NewGameError, NewGameOptions, Replay, Simulation, WorldCommand, INITIAL_TICK,
};
use geom::vec2;
use prototypes::{GameTime, Money, Tick};

fn options() -> NewGameOptions {
    NewGameOptions {
        terrain_chunks: 16,
        ..NewGameOptions::default()
    }
}

#[test]
fn p4a_seed_zero_matches_legacy_golden() {
    drop(super::TestCtx::new());
    let sim = Simulation::new_game(&NewGameOptions::default()).unwrap();
    let hashes = sim.hashes();
    for (resource, hash) in [
        // The Map hash includes the WATER authority added by W1.
        ("map", 12_149_236_398_957_883_477),
        ("randprovider", 14_593_155_757_080_606_368),
        ("government", 14_969_084_836_084_239_989),
        ("world", 17_476_124_382_984_081_698),
    ] {
        assert_eq!(hashes[resource], hash, "{resource}");
    }
    assert_eq!(hashes, Simulation::new(true).hashes());
    assert!(!sim.read::<Replay>().enabled);
}

#[test]
fn p4a_seed_controls_terrain_and_rng_deterministically() {
    drop(super::TestCtx::new());
    let first = NewGameOptions {
        seed: 1,
        ..options()
    };
    let other = NewGameOptions { seed: 2, ..first };
    let sim = Simulation::new_game(&first).unwrap();
    let same = Simulation::new_game(&first).unwrap();
    let other = Simulation::new_game(&other).unwrap();

    assert_eq!(sim.hashes(), same.hashes());
    assert_ne!(sim.hashes()["map"], other.hashes()["map"]);
    let map = sim.map();
    let other_map = other.map();
    let (width, height) = map.environment().size();
    let samples_per_chunk = crate::map::TerrainChunkID::SIZE as usize / 64;
    let mut different = 0;
    let (width, height) = (
        usize::from(width) * samples_per_chunk,
        usize::from(height) * samples_per_chunk,
    );
    for y in 0..height {
        for x in 0..width {
            let point = vec2(x as f32, y as f32) * 64.0;
            let a = map.environment().true_height(point).unwrap();
            let b = other_map.environment().true_height(point).unwrap();
            different += usize::from((a - b).abs() > 0.5);
        }
    }
    assert!(
        different >= width * height / 4,
        "seeds 1 and 2 differed by over 0.5 m at {different}/{} grid points",
        width * height,
    );
    assert_eq!(
        sim.write::<RandProvider>().next_u64(),
        RandProvider::new(first.seed).next_u64(),
    );
}

#[test]
fn p4a_rejects_invalid_size_and_opening_treasury() {
    for terrain_chunks in [0, 15, 81, u16::MAX] {
        assert!(matches!(
            Simulation::new_game(&NewGameOptions { terrain_chunks, ..options() }),
            Err(NewGameError::TerrainChunks(actual)) if actual == terrain_chunks
        ));
    }
    for starting_money_bucks in [0, 9_999, 10_000_001, u32::MAX] {
        assert!(matches!(
            Simulation::new_game(&NewGameOptions { starting_money_bucks, ..options() }),
            Err(NewGameError::StartingMoneyBucks(actual)) if actual == starting_money_bucks
        ));
    }
}

#[test]
fn p4a_flat_preset_grows_seeded_trees_on_a_constant_heightmap() {
    drop(super::TestCtx::new());
    let flat = NewGameOptions {
        preset: MapPreset::Flat,
        starting_money_bucks: NewGameOptions::MAX_STARTING_MONEY_BUCKS,
        ..options()
    };
    let sim = Simulation::new_game(&flat).unwrap();
    let other = Simulation::new_game(&NewGameOptions {
        seed: u64::MAX,
        ..flat
    })
    .unwrap();
    for city in [&sim, &other] {
        let map = city.map();
        assert_eq!(map.environment().size(), (16, 16));
        let elevation = map.environment().true_height(vec2(0.0, 0.0)).unwrap();
        assert!((elevation - 10.0).abs() < 0.016);
        for point in [vec2(0.0, 0.0), vec2(677.0, 2180.0), vec2(8180.0, 8179.0)] {
            assert_eq!(map.environment().true_height(point), Some(elevation));
        }
        assert!(map.environment().trees.objects().next().is_some());
    }
    assert_ne!(sim.hashes()["map"], other.hashes()["map"]);
}

#[test]
fn p4a_starter_layout_is_connected_and_affordable_at_both_size_limits() {
    drop(super::TestCtx::new());
    for terrain_chunks in [16, 80] {
        for seed in [0, 1, 77] {
            for preset in [MapPreset::Procedural, MapPreset::Flat] {
                let opts = NewGameOptions {
                    preset,
                    seed,
                    terrain_chunks,
                    starting_money_bucks: NewGameOptions::MIN_STARTING_MONEY_BUCKS,
                    ..Default::default()
                };
                let sim = Simulation::new_game(&opts).unwrap();
                let map = sim.map();
                assert_eq!(map.environment().size(), (terrain_chunks, terrain_chunks));
                let station = map
                    .buildings()
                    .values()
                    .find(|b| matches!(b.kind, BuildingKind::RailFreightStation(_)))
                    .unwrap_or_else(|| panic!("no starter depot for {opts:?}"));
                let external = map.get(map.external_train_stations[0]).unwrap();
                let depot = station.obb.center().z(station.height);
                let outside = external.obb.center().z0();
                for (start, end) in [(depot, outside), (outside, depot)] {
                    assert!(
                        Itinerary::route(Tick(sim.get_tick()), start, end, &map, PathKind::Rail)
                            .is_some(),
                        "{opts:?} from {start:?} to {end:?}"
                    );
                }
                let balance = sim.read::<Government>().money;
                let charges = sim
                    .read::<Budget>()
                    .total(BudgetCategory::StarterInfrastructure);
                assert!(charges < 0 && balance > Money::ZERO, "{opts:?}");
                assert_eq!(
                    i128::from(balance.inner()),
                    i128::from(Money::new_bucks(i64::from(opts.starting_money_bucks)).inner())
                        + charges,
                    "{opts:?}"
                );
            }
        }
    }
}

#[test]
fn p4a_seeded_new_game_replay_preserves_paused_batches_and_clock_changes() {
    drop(super::TestCtx::new());
    let mut city = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        seed: 77,
        starting_money_bucks: 25_000,
        ..options()
    })
    .unwrap();
    let (bootstrap, loader) = super::load_recording(&city);
    assert_eq!(loader.pastt, Tick(bootstrap.get_tick()));
    assert_eq!(bootstrap.get_tick(), INITIAL_TICK);
    assert_eq!(bootstrap.hashes(), city.hashes());
    let edit = WorldCommand::SetTaxRate {
        zone: crate::map::ZoneKind::Residential,
        rate_permille: 125,
    };
    city.apply_commands([
        &WorldCommand::SetGameTime(GameTime::new(Tick(90_000))),
        &edit,
    ]);
    city.tick(
        &mut Simulation::schedule(),
        &[WorldCommand::SetGameTime(GameTime::new(Tick(50)))],
    );
    city.apply_commands([&WorldCommand::Terraform {
        kind: crate::map::TerraformKind::Elevation,
        amount: 9.0,
        center: vec2(400.0, 400.0),
        radius: 48.0,
        level: 0.0,
        slope: None,
    }]);
    super::assert_replay_matches(&city);
}

#[test]
fn s5_zone_lock_precedes_charging_and_keeps_erase_available() {
    use crate::economy::progression::ProgressionMode;
    use crate::map::{ZoneGrid, ZoneKind};
    use crate::world_command::{CommandOutcomes, Rejection};
    use geom::{Vec2, OBB};
    use prototypes::Density;

    drop(super::TestCtx::new());
    let area = OBB::new(vec2(256.0, 256.0), Vec2::X, 16.0, 16.0);
    let paint = WorldCommand::ZonePaint {
        area,
        kind: Some(ZoneKind::Residential),
        density: Density::High,
    };
    let mut guided = Simulation::new_game(&NewGameOptions {
        progression: ProgressionMode::Guided,
        ..options()
    })
    .unwrap();
    let before = guided.hashes();
    guided.apply_commands([&paint]);
    assert_eq!(
        guided.read::<CommandOutcomes>().last,
        [("ZonePaint", Rejection::Locked { milestone: 2 })]
    );
    assert!(guided.read::<CommandOutcomes>().accepted.is_empty());
    assert_eq!(guided.hashes(), before);

    guided.apply_commands([&WorldCommand::ZonePaint {
        area,
        kind: None,
        density: Density::High,
    }]);
    assert_eq!(guided.read::<CommandOutcomes>().accepted.len(), 1);

    let mut free_build = Simulation::new_game(&options()).unwrap();
    free_build.apply_commands([&paint]);
    assert_eq!(free_build.read::<CommandOutcomes>().accepted.len(), 1);
    assert!(free_build
        .read::<ZoneGrid>()
        .cells()
        .values()
        .any(|cell| cell.density == Density::High));
}

#[test]
fn s5b_capability_resolves_registered_goods_companies_by_name() {
    use crate::economy::progression::BuildCapability;
    use prototypes::{GoodsCompanyID, ServiceID};

    let _test = super::TestCtx::new();
    assert_eq!(
        BuildCapability::prototype_from_name("bakery"),
        Some(BuildCapability::Prototype(GoodsCompanyID::new("bakery")))
    );
    assert_eq!(
        BuildCapability::prototype_from_name("clinic"),
        Some(BuildCapability::Prototype(ServiceID::new("clinic").into()))
    );
    assert_eq!(BuildCapability::prototype_from_name("bread"), None);
    assert_eq!(BuildCapability::prototype_from_name("missing-warehouse"), None);
}
