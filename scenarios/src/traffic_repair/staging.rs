//! Stage exact pinned slot members into a private root before use in a scored run.
use super::{verify_fixture, FixtureClass, FixtureFingerprint, OpeningFingerprint};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use simulation::diagnostics::elapsed_tick;
use simulation::{Simulation, SimulationOptions, SlotId, SlotMeta, SlotPaths};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_MEMBER_BYTES: usize = 64 * 1024 * 1024;
const MAX_FIXTURES: usize = 32;
const AT_STAGING: &str = "at-staging";

#[derive(Default)]
pub struct StageOptions<'a> {
    /// Directory containing relative snapshot, metadata, replay and report paths.
    /// Schedule paths always resolve relative to the manifest itself.
    pub bundle_root: Option<&'a Path>,
    /// Required for a pack with a run source marked "at-staging".
    pub run_source_commit: Option<&'a str>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlotFixturePack {
    pub version: u32,
    pub fixtures: Vec<SlotFixture>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlotFixture {
    pub id: String,
    pub slot: String,
    pub class: FixtureClass,
    pub original: SlotFixtureFiles,
    pub fingerprint: FixtureFingerprint,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlotFixtureFiles {
    pub snapshot: PathBuf,
    pub metadata: Option<PathBuf>,
    pub replay: Option<PathBuf>,
    pub opening_report: Option<PathBuf>,
    pub schedule: PathBuf,
}

/// Copy bounded, digest-verified inputs into a fresh output directory.
/// The caller initializes prototypes and provides absolute paths.
pub fn stage_published_slots(
    manifest: &Path,
    requested: FixtureClass,
    output: &Path,
    options: StageOptions<'_>,
) -> Result<Vec<String>, String> {
    let directory = manifest.parent().ok_or("slot manifest has no directory")?;
    let mut pack: SlotFixturePack =
        serde_json::from_slice(&read_member(manifest, MAX_MANIFEST_BYTES)?)
            .map_err(|err| err.to_string())?;
    if pack.version != 1 || pack.fixtures.len() > MAX_FIXTURES {
        return Err("unsupported fixture pack version or size".into());
    }
    pack.fixtures.retain(|row| row.class == requested);
    if pack.fixtures.is_empty() {
        return Err(format!("no {requested:?} fixtures in pack"));
    }

    let run_source = git(&["HEAD"])?;
    if options
        .run_source_commit
        .is_some_and(|expected| expected != run_source)
    {
        return Err("run source commit differs from the scoring checkout".into());
    }
    let mut ids = BTreeSet::new();
    for fixture in &mut pack.fixtures {
        if fixture.fingerprint.run_source_commit == AT_STAGING {
            fixture.fingerprint.run_source_commit = options
                .run_source_commit
                .ok_or("--run-source is required by this fixture pack")?
                .to_owned();
        }
        if fixture.fingerprint.build_profile == AT_STAGING {
            fixture.fingerprint.build_profile = build_profile().into();
        }
        SlotId::new(&fixture.id).map_err(|err| err.to_string())?;
        SlotId::new(&fixture.slot).map_err(|err| err.to_string())?;
        if !ids.insert(fixture.id.clone()) {
            return Err(format!("duplicate fixture ID {}", fixture.id));
        }
        verify_fixture(
            fixture.class,
            requested,
            &fixture.fingerprint,
            &fixture.fingerprint,
        )
        .map_err(|err| format!("{}: {err:?}", fixture.id))?;
    }

    fs::create_dir(output).map_err(|err| format!("{}: {err}", output.display()))?;
    let stage = || {
        let bundle_root = options.bundle_root.unwrap_or(directory);
        for fixture in &mut pack.fixtures {
            fixture.original = stage_slot(directory, bundle_root, output, &run_source, fixture)
                .map_err(|err| format!("{}: {err}", fixture.id))?;
        }
        let bytes = serde_json::to_vec_pretty(&pack).map_err(|err| err.to_string())?;
        fs::write(output.join("manifest.json"), bytes).map_err(|err| err.to_string())?;
        Ok(pack
            .fixtures
            .into_iter()
            .map(|fixture| fixture.id)
            .collect())
    };
    let result = stage();
    if result.is_err() {
        let _ = fs::remove_dir_all(output);
    }
    result
}

fn stage_slot(
    directory: &Path,
    bundle_root: &Path,
    output: &Path,
    run_source: &str,
    fixture: &SlotFixture,
) -> Result<SlotFixtureFiles, String> {
    let input = &fixture.original;
    let pin = &fixture.fingerprint;
    let snapshot = read_member(&bundle_root.join(&input.snapshot), MAX_MEMBER_BYTES)?;
    let optional_member = |path: &Option<PathBuf>, limit| {
        path.as_ref()
            .map(|path| read_member(&bundle_root.join(path), limit))
            .transpose()
    };
    let metadata = optional_member(&input.metadata, MAX_MANIFEST_BYTES)?;
    let replay = optional_member(&input.replay, MAX_MEMBER_BYTES)?;
    let report = optional_member(&input.opening_report, MAX_MANIFEST_BYTES)?;
    let schedule = read_member(&directory.join(&input.schedule), MAX_MANIFEST_BYTES)?;
    let snapshot_sha = digest(&snapshot)?;
    let metadata_sha = digest(metadata.as_deref().unwrap_or_default())?;
    let replay_sha = replay.as_deref().map(digest).transpose()?;
    let schedule_sha = digest(&schedule)?;
    let report_sha = report.as_deref().map(digest).transpose()?;
    if snapshot_sha != pin.snapshot_sha256
        || metadata_sha != pin.metadata_sha256
        || replay_sha != pin.replay_sha256
        || schedule_sha != pin.schedule_sha256
        || report_sha.as_ref() != pin.opening.as_ref().map(|opening| &opening.report_sha256)
    {
        return Err("FingerprintMismatch (input bytes)".into());
    }

    let header =
        Simulation::compressed_snapshot_header(&snapshot).map_err(|err| err.to_string())?;
    let revision = header.revision;
    let mut staged = FixtureFingerprint {
        source_commit: git(&[&format!("{}^{{commit}}", pin.source_commit)])?,
        run_source_commit: run_source.to_owned(),
        prototype_tree: git(&[&format!("{run_source}:prototypes")])?,
        base_mod_tree: git(&[&format!("{run_source}:base_mod")])?,
        asset_tree: git(&[&format!("{run_source}:assets")])?,
        build_profile: build_profile().into(),
        features: Vec::new(),
        snapshot_schema: header.version,
        snapshot_revision: format!("{:032x}", revision.map_or(0, |revision| revision.get())),
        snapshot_sha256: snapshot_sha,
        metadata_sha256: metadata_sha,
        replay_sha256: replay_sha,
        schedule_sha256: schedule_sha,
        // Historical diagnostic blobs are never decoded with the scoring build.
        initial_elapsed_tick: pin.initial_elapsed_tick,
        workers: 1,
        opening: None,
    };
    let opening = report
        .as_deref()
        .zip(report_sha)
        .map(|(bytes, sha)| parse_opening(bytes, sha))
        .transpose()?;
    if let Some(opening) = &opening {
        staged.opening = Some(opening.fingerprint.clone());
    }
    verify_fixture(fixture.class, fixture.class, pin, &staged).map_err(|err| format!("{err:?}"))?;

    // Decode and check only after the bounded files and run context match the pins.
    if fixture.class == FixtureClass::Release {
        if staged.snapshot_schema != include_str!("../../../VERSION").trim() {
            return Err("release snapshot must use the scoring build's save schema".into());
        }
        let sim = Simulation::load_compressed_snapshot(&snapshot, MAX_MEMBER_BYTES)
            .map_err(|err| err.to_string())?;
        let metadata: SlotMeta = serde_json::from_slice(
            metadata
                .as_deref()
                .ok_or("release snapshot has no metadata")?,
        )
        .map_err(|err| err.to_string())?;
        let opening = opening.ok_or("release snapshot has no opening report")?;
        let actual_population = sim.population_scale().persons(sim.world().humans.len());
        if elapsed_tick(&sim).0 != pin.initial_elapsed_tick
            || metadata.game_tick != sim.get_tick()
            || metadata.population != actual_population
            || metadata.persons_per_agent != sim.population_scale().persons_per_agent()
            || opening.fingerprint.population != actual_population
            || opening.manifest["opening_tick"] != sim.get_tick()
            || opening.report["slot"] != fixture.slot
            || opening.report["save_version"] != staged.snapshot_schema
            || opening.report["snapshot_revision"] != staged.snapshot_revision
            || !revision.is_some_and(|revision| revision.matches_sidecar(metadata.revision))
        {
            return Err(
                "loaded snapshot disagrees with its opening, elapsed tick or metadata".into(),
            );
        }
        if fixture.id == "lesce-release" || fixture.slot == "lesce_town" {
            verify_wet_lesce(&opening.report, &sim, &schedule)?;
        }
        if let Some(replay) = &replay {
            #[derive(Deserialize)]
            struct ReplayHeader {
                revision: Option<u128>,
            }
            let replay: ReplayHeader =
                serde_json::from_slice(replay).map_err(|err| err.to_string())?;
            if !revision.is_some_and(|revision| revision.matches_sidecar(replay.revision)) {
                return Err("release replay revision disagrees with the snapshot".into());
            }
        }
    }

    let relative = Path::new(&fixture.id);
    fs::create_dir(output.join(relative)).map_err(|err| err.to_string())?;
    let write = |name: &str, bytes: &[u8]| {
        let relative = relative.join(name);
        fs::write(output.join(&relative), bytes).map_err(|err| err.to_string())?;
        Ok::<_, String>(relative)
    };
    let member = |suffix: &str, bytes: Option<&[u8]>| {
        bytes
            .map(|bytes| write(&format!("{}.{suffix}", fixture.slot), bytes))
            .transpose()
    };
    Ok(SlotFixtureFiles {
        snapshot: write(&format!("{}.zip", fixture.slot), &snapshot)?,
        metadata: member("meta.json", metadata.as_deref())?,
        replay: replay
            .as_deref()
            .map(|bytes| write(&format!("{}_replay.json", fixture.slot), bytes))
            .transpose()?,
        opening_report: member("build.json", report.as_deref())?,
        schedule: write("schedule.json", &schedule)?,
    })
}

struct Opening {
    report: Value,
    manifest: Value,
    fingerprint: OpeningFingerprint,
}

fn parse_opening(bytes: &[u8], report_sha256: String) -> Result<Opening, String> {
    let report: Value = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    let manifest = report["opening_manifest"].clone();
    if !manifest.is_object() {
        return Err("opening manifest is absent".into());
    }
    let population = report["opening"]["opening"]["residents"]
        .as_u64()
        .unwrap_or(0);
    let passed = report["opening"]["failures"]
        .as_array()
        .is_some_and(Vec::is_empty);
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|err| err.to_string())?;
    Ok(Opening {
        report,
        manifest,
        fingerprint: OpeningFingerprint {
            manifest_sha256: digest(&manifest_bytes)?,
            report_sha256,
            population,
            passed,
        },
    })
}

fn verify_wet_lesce(report: &Value, sim: &Simulation, schedule: &[u8]) -> Result<(), String> {
    let schedule: Value = serde_json::from_slice(schedule).map_err(|err| err.to_string())?;
    let town = &report["opening"]["opening"];
    let population = town["residents"].as_u64().unwrap_or(0);
    let employed = town["employed_residents"].as_u64().unwrap_or(0);
    let utilities_ok = town["utilities"]
        .as_array()
        .is_some_and(|nets| !nets.is_empty() && nets.iter().all(|net| net["adequate"] == true));
    if report["variant"] != "wet"
        || report["provenance"]["lesce"]["sha256"] != crate::citymap::LESCE_SHA256
        || report["provenance"]["sava"]["sha256"] != crate::citymap::SAVA_SHA256
        || schedule["opening_tick"] != sim.get_tick()
        || schedule["seed"] != sim.read::<SimulationOptions>().seed
        || sim.map().environment().freshwater_bodies().next().is_none()
        || population < 2500
        || u128::from(employed) * 100 < u128::from(population) * 75
        || !utilities_ok
        || !town["unserved_buildings"]
            .as_array()
            .is_some_and(Vec::is_empty)
        || report["wet_acceptance"]["scored_bridge_crossings"]
            .as_object()
            .is_none_or(|map| map.len() < 2)
        || report["opening"]["freight"]["proven_delivery"]["observed_unload_units"]
            .as_u64()
            .is_none_or(|units| units == 0)
        || ["employment", "neighbourhood"].into_iter().any(|line| {
            !report["opening"]["days"].as_array().is_some_and(|days| {
                !days.is_empty()
                    && days.iter().all(|day| {
                        day["line_boardings"][line]
                            .as_u64()
                            .is_some_and(|count| count > 0)
                            && day["line_loops"][line]
                                .as_u64()
                                .is_some_and(|count| count > 0)
                    })
            })
        })
    {
        return Err(
            "release fixture lacks passing wet Lesce opening activity, facilities or freight"
                .into(),
        );
    }
    Ok(())
}

fn read_member(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        SlotPaths::open_member(path)?
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(std::io::Error::other(format!(
                "member exceeds {max_bytes} bytes"
            )));
        }
        Ok(bytes)
    };
    read().map_err(|err| format!("{}: {err}", path.display()))
}

fn build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn git(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("rev-parse")
        .args(args)
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn digest(bytes: &[u8]) -> Result<String, String> {
    // The headless CITYMAP producer already uses sha256sum for its input pins.
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let written = child.stdin.take().unwrap().write_all(bytes);
    let output = child.wait_with_output().map_err(|err| err.to_string())?;
    written.map_err(|err| err.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .next()
        .filter(|digest| output.status.success() && digest.len() == 64)
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum did not return a digest".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulation::SnapshotRevision;

    #[test]
    fn p7_comparison_staging_preserves_slot_paths_and_rejects_changed_bytes() {
        crate::ScenarioCtx::initialize();
        let root = std::env::temp_dir().join(format!("p7-stage-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(root);
        let base = &root.0;
        let manifest = base.join("manifest.json");
        let output = base.join("staged");

        let sim = Simulation::new_authored_flat(1, 6042).unwrap();
        let revision = SnapshotRevision::new(42);
        let snapshot = sim.encode_slot(revision).unwrap().into_snapshot_bytes();
        let replay = br#"{"revision":42}"#;
        let schedule = br#"{"purpose":"diagnostic"}"#;
        fs::write(base.join("original.zip"), &snapshot).unwrap();
        fs::write(base.join("original.json"), replay).unwrap();
        fs::write(base.join("schedule.json"), schedule).unwrap();
        let head = git(&["HEAD"]).unwrap();
        let pack = SlotFixturePack {
            version: 1,
            fixtures: vec![SlotFixture {
                id: "diagnostic".into(),
                slot: "sample_slot".into(),
                class: FixtureClass::Diagnostic,
                original: SlotFixtureFiles {
                    snapshot: "original.zip".into(),
                    metadata: None,
                    replay: Some("original.json".into()),
                    opening_report: None,
                    schedule: "schedule.json".into(),
                },
                fingerprint: FixtureFingerprint {
                    source_commit: head.clone(),
                    run_source_commit: head,
                    prototype_tree: git(&["HEAD:prototypes"]).unwrap(),
                    base_mod_tree: git(&["HEAD:base_mod"]).unwrap(),
                    asset_tree: git(&["HEAD:assets"]).unwrap(),
                    build_profile: build_profile().into(),
                    features: Vec::new(),
                    snapshot_schema: Simulation::compressed_snapshot_header(&snapshot)
                        .unwrap()
                        .version,
                    snapshot_revision: format!("{:032x}", revision.get()),
                    snapshot_sha256: digest(&snapshot).unwrap(),
                    metadata_sha256: digest(&[]).unwrap(),
                    replay_sha256: Some(digest(replay).unwrap()),
                    schedule_sha256: digest(schedule).unwrap(),
                    initial_elapsed_tick: elapsed_tick(&sim).0,
                    workers: 1,
                    opening: None,
                },
            }],
        };
        fs::write(&manifest, serde_json::to_vec(&pack).unwrap()).unwrap();

        fs::write(base.join("schedule.json"), b"{}").unwrap();
        let changed = stage_published_slots(
            &manifest,
            FixtureClass::Diagnostic,
            &output,
            StageOptions::default(),
        )
        .unwrap_err();
        assert!(changed.contains("FingerprintMismatch"), "{changed}");
        assert!(!output.exists());

        fs::write(base.join("schedule.json"), schedule).unwrap();
        assert_eq!(
            stage_published_slots(
                &manifest,
                FixtureClass::Diagnostic,
                &output,
                StageOptions::default()
            )
            .unwrap(),
            ["diagnostic"],
        );
        let staged = output.join("diagnostic");
        assert_eq!(fs::read(staged.join("sample_slot.zip")).unwrap(), snapshot);
        assert_eq!(
            fs::read(staged.join("sample_slot_replay.json")).unwrap(),
            replay
        );
    }
}
