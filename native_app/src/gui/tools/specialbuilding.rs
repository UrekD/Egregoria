use crate::gui::{ErrorTooltip, PendingPlacement, PlacementPreview, PotentialCommands, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use crate::uiworld::UiWorld;
use engine::AudioKind;
use geom::{Degrees, Intersect, OBB};
use prototypes::{GoodsCompanyID, RenderAsset, Size2D};
use simulation::economy::progression::{can_build, BuildCapability};
use simulation::map::special_placement::{snap_special_building, PlacementError};
use simulation::map::{ProjectFilter, RoadID};
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand};
use simulation::Simulation;
use std::borrow::Cow;

const MP_PLACEMENT_TIMEOUT_FRAMES: u64 = 300;

pub struct SpecialBuildArgs {
    pub obb: OBB,
    pub connected_road: Option<RoadID>,
}

pub struct SpecialBuildKind {
    pub company: Option<GoodsCompanyID>,
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
    frame: u64,
}

/// SpecialBuilding tool
/// Allows to build special buildings like farms, factories, etc.
pub fn specialbuilding(sim: &Simulation, uiworld: &UiWorld) {
    specialbuilding_input(sim, uiworld, || crate::network::is_singleplayer(uiworld));
}

fn specialbuilding_input(sim: &Simulation, uiworld: &UiWorld, is_local: impl FnOnce() -> bool) {
    profiling::scope!("gui::specialbuilding");
    let mut state = uiworld.write::<SpecialBuildingResource>();
    let tool = *uiworld.read::<Tool>();
    let mut preview = uiworld.write::<PlacementPreview>();
    if tool != Tool::SpecialBuilding {
        state.opt = None;
        state.last_obb = None;
        *preview = PlacementPreview::default();
        return;
    }

    state.frame = state.frame.wrapping_add(1);
    preview.ghost = None;
    uiworld.write::<PotentialCommands>().0.clear();
    let batch_seq = sim.read::<CommandOutcomes>().batch_seq();
    if let Some(pending) = &preview.pending {
        let echoed = uiworld
            .received_commands()
            .iter()
            .any(|command| pending.matches(command));
        let local = is_local();
        let waiting = if local {
            batch_seq == state.pending_batch_seq
        } else {
            state.frame.wrapping_sub(pending.frame) < MP_PLACEMENT_TIMEOUT_FRAMES
        };
        if !echoed && waiting {
            return;
        }
        // Singleplayer publishes only accepted commands. Multiplayer echoes attempted
        // commands, so also check that the exact placement exists in the applied world.
        let accepted = echoed
            && (local
                || sim.map().buildings().values().any(|building| {
                    building.kind == pending.kind
                        && building.obb.corners == pending.obb.corners
                        && building.connected_road == pending.road
                }));
        preview.pending = None;
        state.last_obb = None;
        if accepted {
            state.opt = None;
        }
    }
    preview.company = state.opt.as_ref().and_then(|kind| kind.company);

    let inp = uiworld.read::<InputMap>();
    if goryak::pointer_over_ui(inp.screen.x, inp.screen.y) || engine::pointer_over_egui(inp.screen)
    {
        return;
    }
    let map = sim.map();
    let mut draw = uiworld.write::<ImmediateDraw>();

    if inp.act.contains(&InputAction::Rotate) {
        state.rotation += Degrees(inp.wheel);
        state.rotation.normalize();
    }

    let SpecialBuildKind {
        company,
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

    if let Some(lock) = company.and_then(|id| can_build(sim, BuildCapability::Prototype(id)).err())
    {
        draw(hover_obb, mpos.z, true);
        *uiworld.write::<ErrorTooltip>() =
            ErrorTooltip::new(format!("Unlocks at {} citizens", lock.population));
        return;
    }

    let (obb, height, cmds, road, using_fallback) = if road_snap {
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
            Ok((snap, cmds)) => (
                snap.obb,
                snap.height,
                cmds,
                Some(snap.connected_road),
                snap.using_fallback,
            ),
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
        (hover_obb, mpos.z, cmds, None, false)
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
    preview.ghost = road.map(|road| (obb, road));
    if using_fallback {
        *uiworld.write::<ErrorTooltip>() =
            ErrorTooltip::new("Placement uses a nearby road because the closest road is blocked");
    }

    if inp.just_act.contains(&InputAction::Select) {
        if let Some(WorldCommand::MapBuildSpecialBuilding { kind, .. }) = cmds.last() {
            preview.pending = Some(PendingPlacement {
                kind: *kind,
                obb,
                road,
                frame: state.frame,
            });
        }
        uiworld.commands().extend(cmds);
        uiworld
            .write::<ImmediateSound>()
            .play("road_lay", AudioKind::Ui);
        state.last_obb = Some(obb);
        state.pending_batch_seq = batch_seq;
    } else if let Some(last) = cmds.last() {
        uiworld.write::<PotentialCommands>().set(last.clone());
    }
}

impl PendingPlacement {
    fn matches(&self, command: &WorldCommand) -> bool {
        matches!(command, WorldCommand::MapBuildSpecialBuilding { kind, pos, connected_road, .. }
            if *kind == self.kind && pos.corners == self.obb.corners && *connected_road == self.road)
    }
}

#[cfg(test)]
mod tests {
    use geom::{vec2, vec3, Camera};
    use scenarios::{builders::grid_city, ScenarioCtx};
    use simulation::economy::Government;
    use simulation::map::BuildingKind;
    use simulation::world_command::WorldCommands;
    use simulation::{SimulationOptions, SlotId};

    use super::*;
    use crate::gui::overlays::{effective_overlay, OverlayMode};
    use crate::rendering::OrbitCamera;
    use crate::session::{self, WorldReplacement};
    use crate::uiworld::ReceivedCommands;

    #[test]
    fn u3_multiplayer_placement_releases_pending_when_echo_is_lost() {
        let fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            ..SimulationOptions::LEGACY
        });
        crate::init::init();
        let uiw = UiWorld::with_defaults();
        *uiw.write::<Tool>() = Tool::SpecialBuilding;
        let obb = OBB::new(vec2(700.0, 700.0), geom::Vec2::X, 20.0, 20.0);
        let company = GoodsCompanyID::new("school");
        let prototype = company.prototype();
        {
            let mut state = uiw.write::<SpecialBuildingResource>();
            state.last_obb = Some(obb);
            state.opt = Some(SpecialBuildKind {
                company: Some(company),
                make: Box::new(|_| Vec::new()),
                size: prototype.size,
                asset: prototype.asset.clone(),
                road_snap: true,
            });
        }
        uiw.write::<PlacementPreview>().pending = Some(PendingPlacement {
            kind: BuildingKind::GoodsCompany(company),
            obb,
            road: None,
            frame: 0,
        });

        specialbuilding_input(&fixture.sim, &uiw, || false);
        assert!(uiw.read::<PlacementPreview>().pending.is_some());
        for _ in 1..MP_PLACEMENT_TIMEOUT_FRAMES {
            specialbuilding_input(&fixture.sim, &uiw, || false);
        }
        assert!(uiw.read::<PlacementPreview>().pending.is_none());
        assert!(uiw.read::<SpecialBuildingResource>().last_obb.is_none());
        assert_eq!(uiw.read::<PlacementPreview>().company, Some(company));
    }

    #[test]
    fn u3_placement_restores_manual_mode_across_gestures_and_worlds() {
        let options = SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        };
        let mut fixture = ScenarioCtx::new(options);
        grid_city(&mut fixture, vec2(700.0, 700.0), 2, 2, 160.0);
        crate::init::init();
        let mut uiw = UiWorld::with_defaults();
        let camera = Camera::new(vec3(770.0, 700.0, 0.0), 1024.0, 768.0);
        uiw.insert(camera);
        uiw.insert(OrbitCamera::from_camera(camera));

        let company = GoodsCompanyID::new("school");
        let prototype = company.prototype();
        let kind = BuildingKind::GoodsCompany(company);
        let make = move |args: &SpecialBuildArgs| {
            vec![WorldCommand::MapBuildSpecialBuilding {
                kind,
                pos: args.obb,
                gen: prototype.bgen,
                zone: None,
                connected_road: args.connected_road,
            }]
        };
        let arm = || {
            *uiw.write::<Tool>() = Tool::SpecialBuilding;
            uiw.write::<SpecialBuildingResource>().opt = Some(SpecialBuildKind {
                company: Some(company),
                make: Box::new(make),
                size: prototype.size,
                asset: prototype.asset.clone(),
                road_snap: true,
            });
        };
        let manual = OverlayMode::Transit;
        *uiw.write::<OverlayMode>() = manual;
        let effective = || {
            effective_overlay(
                *uiw.read::<OverlayMode>(),
                *uiw.read::<Tool>(),
                &uiw.read::<PlacementPreview>(),
            )
        };
        let auto = OverlayMode::Coverage(prototypes::ServiceKind::School);

        arm();
        uiw.write::<InputMap>().unprojected = Some(vec3(770.0, 688.0, 0.0));
        specialbuilding(&fixture.sim, &uiw);
        assert_eq!(effective(), auto);
        let ghost = uiw
            .read::<PlacementPreview>()
            .ghost
            .expect("valid school ghost");
        uiw.write::<InputMap>().just_act.insert(InputAction::Select);
        specialbuilding(&fixture.sim, &uiw);
        uiw.write::<InputMap>().just_act.clear();
        assert_eq!(
            uiw.read::<PlacementPreview>()
                .pending
                .as_ref()
                .unwrap()
                .obb
                .corners,
            ghost.0.corners
        );
        let batch = std::mem::take(&mut *uiw.commands());
        assert_eq!(batch.as_ref().len(), 1);

        let money = fixture.sim.read::<Government>().money;
        fixture.sim.write::<Government>().money = prototypes::Money::ZERO;
        fixture.sim.apply_commands(batch.iter());
        assert!(fixture.sim.read::<CommandOutcomes>().accepted.is_empty());
        specialbuilding(&fixture.sim, &uiw);
        assert!(uiw.read::<PlacementPreview>().pending.is_none());
        assert_eq!(effective(), auto);

        fixture.sim.write::<Government>().money = money;
        uiw.write::<InputMap>().just_act.insert(InputAction::Select);
        specialbuilding(&fixture.sim, &uiw);
        uiw.write::<InputMap>().just_act.clear();
        let batch = std::mem::take(&mut *uiw.commands());
        fixture.sim.apply_commands(batch.iter());
        assert_eq!(fixture.sim.read::<CommandOutcomes>().accepted, [0]);
        *uiw.write::<ReceivedCommands>() =
            ReceivedCommands::new(WorldCommands::from(batch.as_ref().to_vec()));
        specialbuilding(&fixture.sim, &uiw);
        assert_eq!(effective(), manual);
        assert!(uiw.read::<PlacementPreview>().ghost.is_none());

        arm();
        specialbuilding(&fixture.sim, &uiw);
        session::close_modal(&uiw);
        assert_eq!(effective(), manual);
        specialbuilding(&fixture.sim, &uiw);

        arm();
        specialbuilding(&fixture.sim, &uiw);
        *uiw.write::<Tool>() = Tool::RoadbuildStraight;
        specialbuilding(&fixture.sim, &uiw);
        assert_eq!(effective(), manual);
        assert!(uiw.read::<SpecialBuildingResource>().opt.is_none());

        arm();
        specialbuilding(&fixture.sim, &uiw);
        let next = ScenarioCtx::new(options).sim;
        session::apply_world_replacement(
            &uiw,
            &mut fixture.sim,
            &mut Simulation::schedule(),
            WorldReplacement::Snapshot(next, SlotId::new("u3-overlay-test").unwrap()),
            |_| {},
        );
        assert_eq!(effective(), manual);
        assert!(uiw.read::<PlacementPreview>().company.is_none());
        assert!(uiw.read::<SpecialBuildingResource>().opt.is_none());
    }
}
