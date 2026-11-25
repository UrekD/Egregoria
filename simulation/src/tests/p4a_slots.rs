use crate::{NewGameOptions, SaveLoadError, SavePhase, Simulation, SnapshotHeader};
use common::saveload::{
    list_slots_with_revisions, read_slot_meta, slot_paths, AtomicWriteError, Bincode, Encoder,
    SlotId, SnapshotKind, SnapshotRevision,
};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "egregoria-p4a-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn options() -> NewGameOptions {
    drop(super::TestCtx::new());
    NewGameOptions {
        seed: 77,
        terrain_chunks: 16,
        ..NewGameOptions::default()
    }
}

#[test]
fn p4a_slot_round_trip_tracks_revision_and_removes_an_old_replay() {
    let options = options();
    let root = TestRoot::new();
    let id = SlotId::new("Alpine").unwrap();
    let revision = SnapshotRevision::new(u128::MAX);
    let city = Simulation::new_game_with_replay(&options).unwrap();
    let phase = city
        .encode_slot(revision)
        .unwrap()
        .with_display_name("Alpine City".into())
        .publish(&root.0, &id)
        .unwrap();
    assert!(matches!(phase, SavePhase::Complete));
    let paths = slot_paths(&root.0, &id).unwrap();
    assert_eq!(
        Simulation::read_slot_snapshot_header(&paths.zip, SnapshotKind::Zip).unwrap(),
        SnapshotHeader {
            version: crate::VERSION.into(),
            revision: Some(revision),
            registry: Some(prototypes::prototypes().fingerprint()),
        },
    );
    let meta = read_slot_meta(&root.0, &id).unwrap();
    assert_eq!(meta.display_name, "Alpine City");
    assert_eq!(meta.revision, Some(revision.get()));
    assert_eq!(meta.game_tick, city.get_tick());
    assert_eq!(
        meta.money_cents,
        city.read::<crate::economy::Government>().money.cents()
    );
    assert_eq!(meta.population, city.world().humans.len() as u64);
    assert_eq!(meta.persons_per_agent, 1);
    assert_eq!(
        Simulation::load_from_slot(&root.0, &id).unwrap().hashes(),
        city.hashes()
    );
    let replay = Simulation::load_replay_from_slot(&root.0, &id).unwrap();
    assert_eq!(
        Simulation::try_from_replay(replay).unwrap().0.hashes(),
        city.hashes()
    );

    let stale_replay = fs::read(&paths.replay).unwrap();
    let city = Simulation::new_game(&options).unwrap();
    let phase = city
        .save_to_slot(&root.0, &id, SnapshotRevision::new(9))
        .unwrap();
    assert!(matches!(phase, SavePhase::Complete));
    assert!(!paths.replay.exists());
    fs::write(&paths.replay, stale_replay).unwrap();
    assert!(matches!(
        Simulation::load_replay_from_slot(&root.0, &id),
        Err(SaveLoadError::Corrupt { .. })
    ));
    assert_eq!(
        Simulation::load_from_slot(&root.0, &id).unwrap().hashes(),
        city.hashes()
    );
}

#[test]
fn population_scale_slot_round_trip_advertises_eight_before_opening() {
    let options = NewGameOptions {
        persons_per_agent: 8,
        ..options()
    };
    let mut city = Simulation::new_game_with_replay(&options).unwrap();
    city.tick(&mut Simulation::schedule(), &[]);
    let root = TestRoot::new();
    let id = SlotId::new("scaled").unwrap();
    let revision = SnapshotRevision::new(91);
    assert!(matches!(
        city.save_to_slot(&root.0, &id, revision).unwrap(),
        SavePhase::Complete
    ));
    let listed = crate::list_slots_with_revisions(&root.0, |path, kind| {
        Simulation::read_slot_snapshot_header(path, kind)
            .unwrap()
            .revision
    })
    .unwrap();
    let entry = &listed[0];
    let meta = entry.meta.as_ref().unwrap();
    assert_eq!(meta.revision, Some(revision.get()));
    assert_eq!(meta.persons_per_agent, 8);
    assert_eq!(
        meta.population,
        city.population_scale().persons(city.world().humans.len())
    );
    let restored = Simulation::load_listed_snapshot(&root.0, entry).unwrap();
    assert_eq!(restored.population_scale(), city.population_scale());
    assert_eq!(restored.hashes(), city.hashes());

    let replay = Simulation::load_replay_from_slot(&root.0, &id).unwrap();
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert_eq!(replayed.population_scale().persons_per_agent(), 8);
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(replayed.hashes(), city.hashes());
}

#[test]
#[cfg(any(unix, windows))]
fn p4a_precommit_failure_keeps_slot_bytes() {
    let city = Simulation::new_game_with_replay(&options()).unwrap();
    let root = TestRoot::new();
    let id = SlotId::new("alpine").unwrap();
    city.save_to_slot(&root.0, &id, SnapshotRevision::new(1))
        .unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    let members = [&paths.zip, &paths.replay, &paths.meta];
    let before = members.map(|path| fs::read(path).unwrap());

    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::metadata(&root.0).unwrap().permissions();
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o500)).unwrap();
        let blocked = fs::File::create(root.0.join("probe")).is_err();
        if !blocked {
            fs::set_permissions(&root.0, permissions).unwrap();
            return;
        }
        let result = city.save_to_slot(&root.0, &id, SnapshotRevision::new(2));
        fs::set_permissions(&root.0, permissions).unwrap();
        result
    };
    #[cfg(windows)]
    let result = {
        let handle = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::File::options()
                .read(true)
                .share_mode(1)
                .open(&paths.zip)
                .unwrap()
        };
        let result = city.save_to_slot(&root.0, &id, SnapshotRevision::new(2));
        drop(handle);
        result
    };
    assert!(matches!(
        result,
        Err(SaveLoadError::SnapshotWrite(AtomicWriteError::PreCommit(_)))
    ));
    assert_eq!(members.map(|path| fs::read(path).unwrap()), before);
}

#[test]
fn p4a_sidecar_failure_leaves_new_snapshot_loadable_and_old_meta_stale() {
    let mut city = Simulation::new_game_with_replay(&options()).unwrap();
    let root = TestRoot::new();
    let id = SlotId::new("alpine").unwrap();
    city.save_to_slot(&root.0, &id, SnapshotRevision::new(1))
        .unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    let stale_meta = fs::read(&paths.meta).unwrap();
    fs::remove_file(&paths.meta).unwrap();
    fs::create_dir(&paths.meta).unwrap();
    city.tick(&mut Simulation::schedule(), &[]);
    assert!(matches!(
        city.save_to_slot(&root.0, &id, SnapshotRevision::new(2))
            .unwrap(),
        SavePhase::SidecarsFailed {
            replay: None,
            meta: Some(AtomicWriteError::PreCommit(_))
        }
    ));
    assert_eq!(
        Simulation::load_from_slot(&root.0, &id).unwrap().hashes(),
        city.hashes()
    );
    fs::remove_dir(&paths.meta).unwrap();
    fs::write(&paths.meta, stale_meta).unwrap();
    let entries = list_slots_with_revisions(&root.0, |path, kind| {
        Simulation::read_slot_snapshot_header(path, kind)
            .ok()?
            .revision
    })
    .unwrap();
    assert!(entries[0].meta.is_none());
    assert!(entries[0].replay_available);
}

#[test]
fn p4a_failed_slot_load_leaves_the_entire_family_unchanged() {
    let city = Simulation::new_game(&options()).unwrap();
    let root = TestRoot::new();
    let id = SlotId::legacy();
    let paths = slot_paths(&root.0, &id).unwrap();
    fs::write(&paths.bc, Bincode::encode(&city).unwrap()).unwrap();
    assert_eq!(
        Simulation::load_from_slot(&root.0, &id).unwrap().hashes(),
        city.hashes()
    );
    fs::write(&paths.zip, b"broken compressed snapshot").unwrap();
    fs::write(&paths.replay, b"original replay").unwrap();
    fs::write(&paths.meta, b"original metadata").unwrap();
    let members = [&paths.zip, &paths.bc, &paths.replay, &paths.meta];
    let before = members.map(|path| fs::read(path).unwrap());
    assert!(matches!(
        Simulation::load_from_slot(&root.0, &id),
        Err(SaveLoadError::Corrupt { .. })
    ));
    assert_eq!(members.map(|path| fs::read(path).unwrap()), before);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), members.len());
    assert!(matches!(
        Simulation::load_from_slot(&root.0, &SlotId::new("missing").unwrap()),
        Err(SaveLoadError::NotFound)
    ));
    assert!(
        matches!(fs::metadata(root.0.join("missing.zip")), Err(err) if err.kind() == ErrorKind::NotFound)
    );
}

#[test]
fn s5_slot_reports_0_8_7_as_incompatible() {
    let root = TestRoot::new();
    let id = SlotId::legacy();
    let paths = slot_paths(&root.0, &id).unwrap();
    let bytes = Bincode::encode(&crate::SaveHeader {
        version: "0.8.7".into(),
    })
    .unwrap();
    fs::write(&paths.bc, bytes).unwrap();
    assert!(matches!(
        Simulation::load_from_slot(&root.0, &id),
        Err(SaveLoadError::Incompatible { version }) if version == "0.8.7"
    ));
}

#[test]
fn s5_progression_mode_survives_slot_and_replay_round_trips() {
    use crate::economy::progression::{ProgressionMode, ProgressionState};

    let root = TestRoot::new();
    let id = SlotId::new("progression").unwrap();
    let options = options();
    for mode in [ProgressionMode::Guided, ProgressionMode::FreeBuild] {
        let city = Simulation::new_game_with_replay(&NewGameOptions {
            progression: mode,
            ..options
        })
        .unwrap();
        assert_eq!(city.read::<ProgressionState>().mode, mode);
        city.save_to_slot(&root.0, &id, SnapshotRevision::new(mode as u128 + 1))
            .unwrap();

        let snapshot = Simulation::load_from_slot(&root.0, &id).unwrap();
        let replay = Simulation::load_replay_from_slot(&root.0, &id).unwrap();
        let (replayed, _) = Simulation::try_from_replay(replay).unwrap();
        for restored in [snapshot, replayed] {
            assert_eq!(restored.read::<ProgressionState>().mode, mode);
            assert_eq!(restored.hashes(), city.hashes());
        }
    }
}

#[test]
fn s5b_deliver_load_survives_slot_round_trip() {
    use crate::economy::cargo::LoadId;
    use crate::souls::human::{spawn_human, HumanDecisionKind};
    use geom::vec2;

    let root = TestRoot::new();
    let id = SlotId::new("delivery").unwrap();
    let mut test = crate::tests::TestCtx::new();
    super::street(&test);
    let house = test.build_house_near(vec2(150.0, 125.0));
    let destination = test.build_house_near(vec2(250.0, 125.0));
    let driver = spawn_human(&mut test.g, house).unwrap();
    let load = LoadId(42);
    test.g.world_mut_unchecked().humans[driver].decision.kind =
        HumanDecisionKind::MultiStack(vec![HumanDecisionKind::DeliverLoad { destination, load }]);

    test.g
        .save_to_slot(&root.0, &id, SnapshotRevision::new(1))
        .unwrap();
    let mut restored = Simulation::load_from_slot(&root.0, &id).unwrap();
    assert_eq!(restored.hashes(), test.g.hashes());
    let decision = &restored.get(driver).unwrap().decision.kind;
    assert!(matches!(decision, HumanDecisionKind::MultiStack(steps) if matches!(
        steps.as_slice(),
        [HumanDecisionKind::DeliverLoad { destination: saved_destination, load: saved_load }]
            if *saved_destination == destination && *saved_load == load
    )));

    crate::WorldCommand::MapRemoveBuilding(destination).apply(&mut restored);
    assert!(matches!(
        restored.get(driver).unwrap().decision.kind,
        HumanDecisionKind::Yield
    ));
}
