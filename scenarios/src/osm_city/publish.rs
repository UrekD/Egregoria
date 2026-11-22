use super::{check, lesce, normalize, read_overpass, source, ImportReport, Source, LESCE_BOUNDS};
use crate::{ensure_repo_root_cwd, ScenarioCtx};
use simulation::economy::{Budget, BudgetCategory, Government};
use simulation::map::ZoneGrid;
use simulation::{SavePhase, Simulation, SlotCamera, SlotId, SnapshotRevision};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
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
    let _lock = PublicationLock::acquire(&root)?;
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
        policies: vec![
            "Road boundary stubs are cosmetic. External trade is rail only.",
            "Turn restrictions and lane-specific turn markings are not imported.",
            "Rail uses one game lane per direction. Bridges and tunnels remain flat and unconnected at crossings.",
            "The passenger station is a static marker using a generic building mesh.",
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
    let mut ctx = ScenarioCtx::from_sim(sim);
    let facilities = lesce::place_facilities(&mut ctx, &city, &network, &layout, &mut report)?;
    lesce::check_routes(&ctx.sim, &city, &network, &layout, &facilities, &mut report)?;
    super::zoning::zone_and_seed(&mut ctx.sim, &city, &network, &facilities, &mut report)?;
    super::zoning::filter_vegetation(&mut ctx.sim, &city)?;
    if ctx.sim.read::<Government>().money.cents() <= 0 {
        return Err("construction exhausted the setup budget".into());
    }
    let mut opening = check::warm_opening(&mut ctx, &facilities).map_err(|error| {
        format!(
            "{error}; opening placement counts {:?}",
            report.feature_counts
        )
    })?;
    // Construction and commissioning belong to scenario setup. Give the
    // player the advertised FreeBuild treasury at the actual opening tick.
    let opening_money = prototypes::Money::new_bucks(lesce::OPTIONS.starting_money_bucks as i64);
    ctx.sim.write::<Government>().money = opening_money;
    opening.money_cents = opening_money.cents();
    report.opening = Some(opening);
    if ctx
        .sim
        .read::<Budget>()
        .total(BudgetCategory::StarterInfrastructure)
        != 0
    {
        return Err("import charged for starter infrastructure".into());
    }
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
    let stage = Scratch::create(&root)?;
    publish_snapshot(&ctx.sim, &stage.0, &id, revision)?;
    verify_catalog(&stage.0, &id, revision)?;
    report.snapshot_bytes = fs::metadata(stage.0.join(format!("{SLOT}.zip")))
        .map_err(|e| e.to_string())?
        .len();
    let loaded =
        Simulation::load_from_slot(&stage.0, &id).map_err(|e| format!("staged slot: {e}"))?;
    check::validate_opening(&loaded, &facilities)?;
    report.generation_seconds = start.elapsed().as_secs_f64();
    if check {
        report.headless = Some(check::check_days(loaded, facilities)?);
    }

    commit_snapshot(&stage.0, &root, &report.revision, &report, replace)?;
    Ok(report)
}

fn commit_snapshot(
    stage: &Path,
    root: &Path,
    revision: &str,
    report: &impl serde::Serialize,
    replace: bool,
) -> Result<(), String> {
    let revision_report = format!("{SLOT}.{revision}.import-report.json");
    let revision_attribution = format!("{SLOT}.{revision}.ATTRIBUTION-OSM.txt");
    let aliases = ["ATTRIBUTION-OSM.txt", REPORT];
    let required = [revision_attribution.as_str(), revision_report.as_str()];
    check_target(root, replace)?;
    for name in [ZIP, META].into_iter().chain(required).chain(aliases) {
        check_destination(&root.join(name))?;
    }
    let diagnostics = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    for (name, content) in [
        (required[0], ATTRIBUTION.as_bytes()),
        (required[1], diagnostics.as_slice()),
        (aliases[0], ATTRIBUTION.as_bytes()),
        (aliases[1], diagnostics.as_slice()),
    ] {
        write_staged(stage, name, content)?;
    }

    // The ZIP is the commit point. The catalog correlates metadata by revision.
    let old_meta = stage.join("previous.meta.json");
    let had_meta = root.join(META).is_file();
    if had_meta {
        fs::copy(root.join(META), &old_meta).map_err(|e| format!("backup metadata: {e}"))?;
    }
    let install = |name| {
        fs::rename(stage.join(name), root.join(name)).map_err(|e| format!("publish {name}: {e}"))
    };
    for name in required {
        install(name)?;
    }
    install(META)?;
    if let Err(error) = install(ZIP) {
        let restore = if had_meta {
            fs::rename(old_meta, root.join(META))
        } else {
            fs::remove_file(root.join(META))
        };
        return Err(match restore {
            Ok(()) => error,
            Err(restore) => format!("{error}; restore previous metadata: {restore}"),
        });
    }
    for name in aliases {
        if let Err(error) = install(name) {
            eprintln!("{error}; the revision-associated copy is available");
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
}

impl PublicationLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let path = root.join(".lesce_osm.import.lock");
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.try_lock().map_err(|e| {
            format!(
                "{}: {e}; wait for the current import to finish",
                path.display()
            )
        })?;
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
                fs::remove_dir_all(entry.path())
                    .map_err(|e| format!("clean abandoned stage: {e}"))?;
            }
        }
        Ok(Self { _file: file })
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn create(root: &Path) -> Result<Self, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let stage = root.join(format!(".lesce_osm-stage-{}-{stamp}", std::process::id()));
        fs::create_dir(&stage).map_err(|e| e.to_string())?;
        Ok(Self(stage))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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

        let root = Scratch::create(&std::env::temp_dir()).unwrap();
        let stale = root.0.join(".lesce_osm-stage-123-456");
        fs::create_dir(&stale).unwrap();
        let lock = PublicationLock::acquire(&root.0).unwrap();
        assert!(!stale.exists());
        assert!(PublicationLock::acquire(&root.0).is_err());
        let stage = Scratch::create(&root.0).unwrap();
        let id = SlotId::new(SLOT).unwrap();
        let revision = snapshot_revision(&ctx.sim);
        let revision_text = format!("{:032x}", revision.get());
        publish_snapshot(&ctx.sim, &stage.0, &id, revision).unwrap();

        let attribution = root
            .0
            .join(format!("{SLOT}.{revision_text}.ATTRIBUTION-OSM.txt"));
        fs::create_dir(&attribution).unwrap();
        assert!(commit_snapshot(&stage.0, &root.0, &revision_text, &(), false).is_err());
        assert!(!root.0.join(ZIP).exists());
        assert!(!root.0.join(META).exists());
        fs::remove_dir(&attribution).unwrap();
        commit_snapshot(&stage.0, &root.0, &revision_text, &(), false).unwrap();
        verify_catalog(&root.0, &id, revision).unwrap();
        let zip = fs::read(root.0.join(ZIP)).unwrap();
        let metadata = fs::read(root.0.join(META)).unwrap();

        let replacement = SnapshotRevision::new(revision.get() ^ 1);
        let replacement_text = format!("{:032x}", replacement.get());
        publish_snapshot(&ctx.sim, &stage.0, &id, replacement).unwrap();
        assert!(check_target(&root.0, false)
            .unwrap_err()
            .contains("already exists"));
        let obstruction = root
            .0
            .join(format!("{SLOT}.{replacement_text}.import-report.json"));
        fs::create_dir(&obstruction).unwrap();
        assert!(commit_snapshot(&stage.0, &root.0, &replacement_text, &(), true).is_err());
        assert_eq!(fs::read(root.0.join(ZIP)).unwrap(), zip);
        assert_eq!(fs::read(root.0.join(META)).unwrap(), metadata);
        verify_catalog(&root.0, &id, revision).unwrap();
        drop(lock);
        PublicationLock::acquire(&root.0).unwrap();
    }
}
