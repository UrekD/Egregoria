mod normalize;
mod source;

pub use normalize::{normalize, Feature, GeoBounds, LinkClass, NormalizedCity, Site};
pub use source::{read_overpass, OverpassExtract};

pub type ImportError = Box<dyn std::error::Error + Send + Sync>;

pub const LESCE_BOUNDS: GeoBounds = GeoBounds {
    south: 46.343,
    west: 14.133,
    north: 46.378,
    east: 14.188,
};

use crate::builders::{place_company, CommandSink};
use crate::{ensure_repo_root_cwd, ScenarioCtx};
use geom::{vec2, Shape, Vec2, AABB, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID, RollingStockID, Tick, TICKS_PER_HOUR};
use serde::Serialize;
use simulation::economy::{Budget, BudgetCategory, Government};
use simulation::map::import::{ImportedJunction, ImportedLink, ImportedNetwork};
use simulation::map::{
    BuildingID, BuildingKind, Density, LaneID, LaneKind, LanePattern, LanePatternBuilder, LotID,
    Map, PathKind, Pathfinder, ProjectFilter, ProjectKind, RoadID, Traversable, TraverseDirection,
    TraverseKind, ZoneGrid, ZoneKind, ZONE_CELL_SIZE,
};
use simulation::map_dynamic::{BuildingInfos, ElectricityFlow, Itinerary, UtilityFlow};
use simulation::souls::freight_station::FreightTrainState;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{
    MapPreset, NewGameOptions, SavePhase, Simulation, SlotCamera, SlotId, SnapshotRevision,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SLOT: &str = "lesce_osm";
const NAME: &str = "Lesce, Slovenia";
const REPORT: &str = "lesce_osm.import-report.json";
const ATTRIBUTION: &str = include_str!("../../../scripts/osm/ATTRIBUTION-OSM.txt");
const QUERY: &str = include_str!("../../../scripts/osm/lesce.overpassql");
const OPTIONS: NewGameOptions = NewGameOptions {
    preset: MapPreset::Flat,
    seed: 46_360_514_160,
    terrain_chunks: 16,
    starting_money_bucks: 1_000_000,
};
const STATION: Vec2 = vec2(3890.0, 4080.0);
const CAMERA: SlotCamera = SlotCamera {
    position: [3930.0, 4240.0, 10.0],
    yaw: -0.35,
    pitch: 1.22,
    distance: 1490.0,
};

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub source: Source,
    pub revision: String,
    pub feature_counts: BTreeMap<String, usize>,
    pub skipped: Vec<(String, String)>,
    pub synthetic: Vec<String>,
    pub route_checks: BTreeMap<String, bool>,
    pub flat_crossings: Vec<(String, String)>,
    pub short_lanes: Vec<(Vec<u64>, f32)>,
    pub policies: Vec<&'static str>,
    pub opening_budget_cents: i64,
    pub generation_seconds: f64,
    pub snapshot_bytes: u64,
    pub headless: Option<HeadlessReport>,
    pub attribution: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Source {
    pub sha256: String,
    pub timestamp: Option<String>,
    pub bounds: GeoBounds,
    pub query: &'static str,
}

#[derive(Debug, Serialize)]
pub struct HeadlessReport {
    pub ticks: u64,
    pub ticks_per_second: f64,
    pub population_before: usize,
    pub population_after: usize,
    pub homes_before: usize,
    pub homes_after: usize,
    pub developed: BTreeMap<String, usize>,
    pub unserved_buildings: usize,
    pub utility_workers: BTreeMap<String, usize>,
    pub below_perf_baseline: bool,
    pub freight_completed: bool,
}

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
            "Passenger service and the passenger station mesh are omitted.",
        ],
        opening_budget_cents: 0,
        generation_seconds: 0.0,
        snapshot_bytes: 0,
        headless: None,
        attribution: ATTRIBUTION,
    };
    let layout = add_lesce_infrastructure(&mut city, &mut report)?;
    ensure_repo_root_cwd();
    simulation::init::init();
    let (sim, network) = Simulation::from_imported_city(&city.city, &OPTIONS)?;
    report.short_lanes = network
        .short_lanes
        .iter()
        .map(|&(index, length)| (city.city.links[index].source_way_ids.clone(), length))
        .collect();
    let mut ctx = ScenarioCtx::from_sim(sim);
    let facilities = place_facilities(&mut ctx, &city, &network, &layout, &mut report)?;
    check_routes(&ctx.sim, &city, &network, &layout, &facilities, &mut report)?;
    zone_and_seed(&mut ctx, &city, &network, &facilities, &mut report)?;
    if ctx
        .sim
        .read::<Budget>()
        .total(BudgetCategory::StarterInfrastructure)
        != 0
    {
        return Err("import charged for starter infrastructure".into());
    }
    report.opening_budget_cents = ctx.sim.read::<Government>().money.cents();
    if report.opening_budget_cents <= 0 {
        return Err("construction exhausted the opening budget".into());
    }
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
    report.generation_seconds = start.elapsed().as_secs_f64();
    if check {
        report.headless = Some(check_day(loaded, facilities, &report.route_checks)?);
    }

    let diagnostics = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
    let revision_report = format!("{SLOT}.{}.import-report.json", report.revision);
    let revision_attribution = format!("{SLOT}.{}.ATTRIBUTION-OSM.txt", report.revision);
    for (name, content) in [
        (REPORT, diagnostics.as_slice()),
        (revision_report.as_str(), diagnostics.as_slice()),
        ("ATTRIBUTION-OSM.txt", ATTRIBUTION.as_bytes()),
        (revision_attribution.as_str(), ATTRIBUTION.as_bytes()),
    ] {
        write_staged(&stage.0, name, content)?;
    }
    check_target(&root, replace)?;
    publish_snapshot(&ctx.sim, &root, &id, revision)?;
    verify_catalog(&root, &id, revision)
        .map_err(|e| format!("snapshot published but catalog verification failed: {e}"))?;
    for name in [
        revision_attribution.as_str(),
        revision_report.as_str(),
        "ATTRIBUTION-OSM.txt",
        REPORT,
    ] {
        fs::rename(stage.0.join(name), root.join(name))
            .map_err(|e| format!("snapshot published but {name} could not be published: {e}"))?;
    }
    Ok(report)
}

fn source_digest(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start sha256sum: {e}"))?;
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

struct PublicationLock(PathBuf);

impl PublicationLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let path = root.join(".lesce_osm.import.lock");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for PublicationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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

struct Layout {
    utility_road: usize,
    external_rail: usize,
    freight_rail: usize,
    mainline: [simulation::map::import::BoundaryStub; 2],
    motorway: Vec<simulation::map::import::BoundaryStub>,
}

fn add_lesce_infrastructure(
    city: &mut NormalizedCity,
    report: &mut ImportReport,
) -> Result<Layout, String> {
    let road_branch = split_near(city, 168406420, vec2(2698.0, 4270.0))?;
    let siding_switch = split_near(city, 61695232, vec2(3754.0, 4395.0))?;
    let stubs = city.city.boundary_stubs.clone();
    let mainline: Vec<_> = stubs
        .iter()
        .copied()
        .filter(|s| city.classes[s.link] == LinkClass::MainRail)
        .collect();
    let mut mainline: [_; 2] = mainline.try_into().map_err(|stubs: Vec<_>| {
        format!(
            "expected two clipped Lesce mainline ends, found {}",
            stubs.len()
        )
    })?;
    let point = |city: &NormalizedCity, id: usize| city.city.junctions[id].position;
    mainline.sort_by(|a, b| {
        point(city, a.junction)
            .y
            .total_cmp(&point(city, b.junction).y)
    });
    let south_stub = mainline[0];
    let south = point(city, south_stub.junction);
    let line = &city.city.links[south_stub.link].points;
    let inside = if south_stub.at_start {
        line[1]
    } else {
        line[line.len() - 2]
    };
    let direction = (south - inside)
        .try_normalize()
        .ok_or("invalid mainline clip direction")?;
    let rail_end = south + direction * 390.0;
    let rail_end_id = add_junction(city, rail_end);
    let external_rail = add_link(
        city,
        south_stub.junction,
        rail_end_id,
        vec![south, rail_end],
        LinkClass::MainRail,
        LanePatternBuilder::new()
            .rail(true)
            .speed_limit(22.0)
            .build(),
    );
    report
        .synthetic
        .push("390 m mainline extension and external rail market in the southern buffer".into());

    let motorway: Vec<_> = stubs
        .iter()
        .copied()
        .filter(|s| city.classes[s.link] == LinkClass::Motorway)
        .collect();
    if motorway.len() != 4 {
        return Err(format!(
            "expected four clipped A2 carriageways, found {}",
            motorway.len()
        ));
    }
    let (min, max) = LESCE_BOUNDS.limits();
    for (y, outward) in [(min.y, -Vec2::Y), (max.y, Vec2::Y)] {
        let is_on_edge = |stub: &&simulation::map::import::BoundaryStub| {
            (point(city, stub.junction).y - y).abs() < 0.1
        };
        let pair: Vec<_> = motorway.iter().filter(is_on_edge).copied().collect();
        let [outbound, inbound] = if let [a, b] = pair.as_slice() {
            if a.at_start {
                [*b, *a]
            } else {
                [*a, *b]
            }
        } else {
            return Err(format!("could not pair A2 carriageways at y={y}"));
        };
        if outbound.at_start || !inbound.at_start {
            return Err(format!("A2 boundary at y={y} has incompatible directions"));
        }
        let a = point(city, outbound.junction);
        let b = point(city, inbound.junction);
        let side = (b - a).normalize();
        let points = vec![
            a,
            a + outward * 45.0,
            a + outward * 74.0 - side * 8.0,
            (a + b) * 0.5 + outward * 88.0,
            b + outward * 74.0 + side * 8.0,
            b + outward * 45.0,
            b,
        ];
        add_link(
            city,
            outbound.junction,
            inbound.junction,
            points,
            LinkClass::Motorway,
            LanePatternBuilder::new()
                .sidewalks(false)
                .parking(false)
                .n_lanes(2)
                .one_way(true)
                .speed_limit(10.0)
                .build(),
        );
        report.synthetic.push(format!(
            "A2 carriageway turnaround in the {} buffer",
            if y == min.y { "southern" } else { "northern" }
        ));
    }

    let siding_end = vec2(3652.0, 4709.0);
    let siding_end_id = add_junction(city, siding_end);
    let freight_rail = add_link(
        city,
        siding_switch,
        siding_end_id,
        vec![point(city, siding_switch), vec2(3756.0, 4466.0), siding_end],
        LinkClass::OtherRail,
        LanePatternBuilder::new()
            .rail(true)
            .speed_limit(14.0)
            .build(),
    );
    report
        .synthetic
        .push("freight siding parallel to the mainline north of Lesce-Bled".into());

    let from = point(city, road_branch);
    let end = vec2(2130.0, 4150.0);
    let end_id = add_junction(city, end);
    let utility_road = add_link(
        city,
        road_branch,
        end_id,
        vec![from, end],
        LinkClass::Street,
        LanePatternBuilder::new().build(),
    );
    report
        .synthetic
        .push("utility access spur west of Šobčeva cesta".into());
    Ok(Layout {
        utility_road,
        external_rail,
        freight_rail,
        mainline,
        motorway,
    })
}

fn add_junction(city: &mut NormalizedCity, position: Vec2) -> usize {
    let id = city.city.junctions.len();
    city.city.junctions.push(ImportedJunction {
        id,
        position,
        roundabout_radius: None,
    });
    id
}

fn add_link(
    city: &mut NormalizedCity,
    from: usize,
    to: usize,
    points: Vec<Vec2>,
    class: LinkClass,
    pattern: LanePattern,
) -> usize {
    let index = city.city.links.len();
    city.city.links.push(ImportedLink {
        source_way_ids: Vec::new(),
        from,
        to,
        points,
        pattern,
        frontage: false,
    });
    city.classes.push(class);
    index
}

fn split_near(city: &mut NormalizedCity, source: u64, target: Vec2) -> Result<usize, String> {
    let (index, offset, position) = city
        .city
        .links
        .iter()
        .enumerate()
        .filter(|(_, link)| link.source_way_ids.contains(&source))
        .flat_map(|(i, link)| {
            link.points
                .iter()
                .enumerate()
                .map(move |(j, &pos)| (i, j, pos))
        })
        .min_by(|a, b| a.2.distance2(target).total_cmp(&b.2.distance2(target)))
        .filter(|(_, _, pos)| pos.distance(target) < 20.0)
        .ok_or_else(|| format!("missing connection anchor on way/{source}"))?;
    let link = &city.city.links[index];
    if offset == 0 {
        return Ok(link.from);
    }
    if offset == link.points.len() - 1 {
        return Ok(link.to);
    }
    let mut tail = link.clone();
    let junction = add_junction(city, position);
    tail.points.drain(..offset);
    tail.from = junction;
    let first = &mut city.city.links[index];
    first.points.truncate(offset + 1);
    first.to = junction;
    let tail_index = city.city.links.len();
    for stub in &mut city.city.boundary_stubs {
        if stub.link == index && !stub.at_start {
            stub.link = tail_index;
        }
    }
    city.city.links.push(tail);
    city.classes.push(city.classes[index]);
    Ok(junction)
}

struct Facilities {
    plant: BuildingID,
    utilities: Vec<BuildingID>,
    terminal: BuildingID,
    external: BuildingID,
}

fn place_facilities(
    ctx: &mut ScenarioCtx,
    city: &NormalizedCity,
    network: &ImportedNetwork,
    layout: &Layout,
    report: &mut ImportReport,
) -> Result<Facilities, String> {
    let utility_road = network.roads[&layout.utility_road];
    let mut utilities = Vec::new();
    for (name, site) in [
        ("coal-power-plant", vec2(2527.0, 4328.0)),
        ("coal-mine", vec2(2300.0, 4270.0)),
        ("water-pump", vec2(2450.0, 4194.0)),
        ("sewage-outlet", vec2(2230.0, 4151.0)),
    ] {
        let id = place_company(ctx, name, site)
            .filter(|id| ctx.sim.map().buildings()[*id].connected_road == Some(utility_road))
            .ok_or_else(|| format!("could not place {name} on the utility access spur"))?;
        utilities.push(id);
        report
            .synthetic
            .push(format!("{name} on the utility access spur"));
    }
    let plant = utilities[0];

    let (external_site, train_lane, train_dist) = {
        let map = ctx.sim.map();
        let road = &map.roads()[network.roads[&layout.external_rail]];
        let train_lane = road.incoming_lanes_to(road.src)[0].0;
        let points = &map.lanes()[train_lane].points;
        let dist = points.length() * 0.5;
        let (middle, dir) = points.point_dir_along(dist);
        (
            OBB::new(middle.xy(), dir.xy(), 150.0, 72.2),
            train_lane,
            dist,
        )
    };
    let external = ctx
        .sim
        .write::<Map>()
        .build_special_building(
            &external_site,
            BuildingKind::ExternalTrading,
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            None,
            None,
        )
        .ok_or("could not build the external rail market")?;
    ctx.sim.write::<BuildingInfos>().insert(external);

    let (obb, access, door) = freight_site(
        &ctx.sim.map(),
        network.roads[&layout.freight_rail],
        utility_road,
        external_site.center().z0(),
    )?;
    let kind = BuildingKind::RailFreightStation(FreightStationPrototypeID::new("freight-station"));
    apply_checked(
        ctx,
        &[WorldCommand::MapBuildSpecialBuilding {
            pos: obb,
            kind,
            gen: BuildingGen::NoWalkway { door_pos: door },
            zone: None,
            connected_road: Some(access),
        }],
    )?;
    let terminal = ctx
        .sim
        .map()
        .buildings()
        .values()
        .find(|building| building.kind == kind && building.connected_road == Some(access))
        .map(|building| building.id)
        .ok_or("could not build the Lesce freight terminal")?;
    apply_checked(
        ctx,
        &[WorldCommand::SpawnTrain {
            wagons: std::iter::once(RollingStockID::new("locomotive"))
                .chain([RollingStockID::new("freight-wagon"); 3])
                .collect(),
            lane: train_lane,
            dist: train_dist,
        }],
    )?;
    if ctx.sim.world().trains.len() != 1 {
        return Err("freight consist was not spawned".into());
    }
    report.synthetic.push("road-connected freight terminal and locomotive with three freight wagons at the Lesce-Bled freight siding".into());

    let mut service_count = 0;
    for site in &city.sites {
        if site.kind == "passenger station" {
            let name = if site.name.is_empty() {
                "Lesce-Bled"
            } else {
                &site.name
            };
            report.skipped.push((
                site.source.clone(),
                format!("{name} passenger site retained as provenance; no station mesh"),
            ));
            continue;
        }
        let before: BTreeSet<_> = ctx.sim.map().buildings().keys().collect();
        let placed = place_company(ctx, site.kind, site.position);
        let connected = placed.is_some_and(|id| {
            let map = ctx.sim.map();
            let building = &map.buildings()[id];
            building.obb.center().distance(site.position) <= 125.0
                && map.electricity.net_id(id) == map.electricity.net_id(plant)
                && !before.contains(&id)
        });
        if connected {
            service_count += 1;
            *report
                .feature_counts
                .entry(format!("placed_{}", site.kind))
                .or_default() += 1;
        } else {
            if let Some(id) = placed.filter(|id| !before.contains(id)) {
                apply_checked(ctx, &[WorldCommand::MapRemoveBuilding(id)])?;
            }
            report.skipped.push((
                site.source.clone(),
                format!("no clear, connected {} site within 125 m", site.kind),
            ));
        }
    }
    report
        .feature_counts
        .insert("placed_mapped_services".into(), service_count);
    Ok(Facilities {
        plant,
        utilities,
        terminal,
        external,
    })
}

fn freight_site(
    map: &Map,
    rail: RoadID,
    utility: RoadID,
    market: geom::Vec3,
) -> Result<(OBB, RoadID, Vec2), String> {
    let utility_net = map.electricity.net_id(utility);
    let mut best = None::<(f32, OBB, RoadID, Vec2)>;
    // The dispatch target is 75 m along the station and 40 m off its centerline.
    for (lane, _) in map.roads()[rail].lanes_iter() {
        let lane = &map.lanes()[lane];
        let points = &lane.points;
        for offset in (16..points.length() as usize).step_by(8) {
            let (berth, tangent) = points.point_dir_along(offset as f32);
            let Some(dir) = tangent.xy().try_normalize() else {
                continue;
            };
            let across = dir.perpendicular();
            let center = berth.xy() - dir * 75.0 + across * 40.0;
            if center.distance(STATION) > 515.0 {
                continue;
            }
            for width in [84.0, 64.0, 44.0] {
                let obb = OBB::new(center, dir, 156.0, width);
                if map
                    .spatial_map()
                    .query(obb, ProjectFilter::BUILDING | ProjectFilter::INTER)
                    .any(|project| match project {
                        ProjectKind::Building(_) => true,
                        ProjectKind::Intersection(id) => {
                            map.intersections()[id].roads.iter().any(|rid| {
                                map.roads()[*rid]
                                    .lanes_iter()
                                    .any(|(_, kind)| kind == LaneKind::Driving)
                            })
                        }
                        _ => false,
                    })
                {
                    continue;
                }
                let mut passes_over_rail = false;
                let road_overlap =
                    map.spatial_map()
                        .query(obb, ProjectFilter::ROAD)
                        .any(|project| {
                            let ProjectKind::Road(id) = project else {
                                return false;
                            };
                            passes_over_rail |= map.roads()[id]
                                .lanes_iter()
                                .any(|(_, kind)| kind == LaneKind::Rail);
                            map.roads()[id]
                                .lanes_iter()
                                .any(|(_, kind)| kind == LaneKind::Driving)
                        });
                if road_overlap || !passes_over_rail {
                    continue;
                }
                for project in map
                    .spatial_map()
                    .query_around(center, 130.0, ProjectFilter::ROAD)
                {
                    let ProjectKind::Road(id) = project else {
                        continue;
                    };
                    let access = &map.roads()[id];
                    if access.sidewalks(access.src).incoming.is_none()
                        || map.electricity.net_id(id) != utility_net
                    {
                        continue;
                    }
                    let near = access.points().project_2d(center).xy();
                    let to_road = near - center;
                    let door = vec2(
                        to_road.dot(across).clamp(-width * 0.5, width * 0.5),
                        to_road.dot(dir).clamp(-78.0, 78.0),
                    );
                    let door_world = center + across * door.x + dir * door.y;
                    let distance = door_world.distance(near);
                    let score = distance * 3.0 + center.distance(STATION) + (84.0 - width) * 0.5;
                    if distance <= 65.0
                        && best
                            .as_ref()
                            .is_none_or(|(s, _, rid, _)| (score, id) < (*s, *rid))
                        && Itinerary::route(Tick(0), berth, market, map, PathKind::Rail).is_some()
                        && Itinerary::route(Tick(0), market, berth, map, PathKind::Rail).is_some()
                    {
                        best = Some((score, obb, id, door));
                    }
                }
            }
        }
    }
    best.map(|(_, obb, road, door)| (obb, road, door))
        .ok_or_else(|| "no clear, routable freight footprint along the Lesce-Bled siding".into())
}

fn apply_checked(ctx: &mut ScenarioCtx, commands: &[WorldCommand]) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }
    ctx.submit(commands);
    ctx.flush();
    let result = ctx.sim.read::<CommandOutcomes>();
    if !result.last.is_empty() || result.accepted.len() != commands.len() {
        return Err(format!(
            "{} of {} city commands applied, rejections {:?}",
            result.accepted.len(),
            commands.len(),
            result.last
        ));
    }
    Ok(())
}

fn check_routes(
    sim: &Simulation,
    city: &NormalizedCity,
    network: &ImportedNetwork,
    layout: &Layout,
    facilities: &Facilities,
    report: &mut ImportReport,
) -> Result<(), String> {
    let map = sim.map();
    let tick = Tick(sim.get_tick());
    let terminal = &map.buildings()[facilities.terminal];
    let external = &map.buildings()[facilities.external];
    let axis = terminal.obb.axis()[1].normalize();
    let berth =
        (terminal.obb.center() + axis * 75.0 - axis.perpendicular() * 40.0).z(terminal.height);
    let market = external.obb.center().z0();
    for (label, a, b) in [
        ("freight outbound", berth, market),
        ("freight inbound", market, berth),
    ] {
        route_result(
            report,
            label.to_owned(),
            Itinerary::route(tick, a, b, &map, PathKind::Rail).is_some(),
        )?;
    }
    let town = network
        .roads
        .iter()
        .filter(|&(index, _)| city.classes[*index] == LinkClass::Arterial)
        .map(|(_, id)| &map.roads()[*id])
        .filter(|road| {
            road.lanes_iter()
                .filter(|(_, kind)| *kind == LaneKind::Driving)
                .count()
                >= 2
        })
        .min_by(|a, b| {
            a.points()
                .project_2d(STATION)
                .xy()
                .distance2(STATION)
                .total_cmp(&b.points().project_2d(STATION).xy().distance2(STATION))
        })
        .ok_or("could not find a Lesce arterial for routing checks")?;
    let town_mid = town.points().point_along(town.length() * 0.5);
    for (label, a, b) in [
        ("freight road access inbound", town_mid, terminal.door_pos),
        ("freight road access outbound", terminal.door_pos, town_mid),
    ] {
        route_result(
            report,
            label.to_owned(),
            Itinerary::route(tick, a, b, &map, PathKind::Vehicle).is_some(),
        )?;
    }

    let town_lanes: Vec<_> = town
        .lanes_iter()
        .filter_map(|(id, kind)| (kind == LaneKind::Driving).then_some(id))
        .collect();
    for stub in &layout.motorway {
        let link = &city.city.links[stub.link];
        let label = format!(
            "A2 way/{} {}",
            link.source_way_ids[0],
            if stub.at_start {
                "into Lesce"
            } else {
                "out of Lesce"
            }
        );
        let road = &map.roads()[network.roads[&stub.link]];
        let lanes = if stub.at_start {
            road.outgoing_lanes_from(road.src)
        } else {
            road.incoming_lanes_to(road.dst)
        };
        let ok = lanes
            .iter()
            .filter(|(_, kind)| *kind == LaneKind::Driving)
            .all(|&(lane, _)| {
                town_lanes.iter().any(|&town| {
                    let (from, to) = if stub.at_start {
                        (lane, town)
                    } else {
                        (town, lane)
                    };
                    lane_route(&map, tick, PathKind::Vehicle, from, to)
                })
            });
        route_result(report, label, ok)?;
    }
    for (index, label) in [
        (0, "mainline south to north"),
        (1, "mainline north to south"),
    ] {
        let [a, b] = [layout.mainline[index], layout.mainline[1 - index]];
        let first = &map.roads()[network.roads[&a.link]];
        let last = &map.roads()[network.roads[&b.link]];
        let from = first.outgoing_lanes_from(network.junctions[&a.junction])[0].0;
        let to = last.incoming_lanes_to(network.junctions[&b.junction])[0].0;
        route_result(
            report,
            label.into(),
            lane_route(&map, tick, PathKind::Rail, from, to),
        )?;
    }
    let crossing = LESCE_BOUNDS.project(46.3622451, 14.1566347);
    let check_crossing = |way, lane_kind, path_kind| {
        let mut legs = network
            .roads
            .iter()
            .filter(|&(index, _)| city.city.links[*index].source_way_ids.contains(&way))
            .filter_map(|(_, id)| {
                let road = &map.roads()[*id];
                let joint = [road.src, road.dst]
                    .into_iter()
                    .find(|id| map.intersections()[*id].pos.xy().is_close(crossing, 0.1))?;
                Some((road, joint))
            });
        let ((first, a), (last, b)) = (legs.next()?, legs.next()?);
        if a != b || legs.next().is_some() {
            return None;
        }
        for (source, destination) in [(first, last), (last, first)] {
            let from = source
                .incoming_lanes_to(a)
                .iter()
                .find(|(_, kind)| *kind == lane_kind)?
                .0;
            let to = destination
                .outgoing_lanes_from(a)
                .iter()
                .find(|(_, kind)| *kind == lane_kind)?
                .0;
            if !lane_route(&map, tick, path_kind, from, to)
                || !map.intersections()[a]
                    .turns()
                    .any(|turn| turn.id.src == from && turn.id.dst == to)
            {
                return None;
            }
        }
        Some(a)
    };
    let road_crossing = check_crossing(1230075924, LaneKind::Driving, PathKind::Vehicle);
    let rail_crossing = check_crossing(61695232, LaneKind::Rail, PathKind::Rail);
    route_result(
        report,
        "independent road and rail through routes at node/253609833".into(),
        road_crossing
            .zip(rail_crossing)
            .is_some_and(|(road, rail)| road != rail),
    )?;
    Ok(())
}

fn lane_route(map: &Map, tick: Tick, kind: PathKind, from: LaneID, to: LaneID) -> bool {
    kind.path(
        map,
        tick,
        Traversable::new(TraverseKind::Lane(from), TraverseDirection::Forward),
        to,
    )
    .is_some()
}

fn route_result(report: &mut ImportReport, label: String, result: bool) -> Result<(), String> {
    report.route_checks.insert(label.clone(), result);
    if result {
        Ok(())
    } else {
        Err(format!("required route failed: {label}"))
    }
}

#[derive(Clone, Copy)]
struct LotZone {
    kind: ZoneKind,
    density: Density,
    feature: Option<usize>,
    seed_house: bool,
}

fn zone_and_seed(
    ctx: &mut ScenarioCtx,
    city: &NormalizedCity,
    network: &ImportedNetwork,
    facilities: &Facilities,
    report: &mut ImportReport,
) -> Result<(), String> {
    let mut paint = BTreeMap::<(i32, i32), (u8, usize, LotZone)>::new();
    let mut plans = BTreeMap::new();
    let mut synthetic_lots = BTreeMap::<ZoneKind, usize>::new();
    {
        let map = ctx.sim.map();
        let net = map
            .electricity
            .net_id(facilities.plant)
            .ok_or("power plant is disconnected")?;
        let arterials: Vec<_> = network
            .roads
            .iter()
            .filter(|&(index, _)| city.classes[*index] == LinkClass::Arterial)
            .map(|(_, id)| &map.roads()[*id])
            .collect();
        let features: Vec<_> = city
            .features
            .iter()
            .map(|feature| {
                let points = feature.outers.iter().flatten().copied();
                let lo = points.clone().fold(Vec2::splat(f32::INFINITY), Vec2::min);
                let hi = points.fold(Vec2::splat(f32::NEG_INFINITY), Vec2::max);
                (lo, hi)
            })
            .collect();
        let filter = ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING;
        let lots: Vec<_> = map
            .lots()
            .values()
            .filter(|lot| map.electricity.net_id(lot.parent) == Some(net))
            .filter(|lot| {
                !map.spatial_map()
                    .query(lot.shape, filter)
                    .any(|object| object != ProjectKind::Road(lot.parent))
            })
            .collect();
        for lot in &lots {
            let bounds = lot.shape.bbox();
            let cells: Vec<_> = cells_under(lot.shape).map(cell_center).collect();
            let mut votes = BTreeMap::<(bool, ZoneKind, Density), (usize, usize, usize)>::new();
            for (index, feature) in city.features.iter().enumerate() {
                let (lo, hi) = features[index];
                if lo.x > bounds.ur.x
                    || hi.x < bounds.ll.x
                    || lo.y > bounds.ur.y
                    || hi.y < bounds.ll.y
                {
                    continue;
                }
                let samples = cells.iter().filter(|&&p| feature.contains(p)).count();
                let center = feature.building
                    && feature.outers.iter().any(|ring| {
                        let p = normalize::ring_center(ring);
                        lot.shape.contains(p) && feature.contains(p)
                    });
                if samples == 0 && !center {
                    continue;
                }
                let score = samples * 2 + usize::from(center) * 3;
                let vote = votes
                    .entry((feature.building, feature.kind, feature.density))
                    .or_insert((0, index, 0));
                vote.0 += score;
                if score > vote.2 {
                    vote.1 = index;
                    vote.2 = score;
                }
            }
            if let Some(((_, kind, density), (_, index, _))) =
                votes
                    .into_iter()
                    .max_by_key(|((building, _, _), (score, index, _))| {
                        (*building, *score, std::cmp::Reverse(*index))
                    })
            {
                let feature = &city.features[index];
                plans.insert(
                    lot.id,
                    LotZone {
                        kind,
                        density,
                        feature: Some(index),
                        seed_house: feature.house,
                    },
                );
            }
        }

        for (kind, target) in [
            (ZoneKind::Commercial, 30_usize),
            (ZoneKind::Industrial, 32),
            (ZoneKind::Office, 8),
        ] {
            let mut candidates: Vec<_> = lots
                .iter()
                .copied()
                .filter(|lot| {
                    lot.shape.axis()[0].mag2() >= 900.0
                        && !plans.contains_key(&lot.id)
                        && !city.features.iter().any(|feature| {
                            feature
                                .holes
                                .iter()
                                .any(|hole| normalize::ring_contains(hole, lot.shape.center()))
                        })
                })
                .filter_map(|lot| {
                    let center = lot.shape.center();
                    let arterial_distance = arterials
                        .iter()
                        .map(|road| road.points().project_2d(center).xy().distance2(center))
                        .min_by(f32::total_cmp)
                        .unwrap_or(f32::INFINITY)
                        .sqrt();
                    (arterial_distance <= 130.0 && center.distance(STATION) <= 1900.0)
                        .then_some((arterial_distance * 3.0 + center.distance(STATION), lot))
                })
                .collect();
            candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.id.cmp(&b.1.id)));
            let existing = plans.values().filter(|zone| zone.kind == kind).count();
            for (_, lot) in candidates.into_iter().take(target.saturating_sub(existing)) {
                plans.insert(
                    lot.id,
                    LotZone {
                        kind,
                        density: Density::Low,
                        feature: None,
                        seed_house: false,
                    },
                );
                *synthetic_lots.entry(kind).or_default() += 1;
            }
        }
        for lot in lots {
            let Some(&zone) = plans.get(&lot.id) else {
                continue;
            };
            let rank = zone
                .feature
                .map_or(0, |index| 1 + u8::from(city.features[index].building));
            for cell in cells_under(lot.shape) {
                let point = cell_center(cell);
                let protected = city.features.iter().enumerate().any(|(index, feature)| {
                    let (lo, hi) = features[index];
                    rank < 2
                        && !feature.holes.is_empty()
                        && (lo.x..=hi.x).contains(&point.x)
                        && (lo.y..=hi.y).contains(&point.y)
                        && feature
                            .holes
                            .iter()
                            .any(|hole| normalize::ring_contains(hole, point))
                });
                if !protected {
                    let feature = zone.feature.unwrap_or(usize::MAX);
                    let existing = paint.entry(cell).or_insert((rank, feature, zone));
                    if (rank, std::cmp::Reverse(feature))
                        > (existing.0, std::cmp::Reverse(existing.1))
                    {
                        *existing = (rank, feature, zone);
                    }
                }
            }
        }
    }

    let mut commands = Vec::new();
    let mut cells = paint.into_iter().peekable();
    while let Some(((x, y), (_, _, zone))) = cells.next() {
        let mut last = y;
        while cells
            .peek()
            .is_some_and(|((next_x, next_y), (_, _, next))| {
                *next_x == x
                    && *next_y == last + 1
                    && last - y < 49
                    && next.kind == zone.kind
                    && next.density == zone.density
            })
        {
            last += 1;
            cells.next();
        }
        let first = cell_center((x, y));
        let end = cell_center((x, last));
        commands.push(WorldCommand::ZonePaint {
            area: OBB::new(
                (first + end) * 0.5,
                Vec2::X,
                ZONE_CELL_SIZE,
                (last - y + 1) as f32 * ZONE_CELL_SIZE,
            ),
            kind: Some(zone.kind),
            density: zone.density,
        });
    }
    apply_checked(ctx, &commands)?;

    let mut seeds = Vec::<(f32, LotID)>::new();
    {
        let map = ctx.sim.map();
        let grid = ctx.sim.read::<ZoneGrid>();
        for (id, zone) in plans {
            let lot = &map.lots()[id];
            let Some(actual) = grid.lot_zone(&lot.shape) else {
                continue;
            };
            if actual.kind != zone.kind || actual.density != zone.density {
                continue;
            }
            *report
                .feature_counts
                .entry(format!("zoned_{:?}_{:?}_lots", zone.kind, zone.density).to_lowercase())
                .or_default() += 1;
            if zone.seed_house {
                seeds.push((lot.shape.center().distance2(STATION), id));
            }
        }
    }
    seeds.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let count = (seeds.len() * 2 / 3).min(75);
    if count < 12 {
        return Err(format!(
            "only {} usable house lots matched the extract",
            seeds.len()
        ));
    }
    let houses: Vec<_> = seeds
        .into_iter()
        .take(count)
        .map(|(_, lot)| WorldCommand::MapBuildHouse(lot))
        .collect();
    apply_checked(ctx, &houses)?;
    report.feature_counts.insert("seeded_houses".into(), count);
    for (kind, count) in synthetic_lots {
        report
            .synthetic
            .push(format!("{count} {kind:?} frontage lots near arterials"));
    }
    Ok(())
}

fn cells_under(shape: OBB) -> impl Iterator<Item = (i32, i32)> {
    let AABB { ll, ur } = shape.bbox();
    let index = |x: f32| (x / ZONE_CELL_SIZE).floor() as i32;
    (index(ll.x)..=index(ur.x))
        .flat_map(move |x| (index(ll.y)..=index(ur.y)).map(move |y| (x, y)))
        .filter(move |&cell| shape.contains(cell_center(cell)))
}

fn cell_center((x, y): (i32, i32)) -> Vec2 {
    vec2(x as f32 + 0.5, y as f32 + 0.5) * ZONE_CELL_SIZE
}

fn check_day(
    sim: Simulation,
    facilities: Facilities,
    routes: &BTreeMap<String, bool>,
) -> Result<HeadlessReport, String> {
    let mut ctx = ScenarioCtx::from_sim(sim);
    let population_before = ctx.sim.world().humans.len();
    let homes = |sim: &Simulation| {
        sim.map()
            .buildings()
            .values()
            .filter(|b| b.kind == BuildingKind::House)
            .count()
    };
    let homes_before = homes(&ctx.sim);
    let ticks = 24 * TICKS_PER_HOUR;
    let mut freight_departed = false;
    let mut freight_completed = false;
    let start = Instant::now();
    eprintln!("Checking one game day ({ticks} ticks) from the reloaded slot");
    for hour in 0..24 {
        // Supply test cargo to exercise the live dispatch, loading and return systems.
        if hour == 0 {
            let world = ctx.sim.world_mut_unchecked();
            let station = world
                .freight_stations
                .values_mut()
                .find(|station| station.f.building == facilities.terminal)
                .ok_or("reloaded slot has no freight station soul")?;
            station.f.waiting_cargo = 100;
        }
        for _ in 0..(TICKS_PER_HOUR / 300) {
            ctx.run_ticks(300);
            let station = ctx
                .sim
                .world()
                .freight_stations
                .values()
                .find(|station| station.f.building == facilities.terminal)
                .ok_or("freight station soul was lost")?;
            freight_departed |= station
                .f
                .trains
                .iter()
                .any(|(_, state)| matches!(state, FreightTrainState::Moving));
            freight_completed |= freight_departed && station.f.trains.is_empty();
        }
        if (hour + 1) % 4 == 0 {
            eprintln!(
                "Headless hour {}: {} residents, {} ticks/s",
                hour + 1,
                ctx.sim.world().humans.len(),
                (((hour + 1) * TICKS_PER_HOUR) as f64 / start.elapsed().as_secs_f64()).round()
            );
        }
    }
    let ticks_per_second = ticks as f64 / start.elapsed().as_secs_f64();
    let population_after = ctx.sim.world().humans.len();
    let homes_after = homes(&ctx.sim);
    let mut developed = BTreeMap::from([("residential".to_owned(), homes_after)]);
    let map = ctx.sim.map();
    let infos = ctx.sim.read::<BuildingInfos>();
    let flow = ctx.sim.read::<UtilityFlow>();
    let electricity = ctx.sim.read::<ElectricityFlow>();
    let unserved: Vec<_> = map
        .buildings()
        .values()
        .filter(|building| {
            building.id != facilities.external
                && (!(flow.has_water(building.id) && flow.has_sewage(building.id))
                    || map
                        .electricity
                        .net_id(building.id)
                        .is_none_or(|net| electricity.blackout(net)))
        })
        .map(|building| (building.id, building.kind))
        .collect();
    let utility_workers: BTreeMap<_, _> = facilities
        .utilities
        .iter()
        .filter_map(|&id| {
            let proto = map.buildings()[id].kind.as_goods_company()?.prototype();
            let workers = match infos.owner(id) {
                Some(simulation::SoulID::GoodsCompany(id)) => ctx
                    .sim
                    .world()
                    .companies
                    .get(id)
                    .map_or(0, |company| company.workers.0.len()),
                _ => 0,
            };
            Some((proto.label.clone(), workers))
        })
        .collect();
    for building in map.buildings().values() {
        if let Some(company) = building
            .kind
            .as_goods_company()
            .and_then(|id| prototypes::try_prototype(prototypes::GrowableCompanyID::from(id)))
        {
            *developed
                .entry(format!("{:?}", company.zone_kind).to_lowercase())
                .or_default() += 1;
        }
    }
    let staffing = facilities.utilities.iter().all(|&id| {
        let proto = map.buildings()[id]
            .kind
            .as_goods_company()
            .unwrap()
            .prototype();
        utility_workers
            .get(&proto.label)
            .is_some_and(|count| *count * 2 >= proto.n_workers as usize)
    });
    let enough_growth = population_after >= 25
        && population_after > population_before
        && homes_after > homes_before
        && ["commercial", "industrial"]
            .iter()
            .any(|kind| developed.get(*kind).is_some_and(|n| *n > 0));
    if !(enough_growth
        && unserved.is_empty()
        && staffing
        && freight_completed
        && routes.values().all(|passed| *passed))
    {
        return Err(format!("one-day gate failed | population {population_before}->{population_after}, homes {homes_before}->{homes_after}, developed {developed:?}, unserved {} ({:?}), workers {utility_workers:?}, freight completed {freight_completed}, {ticks_per_second:.1} ticks/s",
            unserved.len(), &unserved[..unserved.len().min(8)]));
    }
    Ok(HeadlessReport {
        ticks,
        ticks_per_second,
        population_before,
        population_after,
        homes_before,
        homes_after,
        developed,
        unserved_buildings: unserved.len(),
        utility_workers,
        below_perf_baseline: ticks_per_second < crate::builtin::perf::MIN_TICKS_PER_SEC,
        freight_completed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osm_publication_preflights_an_existing_slot_and_reports_failed_metadata() {
        ensure_repo_root_cwd();
        simulation::init::init();
        let normalized = normalize(
            read_overpass(include_bytes!("../../tests/fixtures/osm-city-small.json").as_slice())
                .unwrap(),
            LESCE_BOUNDS,
        )
        .unwrap();
        let (sim, _) = Simulation::from_imported_city(&normalized.city, &OPTIONS).unwrap();
        let mut ctx = ScenarioCtx::from_sim(sim);
        let school = place_company(&mut ctx, "school", vec2(3650.0, 3950.0)).unwrap();
        assert!(ctx.sim.map().buildings()[school].connected_road.is_some());

        let scratch = Scratch::create(&std::env::temp_dir()).unwrap();
        let root = &scratch.0;
        let id = SlotId::new(SLOT).unwrap();
        let revision = snapshot_revision(&ctx.sim);
        publish_snapshot(&ctx.sim, root, &id, revision).unwrap();
        verify_catalog(root, &id, revision).unwrap();
        let zip = root.join(format!("{SLOT}.zip"));
        fs::write(&zip, b"player's edited slot").unwrap();
        assert!(check_target(root, false)
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(fs::read(&zip).unwrap(), b"player's edited slot");
        assert!(check_target(root, true).is_ok());

        let meta = root.join(format!("{SLOT}.meta.json"));
        fs::remove_file(&meta).unwrap();
        fs::create_dir(&meta).unwrap();
        let error = publish_snapshot(&ctx.sim, root, &id, revision).unwrap_err();
        assert!(
            error.contains("snapshot published but sidecars failed"),
            "{error}"
        );
        assert!(Simulation::load_from_slot(root, &id).is_ok());
        assert!(verify_catalog(root, &id, revision).is_err());
    }
}
