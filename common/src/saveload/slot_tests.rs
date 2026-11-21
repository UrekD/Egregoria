use super::*;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

pub(super) struct TestRoot(PathBuf);

impl TestRoot {
    pub(super) fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("egr-slots-{}-{n}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test root: {error}"),
            }
        }
    }

    pub(super) fn file(&self, name: &str, contents: &[u8], modified_unix: u64) -> PathBuf {
        let path = self.0.join(name);
        let mut file = File::create(&path).unwrap();
        file.write_all(contents).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(modified_unix))
            .unwrap();
        path
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn meta(saved_at_unix: u64) -> SlotMeta {
    SlotMeta {
        display_name: "Harbor City".into(),
        game_tick: 123,
        population: 45,
        money_cents: -678,
        saved_at_unix,
    }
}

#[test]
fn slot_id_validation_and_uniquification() {
    for invalid in [
        "",
        "..",
        "../world",
        "nested/world",
        r"nested\world",
        "two words",
        "été",
        "world.tmp",
        "world.bak",
        "foo_replay",
        &"a".repeat(33),
    ] {
        assert!(SlotId::new(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(SlotId::new(&"a".repeat(32)).is_ok());
    assert_eq!(SlotId::new("A-z_09").unwrap().as_str(), "A-z_09");
    assert_eq!(SlotId::legacy().as_str(), "world");

    let existing = [
        SlotId::new("Harbor_City").unwrap(),
        SlotId::new("Harbor_City_2").unwrap(),
    ];
    assert_eq!(
        SlotId::from_display_name("  Harbor / City! ", &existing).as_str(),
        "Harbor_City_3"
    );
    let long = "a".repeat(50);
    let full_length = SlotId::from_display_name(&long, &[]);
    let uniquified = SlotId::from_display_name(&long, &[full_length]);
    assert_eq!(uniquified.as_str(), format!("{}_2", "a".repeat(30)));
    for display_name in ["", "🌆", "foo_replay"] {
        let id = SlotId::from_display_name(display_name, &[]);
        assert_eq!(SlotId::new(id.as_str()).unwrap(), id);
    }
}

#[test]
fn slot_listing_groups_exact_families_and_falls_back_to_snapshot_time() {
    let root = TestRoot::new();
    for (name, modified) in [
        ("world.bc", 900),
        ("world.zip", 500),
        ("world_replay.json", 510),
        ("alpha.bc", 300),
        ("alpha_replay.zip", 900),
        ("bravo.zip", 300),
        ("harbor.zip", 100),
        ("harbor.bc", 800),
        ("stale.zip", 400),
        ("corrupt.zip", 200),
        ("corrupt.meta.json", 750),
        ("world.zip.0.6.1.bak", 900),
        ("world.bc.bak", 900),
        ("world.backup.zip", 900),
        ("world.zip.42.0.tmp", 900),
        ("world.tmp.zip", 900),
        ("orphan_replay.json", 900),
        ("orphan_replay.zip", 900),
        ("compressed_replay.zip", 900),
        ("settings.json", 900),
        ("settings.zip", 900),
        ("camera3D.json", 900),
        ("camera3D.bc", 900),
        ("x.zip.txt", 900),
    ] {
        root.file(name, b"fixture", modified);
    }
    fs::create_dir(root.0.join("directory.zip")).unwrap();
    root.file("harbor.meta.json", &JSON::encode(&meta(600)).unwrap(), 150);
    let stale = root.file("stale.meta.json", &JSON::encode(&meta(999)).unwrap(), 400);
    File::options()
        .write(true)
        .open(stale)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(399_999))
        .unwrap();

    let entries = list_slots(&root.0).unwrap();
    let summary: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                entry.id.as_str(),
                entry.snapshot,
                entry.replay_available,
                entry.modified_unix,
                entry.meta.as_ref().map(|meta| meta.saved_at_unix),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("harbor", SnapshotKind::Zip, false, 100, Some(600)),
            ("world", SnapshotKind::Zip, true, 500, None),
            ("stale", SnapshotKind::Zip, false, 400, None),
            ("alpha", SnapshotKind::Bc, false, 300, None),
            ("bravo", SnapshotKind::Zip, false, 300, None),
            ("corrupt", SnapshotKind::Zip, false, 200, None),
        ]
    );
    assert_eq!(entries[1].id, SlotId::legacy());
    assert!(list_slots(&root.0.join("missing")).unwrap().is_empty());
}

#[test]
fn slot_paths_respect_configuration_stems_and_metadata_round_trips() {
    let root = TestRoot::new();
    let id = SlotId::new("harbor").unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    assert_eq!(paths.zip, root.0.join("harbor.zip"));
    assert_eq!(paths.bc, root.0.join("harbor.bc"));
    assert_eq!(paths.replay, root.0.join("harbor_replay.json"));
    assert_eq!(paths.meta, root.0.join("harbor.meta.json"));
    assert_eq!(
        read_slot_meta(&root.0, &id).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    write_slot_meta(&root.0, &id, &meta(200)).unwrap();
    assert_eq!(read_slot_meta(&root.0, &id).unwrap(), meta(200));
    root.file("settings.json", b"{}", 100);
    let id = SlotId::new("settings").unwrap();
    assert_eq!(
        slot_paths(&root.0, &id).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert!(matches!(
        write_slot_meta(&root.0, &id, &meta(200)),
        Err(AtomicWriteError::PreCommit(_))
    ));
}

#[test]
fn slot_atomic_write_keeps_old_bytes_on_staging_failure() {
    let root = TestRoot::new();
    let path = root.file("world.zip", b"old", 100);
    let result = write_staged(&path, |file| {
        file.write_all(b"partial")?;
        Err(io::Error::other("injected write failure"))
    });
    let Err(AtomicWriteError::PreCommit(error)) = result else {
        panic!("unexpected result: {result:?}");
    };
    assert_eq!(error.to_string(), "injected write failure");
    assert_eq!(fs::read(&path).unwrap(), b"old");
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);

    write_atomic(&path, b"replacement").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

#[test]
fn slot_atomic_writers_stage_separately() {
    let root = TestRoot::new();
    let path = root.file("world.zip", b"old", 100);
    let first = b"first writer".as_slice();
    let second = b"second writer".as_slice();
    std::thread::scope(|scope| {
        let (ready, staged) = mpsc::channel();
        let workers = [first, second].map(|bytes| {
            let (release, wait) = mpsc::channel();
            let ready = ready.clone();
            let path = &path;
            let worker = scope.spawn(move || {
                write_staged(path, |file| {
                    file.write_all(bytes)?;
                    ready.send(()).map_err(io::Error::other)?;
                    wait.recv().map_err(io::Error::other)
                })
            });
            (release, worker)
        });
        drop(ready);
        for _ in &workers {
            staged.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let files: Vec<_> = fs::read_dir(&root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert_eq!(files.len(), 2);
        for name in files {
            assert!(name.starts_with(&format!("world.zip.{}.", std::process::id())));
        }
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let [(release_first, first_writer), (release_second, second_writer)] = workers;
        release_first.send(()).unwrap();
        release_second.send(()).unwrap();
        first_writer.join().unwrap().unwrap();
        second_writer.join().unwrap().unwrap();
    });
    let bytes = fs::read(path).unwrap();
    assert!(bytes == first || bytes == second);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}
