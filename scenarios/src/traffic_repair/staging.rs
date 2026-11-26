//! Stage exact pinned slot members into a private root before use in a scored run.
use super::{verify_fixture, FixtureClass, FixtureFingerprint, OpeningFingerprint};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use simulation::diagnostics::elapsed_tick;
use simulation::{slot_paths, Simulation, SimulationOptions, SlotId, SlotMeta, SlotPaths};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
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
    /// Optionally require the scorer to be built at this commit.
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

    let run_source = run_revision()?;
    if options
        .run_source_commit
        .is_some_and(|expected| expected != run_source)
    {
        return Err("run source commit differs from the scoring executable".into());
    }
    let mut ids = BTreeSet::new();
    for fixture in &mut pack.fixtures {
        if fixture.fingerprint.run_source_commit == AT_STAGING {
            fixture.fingerprint.run_source_commit = run_source.to_owned();
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
    verify_runtime_data(&std::env::current_dir().map_err(|error| error.to_string())?)?;

    fs::create_dir(output).map_err(|err| format!("{}: {err}", output.display()))?;
    let stage = || {
        let bundle_root = options.bundle_root.unwrap_or(directory);
        for fixture in &mut pack.fixtures {
            fixture.original = stage_slot(directory, bundle_root, output, fixture)
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
    fixture: &SlotFixture,
) -> Result<SlotFixtureFiles, String> {
    let SlotInputs { snapshot, metadata, replay, report, schedule } =
        verify_slot(directory, bundle_root, fixture)?;
    let relative = Path::new(&fixture.id);
    fs::create_dir(output.join(relative)).map_err(|err| err.to_string())?;
    let slot = SlotId::new(&fixture.slot).map_err(|err| err.to_string())?;
    let paths = slot_paths(&output.join(relative), &slot).map_err(|err| err.to_string())?;
    let write = |path: PathBuf, bytes: &[u8]| {
        fs::write(&path, bytes).map_err(|err| err.to_string())?;
        path.strip_prefix(output).map(Path::to_path_buf).map_err(|err| err.to_string())
    };
    let member = |path: PathBuf, bytes: Option<&[u8]>| {
        bytes.map(|bytes| write(path, bytes)).transpose()
    };
    Ok(SlotFixtureFiles {
        snapshot: write(paths.zip, &snapshot)?,
        metadata: member(paths.meta, metadata.as_deref())?,
        replay: member(paths.replay, replay.as_deref())?,
        opening_report: member(output.join(relative).join(format!("{slot}.build.json")), report.as_deref())?,
        schedule: write(output.join(relative).join("schedule.json"), &schedule)?,
    })
}

pub(super) struct SlotInputs {
    pub snapshot: Vec<u8>,
    metadata: Option<Vec<u8>>,
    replay: Option<Vec<u8>>,
    report: Option<Vec<u8>>,
    pub schedule: Vec<u8>,
}

pub(super) fn load_staged_slot(root: &Path, id: &str) -> Result<(SlotFixture, SlotInputs), String> {
    verify_runtime_data(&std::env::current_dir().map_err(|error| error.to_string())?)?;
    let pack: SlotFixturePack = serde_json::from_slice(
        &read_rooted(root, Path::new("manifest.json"), MAX_MANIFEST_BYTES)?,
    ).map_err(|error| error.to_string())?;
    if pack.version != 1 || pack.fixtures.len() > MAX_FIXTURES {
        return Err("unsupported fixture pack version or size".into());
    }
    let mut matching = pack.fixtures.into_iter().filter(|row| row.id == id);
    let fixture = matching.next().ok_or_else(|| format!("no staged fixture {id}"))?;
    if fixture.class != FixtureClass::Release || matching.next().is_some() {
        return Err(format!("{id} must identify one staged release fixture"));
    }
    SlotId::new(&fixture.slot).map_err(|err| err.to_string())?;
    let inputs = verify_slot(root, root, &fixture)?;
    Ok((fixture, inputs))
}

fn verify_slot(directory: &Path, bundle_root: &Path, fixture: &SlotFixture) -> Result<SlotInputs, String> {
    let input = &fixture.original;
    let pin = &fixture.fingerprint;
    verify_fixture(fixture.class, fixture.class, pin, pin).map_err(|err| format!("{err:?}"))?;
    let snapshot = read_rooted(bundle_root, &input.snapshot, MAX_MEMBER_BYTES)?;
    let optional_member = |path: &Option<PathBuf>, limit| {
        path.as_ref()
            .map(|path| read_rooted(bundle_root, path, limit))
            .transpose()
    };
    let metadata = optional_member(&input.metadata, MAX_MANIFEST_BYTES)?;
    let replay = optional_member(&input.replay, MAX_MEMBER_BYTES)?;
    let report = optional_member(&input.opening_report, MAX_MANIFEST_BYTES)?;
    let schedule = read_rooted(directory, &input.schedule, MAX_MANIFEST_BYTES)?;
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
        run_source_commit: run_revision()?.to_owned(),
        prototype_tree: build_pin(option_env!("P7_PROTOTYPE_TREE"))?.into(),
        base_mod_tree: build_pin(option_env!("P7_BASE_MOD_TREE"))?.into(),
        asset_tree: build_pin(option_env!("P7_ASSET_TREE"))?.into(),
        build_profile: build_profile().into(),
        features: build_features(),
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
        // Keep the published P6 Lesce bytes as the same-slot traffic baseline.
        let p6_lesce_baseline = staged.snapshot_schema == "0.11.1"
            && fixture.id == "lesce-release"
            && fixture.slot == "lesce_town";
        if !p6_lesce_baseline && staged.snapshot_schema != include_str!("../../../VERSION").trim() {
            return Err(
                "release snapshot requires the current save schema or the P6 Lesce baseline".into(),
            );
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
            Simulation::decode_replay_sidecar(replay, revision)
                .map_err(|err| format!("release replay sidecar: {err}"))?;
        }
    }

    Ok(SlotInputs { snapshot, metadata, replay, report, schedule })
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

pub(super) fn read_member(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let file = SlotPaths::open_member(path).map_err(|err| format!("{}: {err}", path.display()))?;
    read_bounded(file, max_bytes).map_err(|err| format!("{}: {err}", path.display()))
}

fn read_bounded(reader: impl Read, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::other(format!("member exceeds {max_bytes} bytes")));
    }
    Ok(bytes)
}

pub(super) fn read_rooted(root: &Path, relative: &Path, limit: usize) -> Result<Vec<u8>, String> {
    if relative.as_os_str().is_empty()
        || relative.components().any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("fixture member is not a relative path: {}", relative.display()));
    }
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let path = root.join(relative);
    let parent = path.parent().unwrap().canonicalize().map_err(|error| error.to_string())?;
    if !parent.starts_with(&root) {
        return Err(format!("fixture member escapes its input root: {}", relative.display()));
    }
    let file = SlotPaths::open_member(&parent.join(path.file_name().unwrap()))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    // Recheck the opened file before reading bytes in case an input directory
    // was renamed or replaced after resolving its path.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let opened = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .map_err(|error| error.to_string())?;
        if !opened.starts_with(&root) {
            return Err(format!("opened fixture member escapes its input root: {}", relative.display()));
        }
    }
    read_bounded(file, limit).map_err(|error| format!("{}: {error}", path.display()))
}

fn build_pin(pin: Option<&'static str>) -> Result<&'static str, String> {
    pin.ok_or_else(|| "the scoring executable was built without Git provenance".into())
}

fn run_revision() -> Result<&'static str, String> {
    build_pin(option_env!("P7_RUN_REVISION"))
}

fn build_profile() -> &'static str {
    env!("P7_RUN_PROFILE")
}

fn build_features() -> Vec<String> {
    let mut features: Vec<_> = env!("P7_RUN_FEATURES")
        .split(',')
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
        .collect();
    features.sort();
    features
}

pub(super) fn verify_runtime_data(root: &Path) -> Result<(), String> {
    let manifest = include_str!(concat!(env!("OUT_DIR"), "/p7-base-mod.git-tree"));
    let mut paths = Vec::new();
    let mut hashes = Vec::new();
    for line in manifest.lines() {
        let (entry, path) = line.split_once('\t').ok_or("invalid base_mod build pin")?;
        let (_, hash) = entry.split_once(" blob ").ok_or("invalid base_mod build pin")?;
        paths.push(PathBuf::from(path));
        hashes.push(hash);
    }
    if paths.is_empty() {
        return Err("the scoring executable has no base_mod pins".into());
    }
    let mut dirs = vec![PathBuf::from("base_mod")];
    let mut present = BTreeSet::new();
    while let Some(dir) = dirs.pop() {
        let absolute = root.join(&dir);
        if !absolute.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            return Err(format!("runtime prototype directory is invalid: {}", absolute.display()));
        }
        for entry in fs::read_dir(absolute).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = dir.join(entry.file_name());
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file() {
                present.insert(path);
            } else {
                return Err(format!("unexpected runtime prototype path {}", path.display()));
            }
        }
    }
    if present.len() != paths.len() || paths.iter().any(|path| !present.contains(path)) {
        return Err("runtime base_mod files differ from the scoring executable's pins".into());
    }
    let output = Command::new("git").current_dir(root)
        .args(["hash-object", "--no-filters", "--"])
        .args(&paths).output().map_err(|error| error.to_string())?;
    let actual = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    if !output.status.success() || actual.lines().ne(hashes) {
        return Err("runtime base_mod contents differ from the scoring executable's pins".into());
    }
    Ok(())
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

pub(super) fn digest(bytes: &[u8]) -> Result<String, String> {
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
    fn p7_comparison_release_stages_a_loader_readable_sidecar() {
        use crate::builders::{fill_houses, CommandSink};
        use geom::vec3;
        use simulation::map::{LanePatternBuilder, MapProject};
        use simulation::world_command::WorldCommand;

        let mut ctx = crate::ScenarioCtx::new(SimulationOptions {
            terrain_size: simulation::NewGameOptions::MIN_TERRAIN_CHUNKS,
            preset: simulation::MapPreset::Flat,
            ..SimulationOptions::LEGACY
        });
        ctx.submit(&[WorldCommand::MapMakeConnection {
            from: MapProject::ground(vec3(200.0, 200.0, 0.0)),
            to: MapProject::ground(vec3(450.0, 200.0, 0.0)),
            inter: None,
            pat: LanePatternBuilder::default().build(),
        }]);
        ctx.flush();
        let houses = fill_houses(&mut ctx, 2);
        assert_eq!(houses.len(), 2);
        ctx.run_ticks(2 * simulation::economy::balance::DEMAND_INTERVAL_TICKS);
        assert!(!ctx.sim.world().humans.is_empty());

        let root = std::env::temp_dir().join(format!("p7-release-sidecar-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(root);
        let base = &root.0;
        let manifest = base.join("source.json");
        let output = base.join("staged");
        let revision = SnapshotRevision::new(42);
        let slot = SlotId::new("replay_sample").unwrap();
        ctx.sim.save_to_slot(base, &slot, revision).unwrap();
        let paths = slot_paths(base, &slot).unwrap();
        let metadata: SlotMeta = serde_json::from_slice(&fs::read(&paths.meta).unwrap()).unwrap();
        let header =
            Simulation::compressed_snapshot_header(&fs::read(&paths.zip).unwrap()).unwrap();
        let opening_manifest = serde_json::json!({"opening_tick": metadata.game_tick});
        let report = serde_json::to_vec(&serde_json::json!({
            "opening_manifest": opening_manifest,
            "opening": {"opening": {"residents": metadata.population}, "failures": []},
            "slot": slot.as_str(),
            "save_version": header.version,
            "snapshot_revision": format!("{:032x}", revision.get()),
        })).unwrap();
        fs::write(base.join("opening.json"), &report).unwrap();
        fs::write(base.join("score.json"), b"{}").unwrap();
        let replay = fs::read(&paths.replay).unwrap();
        let mut pack = SlotFixturePack {
            version: 1,
            fixtures: vec![SlotFixture {
                id: "sample-release".into(),
                slot: slot.as_str().to_owned(),
                class: FixtureClass::Release,
                original: SlotFixtureFiles {
                    snapshot: paths.zip.file_name().unwrap().into(),
                    metadata: Some(paths.meta.file_name().unwrap().into()),
                    replay: Some(paths.replay.file_name().unwrap().into()),
                    opening_report: Some("opening.json".into()),
                    schedule: "score.json".into(),
                },
                fingerprint: FixtureFingerprint {
                    source_commit: run_revision().unwrap().into(),
                    run_source_commit: AT_STAGING.into(),
                    prototype_tree: build_pin(option_env!("P7_PROTOTYPE_TREE")).unwrap().into(),
                    base_mod_tree: build_pin(option_env!("P7_BASE_MOD_TREE")).unwrap().into(),
                    asset_tree: build_pin(option_env!("P7_ASSET_TREE")).unwrap().into(),
                    build_profile: AT_STAGING.into(),
                    features: build_features(),
                    snapshot_schema: header.version,
                    snapshot_revision: format!("{:032x}", revision.get()),
                    snapshot_sha256: digest(&fs::read(&paths.zip).unwrap()).unwrap(),
                    metadata_sha256: digest(&fs::read(&paths.meta).unwrap()).unwrap(),
                    replay_sha256: Some(digest(&replay).unwrap()),
                    schedule_sha256: digest(b"{}").unwrap(),
                    initial_elapsed_tick: elapsed_tick(&ctx.sim).0,
                    workers: 1,
                    opening: Some(OpeningFingerprint {
                        manifest_sha256: digest(&serde_json::to_vec(&opening_manifest).unwrap()).unwrap(),
                        report_sha256: digest(&report).unwrap(),
                        population: metadata.population,
                        passed: true,
                    }),
                },
            }],
        };
        let bad = br#"{"revision":42}"#;
        fs::write(&paths.replay, bad).unwrap();
        pack.fixtures[0].fingerprint.replay_sha256 = Some(digest(bad).unwrap());
        fs::write(&manifest, serde_json::to_vec(&pack).unwrap()).unwrap();
        let refused = stage_published_slots(&manifest, FixtureClass::Release, &output, StageOptions::default());
        assert!(refused.is_err(), "{refused:?}");
        assert!(!output.exists());

        fs::write(&paths.replay, &replay).unwrap();
        pack.fixtures[0].fingerprint.replay_sha256 = Some(digest(&replay).unwrap());
        fs::write(&manifest, serde_json::to_vec(&pack).unwrap()).unwrap();
        stage_published_slots(&manifest, FixtureClass::Release, &output, StageOptions::default()).unwrap();
        let staged = output.join("sample-release");
        assert_eq!(fs::read(slot_paths(&staged, &slot).unwrap().replay).unwrap(), replay);
        let saved = Simulation::load_from_slot(&staged, &slot).unwrap();
        let replay = Simulation::load_replay_from_slot(&staged, &slot).unwrap();
        let (mut played, mut cursor) = Simulation::try_from_replay(replay).unwrap();
        let mut schedule = Simulation::schedule();
        cursor.speed = 1000;
        while !cursor.advance_tick(&mut played, &mut schedule) {}
        let mut played = played.hashes();
        let mut saved = saved.hashes();
        played.remove("replay");
        saved.remove("replay");
        assert_eq!(played, saved);
    }

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
        let head = run_revision().unwrap().to_owned();
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
                    prototype_tree: build_pin(option_env!("P7_PROTOTYPE_TREE")).unwrap().into(),
                    base_mod_tree: build_pin(option_env!("P7_BASE_MOD_TREE")).unwrap().into(),
                    asset_tree: build_pin(option_env!("P7_ASSET_TREE")).unwrap().into(),
                    build_profile: build_profile().into(),
                    features: build_features(),
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

        let nested = base.join("nested");
        fs::create_dir(&nested).unwrap();
        let escaped_manifest = nested.join("manifest.json");
        let mut escaped = pack;
        escaped.fixtures[0].original.schedule = "../schedule.json".into();
        fs::write(&escaped_manifest, serde_json::to_vec(&escaped).unwrap()).unwrap();
        let attempt = base.join("escaped");
        let options = || StageOptions { bundle_root: Some(base), run_source_commit: None };
        assert!(stage_published_slots(
            &escaped_manifest, FixtureClass::Diagnostic, &attempt, options()
        ).is_err());
        assert!(!attempt.exists());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base, nested.join("linked")).unwrap();
            escaped.fixtures[0].original.schedule = "linked/schedule.json".into();
            fs::write(&escaped_manifest, serde_json::to_vec(&escaped).unwrap()).unwrap();
            assert!(stage_published_slots(
                &escaped_manifest, FixtureClass::Diagnostic, &attempt, options()
            ).is_err());
            assert!(!attempt.exists());
        }
    }
}
