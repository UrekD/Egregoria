use crate::builders::{grant_money, place_company, seed_households};
use crate::ScenarioCtx;
use geom::{vec2, vec3, Vec2, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID, GoodsCompanyID, Money};
use simulation::economy::balance::DEMAND_INTERVAL_TICKS;
use simulation::map::{
    BuildingID, BuildingKind, LanePatternBuilder, LotLayout, MapProject, ProjectFilter,
    ProjectKind, RoadID, WaterBasin, WaterBodyId, WaterEdit, WaterStroke,
};
use simulation::map_dynamic::{EnvGrid, UtilityFlow};
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand};
use simulation::{SavePhase, Simulation, SlotCamera, SlotId, SnapshotRevision};
use std::ffi::OsString;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

pub(super) const COAST: Vec2 = vec2(24.0, 760.0);
pub(super) const UPSTREAM: Vec2 = vec2(600.0, 760.0);
pub(super) const DOWNSTREAM: Vec2 = vec2(1384.0, 840.0);
pub(super) const LAKE: Vec2 = vec2(1048.0, 1496.0);
pub(super) const BANK: Vec2 = vec2(744.0, 696.0);
pub(super) const FAR_INLAND: Vec2 = vec2(744.0, 488.0);
const BANK_ROAD_Y: f32 = 640.0;
pub(super) const SEAM_STAMP: OBB = OBB {
    corners: [
        vec2(976.0, 704.0),
        vec2(1072.0, 704.0),
        vec2(1072.0, 784.0),
        vec2(976.0, 784.0),
    ],
};

pub(super) struct Bodies {
    pub river: WaterBodyId,
    pub lake: WaterBodyId,
}

pub(super) fn install_water(sim: &mut Simulation) -> Result<Bodies, String> {
    // A temporary edge-reaching cut supplies a known sea bed on Flat terrain.
    let coast = carve(
        sim,
        &[vec2(32.0, 480.0), vec2(48.0, 752.0), vec2(48.0, 1072.0)],
        176.0,
        8.0,
    )?;
    remove(sim, coast)?;
    sea(sim, Some(0))?;

    let river = carve(
        sim,
        &[vec2(352.0, 768.0), vec2(1024.0, 768.0), vec2(1712.0, 896.0)],
        96.0,
        5.0,
    )?;
    let lake = sim
        .edit_water(WaterEdit::FillLake {
            basin: WaterBasin::new(&[
                vec2(872.0, 1456.0),
                vec2(944.0, 1352.0),
                vec2(1104.0, 1376.0),
                vec2(1224.0, 1504.0),
                vec2(1112.0, 1648.0),
                vec2(960.0, 1616.0),
            ])
            .map_err(|error| format!("lake basin: {error}"))?,
            level_dm: 10,
            depth_m: 6.0,
        })
        .map_err(|error| format!("fill lake: {error}"))?
        .body
        .ok_or("fill lake returned no body")?;
    Ok(Bodies { river, lake })
}

fn carve(
    sim: &mut Simulation,
    points: &[Vec2],
    width_m: f32,
    depth_m: f32,
) -> Result<WaterBodyId, String> {
    sim.edit_water(WaterEdit::CarveRiver {
        polyline: WaterStroke::new(points).map_err(|error| format!("river stroke: {error}"))?,
        width_m,
        depth_m,
        level_dm: Some(0),
    })
    .map_err(|error| format!("carve river: {error}"))?
    .body
    .ok_or_else(|| "carve river returned no body".into())
}

fn sea(sim: &mut Simulation, level_dm: Option<i16>) -> Result<(), String> {
    sim.edit_water(WaterEdit::SetSeaLevel { level_dm })
        .map_err(|error| format!("set sea to {level_dm:?}: {error}"))?;
    Ok(())
}

pub(super) fn remove(sim: &mut Simulation, body: WaterBodyId) -> Result<(), String> {
    sim.edit_water(WaterEdit::RemoveFreshWater { body })
        .map_err(|error| format!("remove water body {body:?}: {error}"))?;
    Ok(())
}

pub(super) fn station_at(center: Vec2) -> WorldCommand {
    let id = FreightStationPrototypeID::new("freight-station");
    let size = id.prototype().size;
    WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(center, Vec2::X, size.w, size.h),
        kind: BuildingKind::RailFreightStation(id),
        gen: BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        },
        zone: None,
        connected_road: None,
    }
}

pub(super) fn add_bank_roads(sim: &mut Simulation) -> Result<(), String> {
    let roads = [BANK_ROAD_Y, 992.0].map(|y| WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(448.0, y, 0.0)),
        to: MapProject::ground(vec3(1280.0, y, 0.0)),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    });
    sim.apply_commands(&roads);
    let outcomes = sim.read::<CommandOutcomes>();
    if outcomes.accepted != [0, 1] {
        return Err(format!("bank roads: {:?}", outcomes.last));
    }
    Ok(())
}

fn build_fixture() -> Result<(Simulation, Bodies, [BuildingID; 2]), String> {
    let mut sim = Simulation::new_authored_flat(4, 0).map_err(|error| error.to_string())?;
    let bodies = install_water(&mut sim)?;
    add_bank_roads(&mut sim)?;
    sim.apply_commands([&station_at(vec2(608.0, 512.0))]);
    if sim.read::<CommandOutcomes>().accepted != [0] {
        return Err(format!(
            "dry bank station: {:?}",
            sim.read::<CommandOutcomes>().last
        ));
    }
    let sites = [vec2(864.0, 670.0), vec2(1144.0, 609.0)];
    let homes = sites.map(|site| {
        let map = sim.map();
        map.lots()
            .values()
            .min_by(|a, b| {
                a.shape
                    .center()
                    .distance2(site)
                    .total_cmp(&b.shape.center().distance2(site))
            })
            .map(|lot| lot.id)
    });
    let [Some(bank), Some(inland)] = homes else {
        return Err("fixture needs buildable bank and inland lots".into());
    };
    sim.apply_commands([
        &WorldCommand::MapBuildHouse(bank),
        &WorldCommand::MapBuildHouse(inland),
    ]);
    if sim.read::<CommandOutcomes>().accepted != [0, 1] {
        return Err(format!(
            "land value homes: {:?}",
            sim.read::<CommandOutcomes>().last
        ));
    }
    let homes: Vec<_> = sim
        .map()
        .buildings()
        .values()
        .filter(|building| building.kind == BuildingKind::House)
        .map(|building| building.id)
        .collect();
    let homes = homes
        .try_into()
        .map_err(|_| "fixture needs two land value homes")?;
    Ok((sim, bodies, homes))
}

fn land_values(sim: &mut Simulation, homes: [BuildingID; 2]) -> Result<[u8; 2], String> {
    let mut schedule = Simulation::schedule();
    for _ in 0..=DEMAND_INTERVAL_TICKS {
        sim.tick(&mut schedule, &[]);
    }
    let env = sim.read::<EnvGrid>();
    if let [Some(bank), Some(inland)] = homes.map(|id| env.land_value_sample(id)) {
        Ok([bank, inland])
    } else {
        Err("fixture homes did not receive land value samples".into())
    }
}

/// Publish disposable map pairs for native water acceptance.
pub fn generate(root: &Path) -> Result<(), String> {
    let root = reserve_output(root)?;
    ScenarioCtx::initialize();
    let (mut sim, bodies, wet_homes) = build_fixture()?;

    // Reload must invalidate meshes even when both worlds have the same water revision.
    // Three sea-level changes on the wet slot balance the three removals on the dry slot.
    // Both slots retain the same packed beds and use the same road and building sites.
    for level in [-10, -20, 0] {
        sea(&mut sim, Some(level))?;
    }
    super::water_basic::check_water(&sim, &bodies)?;

    let (mut dry, dry_bodies, dry_homes) = build_fixture()?;
    remove(&mut dry, dry_bodies.river)?;
    remove(&mut dry, dry_bodies.lake)?;
    sea(&mut dry, None)?;
    {
        let map = dry.map();
        let env = map.environment();
        if env.water_revision() != sim.map().environment().water_revision()
            || env
                .chunks()
                .any(|(chunk, _)| env.water_tiles(chunk).next().is_some())
            || !env.base_height(LAKE).is_some_and(|height| height < 0.0)
            || env.height(LAKE) != Some(0.0)
        {
            return Err("dry fixture must expose its negative lake bed at the wet revision".into());
        }
    }
    let [bank, inland] = land_values(&mut sim, wet_homes)?;
    let [dry_bank, dry_inland] = land_values(&mut dry, dry_homes)?;
    if bank <= inland || bank <= dry_bank || inland != dry_inland {
        return Err(format!(
            "shoreline value must raise the near home alone (wet {bank}/{inland}, dry {dry_bank}/{dry_inland})"
        ));
    }
    publish(&sim, &root, "water_acceptance", "Water Acceptance")?;
    publish(&dry, &root, "water_dry", "Drained Water Acceptance")
}

fn reserve_output(root: &Path) -> Result<PathBuf, String> {
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    std::fs::create_dir(&root).map_err(|error| {
        format!(
            "could not create fixture directory {}: {error}",
            root.display()
        )
    })?;
    Ok(root)
}

pub fn perf_cli(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    const USAGE: &str =
        "usage: scenarios water-perf-fixtures --save-root <new-dir> [--allow-missing-bridge]";
    if args.next().as_deref().and_then(|arg| arg.to_str()) != Some("--save-root") {
        return Err(USAGE.into());
    }
    let root = PathBuf::from(
        args.next()
            .filter(|root| !root.is_empty() && !root.as_encoded_bytes().starts_with(b"--"))
            .ok_or(USAGE)?,
    );
    let allow_missing_bridge = match args.next().as_deref() {
        None => false,
        Some(flag) if flag == "--allow-missing-bridge" && args.next().is_none() => true,
        _ => return Err(USAGE.into()),
    };
    generate_perf(&root, allow_missing_bridge)
}

fn generate_perf(root: &Path, allow_missing_bridge: bool) -> Result<(), String> {
    let root = reserve_output(root)?;
    ScenarioCtx::initialize();
    for (agents, side) in [(10_000, 30), (39_000, 56)] {
        publish_perf_pair(&root, agents, side, allow_missing_bridge)?;
    }
    Ok(())
}

fn publish_perf_pair(
    root: &Path,
    agents: usize,
    side: u32,
    allow_missing_bridge: bool,
) -> Result<(), String> {
    let (sim, bodies) = build_perf_fixture(agents, side, allow_missing_bridge)?;
    let wet = format!("water_perf_{agents}_wet");
    let dry = format!("water_perf_{agents}_dry");
    publish(&sim, root, &wet, &wet)?;
    let mut drained = Simulation::load_from_slot(root, &SlotId::new(&wet).unwrap())
        .map_err(|error| error.to_string())?;
    remove(&mut drained, bodies.river)?;
    remove(&mut drained, bodies.lake)?;
    sea(&mut drained, None)?;
    if drained.hashes()["world"] != sim.hashes()["world"]
        || !drained.map().roads().keys().eq(sim.map().roads().keys())
        || !drained
            .map()
            .buildings()
            .keys()
            .eq(sim.map().buildings().keys())
        || drained.map().environment().has_water_configuration()
    {
        return Err(format!("{dry}: draining changed the matched workload"));
    }
    publish(&drained, root, &dry, &dry)
}

fn build_perf_fixture(
    agents: usize,
    side: u32,
    allow_missing_bridge: bool,
) -> Result<(Simulation, Bodies), String> {
    const ORIGIN: Vec2 = vec2(2304.0, BANK_ROAD_Y);
    const SPACING: f32 = 280.0;
    let width = ORIGIN.x + side as f32 * SPACING;
    let chunks = (width / 512.0).ceil() as u16;
    let sim = Simulation::new_authored_flat(chunks, 0).map_err(|error| error.to_string())?;
    let mut ctx = ScenarioCtx::from_sim(sim);
    grant_money(&mut ctx, Money::new_bucks(10_000_000));
    apply_all(
        &mut ctx.sim,
        &[
            WorldCommand::SetLotLayout(LotLayout::Uniform),
            WorldCommand::MapLoadTestField {
                pos: ORIGIN + Vec2::splat(side as f32 * SPACING * 0.5),
                size: side,
                spacing: SPACING,
            },
        ],
    )?;
    let grid_roads: Vec<_> = ctx.sim.map().roads().keys().collect();
    let bodies = install_water(&mut ctx.sim)?;
    add_bank_roads(&mut ctx.sim)?;
    for (from, to) in [
        (vec2(1280.0, BANK_ROAD_Y), ORIGIN),
        (vec2(1280.0, 992.0), vec2(1936.0, 992.0)),
        (vec2(1936.0, 992.0), vec2(1936.0, BANK_ROAD_Y)),
    ] {
        let link = road_link(&ctx.sim, from, to);
        apply_all(&mut ctx.sim, &[link])?;
    }
    let crossing = road_link(&ctx.sim, vec2(944.0, BANK_ROAD_Y), vec2(944.0, 992.0));
    ctx.sim.apply_commands([&crossing]);
    {
        let outcomes = ctx.sim.read::<CommandOutcomes>();
        if allow_missing_bridge && outcomes.last == [("MapMakeConnection", Rejection::WetFootprint)]
        {
            eprintln!("{agents}-agent fixture: W2a bridge unavailable; banks connect via the dry approach");
        } else if outcomes.accepted != [0] {
            return Err(format!("perf crossing: {:?}", outcomes.last));
        }
    }

    let utility_sites = {
        let map = ctx.sim.map();
        [("water-pump", 608.0), ("sewage-outlet", 848.0)].map(|(name, x)| {
            let ProjectKind::Road(road) = map
                .project(vec3(x, BANK_ROAD_Y, 10.0), 5.0, ProjectFilter::ROAD)
                .kind
            else {
                return Err(format!("{name} needs a bank road"));
            };
            Ok(roadside_company(road, name, vec2(x, 667.0), Vec2::Y))
        })
    };
    let [pump, outlet] = utility_sites;
    apply_all(&mut ctx.sim, &[pump?, outlet?])?;
    place_company(&mut ctx, "coal-power-plant", vec2(1510.0, 500.0))
        .ok_or("could not connect perf power plant")?;
    // Each plant generates 2.46 MW when staffed. Houses and shops together
    // can draw a little over 250 W per resident at these fixture densities.
    for plant in 1..agents.div_ceil(7_000) {
        let site = ORIGIN + vec2(SPACING * (plant as f32 + 0.5), -100.0);
        place_company(&mut ctx, "coal-power-plant", site)
            .ok_or_else(|| format!("could not connect perf power plant at {site:?}"))?;
    }

    // Distribute jobs over the grid so the measured workload has everyday traffic.
    let employers = agents.div_ceil(18);
    let jobs: Vec<_> = {
        let map = ctx.sim.map();
        let size = GoodsCompanyID::new("clothes-store").prototype().size;
        let sites: Vec<_> = grid_roads
            .iter()
            .filter_map(|&id| {
                let road = map.get(id)?;
                let points = road.points();
                let (mid, direction) = points.point_dir_along(points.length() * 0.5);
                let out = direction.xy().normalize().perpendicular();
                let center = mid.xy() + out * (road.width * 0.5 + size.w * 0.5 + 1.0);
                let footprint = OBB::new(center, out, size.w, size.h);
                (!map.building_overlaps(footprint)).then_some((id, center, out))
            })
            .collect();
        if sites.len() < employers {
            return Err(format!(
                "only {} dry job sites for {employers} employers",
                sites.len()
            ));
        }
        (0..employers)
            .map(|index| {
                let (road, center, out) = sites[index * sites.len() / employers];
                roadside_company(road, "clothes-store", center, out)
            })
            .collect()
    };
    apply_all(&mut ctx.sim, &jobs)?;
    let bank_lots = {
        let map = ctx.sim.map();
        [vec2(736.0, 668.0), vec2(736.0, 968.0)].map(|site| {
            map.lots()
                .values()
                .filter(|lot| lot.shape.center().distance2(site) < 96.0 * 96.0)
                .min_by(|a, b| {
                    a.shape
                        .center()
                        .distance2(site)
                        .total_cmp(&b.shape.center().distance2(site))
                })
                .map(|lot| lot.id)
        })
    };
    let [Some(south), Some(north)] = bank_lots else {
        return Err("perf fixture needs a buildable house lot on each bank".into());
    };
    apply_all(
        &mut ctx.sim,
        &[
            WorldCommand::MapBuildHouse(south),
            WorldCommand::MapBuildHouse(north),
        ],
    )?;
    // The debug grid was created without water. Its end lots can extend beyond the
    // usable sidewalk once strict water siting checks the finalized junctions.
    let housing = {
        let map = ctx.sim.map();
        let mut candidates: Vec<_> = map
            .lots()
            .values()
            .filter(|lot| map.has_dry_frontage(lot.parent, &lot.shape))
            .map(|lot| lot.id)
            .collect();
        candidates.sort_unstable();
        let needed = agents - 2;
        if candidates.len() < needed {
            return Err(format!(
                "only {} lots with dry frontage for {needed} additional perf houses",
                candidates.len()
            ));
        }
        candidates.truncate(needed);
        candidates
            .into_iter()
            .map(WorldCommand::MapBuildHouse)
            .collect::<Vec<_>>()
    };
    apply_all(&mut ctx.sim, &housing)?;
    let mut houses: Vec<_> = ctx
        .sim
        .map()
        .buildings()
        .values()
        .filter(|building| building.kind == BuildingKind::House)
        .map(|building| building.id)
        .collect();
    houses.sort_unstable();
    if houses.len() != agents {
        return Err(format!("built {} of {agents} perf houses", houses.len()));
    }
    seed_households(&mut ctx.sim, &houses)?;
    ctx.run_ticks(DEMAND_INTERVAL_TICKS + 2);
    let flow = ctx.sim.read::<UtilityFlow>();
    if !flow.networks().any(|(_, totals)| {
        totals.water_production > 0.0 && totals.sewage_capacity > 0.0 && totals.water_demand > 0.0
    }) {
        return Err("perf fixture did not commission both utility providers".into());
    }
    drop(flow);
    if ctx.sim.world().humans.len() != agents {
        return Err(format!(
            "expected {agents} residents, found {}",
            ctx.sim.world().humans.len()
        ));
    }
    Ok((ctx.sim, bodies))
}

fn roadside_company(road: RoadID, name: &str, center: Vec2, outward: Vec2) -> WorldCommand {
    let id = GoodsCompanyID::new(name);
    let proto = id.prototype();
    WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(center, outward, proto.size.w, proto.size.h),
        kind: BuildingKind::GoodsCompany(id),
        gen: proto.bgen,
        zone: None,
        connected_road: Some(road),
    }
}

fn road_link(sim: &Simulation, from: Vec2, to: Vec2) -> WorldCommand {
    let map = sim.map();
    let project =
        |pos: Vec2| map.project(pos.z(10.0), 8.0, ProjectFilter::INTER | ProjectFilter::ROAD);
    WorldCommand::MapMakeConnection {
        from: project(from),
        to: project(to),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }
}

fn apply_all(sim: &mut Simulation, commands: &[WorldCommand]) -> Result<(), String> {
    sim.apply_commands(commands);
    let outcomes = sim.read::<CommandOutcomes>();
    if outcomes.accepted.len() != commands.len() {
        return Err(format!(
            "perf setup accepted {} of {} commands: {:?}",
            outcomes.accepted.len(),
            commands.len(),
            outcomes.last.first(),
        ));
    }
    Ok(())
}

fn publish(sim: &Simulation, root: &Path, stem: &str, display_name: &str) -> Result<(), String> {
    let id = SlotId::new(stem).map_err(|error| error.to_string())?;
    let mut hasher = DefaultHasher::new();
    (stem, sim.hashes()).hash(&mut hasher);
    let revision = SnapshotRevision::new(u128::from(hasher.finish()));
    match sim
        .encode_slot(revision)
        .and_then(|slot| {
            slot.with_display_name(display_name.to_owned())
                .with_camera(SlotCamera {
                    position: [920.0, 960.0, 0.0],
                    yaw: -0.91,
                    pitch: 1.22,
                    distance: 1950.0,
                })
                .publish(root, &id)
        })
        .map_err(|error| format!("{stem}: {error}"))?
    {
        SavePhase::Complete => {
            println!("Wrote {display_name} ({id}) to {}", root.display());
            Ok(())
        }
        SavePhase::SidecarsFailed { replay, meta } => Err(format!(
            "{stem}: snapshot published but sidecars failed: replay={replay:?}, metadata={meta:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::citymap::{
        plan_town, read_inputs, SourcePaths, SpacingLimits, TownConfig, DEFAULT_LESCE, DEFAULT_SAVA,
    };
    use simulation::map::{RiverBank, TerrainChunkID, WaterKind, ZoneGrid};
    use simulation::world_command::Rejection;

    #[test]
    fn p6_water_perf_pair_has_occupied_and_commissioned_wet_workload() {
        ScenarioCtx::initialize();
        let root = TempRoot::new();
        publish_perf_pair(&root.0, 64, 4, true).unwrap();
        for condition in ["wet", "dry"] {
            let id = SlotId::new(&format!("water_perf_64_{condition}")).unwrap();
            let sim = Simulation::load_from_slot(&root.0, &id).unwrap();
            assert_eq!(sim.world().humans.len(), 64);
            assert!(sim.map().roads().len() > 24);
            assert_eq!(sim.map().environment().is_water(LAKE), condition == "wet");
        }
    }

    #[test]
    #[ignore = "W3 owns live provider revalidation after their shoreline is drained"]
    fn p6_water_provider_totals_update_on_the_first_refresh_after_slot_reload() {
        ScenarioCtx::initialize();
        let root = TempRoot::new();
        publish_perf_pair(&root.0, 64, 4, true).unwrap();
        let sim = Simulation::load_from_slot(&root.0, &SlotId::new("water_perf_64_dry").unwrap())
            .unwrap();
        let active = |sim: &Simulation| {
            sim.read::<UtilityFlow>()
                .networks()
                .any(|(_, t)| t.water_production > 0.0 && t.sewage_capacity > 0.0)
        };
        assert!(active(&sim));
        assert!(sim.read::<UtilityFlow>().water_pending(&sim.map()));
        let mut resumed = ScenarioCtx::from_sim(sim);
        for _ in 0..DEMAND_INTERVAL_TICKS {
            resumed.run_ticks(1);
            if !resumed
                .sim
                .read::<UtilityFlow>()
                .water_pending(&resumed.sim.map())
            {
                assert!(!active(&resumed.sim));
                return;
            }
            assert!(active(&resumed.sim));
        }
        panic!("the restored utility flow did not refresh");
    }

    #[test]
    fn p6_water_fixture_export_preserves_existing_slot_families() {
        let root = TempRoot::new();
        let output = root.0.join("slots");
        std::fs::create_dir(&output).unwrap();
        for name in ["water_acceptance.zip", "water_acceptance_replay.json"] {
            std::fs::write(output.join(name), name).unwrap();
        }
        assert!(generate(&output).is_err());
        for name in ["water_acceptance.zip", "water_acceptance_replay.json"] {
            assert_eq!(std::fs::read_to_string(output.join(name)).unwrap(), name);
        }
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 2);
    }

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let output = std::process::Command::new("mktemp")
                .arg("-d")
                .arg(std::env::temp_dir().join("egregoria-water-fixtures.XXXXXXXXXX"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            Self(String::from_utf8(output.stdout).unwrap().trim_end().into())
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn p6_water_published_slots_support_the_scripted_commands() {
        let root = TempRoot::new();
        check_slots(&root.0.join("slots")).unwrap();
    }

    #[test]
    fn p6_water_citymap_authored_sava_round_trips_both_banks() {
        let root = TempRoot::new();
        let (sim, plan, river) = authored_sava();
        publish(&sim, &root.0, "water_sava_terrain", "CITYMAP Sava Terrain").unwrap();
        let city = Simulation::load_from_slot(&root.0, &SlotId::new("water_sava_terrain").unwrap())
            .unwrap();
        assert_eq!(city.hashes(), sim.hashes());
        let map = city.map();
        let env = map.environment();
        let reach = &plan.river[0].downstream;
        let first = reach.first().unwrap();
        let last = reach.last().unwrap();
        // The exporter clips the authored trace to the west and south map edges.
        assert_eq!(first.x, 0.0);
        assert_eq!(last.y, 0.0);
        for endpoint in [*first, *last] {
            assert_eq!(env.water_at(endpoint).unwrap().body, river);
        }
        let mut previous = None;
        for segment in reach.windows(2).skip(8).step_by(16) {
            let point = (segment[0] + segment[1]) * 0.5;
            let water = env.water_at(point).unwrap();
            let metres = water.reach_m.unwrap();
            assert_eq!(water.body, river);
            assert_eq!(water.kind, WaterKind::River);
            assert!(water.depth_m >= 5.0);
            assert!(previous.is_none_or(|previous| previous < metres));
            for bank in [RiverBank::Left, RiverBank::Right] {
                let contact = env.river_bank_near(river, metres, bank, 64).unwrap();
                assert_eq!(env.water_at(contact.wet_tile.center()).unwrap().body, river);
                assert!(!env.is_water(contact.dry_tile.center()));
            }
            previous = Some(metres);
        }
        // Distinct CITYMAP approaches must stay dry while their crossing centres are wet.
        assert!(!plan.crossings.is_empty());
        for crossing in &plan.crossings {
            assert_eq!(
                env.water_at(crossing.point).unwrap().body,
                river,
                "{}",
                crossing.key
            );
            assert!(
                crossing
                    .approaches
                    .iter()
                    .all(|&point| !env.is_water(point)),
                "{}",
                crossing.key
            );
        }
        let extent = f32::from(plan.config.map_chunks) * TerrainChunkID::SIZE_F32;
        assert!(env.water_at(vec2(extent, first.y)).is_none());
    }

    #[test]
    #[ignore = "W2a/W2b and CITYMAP own the live wet crossings and wet town publication"]
    fn p6_water_citymap_published_wet_crossings_link_both_banks() {
        use crate::citymap::{build_network, resolve_network, route_checks};
        use prototypes::Tick;
        use simulation::map::PathKind;
        use simulation::map_dynamic::Itinerary;

        let root = TempRoot::new();
        let (mut sim, plan, _) = authored_sava();
        build_network(&mut sim, &plan.graph, plan.limits).unwrap();
        publish(&sim, &root.0, "water_citymap", "Wet CITYMAP").unwrap();
        let city =
            Simulation::load_from_slot(&root.0, &SlotId::new("water_citymap").unwrap()).unwrap();
        let map = city.map();
        let network = resolve_network(&map, &plan.graph).unwrap();
        assert!(route_checks(&map, &plan.graph, &network)
            .values()
            .all(|passes| *passes));
        for crossing in &plan.crossings {
            let [a, b] = crossing.approaches.map(|point| point.z(10.0));
            for (from, to) in [(a, b), (b, a)] {
                for mode in [PathKind::Vehicle, PathKind::Pedestrian] {
                    assert!(
                        Itinerary::route(Tick(city.get_tick()), from, to, &map, mode).is_some(),
                        "{}",
                        crossing.key
                    );
                }
            }
        }
    }

    #[test]
    fn p6_water_fresh_waters_bank_zone_grows_housing() {
        use simulation::{MapPreset, NewGameOptions};

        ScenarioCtx::initialize();
        let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
            preset: MapPreset::Waters,
            terrain_chunks: 16,
            ..NewGameOptions::default()
        })
        .unwrap();
        let batches = shot_commands().unwrap();
        for batch in &batches[3..] {
            sim.apply_commands(batch);
            assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
        }
        let mut ctx = ScenarioCtx::from_sim(sim);
        ctx.run_ticks(DEMAND_INTERVAL_TICKS * 3);
        let map = ctx.sim.map();
        assert!(map.buildings().values().any(|building| {
            building.kind == BuildingKind::House
                && (7424.0..7664.0).contains(&building.obb.center().x)
                && (5136.0..5192.0).contains(&building.obb.center().y)
                && !map
                    .environment()
                    .intersects_water(&geom::Polygon::from(building.obb.corners.as_slice()))
        }));
    }

    fn authored_sava() -> (Simulation, crate::citymap::TownPlan, WaterBodyId) {
        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let plan = plan_town(&inputs, TownConfig::default(), SpacingLimits::default()).unwrap();
        let [reach] = plan.river.as_slice() else {
            panic!("CITYMAP needs exactly one authored Sava reach");
        };
        let mut sim = Simulation::new_authored_flat(plan.config.map_chunks, 0).unwrap();
        let river = sim
            .carve_river(
                &reach.downstream,
                reach.source_width_scaled.unwrap_or(88.0),
                5.0,
            )
            .unwrap();
        (sim, plan, river)
    }

    fn check_slots(root: &Path) -> Result<(), String> {
        generate(root)?;
        let load = |name| {
            Simulation::load_from_slot(root, &SlotId::new(name).unwrap())
                .map_err(|error| error.to_string())
        };
        let mut wet = load("water_acceptance")?;
        let dry = load("water_dry")?;
        {
            let (wet_map, dry_map) = (wet.map(), dry.map());
            let (wet, dry) = (wet_map.environment(), dry_map.environment());
            assert_eq!(wet.water_revision(), dry.water_revision());
            for (position, kind) in [
                (COAST, WaterKind::Sea),
                (UPSTREAM, WaterKind::River),
                (LAKE, WaterKind::Lake),
            ] {
                assert_eq!(wet.water_body_kind(position), Some(kind));
                assert!(!dry.is_water(position));
                assert_eq!(wet.base_height(position), dry.base_height(position));
            }
        }

        let commands = shot_commands()?;
        wet.apply_commands(&commands[0]);
        assert_eq!(wet.read::<CommandOutcomes>().accepted, [0]);
        let painted: Vec<_> = (122..134).flat_map(|x| [(x, 88), (x, 89)]).collect();
        let cells = |sim: &Simulation| {
            sim.read::<ZoneGrid>()
                .cells()
                .keys()
                .copied()
                .collect::<Vec<_>>()
        };
        assert_eq!(cells(&wet), painted);

        wet.apply_commands(&commands[1]);
        assert_eq!(
            wet.read::<CommandOutcomes>().last,
            [("ZonePaint", Rejection::WetFootprint)]
        );
        assert_eq!(cells(&wet), painted);

        let before = wet.hashes();
        wet.apply_commands(&commands[2]);
        assert_eq!(
            wet.read::<CommandOutcomes>().last,
            [("EditWater", Rejection::WetFootprint)]
        );
        assert_eq!(wet.hashes(), before);
        Ok(())
    }

    fn shot_commands() -> Result<Vec<Vec<WorldCommand>>, String> {
        let steps: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../scripts/shots/p6_water.json"))
                .map_err(|error| error.to_string())?;
        steps
            .into_iter()
            .filter_map(|mut step| step.get_mut("Commands").map(serde_json::Value::take))
            .map(|commands| serde_json::from_value(commands).map_err(|error| error.to_string()))
            .collect()
    }
}
