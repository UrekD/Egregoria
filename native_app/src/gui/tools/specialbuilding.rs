use crate::gui::{ErrorTooltip, PotentialCommands, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use crate::uiworld::UiWorld;
use engine::AudioKind;
use geom::{Degrees, Intersect, OBB};
use prototypes::{RenderAsset, Size2D};
use simulation::map::special_placement::{snap_special_building, PlacementError};
use simulation::map::{ProjectFilter, RoadID};
use simulation::world_command::WorldCommand;
use simulation::Simulation;
use std::borrow::Cow;

pub struct SpecialBuildArgs {
    pub obb: OBB,
    pub connected_road: Option<RoadID>,
}

pub struct SpecialBuildKind {
    pub make: Box<dyn Fn(&SpecialBuildArgs) -> Vec<WorldCommand> + Send + Sync + 'static>,
    pub size: Size2D,
    pub asset: RenderAsset,
    pub road_snap: bool,
}

#[derive(Default)]
pub struct SpecialBuildingResource {
    pub opt: Option<SpecialBuildKind>,
    pub last_obb: Option<OBB>,
    pub rotation: Degrees,
}

/// SpecialBuilding tool
/// Allows to build special buildings like farms, factories, etc.
pub fn specialbuilding(sim: &Simulation, uiworld: &UiWorld) {
    profiling::scope!("gui::specialbuilding");
    let mut state = uiworld.write::<SpecialBuildingResource>();
    let tool = *uiworld.read::<Tool>();
    let inp = uiworld.read::<InputMap>();
    let mut draw = uiworld.write::<ImmediateDraw>();
    let mut sound = uiworld.write::<ImmediateSound>();

    let map = sim.map();

    let commands = &mut *uiworld.commands();

    if !matches!(tool, Tool::SpecialBuilding) {
        state.opt = None;
        return;
    }

    if inp.act.contains(&InputAction::Rotate) {
        state.rotation += Degrees(inp.wheel);
        state.rotation.normalize();
    }

    let SpecialBuildKind {
        size,
        ref asset,
        ref make,
        road_snap,
    } = *unwrap_or!(&state.opt, return);

    let mpos = unwrap_ret!(inp.unprojected);
    let hover_obb = OBB::new(mpos.xy(), state.rotation.vec2(), size.w, size.h);

    let mut draw = |obb: OBB, height, red| {
        let col = if red {
            simulation::colors().gui_danger.adjust_luminosity(1.3)
        } else if road_snap {
            simulation::colors().gui_success.adjust_luminosity(1.5)
        } else {
            simulation::colors().gui_primary.adjust_luminosity(1.5)
        };

        match asset {
            RenderAsset::Mesh { path } => {
                draw.mesh(
                    path.to_string_lossy().to_string(),
                    obb.center().z(height),
                    obb.axis()[0].normalize().z0(),
                )
                .color(col);
            }
            RenderAsset::Sprite { path } => {
                draw.textured_obb(obb, path.to_string_lossy().to_string(), height + 0.1)
                    .color(col);
            }
        }
    };

    let (obb, rid, height) = if road_snap {
        match snap_special_building(&map, mpos, size) {
            Ok(snap) => (snap.obb, Some(snap.connected_road), snap.height),
            Err(error) => {
                let message = match error {
                    PlacementError::NoRoad => Cow::Borrowed("No road nearby"),
                    PlacementError::NoSidewalk => Cow::Borrowed("Sidewalk required"),
                    PlacementError::InsufficientFrontage {
                        required_m,
                        available_m,
                    } => Cow::Owned(format!(
                        "Needs {required_m:.1} m of road frontage, this road has {available_m:.1} m — use a longer road"
                    )),
                    PlacementError::IntersectionOverlap | PlacementError::Occupied => {
                        Cow::Borrowed("Intersecting with something")
                    }
                    PlacementError::InvalidTerrain => Cow::Borrowed("Invalid terrain"),
                };
                *uiworld.write::<ErrorTooltip>() = ErrorTooltip::new(message);
                return draw(hover_obb, mpos.z, true);
            }
        }
    } else {
        (hover_obb, None, mpos.z)
    };

    if (!road_snap
        && map
            .spatial_map()
            .query(
                obb,
                ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING,
            )
            .next()
            .is_some())
        || state.last_obb.map(|x| x.intersects(&obb)).unwrap_or(false)
    {
        *uiworld.write::<ErrorTooltip>() =
            ErrorTooltip::new(Cow::Borrowed("Intersecting with something"));
        draw(obb, height, true);
        return;
    }

    draw(obb, height, false);

    let cmds: Vec<WorldCommand> = make(&SpecialBuildArgs {
        obb,
        connected_road: rid,
    });
    if inp.act.contains(&InputAction::Select) {
        commands.extend(cmds);
        sound.play("road_lay", AudioKind::Ui);
        state.last_obb = Some(obb);
    } else if let Some(last) = cmds.last() {
        uiworld.write::<PotentialCommands>().set(last.clone());
    }
}
