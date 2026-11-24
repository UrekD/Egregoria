use crate::ScenarioCtx;
use geom::{vec2, vec3, Vec2, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID};
use simulation::economy::balance::DEMAND_INTERVAL_TICKS;
use simulation::map::{
    BuildingID, BuildingKind, LanePatternBuilder, MapProject, WaterBasin, WaterBodyId, WaterEdit,
    WaterStroke,
};
use simulation::map_dynamic::EnvGrid;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{SavePhase, Simulation, SlotCamera, SlotId, SnapshotRevision};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

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
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
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
    use simulation::map::{WaterKind, ZoneGrid};
    use simulation::world_command::Rejection;

    #[test]
    fn p6_water_published_slots_support_the_scripted_commands() {
        let root =
            std::env::temp_dir().join(format!("egregoria-water-fixtures-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let result = check_slots(&root);
        std::fs::remove_dir_all(&root).unwrap();
        result.unwrap();
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

        let steps: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../scripts/shots/p6_water.json"))
                .map_err(|error| error.to_string())?;
        let mut commands = steps.into_iter().filter_map(|mut step| {
            step.get_mut("Commands")
                .map(|commands| serde_json::from_value::<Vec<WorldCommand>>(commands.take()))
        });
        let mut next_command = || {
            commands
                .next()
                .expect("missing screenshot command batch")
                .map_err(|error| error.to_string())
        };
        wet.apply_commands(&next_command()?);
        assert_eq!(wet.read::<CommandOutcomes>().accepted, [0]);
        let painted = wet.read::<ZoneGrid>().cells().len();
        assert!((1..120).contains(&painted));

        wet.apply_commands(&next_command()?);
        assert_eq!(
            wet.read::<CommandOutcomes>().last,
            [("ZonePaint", Rejection::WetFootprint)]
        );
        assert_eq!(wet.read::<ZoneGrid>().cells().len(), painted);

        wet.apply_commands(&next_command()?);
        assert_eq!(
            wet.read::<CommandOutcomes>().last,
            [("EditWater", Rejection::WetFootprint)]
        );
        Ok(())
    }
}
