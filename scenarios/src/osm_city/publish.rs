use super::{check, lesce, normalize, read_overpass, source, ImportReport, Source, LESCE_BOUNDS};
use crate::{ensure_repo_root_cwd, ScenarioCtx};
use simulation::economy::{Budget, BudgetCategory, Government};
use simulation::map::import::ImportedNetwork;
use simulation::map::{Intersection, LightPolicy, Map, ZoneGrid};
use simulation::{SavePhase, Simulation, SlotCamera, SlotId, SnapshotRevision};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SLOT: &str = "lesce_osm";
const NAME: &str = "Lesce, Slovenia · © OpenStreetMap contributors";
const ZIP: &str = "lesce_osm.zip";
const META: &str = "lesce_osm.meta.json";
const REPORT: &str = "lesce_osm.import-report.json";
const ALIASES: [&str; 2] = ["ATTRIBUTION-OSM.txt", REPORT];
const LEGACY_LOCK: &str = ".lesce_osm.import.lock";
const COMMIT_REVISION: &str = "commit-revision";
const ATTRIBUTION: &str = include_str!("../../../scripts/osm/ATTRIBUTION-OSM.txt");
const QUERY: &str = include_str!("../../../scripts/osm/lesce.overpassql");
const CAMERA: SlotCamera = SlotCamera {
    position: [3930.0, 4240.0, 10.0],
    yaw: -0.35,
    pitch: 1.22,
    distance: 1490.0,
};

pub fn generate(input: &Path, save_root: &Path) -> Result<ImportReport, String> {
    generate_with_options(input, save_root, false, false)
}

pub fn generate_with_options(
    input: &Path,
    save_root: &Path,
    replace: bool,
    check: bool,
) -> Result<ImportReport, String> {
    let start = Instant::now();
    let input = std::path::absolute(input).map_err(|error| error.to_string())?;
    let root = std::path::absolute(save_root).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    File::open(&input)
        .and_then(|file| file.take(source::MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("{}: {error}", input.display()))?;
    let extract = read_overpass(bytes.as_slice()).map_err(|error| error.to_string())?;
    let source = Source {
        sha256: source_digest(&bytes)?,
        timestamp: extract.timestamp().map(str::to_owned),
        bounds: LESCE_BOUNDS,
        query: QUERY,
        requested_snapshot: "2026-11-22T12:24:51Z",
    };
    let mut city = normalize(extract, LESCE_BOUNDS).map_err(|e| e.to_string())?;
    let id = SlotId::new(SLOT).map_err(|e| e.to_string())?;
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let lock = PublicationLock::acquire(&root)?;
    check_target(&root, replace)?;

    let mut report = ImportReport {
        source,
        revision: String::new(),
        feature_counts: BTreeMap::from([
            ("source_elements".into(), city.source_elements),
            ("mapped_zone_features".into(), city.features.len()),
            ("mapped_sites".into(), city.sites.len()),
            ("mapped_forests".into(), city.forests.len()),
            ("boundary_stubs".into(), city.city.boundary_stubs.len()),
        ]),
        skipped: std::mem::take(&mut city.skipped),
        synthetic: Vec::new(),
        route_checks: BTreeMap::new(),
        flat_crossings: std::mem::take(&mut city.flat_crossings),
        short_lanes: Vec::new(),
        junction_policies: Vec::new(),
        policies: vec![
            "Road boundary stubs are cosmetic. External trade is rail only.",
            "Turn restrictions and lane-specific turn markings are not imported.",
            "Rail uses one game lane per direction. Bridges and tunnels remain flat and unconnected at crossings.",
            "The passenger station is a static marker using a generic building mesh.",
            "Junctions use priority signage without lights or all-way stops. Traffic lights are placed only where OSM maps highway=traffic_signals at the junction or on an approach.",
        ],
        opening_budget_cents: 0,
        opening: None,
        generation_seconds: 0.0,
        snapshot_bytes: 0,
        headless: None,
        attribution: ATTRIBUTION,
    };
    let layout = lesce::add_lesce_infrastructure(&mut city, &mut report)?;
    ensure_repo_root_cwd();
    simulation::init::init();
    let (sim, network) = Simulation::from_imported_city(&city.city, &lesce::OPTIONS)?;
    report.short_lanes = network
        .short_lanes
        .iter()
        .map(|&(index, length)| (city.city.links[index].source_way_ids.clone(), length))
        .collect();
    report.junction_policies = junction_policy_changes(&sim.map(), &city, &network);
    let mut ctx = ScenarioCtx::from_sim(sim);
    let facilities = lesce::place_facilities(&mut ctx, &city, &network, &layout, &mut report)?;
    lesce::check_routes(&ctx.sim, &city, &network, &layout, &facilities, &mut report)?;
    super::zoning::zone_and_seed(&mut ctx.sim, &city, &network, &facilities, &mut report)?;
    super::zoning::filter_vegetation(&ctx.sim, &city)?;
    if ctx.sim.read::<Government>().money.cents() <= 0 {
        return Err("construction exhausted the setup budget".into());
    }
    let mut opening = check::warm_opening(&mut ctx, &facilities).map_err(|error| {
        format!(
            "{error}; opening placement counts {:?}",
            report.feature_counts
        )
    })?;
    let opening_money = prototypes::Money::new_bucks(lesce::OPTIONS.starting_money_bucks as i64);
    if ctx
        .sim
        .read::<Budget>()
        .total(BudgetCategory::StarterInfrastructure)
        != 0
    {
        return Err("import charged for starter infrastructure".into());
    }
    ctx.sim.fund_imported_opening(opening_money)?;
    opening.money_cents = ctx.sim.read::<Government>().money.cents();
    report.opening = Some(opening);
    report.opening_budget_cents = ctx.sim.read::<Government>().money.cents();
    {
        let map = ctx.sim.map();
        report.feature_counts.extend([
            ("roads".into(), map.roads().len()),
            (
                "rail_links".into(),
                city.classes.iter().filter(|class| class.rail()).count(),
            ),
            ("junctions".into(), map.intersections().len()),
            (
                "roundabouts".into(),
                map.intersections()
                    .values()
                    .filter(|j| j.is_roundabout())
                    .count(),
            ),
            ("lots".into(), map.lots().len()),
            ("buildings".into(), map.buildings().len()),
            ("zone_cells".into(), ctx.sim.read::<ZoneGrid>().cells.len()),
        ]);
    }

    let revision = snapshot_revision(&ctx.sim);
    report.revision = format!("{:032x}", revision.get());
    let mut stage = Scratch::create(&root)?;
    publish_snapshot(&ctx.sim, &stage.path, &id, revision)?;
    verify_catalog(&stage.path, &id, revision)?;
    report.snapshot_bytes = fs::metadata(stage.path.join(format!("{SLOT}.zip")))
        .map_err(|e| e.to_string())?
        .len();
    let loaded =
        Simulation::load_from_slot(&stage.path, &id).map_err(|e| format!("staged slot: {e}"))?;
    check::validate_opening(&loaded, &facilities)?;
    report.generation_seconds = start.elapsed().as_secs_f64();
    if check {
        report.headless = Some(check::check_days(loaded, facilities)?);
    }

    commit_snapshot(&mut stage, &root, &report.revision, &report, replace)?;
    if replace {
        lock.prune_legacy(&root)?;
    }
    Ok(report)
}

/// Junctions whose effective control differs from what `LightPolicy::Auto` would apply.
fn junction_policy_changes(
    map: &Map,
    city: &super::NormalizedCity,
    network: &ImportedNetwork,
) -> Vec<(String, String)> {
    let mut auto = map.lanes().clone();
    let describe = |junction: &Intersection, lanes: &simulation::map::Lanes| {
        let controls: Vec<_> = junction
            .roads
            .iter()
            .flat_map(|&road| map.roads()[road].incoming_lanes_to(junction.id).iter())
            .filter(|(_, kind)| kind.needs_light())
            .map(|&(lane, _)| lanes[lane].control)
            .collect();
        if controls.iter().any(|control| control.is_light()) {
            "traffic lights"
        } else if controls.iter().any(|control| control.is_stop_sign()) {
            "all-way stop signs"
        } else {
            "priority (no lights)"
        }
    };
    let mut changes = Vec::new();
    for (&index, &id) in &network.junctions {
        let junction = &map.intersections()[id];
        if junction.light_policy == LightPolicy::Auto {
            continue;
        }
        LightPolicy::Auto.apply(junction, &mut auto, map.roads());
        let (before, after) = (describe(junction, &auto), describe(junction, map.lanes()));
        if before != after {
            changes.push((
                city.junction_sources[index].clone(),
                format!("{before} -> {after}"),
            ));
        }
    }
    changes
}

fn commit_snapshot(
    stage: &mut Scratch,
    root: &Path,
    revision: &str,
    report: &impl serde::Serialize,
    replace: bool,
) -> Result<(), String> {
    prepare_snapshot(&stage.path, root, revision, report, replace)?;
    // The old catalog metadata and aliases remain in place until the ZIP commits.
    install(&stage.path, root, ZIP)?;
    stage.keep_for_recovery = true;
    finish_snapshot(&stage.path, root, revision)?;
    stage.keep_for_recovery = false;
    Ok(())
}

fn prepare_snapshot(
    stage: &Path,
    root: &Path,
    revision: &str,
    report: &impl serde::Serialize,
    replace: bool,
) -> Result<(), String> {
    let required = revision_sidecars(revision);
    check_target(root, replace)?;
    for name in [ZIP, META]
        .into_iter()
        .chain(required.each_ref().map(String::as_str))
        .chain(ALIASES)
    {
        check_destination(&root.join(name))?;
    }
    let diagnostics = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    for ((required, alias), content) in required
        .iter()
        .zip(ALIASES)
        .zip([ATTRIBUTION.as_bytes(), diagnostics.as_slice()])
    {
        write_staged(stage, required, content)?;
        write_staged(stage, alias, content)?;
    }
    write_staged(stage, COMMIT_REVISION, revision.as_bytes())?;
    for name in required {
        install(stage, root, &name)?;
    }
    Ok(())
}

fn finish_snapshot(stage: &Path, root: &Path, revision: &str) -> Result<(), String> {
    for name in [META].into_iter().chain(ALIASES) {
        if stage.join(name).try_exists().map_err(|e| e.to_string())? {
            check_destination(&root.join(name))?;
            install(stage, root, name)?;
        }
    }
    prune_revision_sidecars(root, revision)
}

fn install(stage: &Path, root: &Path, name: &str) -> Result<(), String> {
    fs::rename(stage.join(name), root.join(name)).map_err(|e| format!("publish {name}: {e}"))
}

fn revision_sidecars(revision: &str) -> [String; 2] {
    [
        format!("{SLOT}.{revision}.ATTRIBUTION-OSM.txt"),
        format!("{SLOT}.{revision}.import-report.json"),
    ]
}

fn valid_revision(revision: &str) -> bool {
    revision.len() == 32
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn prune_revision_sidecars(root: &Path, revision: &str) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some((old_revision, suffix)) = name
            .to_str()
            .and_then(|name| name.strip_prefix("lesce_osm."))
            .and_then(|name| name.split_once('.'))
        else {
            continue;
        };
        if old_revision != revision
            && valid_revision(old_revision)
            && matches!(suffix, "ATTRIBUTION-OSM.txt" | "import-report.json")
            && entry.file_type().map_err(|e| e.to_string())?.is_file()
        {
            fs::remove_file(entry.path())
                .map_err(|e| format!("remove obsolete sidecar {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn recover_stage(stage: &Path, root: &Path) -> Result<(), String> {
    let marker = stage.join(COMMIT_REVISION);
    if !marker.symlink_metadata().is_ok_and(|meta| meta.is_file()) || stage.join(ZIP).exists() {
        return Ok(());
    }
    let mut revision = String::new();
    File::open(&marker)
        .and_then(|file| file.take(33).read_to_string(&mut revision))
        .map_err(|e| format!("read interrupted publication at {}: {e}", stage.display()))?;
    if valid_revision(&revision) {
        let zip = root.join(ZIP);
        let entries = simulation::list_slots_with_revisions(root, |path, kind| {
            if path != zip {
                return None;
            }
            Simulation::read_slot_snapshot_header(path, kind)
                .ok()
                .and_then(|header| header.revision)
        })
        .map_err(|e| e.to_string())?;
        let expected = u128::from_str_radix(&revision, 16).unwrap();
        if entries.iter().any(|entry| {
            entry.id.as_str() == SLOT && entry.revision == Some(SnapshotRevision::new(expected))
        }) {
            finish_snapshot(stage, root, &revision)?;
        }
    }
    Ok(())
}

fn check_destination(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!(
            "{} obstructs publication; expected a regular file",
            path.display()
        )),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn source_digest(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "sha256sum is required to record source provenance; install GNU coreutils or add sha256sum to PATH".into(),
            _ => format!("start sha256sum: {e}"),
        })?;
    let result = child.stdin.take().unwrap().write_all(bytes);
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    result.map_err(|e| format!("write to sha256sum: {e}"))?;
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .filter(|hash| {
            output.status.success()
                && hash.len() == 64
                && hash.bytes().all(|ch| ch.is_ascii_hexdigit())
        })
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum failed to digest the extract".into())
}

struct PublicationLock {
    _file: File,
    #[cfg(unix)]
    legacy: Option<File>,
}

impl PublicationLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        // The directory is a stable lock target even when obsolete sidecars disappear.
        #[cfg(unix)]
        let path = root;
        #[cfg(unix)]
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(not(unix))]
        let path = root.join(LEGACY_LOCK);
        #[cfg(not(unix))]
        let file = fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Self::try_lock(&file, &path)?;
        #[cfg(unix)]
        let legacy = {
            let path = root.join(LEGACY_LOCK);
            match path.symlink_metadata() {
                Ok(meta) if meta.is_file() => {
                    let file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                    Self::try_lock(&file, &path)?;
                    Some(file)
                }
                Ok(_) => return Err(format!("{} is not a regular file", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        };
        // A kernel lock expires with the owner, including on SIGINT or SIGKILL.
        for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let stale = name
                .to_str()
                .and_then(|name| name.strip_prefix(".lesce_osm-stage-"))
                .and_then(|suffix| suffix.split_once('-'))
                .is_some_and(|(pid, stamp)| {
                    pid.parse::<u32>().is_ok() && stamp.parse::<u128>().is_ok()
                });
            if stale && entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                recover_stage(&entry.path(), root)?;
                fs::remove_dir_all(entry.path())
                    .map_err(|e| format!("clean abandoned stage: {e}"))?;
            }
        }
        Ok(Self {
            _file: file,
            #[cfg(unix)]
            legacy,
        })
    }

    fn try_lock(file: &File, path: &Path) -> Result<(), String> {
        file.try_lock().map_err(|e| {
            format!(
                "{}: {e}; wait for the current import to finish",
                path.display()
            )
        })
    }

    fn prune_legacy(&self, _root: &Path) -> Result<(), String> {
        #[cfg(unix)]
        if self.legacy.is_some() {
            let path = _root.join(LEGACY_LOCK);
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }
}

struct Scratch {
    path: PathBuf,
    keep_for_recovery: bool,
}

impl Scratch {
    fn create(root: &Path) -> Result<Self, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let stage = root.join(format!(".lesce_osm-stage-{}-{stamp}", std::process::id()));
        fs::create_dir(&stage).map_err(|e| e.to_string())?;
        Ok(Self {
            path: stage,
            keep_for_recovery: false,
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep_for_recovery {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn check_target(root: &Path, replace: bool) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name == format!("{SLOT}.json") {
            return Err(format!("{SLOT} is reserved by {}", entry.path().display()));
        }
        if !replace
            && (["zip", "bc", "meta.json"]
                .iter()
                .any(|ext| name == format!("{SLOT}.{ext}"))
                || name == format!("{SLOT}_replay.json"))
        {
            return Err(format!(
                "slot {SLOT} already exists in {}. Use --replace to regenerate it.",
                root.display()
            ));
        }
    }
    Ok(())
}

fn snapshot_revision(sim: &Simulation) -> SnapshotRevision {
    let hashes = sim.hashes();
    let half = |salt: u8| {
        let mut hasher = DefaultHasher::new();
        (salt, SLOT, &hashes).hash(&mut hasher);
        u128::from(hasher.finish())
    };
    SnapshotRevision::new(half(0) << 64 | half(1))
}

fn publish_snapshot(
    sim: &Simulation,
    root: &Path,
    id: &SlotId,
    revision: SnapshotRevision,
) -> Result<(), String> {
    let phase = sim
        .encode_slot(revision)
        .and_then(|slot| {
            slot.with_display_name(NAME.into())
                .with_camera(CAMERA)
                .publish(root, id)
        })
        .map_err(|e| format!("{NAME}: {e}"))?;
    match phase {
        SavePhase::Complete => Ok(()),
        SavePhase::SidecarsFailed { replay, meta } => Err(format!(
            "{NAME}: snapshot published but sidecars failed (replay: {replay:?}, metadata: {meta:?})"
        )),
    }
}

fn verify_catalog(root: &Path, id: &SlotId, revision: SnapshotRevision) -> Result<(), String> {
    let entries = simulation::list_slots_with_revisions(root, |path, kind| {
        Simulation::read_slot_snapshot_header(path, kind)
            .ok()
            .and_then(|header| header.revision)
    })
    .map_err(|e| e.to_string())?;
    let matched = entries.iter().any(|entry| {
        &entry.id == id
            && entry.revision == Some(revision)
            && !entry.replay_available
            && entry
                .meta
                .as_ref()
                .is_some_and(|meta| meta.display_name == NAME && meta.camera == Some(CAMERA))
    });
    if !matched {
        return Err(format!(
            "{SLOT} is missing its revision, display name or camera in the slot catalog"
        ));
    }
    Ok(())
}

fn write_staged(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    fs::write(dir.join(name), bytes).map_err(|e| format!("staging {name}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::place_company;
    use geom::vec2;

    #[test]
    fn osm_publication_preflights_obstructions_and_preserves_the_slot() {
        ensure_repo_root_cwd();
        simulation::init::init();
        let normalized = normalize(
            read_overpass(include_bytes!("../../tests/fixtures/osm-city-small.json").as_slice())
                .unwrap(),
            LESCE_BOUNDS,
        )
        .unwrap();
        let (sim, _) = Simulation::from_imported_city(&normalized.city, &lesce::OPTIONS).unwrap();
        let mut ctx = ScenarioCtx::from_sim(sim);
        let school = place_company(&mut ctx, "school", vec2(3650.0, 3950.0)).unwrap();
        assert!(ctx.sim.map().buildings()[school].connected_road.is_some());
        let opening = prototypes::Money::new_bucks(lesce::OPTIONS.starting_money_bucks as i64);
        let before_grant = ctx.sim.read::<Government>().money;
        assert!(before_grant < opening);
        ctx.sim.fund_imported_opening(opening).unwrap();
        assert_eq!(ctx.sim.read::<Government>().money, opening);
        let budget = ctx.sim.read::<Budget>();
        assert_eq!(
            budget.total(BudgetCategory::StarterInfrastructure),
            i128::from(opening.inner() - before_grant.inner())
        );
        assert_eq!(
            budget
                .days()
                .values()
                .flat_map(BTreeMap::values)
                .sum::<i128>(),
            0
        );
        drop(budget);

        let root = Scratch::create(&std::env::temp_dir()).unwrap();
        let stale = root.path.join(".lesce_osm-stage-123-456");
        fs::create_dir(&stale).unwrap();
        fs::write(root.path.join(LEGACY_LOCK), []).unwrap();
        let lock = PublicationLock::acquire(&root.path).unwrap();
        assert!(!stale.exists());
        assert!(PublicationLock::acquire(&root.path).is_err());
        let mut stage = Scratch::create(&root.path).unwrap();
        let id = SlotId::new(SLOT).unwrap();
        let revision = snapshot_revision(&ctx.sim);
        let revision_text = format!("{:032x}", revision.get());
        publish_snapshot(&ctx.sim, &stage.path, &id, revision).unwrap();

        let attribution = root
            .path
            .join(format!("{SLOT}.{revision_text}.ATTRIBUTION-OSM.txt"));
        fs::create_dir(&attribution).unwrap();
        assert!(commit_snapshot(&mut stage, &root.path, &revision_text, &(), false).is_err());
        assert!(!root.path.join(ZIP).exists());
        assert!(!root.path.join(META).exists());
        fs::remove_dir(&attribution).unwrap();
        commit_snapshot(&mut stage, &root.path, &revision_text, &(), false).unwrap();
        verify_catalog(&root.path, &id, revision).unwrap();
        let zip = fs::read(root.path.join(ZIP)).unwrap();
        let metadata = fs::read(root.path.join(META)).unwrap();

        let replacement = SnapshotRevision::new(revision.get() ^ 1);
        let replacement_text = format!("{:032x}", replacement.get());
        publish_snapshot(&ctx.sim, &stage.path, &id, replacement).unwrap();
        assert!(check_target(&root.path, false)
            .unwrap_err()
            .contains("already exists"));
        let obstruction = root
            .path
            .join(format!("{SLOT}.{replacement_text}.import-report.json"));
        fs::create_dir(&obstruction).unwrap();
        assert!(commit_snapshot(&mut stage, &root.path, &replacement_text, &(), true).is_err());
        assert_eq!(fs::read(root.path.join(ZIP)).unwrap(), zip);
        assert_eq!(fs::read(root.path.join(META)).unwrap(), metadata);
        verify_catalog(&root.path, &id, revision).unwrap();
        fs::remove_dir(obstruction).unwrap();

        let old_report = fs::read(root.path.join(REPORT)).unwrap();
        prepare_snapshot(&stage.path, &root.path, &replacement_text, &42, true).unwrap();
        verify_catalog(&root.path, &id, revision).unwrap();
        assert_eq!(fs::read(root.path.join(REPORT)).unwrap(), old_report);
        assert_eq!(fs::read(root.path.join(META)).unwrap(), metadata);

        // Simulate process exit before the ZIP rename, then open the old slot again.
        stage.keep_for_recovery = true;
        let abandoned = stage.path.clone();
        drop(stage);
        drop(lock);
        let lock = PublicationLock::acquire(&root.path).unwrap();
        assert!(!abandoned.exists());
        verify_catalog(&root.path, &id, revision).unwrap();
        assert_eq!(fs::read(root.path.join(ZIP)).unwrap(), zip);
        assert_eq!(fs::read(root.path.join(REPORT)).unwrap(), old_report);

        // Once the ZIP has committed, lock recovery completes its staged sidecars.
        let mut stage = Scratch::create(&root.path).unwrap();
        publish_snapshot(&ctx.sim, &stage.path, &id, replacement).unwrap();
        prepare_snapshot(&stage.path, &root.path, &replacement_text, &42, true).unwrap();
        install(&stage.path, &root.path, ZIP).unwrap();
        stage.keep_for_recovery = true;
        drop(stage);
        drop(lock);
        let lock = PublicationLock::acquire(&root.path).unwrap();
        verify_catalog(&root.path, &id, replacement).unwrap();
        assert_eq!(fs::read(root.path.join(REPORT)).unwrap(), b"42");
        assert!(!attribution.exists(), "prior revision attribution remained");
        assert!(!root
            .path
            .join(revision_sidecars(&revision_text)[1].as_str())
            .exists());
        lock.prune_legacy(&root.path).unwrap();
        #[cfg(unix)]
        assert!(!root.path.join(LEGACY_LOCK).exists());
    }

    #[test]
    fn osm_report_lists_junction_policies_that_differ_from_auto() {
        ensure_repo_root_cwd();
        simulation::init::init();
        let city = normalize(
            read_overpass(
                include_bytes!("../../tests/fixtures/osm-junction-control.json").as_slice(),
            )
            .unwrap(),
            LESCE_BOUNDS,
        )
        .unwrap();
        let (sim, network) = Simulation::from_imported_city(&city.city, &lesce::OPTIONS).unwrap();
        let changes = junction_policy_changes(&sim.map(), &city, &network);
        // The signalled crossroads keeps its lights, so only the other two are listed.
        assert_eq!(
            changes,
            [
                (
                    "node/5006".to_owned(),
                    "traffic lights -> priority (no lights)".to_owned()
                ),
                (
                    "node/5011".to_owned(),
                    "all-way stop signs -> priority (no lights)".to_owned()
                ),
            ]
        );
    }
}
