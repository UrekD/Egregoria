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
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if !std::thread::panicking() {
                panic!("could not remove test root: {error}");
            }
        }
    }
}

fn meta(saved_at_unix: u64) -> SlotMeta {
    SlotMeta {
        display_name: "Harbor City".into(),
        game_tick: 123,
        population: 45,
        money_cents: -678,
        saved_at_unix,
        revision: None,
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
        "foo_RePlAy",
        &"a".repeat(33),
    ] {
        assert!(SlotId::new(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(SlotId::new(&"a".repeat(32)).is_ok());
    assert_eq!(SlotId::new("A-z_09").unwrap().as_str(), "a-z_09");
    assert_eq!(SlotId::legacy().as_str(), "world");

    let existing = [
        SlotId::new("Harbor_City").unwrap(),
        SlotId::new("Harbor_City_2").unwrap(),
    ];
    assert_eq!(
        SlotId::from_display_name("  Harbor / City! ", &existing).as_str(),
        "harbor_city_3"
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
fn slot_reserved_stems_are_disambiguated() {
    for name in ["CoN", "PrN", "AUX", "NUL", "COM1", "lPt9"] {
        assert!(SlotId::new(name).is_err(), "accepted {name}");
        assert_eq!(
            SlotId::from_display_name(name, &[]).as_str(),
            format!("{}_2", name.to_ascii_lowercase())
        );
    }
    for name in [
        "Settings",
        "Bindings",
        "lot_brush",
        "netinfo",
        "camera3D",
        "camera",
        "world_replay",
        "AutoSave_0",
        "AutoSave_legacy",
    ] {
        assert!(SlotId::new(name).is_err(), "accepted {name}");
        let id = SlotId::from_display_name(name, &[]);
        assert_eq!(SlotId::new(id.as_str()).unwrap(), id);
    }
    assert!(SlotId::new("COM0").is_ok());
    assert!(SlotId::new("LPT10").is_ok());
}

#[test]
fn slot_case_variants_share_identity_and_keep_their_paths() {
    let root = TestRoot::new();
    let zip = root.file("Harbor.ZIP", b"snapshot", 100);
    let bc = root.file("harbor.bc", b"legacy", 200);
    let replay = root.file("HARBOR_REPLAY.JSON", b"{}", 150);
    let sidecar = root.file("Harbor.meta.Json", &JSON::encode(&meta(160)).unwrap(), 150);
    root.file("Custom.json", b"{}", 100);
    root.file("custom.zip", b"reserved", 100);

    let id = SlotId::new("HARBOR").unwrap();
    let entries = list_slots(&root.0).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id.as_str(), "harbor");
    assert_eq!(entries[0].snapshot, SnapshotKind::Zip);
    assert!(entries[0].replay_available);
    assert_eq!(entries[0].meta, Some(meta(160)));
    assert_eq!(
        entries[0].paths,
        SlotPaths {
            zip,
            bc,
            replay,
            meta: sidecar
        }
    );
    assert_eq!(slot_paths(&root.0, &id).unwrap(), entries[0].paths);
    assert_eq!(read_slot_meta(&root.0, &id).unwrap(), meta(160));
}

#[test]
fn slot_listing_reports_zip_directory_as_obstructed() {
    let root = TestRoot::new();
    root.file("Harbor.bc", b"legacy", 100);
    fs::create_dir(root.0.join("harbor.zip")).unwrap();
    let entries = list_slots(&root.0).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].snapshot, SnapshotKind::Obstructed);
}

#[cfg(unix)]
#[test]
fn slot_discovery_ignores_symlinked_members() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new();
    let outside = TestRoot::new();
    let private = outside.file("private.json", &JSON::encode(&meta(500)).unwrap(), 200);
    root.file("harbor.zip", b"snapshot", 100);
    symlink(&private, root.0.join("harbor.meta.json")).unwrap();
    symlink(&private, root.0.join("harbor_replay.json")).unwrap();
    symlink(&private, root.0.join("linked.bc")).unwrap();
    symlink("missing", root.0.join("dangling.zip")).unwrap();
    symlink("loop.zip", root.0.join("loop.zip")).unwrap();
    root.file("dangling.bc", b"legacy", 100);

    let entries = list_slots(&root.0).unwrap();
    assert_eq!(entries.len(), 3);
    let harbor = entries
        .iter()
        .find(|entry| entry.id.as_str() == "harbor")
        .unwrap();
    assert!(!harbor.replay_available);
    assert_eq!(harbor.meta, None);
    assert!(read_slot_meta(&root.0, &harbor.id).is_err());
    for entry in entries.iter().filter(|entry| entry.id != harbor.id) {
        assert_eq!(entry.snapshot, SnapshotKind::Obstructed);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn slot_discovery_ignores_fifo_sidecars() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> std::ffi::c_int;
    }

    let root = TestRoot::new();
    root.file("harbor.zip", b"snapshot", 100);
    for suffix in [".meta.json", "_replay.json"] {
        let path = root.0.join(format!("harbor{suffix}"));
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // mkfifo receives a live, NUL-terminated path.
        assert_eq!(unsafe { mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    let entries = list_slots(&root.0).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].replay_available);
    assert_eq!(entries[0].meta, None);
    assert!(read_slot_meta(&root.0, &entries[0].id).is_err());
}

#[test]
fn slot_delete_removes_autosave_family_and_case_variants() {
    let root = TestRoot::new();
    let autosave = SlotId::autosave(2);
    assert_eq!(autosave.as_str(), "autosave_2");
    for name in [
        "AutoSave_2.ZIP",
        "autosave_2.bc",
        "AUTOSAVE_2_REPLAY.JSON",
        "AutoSave_2.meta.json",
    ] {
        root.file(name, b"fixture", 100);
    }
    let backup = root.file("autosave_2.zip.backup.bak", b"backup", 100);
    let other = root.file("autosave_20.bc", b"other", 100);
    let canonical = root.0.join("autosave_2.zip");
    let distinct_zip_alias = !canonical.exists();
    if distinct_zip_alias {
        root.file("autosave_2.zip", b"canonical", 200);
    }
    let entries = list_slots(&root.0).unwrap();
    let entry = entries.iter().find(|entry| entry.id == autosave).unwrap();
    if distinct_zip_alias {
        assert_eq!(entry.paths.zip, canonical);
        assert_eq!(entry.modified_unix, 200);
    }
    delete_slot(&root.0, &autosave).unwrap();
    delete_slot(&root.0, &autosave).unwrap();
    let mut files: Vec<_> = fs::read_dir(&root.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files, [backup, other]);
}

#[test]
fn slot_delete_rejects_zip_directory_before_removing_family() {
    let root = TestRoot::new();
    let id = SlotId::new("harbor").unwrap();
    let members = ["harbor.bc", "harbor.meta.json", "harbor_replay.json"]
        .map(|name| root.file(name, b"keep", 100));
    let zip = root.0.join("Harbor.ZIP");
    fs::create_dir(&zip).unwrap();
    let zip_alias =
        (!root.0.join("harbor.zip").exists()).then(|| root.file("harbor.zip", b"snapshot", 200));

    let error = delete_slot(&root.0, &id).unwrap_err();
    for member in members {
        assert_eq!(fs::read(member).unwrap(), b"keep");
    }
    assert_eq!(error.kind(), ErrorKind::IsADirectory);
    assert!(zip.is_dir());
    if let Some(zip_alias) = zip_alias {
        assert_eq!(fs::read(zip_alias).unwrap(), b"snapshot");
    }
}

#[test]
fn slot_revision_binds_sidecars_to_the_selected_snapshot() {
    let root = TestRoot::new();
    let zip = root.file("Harbor.ZIP", b"snapshot", 100);
    let id = SlotId::new("harbor").unwrap();
    let revision = SnapshotRevision::new(u128::MAX);
    let list = |revision| {
        list_slots_with_revisions(&root.0, |path, kind| {
            assert_eq!(path, zip);
            assert_eq!(kind, SnapshotKind::Zip);
            revision
        })
        .unwrap()
        .pop()
        .unwrap()
    };

    root.file("harbor.meta.json", br#"{"display_name":"Harbor City","game_tick":123,"population":45,"money_cents":-678,"saved_at_unix":200}"#, 200);
    assert_eq!(read_slot_meta(&root.0, &id).unwrap().revision, None);
    assert_eq!(list(None).meta, Some(meta(200)));
    assert_eq!(list(Some(revision)).meta, None);

    let sidecar = SlotMeta {
        revision: Some(revision.get()),
        ..meta(200)
    };
    write_slot_meta(&root.0, &id, &sidecar).unwrap();
    root.file("harbor_replay.json", br#"{"revision":1,"replay":[]}"#, 200);
    assert_eq!(list(Some(revision)).meta, Some(sidecar));
    assert!(!list(Some(revision)).replay_available);
    assert_eq!(list(Some(SnapshotRevision::new(1))).meta, None);
    root.file(
        "harbor_replay.json",
        format!(r#"{{"revision":{},"replay":[]}}"#, revision.get()).as_bytes(),
        200,
    );
    assert!(list(Some(revision)).replay_available);
}

#[test]
fn slot_revision_takes_precedence_over_meta_mtime_in_ordering() {
    let root = TestRoot::new();
    let revision = SnapshotRevision::new(42);
    for (id, snapshot_mtime, meta_mtime, saved_at, meta_revision) in [
        ("harbor", 100, 90, 500, 42),
        ("market", 300, 400, 900, 43),
        ("park", 200, 150, 600, 43),
    ] {
        root.file(&format!("{id}.zip"), b"snapshot", snapshot_mtime);
        let sidecar = SlotMeta {
            revision: Some(meta_revision),
            ..meta(saved_at)
        };
        root.file(
            &format!("{id}.meta.json"),
            &JSON::encode(&sidecar).unwrap(),
            meta_mtime,
        );
    }

    let entries = list_slots_with_revisions(&root.0, |_, _| Some(revision)).unwrap();
    let order: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                entry.id.as_str(),
                entry.meta.as_ref().map(|meta| meta.saved_at_unix),
            )
        })
        .collect();
    assert_eq!(
        order,
        [("harbor", Some(500)), ("market", None), ("park", None)]
    );
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
    root.file("Custom.JSON", b"{}", 100);
    let id = SlotId::new("custom").unwrap();
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
    let result = write_staged(&path, Durability::Durable, |file| {
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
                write_staged(path, Durability::Durable, |file| {
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
