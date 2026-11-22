use crate::gui::{ErrorTooltip, PotentialCommands, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use crate::uiworld::UiWorld;
use engine::AudioKind;
use geom::{Degrees, Intersect, OBB};
use prototypes::{RenderAsset, Size2D};
use simulation::map::special_placement::{snap_special_building, PlacementError};
use simulation::map::{ProjectFilter, RoadID};
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand};
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
    pending_batch_seq: u64,
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
    let batch_seq = sim.read::<CommandOutcomes>().batch_seq();

    if let Some(last) = state.last_obb {
        let resolved = if crate::network::is_singleplayer(uiworld) {
            batch_seq != state.pending_batch_seq
        } else {
            uiworld.received_commands().iter().any(|cmd| {
                matches!(cmd, WorldCommand::MapBuildSpecialBuilding { pos, .. } if pos.corners == last.corners)
            })
        };
        if resolved {
            state.last_obb = None;
        }
    }

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

    let (obb, height, cmds, using_fallback) = if road_snap {
        let build_commands = |args: &simulation::map::special_placement::SpecialBuildArgs| {
            let cmds = make(&SpecialBuildArgs {
                obb: args.obb,
                connected_road: Some(args.connected_road),
            });
            for cmd in &cmds {
                cmd.validate_and_price(sim)?;
            }
            Ok(cmds)
        };
        match snap_special_building(&map, mpos, size, build_commands) {
            Ok((snap, cmds)) => (snap.obb, snap.height, cmds, snap.using_fallback),
            Err(failure) => {
                let message = match failure.error {
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
                    PlacementError::CommandRejected(Rejection::InvalidBuilding) => {
                        Cow::Borrowed("Building entrance needs road access")
                    }
                    PlacementError::CommandRejected(Rejection::InsufficientFunds) => {
                        Cow::Borrowed("Not enough money to build here")
                    }
                    PlacementError::CommandRejected(_) => {
                        Cow::Borrowed("Cannot place this building here")
                    }
                };
                *uiworld.write::<ErrorTooltip>() = ErrorTooltip::new(message);
                return if let Some(pose) = failure.attempted {
                    draw(pose.obb, pose.height.unwrap_or(mpos.z), true);
                } else {
                    draw(hover_obb, mpos.z, true);
                };
            }
        }
    } else {
        let cmds = make(&SpecialBuildArgs {
            obb: hover_obb,
            connected_road: None,
        });
        (hover_obb, mpos.z, cmds, false)
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
    if using_fallback {
        *uiworld.write::<ErrorTooltip>() =
            ErrorTooltip::new("Placement uses a nearby road because the closest road is blocked");
    }

    if inp.act.contains(&InputAction::Select) {
        commands.extend(cmds);
        sound.play("road_lay", AudioKind::Ui);
        state.last_obb = Some(obb);
        state.pending_batch_seq = batch_seq;
    } else if let Some(last) = cmds.last() {
        uiworld.write::<PotentialCommands>().set(last.clone());
    }
}
