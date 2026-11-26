use crate::{
    NewGameOptions, RegistrySlotSnapshotHeader, Replay, SaveLoadError, SavePhase, Simulation,
    SlotSnapshotHeader, SnapshotHeader, REGISTRY_SLOT_MAGIC, VERSION,
};
use common::saveload::{
    list_slots_with_revisions, read_slot_meta, slot_paths, AtomicWriteError, Bincode,
    CompressedBincode, Encoder, SlotId, SnapshotKind, SnapshotRevision,
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

fn l8_golden_city() -> Simulation {
    use crate::map::{LotLayout, ZoneKind};
    use crate::world_command::WorldCommand;
    use crate::{MapConstructor, MapOrigin, SimulationOptions, INITIAL_TICK};
    use prototypes::Tick;

    drop(super::TestCtx::new());
    let mut city = Simulation::with_initial_resources();
    let options = SimulationOptions {
        terrain_size: 0,
        save_replay: true,
        ..SimulationOptions::LEGACY
    };
    let origin = MapOrigin::new(MapConstructor::NewGame, LotLayout::Varied);
    WorldCommand::bootstrap(&mut city, options, origin).unwrap();
    city.write::<Replay>()
        .record_constructor_end(Tick(INITIAL_TICK));
    city.apply_commands([&WorldCommand::SetTaxRate {
        zone: ZoneKind::Residential,
        rate_permille: 120,
    }]);
    city
}

#[test]
fn p4a_detached_snapshot_matches_p6_golden_bytes() {
    // Produced by the synchronous slot encoder at 4ad53c84 with revision 0x12345678.
    const ZIP_HEX: &str = concat!(
        "7801eddd6f6c1be51d07f0e712254dd2867551a846b755e586f6af21c4699ab651cbdab43414216d2b644344e174393f",
        "710efbeeccdde3246e1ac15a6d6da795408b60b4afbaf282f505ca20a82f9068b54da3a29a4063d3a40cedc598b655a0",
        "366820b6d937f6bbb3338c5816225d1c9fef6b3567fbb9e79ee7f7fbfc6c4b6d15ff3ebca3efc01d7ddf3cb07f37aba9",
        "ef688fc5da63f9f1ef74b5b8939fbf87775dd9d3511c942446b7ff7f64ecabf543ba396c39f585892d69db1296c8a6b9",
        "62f384ee083bdbf8df856f4a7333ae9b0945b34c3a93d1846e998a96118ec41a47d4745a37b9438f57a70dd55413dce0",
        "a69058ada1a67b0aab2f70a4e1564607c68a4756b8f9093470cd72842a9cfcaf7b24b6de7df4f6b78f1d6ed9e09f2b4c",
        "c3110210889a80fb5d69d7916f3cfdd355514b1cf94200021f09b827ce767df14f5fb71a3e1ac2230840206a02eefe3f",
        "ecd8377be3892f442d71e41b2501e4ba9880fb83eb371dfacb2b1df26213711e0210a85e0157eb4d3e74f4e4bb9baa37",
        "45640601082c26e08e4fdf75ecadefd56e596c22ce430002d52be09eefdd79d7c5edabbaab37456456dd02c82e0801f7",
        "f44f2e7dff4b3f73ef0d6231ac0101088453c0fde7df7a1f7b45be3010cef0113504201084809bb159ecf7fbfe3e12c4",
        "625803021008a7805b7bf7ece34f75ef7d309ce123eaea164076e512709f3bd339d33fd7902cd786d8070210a83c01f7",
        "ea8b33978f9fee1fafbcd010110420502e0177eb030373fcdfbf385cae0db10f04205079026eddb5c14367ef3c71bcf2",
        "424344d52d80ec2a49c05df74efb23b75e383255494121160840a0bc02eef4dea9fb773dd97aaabcdb62370840a09204",
        "dc679f619726952be72a2928c40201089457c0fdd5efbeac6e383af9cbf26e8bddaa5b00d9854dc07d79cf7563e3f4c5",
        "57c31638e285000482137077641e3e3ff844df6bc12d8995200081b009b837bff4fef617deb865366c81235e08402038",
        "01f73ee3daa9f6becb6f05b72456aa6e0164578d026efdd752bdb7cdfde6afd5981c728200043e9d803bf6de94bdb321",
        "f9c1a79b8e59108040b50aa0dd51b5561679416029026877b414ad6a9e8bdc222d807647912e3f9287404100ed8e0a0e",
        "384220d202687714e9f22379081404d0eea8e050dd476407814504d0ee6811209c86401404d0ee280a55468e10584400",
        "ed8e1601c26908444100ed8ec25e65c40f810004d0ee2800442c0181b00ba0dd51d82b88f8211080808b764701286209",
        "08845c00ed8e2ab980880d02651270d1eea84cd2d80602152c807647155c1c8406817209a0dd51b9a4b10f042a5800ed",
        "8e56aa38d81702152480764715540c840281951240bba39592c7be10a82001b43baaa062201408ac9400da1d2d873cd6",
        "8440c804d0ee28640543b810580e01b43b5a0e55ac09819009a0dd51c80a867021b01c026877b45455cc8740950aa0dd",
        "5195161669416049026877b4242e4c8640750aa0dd5175d6155941604902687754e4c21d04a22c80764751ae3e728740",
        "5100ed8e8a10b883409405d0ee28cad547ee10280a44a2dd513157dc4100020b08a0ddd102301886409404d0ee284ad5",
        "46ae10584000ed8e1680c13004a22410fa7647512a167285c07209a0ddd172c9625d08844800ed8e42542c840a81e512",
        "40bba3e592c5ba1008914045b73b0a9123428540a805d0ee28d4e543f010084600ed8e8271c42a1008b500da1d85ba7c",
        "081e02c108ac58bba360c2c72a10804010026877148422d68040c805d0ee28e40544f810084200ed8e8250c41a1008b9",
        "c0b2b43b0ab909c28740e404d0ee28722547c210f8a400da1d7dd204231088a4408bb055dd546cee707b5415ba653a35",
        "8cadb155339eb6ad513dceedd6fca6dd7f1ccafff6cc9b33f94317acb9fcd51d35ff6a4ca80657846ef016e969ba356e",
        "ccefcab39f4bac662d63f5364fa7d46cee4d69421ee5b643abca3d72477b2cd61e93db646eaa43291e977b849de16db2",
        "cd13ba23ecacdcb3b96bebd66d5ddd5bbb37c7b66cdbb6795bac2bd6266b966150348edc3330106b9b90f79bba907b26",
        "64c16d3f74473fc8e59e8e36d951473925e2ed3cbf74dacb8b66cbdfb22d8dc733b69aa2fd1dee6dee5d21545be86642",
        "312c936795a18c96a46d625b3ae8d62653fe09babe18fd3e9bf3de8c9e8ad302298a87ae89d3be72bf497384ad6b8216",
        "6d93fd5336d7f4f4ff3899e6b643c00add2b6a829b14596c7272b06dc0cbeb1e2eee55c70fa8822e9c900f5244647680",
        "3b540253e8aa17b84d27bd8b0d3d95a259b1ce0eba7ad0dbd411540b2d49e96b961da7487a48ceab4e4a3769e604299a",
        "649cd184652bdc247b3aef089ea67c09624815da08a7c70313b237ea7bfad33ac9d59ba638593a61d0946135e5f0c9c1",
        "c9c9667ae9988e2e1447a8c26966ccfdb07863ac6e5827965ac658bda1da492e72cf4bebdd476f7ffbd8e1960d120de7",
        "7f7c94ee3ede62a7b4cf467e7d9dc4a4d22fdccfbd2e4ba5dfbb9d6fdb27d19e5a6ff2a1a327dfdd446be67ef86d56fa",
        "3d9cb975e758e9d7f1e5ced74aa5dfca953bfc6756fae53cb97f9c64a5dfd1e15f5f7bf7ece34f75ef7d90d6cf3fa678",
        "3b3e77a673a67fae21e98d1462b8fae2cce5e3a7fbc7692437fa3956fa3b3cfe1a75d7060f9dbdf3c471efbc17e3ba77",
        "da1fb9f5c291297a5ecc627aefd4fdbb9e6c3de58ddcf7a31acaebd967d8a549e5ca391ac9adfd0a2bfdaffebc795b2d",
        "cd7879cf7563e3f4c557bd195ea63b320f9f1f7ca2ef35ef792b657ef34bef6f7fe18d5b66bde7e66ef6b17f08904aff",
        "36e0bb8dbd3765ef6c487ee0cd1ea534e99e35095b4f17cadbe03df77e5a33424fe922ab8cd18bd1561cd548a738057c",
        "635cd55359856b96691959c5e074ade6486cf5417a252b71eebd87695a53c2a24f04d3a0177f5d9e3df0faf39f353229",
        "a17b6f5a6efb5b7189356ab481ffaaeaf4b65cda4f93a6da094b19b2ace41affcaf964529650526ad6ca088935196a5a",
        "b16c3da19bb54c62ab4bdeebf4baa3cb6e28ac323f2eb1066e8e2a095b8fd77a67fd977edab2853fd4709949fe2e4d8e",
        "6e5869a1d39bbc99d1487e6e7f8d772f35d1bbc17b236bdcf1ae5f338f389cb2c66a186bf491e657ff8c4d9f2f96a18c",
        "f2115d4b71ef037995bfa12e5a0bfb148e1488ffa7d9b6d4b8326c5ba6a04f1589356b9e313df483a3e59be2ba93f6df",
        "e2b6c4ea8732f10417abe852898db31bfc4f2c6bcca4cfa6113d5d23b1b53cc535af7a54046538658d49ec3f197fbd63",
    );
    const REPLAY: &[u8] = br#"{
  "revision": 305419896,
  "version": "0.11.1",
  "enabled": true,
  "registry": 3477846763158838141,
  "commands": [
    [
      1,
      {
        "Init": {
          "terrain_size": 0,
          "save_replay": true,
          "preset": "Procedural",
          "seed": 0,
          "starting_money_bucks": 150000,
          "progression": "FreeBuild",
          "land_mode": "Unrestricted",
          "land_recipe": "Unrestricted",
          "persons_per_agent": 1
        }
      }
    ],
    [
      1,
      {
        "SetTaxRate": {
          "zone": "Residential",
          "rate_permille": 120
        }
      }
    ]
  ],
  "last_tick_recorded": 1,
  "timeline": {
    "constructor_end": 1,
    "steps": 0,
    "batches": [
      {
        "step": 0,
        "end": 2,
        "steps_systems": false
      }
    ]
  }
}"#;
    let zip: Vec<_> = ZIP_HEX
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();

    let root = TestRoot::new();
    let id = SlotId::new("golden").unwrap();
    let revision = SnapshotRevision::new(0x1234_5678);
    let city = l8_golden_city();
    assert!(matches!(
        city.save_to_slot(&root.0, &id, revision).unwrap(),
        SavePhase::Complete
    ));
    let paths = slot_paths(&root.0, &id).unwrap();
    assert_eq!(fs::read(&paths.zip).unwrap(), zip);
    assert_eq!(fs::read(&paths.replay).unwrap(), REPLAY);
}

#[test]
fn p4a_rejects_world_and_resources_at_capture_budget() {
    let sim = super::TestCtx::new().g;
    let revision = SnapshotRevision::new(9);
    for limit in [1, sim.capture_slot(revision).unwrap().prefix.len()] {
        assert!(matches!(
            sim.capture_slot_with_limit(revision, limit),
            Err(SaveLoadError::SnapshotTooLarge { limit: actual }) if actual == limit
        ));
    }
}

#[test]
fn p4a_production_snapshot_accepts_exact_inflate_limit() {
    const PADDING: &str = "l8_test_padding";
    let city = super::TestCtx::new().g;
    let revision = SnapshotRevision::new(10);
    let base = city.encode_slot(revision).unwrap();
    let base_len = CompressedBincode::decompress(&base.snapshot).unwrap().len();
    let limit = crate::MAX_SNAPSHOT_BYTES;
    let root = TestRoot::new();
    let id = SlotId::new("exact-limit").unwrap();
    let mut capture = city.capture_slot(revision).unwrap();
    let count = capture.resources.len();
    let extra_framing = Bincode::encoded_size(&(PADDING, limit)).unwrap()
        + Bincode::encoded_size(&(count + 1)).unwrap()
        - Bincode::encoded_size(&count).unwrap();
    let padding_len = limit - base_len - extra_framing;
    assert_eq!(
        Bincode::encoded_size(&(PADDING, padding_len)).unwrap(),
        Bincode::encoded_size(&(PADDING, limit)).unwrap()
    );

    capture
        .resources
        .insert(PADDING.into(), vec![0; padding_len]);
    let exact = capture.encode().unwrap();
    assert_eq!(
        CompressedBincode::decompress_limited(&exact.snapshot, limit)
            .unwrap()
            .len(),
        limit,
    );
    exact.publish(&root.0, &id).unwrap();
    assert_eq!(
        Simulation::load_from_slot(&root.0, &id).unwrap().hashes(),
        city.hashes()
    );
    let paths = slot_paths(&root.0, &id).unwrap();
    let previous = fs::read(&paths.zip).unwrap();
    let mut capture = city.capture_slot(revision).unwrap();
    capture
        .resources
        .insert(PADDING.into(), vec![0; padding_len + 1]);
    assert!(matches!(
        capture.encode().and_then(|encoded| encoded.publish(&root.0, &id)),
        Err(SaveLoadError::SnapshotTooLarge { limit: actual }) if actual == limit
    ));
    assert_eq!(fs::read(&paths.zip).unwrap(), previous);
}

#[test]
fn p4a_replay_is_staged_before_publication_gate() {
    use crate::utils::replay::ReplayJson;

    let root = TestRoot::new();
    let id = SlotId::new("staging").unwrap();
    let revision = SnapshotRevision::new(11);
    let city = Simulation::new_game_with_replay(&options()).unwrap();
    let pretty = ReplayJson::encode_sidecar(&city.read::<Replay>(), revision).unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    let result = city
        .encode_slot(revision)
        .unwrap()
        .publish_with_gate(&root.0, &id, || {
            let files: Vec<_> = fs::read_dir(&root.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].extension().unwrap(), "tmp");
            assert_eq!(fs::read(&files[0]).unwrap(), pretty);
            assert!(!paths.zip.exists());
            assert!(!paths.replay.exists());
            None::<()>
        });
    assert!(result.is_none());
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
}

#[test]
fn p4a_detached_capture_matches_legacy_bytes_after_the_city_advances() {
    use crate::map::ZoneKind;
    use crate::utils::replay::ReplayJson;
    use crate::WorldCommand;

    let root = TestRoot::new();
    let id = SlotId::new("detached").unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    let options = NewGameOptions {
        persons_per_agent: 8,
        ..options()
    };
    let mut city = Simulation::new_game_with_replay(&options).unwrap();
    let mut schedule = Simulation::schedule();
    for (index, enabled) in [true, false].into_iter().enumerate() {
        let tax_edit = |rate_permille| WorldCommand::SetTaxRate {
            zone: ZoneKind::Residential,
            rate_permille,
        };
        city.tick(&mut schedule, [&tax_edit(120)]);
        let revision = SnapshotRevision::new(u128::MAX - index as u128);
        city.write::<Replay>().enabled = enabled;
        assert!(city.read::<Replay>().command_count() > 1);
        let registry = prototypes::prototypes().fingerprint();
        let legacy = CompressedBincode::encode(&(
            RegistrySlotSnapshotHeader {
                slot: SlotSnapshotHeader {
                    magic: REGISTRY_SLOT_MAGIC,
                    version: VERSION.into(),
                    revision,
                },
                registry,
            },
            &city,
        ))
        .unwrap();
        let legacy_replay =
            enabled.then(|| ReplayJson::encode_sidecar(&city.read::<Replay>(), revision).unwrap());
        let captured_tick = city.get_tick();
        let captured_money = city.read::<crate::economy::Government>().money.cents();
        let capture = city.capture_slot(revision).unwrap();
        let legacy_plain = CompressedBincode::decompress(&legacy).unwrap();
        let inflated_len = legacy_plain.len();
        let legacy_hashes = Simulation::decode_snapshot(&legacy_plain).unwrap().hashes();
        if index == 1 {
            let members = [&paths.zip, &paths.replay, &paths.meta];
            let before = members.map(|path| fs::read(path).unwrap());
            let limit = inflated_len - 1;
            assert!(matches!(
                city.capture_slot(revision)
                    .unwrap()
                    .encode_with_limit(limit)
                    .and_then(|encoded| encoded.publish(&root.0, &id)),
                Err(SaveLoadError::SnapshotTooLarge { limit: actual }) if actual == limit
            ));
            assert_eq!(members.map(|path| fs::read(path).unwrap()), before);
        }
        city.tick(&mut schedule, [&tax_edit(130)]);
        assert_ne!(captured_tick, city.get_tick());

        let phase = std::thread::scope(|worker| {
            worker
                .spawn(|| {
                    capture
                        .encode_with_limit(inflated_len)
                        .unwrap()
                        .publish(&root.0, &id)
                        .unwrap()
                })
                .join()
                .unwrap()
        });
        assert!(matches!(phase, SavePhase::Complete));
        assert_eq!(fs::read(&paths.zip).unwrap(), legacy);
        if let Some(replay) = legacy_replay {
            assert_eq!(fs::read(&paths.replay).unwrap(), replay);
        } else {
            assert!(!paths.replay.exists());
        }
        let meta = read_slot_meta(&root.0, &id).unwrap();
        assert_eq!(meta.game_tick, captured_tick);
        assert_eq!(meta.money_cents, captured_money);
        assert_eq!(meta.revision, Some(revision.get()));
        assert_eq!(meta.persons_per_agent, 8);
        assert_eq!(
            Simulation::load_from_slot(&root.0, &id).unwrap().hashes(),
            legacy_hashes
        );
    }
}

#[test]
fn p7_snapshot_loads_p6_slot_with_disabled_replay() {
    if !crate::tests::in_prototype_fixture_process(
        module_path!(),
        "p7_snapshot_loads_p6_slot_with_disabled_replay",
    ) {
        return;
    }
    crate::tests::load_unprofiled_prototypes();
    let root = TestRoot::new();
    let id = SlotId::new("floor").unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    // Published by P6 (c0041a8d) from new_authored_flat(2, 6042), revision 1.
    fs::write(
        &paths.zip,
        include_bytes!("fixtures/p6-disabled-replay.zip"),
    )
    .unwrap();

    let city = Simulation::load_from_slot(&root.0, &id).unwrap();
    assert_eq!(city.get_tick(), 1);
    let replay = city.read::<crate::Replay>();
    assert!(!replay.enabled);
    assert!(!replay.obsolete_recording_discarded());
    assert_eq!(replay.command_count(), 0);
    assert_eq!(replay.version, crate::utils::replay::replay_version());
    assert!(!paths.replay.exists());
}

#[test]
fn p7_snapshot_loads_p6_slot_with_recorded_replay() {
    if !crate::tests::in_prototype_fixture_process(
        module_path!(),
        "p7_snapshot_loads_p6_slot_with_recorded_replay",
    ) {
        return;
    }
    crate::tests::load_unprofiled_prototypes();
    let root = TestRoot::new();
    let id = SlotId::new("floor").unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    // Published by P6 (c0041a8d) with recording enabled, Flat seed 6042,
    // 16 terrain chunks, and snapshot revision 1.
    fs::write(
        &paths.zip,
        include_bytes!("fixtures/p6-recorded-replay.zip"),
    )
    .unwrap();
    fs::write(
        &paths.replay,
        include_bytes!("fixtures/p6-recorded-replay.json"),
    )
    .unwrap();

    assert!(matches!(
        Simulation::load_replay_from_slot(&root.0, &id),
        Err(crate::SaveLoadError::IncompatibleReplay { version }) if version == "0.11.0"
    ));
    let city = Simulation::load_from_slot(&root.0, &id).unwrap();
    assert_eq!(city.get_tick(), 1);
    let replay = city.read::<crate::Replay>();
    assert!(!replay.enabled);
    assert!(replay.obsolete_recording_discarded());
    assert_eq!(replay.command_count(), 0);
    drop(replay);
    city.save_to_slot(&root.0, &id, SnapshotRevision::new(2))
        .unwrap();
    let reopened = Simulation::load_from_slot(&root.0, &id).unwrap();
    assert_eq!(reopened.hashes(), city.hashes());
    assert!(!reopened
        .read::<crate::Replay>()
        .obsolete_recording_discarded());
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
fn population_scale_snapshot_reloads_compact_authored_town_after_lot_layout_change() {
    use crate::map::{AuthoredTownOptions, LotLayout};
    use crate::WorldCommand;

    crate::tests::init_test_simulation();
    let mut city = Simulation::new_authored_town(AuthoredTownOptions::default()).unwrap();
    assert_eq!(city.map().lot_layout(), LotLayout::Uniform);
    city.apply_commands([&WorldCommand::SetLotLayout(LotLayout::Varied)]);
    assert_eq!(city.map().lot_layout(), LotLayout::Varied);

    let root = TestRoot::new();
    let id = SlotId::new("authored-town").unwrap();
    assert!(matches!(
        city.save_to_slot(&root.0, &id, SnapshotRevision::new(91)).unwrap(),
        SavePhase::Complete
    ));
    let restored = Simulation::load_from_slot(&root.0, &id).unwrap();
    assert_eq!(restored.map().lot_layout(), LotLayout::Varied);
    assert_eq!(
        *restored.read::<crate::MapOrigin>(),
        crate::MapOrigin::new(crate::MapConstructor::AuthoredTown, LotLayout::Uniform)
    );
    assert_eq!(restored.hashes(), city.hashes());
}

#[test]
fn population_scale_populated_slot_reports_represented_population() {
    let mut ctx = super::TestCtx::new();
    ctx.g = Simulation::new_with_options(crate::SimulationOptions {
        terrain_size: 1,
        save_replay: false,
        persons_per_agent: 8,
        ..crate::SimulationOptions::LEGACY
    });
    ctx.build_roads(&[geom::vec3(100.0, 100.0, 0.0), geom::vec3(300.0, 100.0, 0.0)]);
    let house = ctx.build_house_near(geom::vec2(200.0, 100.0));
    assert!(crate::souls::human::spawn_human(&mut ctx.g, house).is_some());

    let root = TestRoot::new();
    let id = SlotId::new("populated").unwrap();
    assert!(matches!(
        ctx.g
            .save_to_slot(&root.0, &id, SnapshotRevision::new(92))
            .unwrap(),
        SavePhase::Complete
    ));
    let listed = list_slots_with_revisions(&root.0, |path, kind| {
        Simulation::read_slot_snapshot_header(path, kind)
            .unwrap()
            .revision
    })
    .unwrap();
    let meta = listed[0].meta.as_ref().unwrap();
    assert_eq!((meta.persons_per_agent, meta.population), (8, 8));
    assert_eq!(
        Simulation::load_listed_snapshot(&root.0, &listed[0])
            .unwrap()
            .world()
            .humans
            .len(),
        1
    );
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
