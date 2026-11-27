use crate::economy::progression::{
    can_build, progression_status, BuildCapability, ProgressionMode,
};
use crate::economy::CityStats;
use crate::map::{AuthoredTownOptions, LotLayout};
use crate::souls::human::spawn_human;
use crate::utils::replay::{replay_version, ReplayJson};
use crate::{
    MapConstructor, MapOrigin, NewGameError, NewGameOptions, PopulationScale, Replay,
    SaveLoadError, Simulation, SimulationDeser, SimulationOptions, WorldCommand,
};
use common::saveload::{Bincode, Encoder, JSON};
use geom::{vec2, vec3};
use prototypes::TICKS_PER_MINUTE;

use super::{init_test_simulation, TestCtx};

fn mark_legacy(snapshot: &mut SimulationDeser, schema: crate::SnapshotSchema, version: &str) {
    snapshot._header.version = version.to_owned();
    snapshot.res.remove("service_incidents");
    snapshot.res.remove("site_remnants");
    snapshot.res.remove("fire_fleet");
    snapshot.res.insert(
        "prototype_registry".into(),
        Bincode::encode(&schema.registry().unwrap()).unwrap(),
    );
    snapshot.res.insert(
        "replay".into(),
        format!(r#"{{"version":"{version}","enabled":false,"commands":[]}}"#).into_bytes(),
    );
}

fn small_options(persons_per_agent: u16) -> SimulationOptions {
    SimulationOptions {
        terrain_size: 1,
        persons_per_agent,
        ..SimulationOptions::LEGACY
    }
}

#[test]
fn population_scale_presets_round_trip_in_snapshot_and_replay() {
    init_test_simulation();
    assert_eq!(NewGameOptions::default().persons_per_agent, 1);
    assert_eq!(SimulationOptions::default().persons_per_agent, 1);
    assert!(Simulation::new(false)
        .try_write::<SimulationOptions>()
        .is_none());
    for n in PopulationScale::PRESETS {
        let scale = PopulationScale::try_new(n).unwrap();
        assert_eq!(scale.persons(3), 3 * u64::from(n));

        let mut sim = Simulation::new_with_options(small_options(n));
        sim.tick(&mut Simulation::schedule(), &[]);
        let snapshot: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
        assert_eq!(snapshot.population_scale(), scale);
        assert_eq!(snapshot.hashes(), sim.hashes());

        let recording = ReplayJson::encode(&*sim.read::<Replay>()).unwrap();
        let (mut replayed, mut loader) =
            Simulation::try_from_replay(ReplayJson::decode(&recording).unwrap()).unwrap();
        assert_eq!(replayed.population_scale(), scale);
        assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
        assert_eq!(replayed.hashes(), sim.hashes());
    }
    if usize::BITS >= 64 {
        assert_eq!(
            PopulationScale::try_new(16).unwrap().persons(usize::MAX),
            u64::MAX
        );
    }
}

#[test]
fn population_scale_rejects_invalid_values_and_defaults_legacy_omissions() {
    init_test_simulation();
    let sim = Simulation::new_with_options(small_options(1));
    let recording = String::from_utf8(ReplayJson::encode(&*sim.read::<Replay>()).unwrap()).unwrap();
    let ratio = ",\"persons_per_agent\":1";
    assert!(recording.contains(ratio));
    let snapshot = Bincode::encode(&sim).unwrap();
    for invalid in [0, 3, 32, u16::MAX] {
        assert!(matches!(
            Simulation::new_game(&NewGameOptions {
                persons_per_agent: invalid,
                ..NewGameOptions::default()
            }),
            Err(NewGameError::PersonsPerAgent(n)) if n == invalid
        ));
        let options = small_options(invalid);
        assert!(!options.valid_for_bootstrap());
        let invalid_recording =
            recording.replacen(ratio, &format!(",\"persons_per_agent\":{invalid}"), 1);
        let error =
            Simulation::try_from_replay(JSON::decode(invalid_recording.as_bytes()).unwrap())
                .err()
                .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("population ratio"));

        let mut decoded: SimulationDeser = Bincode::decode(&snapshot).unwrap();
        decoded
            .res
            .insert("simoptions".into(), Bincode::encode(&options).unwrap());
        assert!(matches!(
            decoded.into_sim(),
            Err(SaveLoadError::Corrupt { reason, .. }) if reason.contains("population ratio")
        ));
    }
    let legacy_recording = recording.replacen(ratio, "", 1);
    let (replayed, _) =
        Simulation::try_from_replay(JSON::decode(legacy_recording.as_bytes()).unwrap()).unwrap();
    assert_eq!(replayed.population_scale(), PopulationScale::REALISTIC);

    let mut decoded: SimulationDeser = Bincode::decode(&snapshot).unwrap();
    mark_legacy(&mut decoded, crate::SnapshotSchema::PreScale, crate::PRE_SCALE_VERSION);
    let mut old_options = decoded.res.remove("simoptions").unwrap();
    assert_eq!(old_options.pop(), Some(1)); // Bincode encodes this appended u16 in one byte.
    decoded.res.insert("simoptions".into(), old_options);
    let restored = decoded.into_sim().unwrap();
    assert_eq!(restored.population_scale(), PopulationScale::REALISTIC);
    assert!(restored.read::<Replay>().migrated_city());
}

#[test]
fn population_scale_replay_rejects_legacy_protocol_in_current_snapshots() {
    init_test_simulation();
    let sim = Simulation::new_with_options(small_options(4));
    let mut replay = sim.read::<Replay>().clone();
    replay.version = format!("{}+commands.2", crate::PRE_SCALE_VERSION);
    let recording = ReplayJson::encode(&replay).unwrap();
    assert!(matches!(
        ReplayJson::decode_snapshot(&recording),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData
            && error.to_string().contains("expected")
    ));
}

#[test]
fn population_scale_snapshot_rejects_options_the_constructors_reject() {
    init_test_simulation();
    let sim = Simulation::new_with_options(small_options(4));
    let snapshot = Bincode::encode(&sim).unwrap();
    for options in [
        SimulationOptions {
            terrain_size: 81,
            ..small_options(4)
        },
        SimulationOptions {
            terrain_size: 16,
            starting_money_bucks: 9_999,
            ..small_options(4)
        },
        SimulationOptions {
            terrain_size: 16,
            starting_money_bucks: 10_000_001,
            ..small_options(4)
        },
        SimulationOptions {
            seed: 42,
            ..small_options(4)
        },
        SimulationOptions {
            preset: crate::MapPreset::Flat,
            ..small_options(4)
        },
    ] {
        assert!(!options.valid_for_bootstrap());
        let mut decoded: SimulationDeser = Bincode::decode(&snapshot).unwrap();
        decoded
            .res
            .insert("simoptions".into(), Bincode::encode(&options).unwrap());
        assert!(
            matches!(decoded.into_sim(), Err(SaveLoadError::Corrupt { .. })),
            "{options:?}"
        );
    }
}

#[test]
fn population_scale_snapshot_uses_a_distinct_compatibility_version() {
    init_test_simulation();
    let sim = Simulation::new_with_options(small_options(4));
    let snapshot = Bincode::encode(&sim).unwrap();
    let (header, _) = crate::snapshot_header(&snapshot).unwrap();
    assert_eq!(header.version, crate::VERSION);
    assert!(!header.version.contains('-'));
    assert_ne!(header.version, "0.11.0");
    assert_eq!(sim.read::<Replay>().version, replay_version());
    assert_eq!(
        Bincode::decode::<Simulation>(&snapshot)
            .unwrap()
            .population_scale(),
        sim.population_scale()
    );
    let mut mislabeled: SimulationDeser = Bincode::decode(&snapshot).unwrap();
    mark_legacy(&mut mislabeled, crate::SnapshotSchema::PreScale, crate::PRE_SCALE_VERSION);
    assert!(matches!(
        mislabeled.into_sim(),
        Err(SaveLoadError::Corrupt { reason, .. }) if reason.contains("must use Realistic")
    ));
}

#[test]
fn population_scale_snapshot_accepts_compact_authored_town() {
    init_test_simulation();
    let sim = Simulation::new_authored_town(crate::map::AuthoredTownOptions::default()).unwrap();
    let snapshot = Bincode::encode(&sim).unwrap();
    let loaded: Simulation = Bincode::decode(&snapshot).unwrap();
    assert_eq!(loaded.hashes(), sim.hashes());
    assert_eq!(loaded.population_scale(), PopulationScale::REALISTIC);
}

#[test]
fn population_scale_snapshot_rejects_scaled_compact_authored_town() {
    init_test_simulation();
    let sim = Simulation::new_authored_town(crate::map::AuthoredTownOptions::default()).unwrap();
    let mut options = *sim.read::<SimulationOptions>();
    options.persons_per_agent = 4;

    let snapshot = Bincode::encode(&sim).unwrap();
    let mut decoded: SimulationDeser = Bincode::decode(&snapshot).unwrap();
    decoded
        .res
        .insert("simoptions".into(), Bincode::encode(&options).unwrap());
    let error = decoded.into_sim().err();
    assert!(matches!(
        error,
        Some(SaveLoadError::Corrupt { ref reason, .. }) if reason.contains("must use Realistic")
    ), "{error:?}");
}

#[test]
fn population_scale_snapshot_rejects_scaled_authored_flat_at_native_size() {
    init_test_simulation();
    let sim = Simulation::new_authored_flat(NewGameOptions::MIN_TERRAIN_CHUNKS, 42).unwrap();
    let mut options = *sim.read::<SimulationOptions>();
    options.persons_per_agent = 4;
    assert!(options.valid_for_bootstrap());
    let mut decoded: SimulationDeser = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    decoded
        .res
        .insert("simoptions".into(), Bincode::encode(&options).unwrap());
    assert!(matches!(
        decoded.into_sim(),
        Err(SaveLoadError::Corrupt { reason, .. }) if reason.contains("must use Realistic")
    ));
}

#[test]
fn population_scale_snapshot_migrates_unambiguous_legacy_origin() {
    init_test_simulation();
    let mut sim = Simulation::new_authored_town(AuthoredTownOptions::default()).unwrap();
    sim.apply_commands([&WorldCommand::SetLotLayout(LotLayout::Varied)]);
    let bytes = Bincode::encode(&sim).unwrap();
    let mut older: SimulationDeser = Bincode::decode(&bytes).unwrap();
    older.res.remove("map_origin").unwrap();
    mark_legacy(&mut older, crate::SnapshotSchema::PreOrigin, crate::PRE_ORIGIN_VERSION);
    let restored = older.into_sim().unwrap();
    assert_eq!(restored.map().lot_layout(), LotLayout::Varied);
    let origin = MapOrigin::new(MapConstructor::AuthoredTown, LotLayout::Uniform);
    assert_eq!(*restored.read::<MapOrigin>(), origin);
    let migrated: Simulation = Bincode::decode(&Bincode::encode(&restored).unwrap()).unwrap();
    assert_eq!(*migrated.read::<MapOrigin>(), origin);
}

#[test]
fn population_scale_snapshot_rejects_ambiguous_legacy_origin() {
    init_test_simulation();
    let mut sim = Simulation::new_authored_town(AuthoredTownOptions {
        starting_money_bucks: SimulationOptions::LEGACY.starting_money_bucks,
        ..AuthoredTownOptions::default()
    })
    .unwrap();
    for layout in [LotLayout::Uniform, LotLayout::Varied] {
        sim.apply_commands([&WorldCommand::SetLotLayout(layout)]);
        let mut older: SimulationDeser = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
        older.res.remove("map_origin").unwrap();
        mark_legacy(&mut older, crate::SnapshotSchema::PreOrigin, crate::PRE_ORIGIN_VERSION);
        assert!(matches!(
            older.into_sim(),
            Err(SaveLoadError::Corrupt { reason, .. }) if reason.contains("Ambiguous legacy snapshot map origin")
        ));
    }
}

#[test]
fn population_scale_snapshot_accepts_compact_authored_flat() {
    init_test_simulation();
    for chunks in [1, 4, NewGameOptions::MIN_TERRAIN_CHUNKS - 1] {
        let sim = Simulation::new_authored_flat(chunks, 42).unwrap();
        let snapshot = Bincode::encode(&sim).unwrap();
        let loaded: Simulation = Bincode::decode(&snapshot).unwrap();
        assert_eq!(loaded.hashes(), sim.hashes());
        assert_eq!(loaded.map().environment().size(), (chunks, chunks));
        assert_eq!(loaded.map().lot_layout(), crate::map::LotLayout::Varied);
        assert_eq!(loaded.population_scale(), PopulationScale::REALISTIC);
    }
}

#[test]
fn population_scale_fixed_agents_preserve_economy_growth_and_history_hashes() {
    let mut baseline = None;
    for n in [1, 8, 16] {
        let mut ctx = TestCtx::new();
        ctx.g = Simulation::new_with_options(SimulationOptions {
            save_replay: false,
            progression: ProgressionMode::Guided,
            ..small_options(n)
        });
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
        let house = ctx.build_house_near(vec2(200.0, 100.0));
        assert!(spawn_human(&mut ctx.g, house).is_some());
        for _ in 0..2 * TICKS_PER_MINUTE {
            ctx.tick_fast();
        }
        let scale = ctx.g.population_scale();
        let stats = ctx.g.read::<CityStats>();
        let sample = stats.history.samples(0).last().unwrap();
        assert!(sample.population > 0);
        assert_eq!(
            scale.persons(sample.population as usize),
            u64::from(sample.population) * u64::from(n)
        );
        let status = progression_status(&ctx.g);
        assert_eq!(status.population, scale.persons(ctx.g.world().humans.len()));
        assert_eq!(status.next.unwrap().1, 100 * u64::from(n));
        assert_eq!(
            can_build(&ctx.g, BuildCapability::BusService)
                .unwrap_err()
                .population,
            100 * u64::from(n)
        );
        drop(stats);

        let mut hashes = ctx.g.hashes();
        assert!(hashes.remove("simoptions").is_some());
        match &baseline {
            Some(baseline) => assert_eq!(baseline, &hashes),
            None => baseline = Some(hashes),
        }
        // An Init submitted after bootstrap cannot change this city's scale.
        ctx.g
            .apply_commands([&WorldCommand::Init(Box::new(small_options(2)))]);
        assert_eq!(ctx.g.population_scale(), scale);
    }
}
