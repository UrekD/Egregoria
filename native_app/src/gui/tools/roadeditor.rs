use crate::gui::{overlays::land_tiles::preview_edit, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use geom::Color;
use simulation::map::{IntersectionID, LightPolicy, TurnPolicy};
use simulation::map::{ProjectFilter, ProjectKind, TurnKind};
use simulation::world_command::WorldCommand;
use simulation::Simulation;

#[derive(Clone)]
pub struct IntersectionComponent {
    pub id: IntersectionID,
    pub turn_policy: TurnPolicy,
    pub light_policy: LightPolicy,
}

#[derive(Default)]
pub struct RoadEditorResource {
    pub inspect: Option<IntersectionComponent>,
    pub dirty: bool,
}

/// RoadEditor tool
/// Allows to edit intersections properties like turns and signals
pub fn roadeditor(sim: &Simulation, uiworld: &UiWorld) {
    profiling::scope!("gui::roadeditor");
    let tool = uiworld.read::<Tool>();
    let inp = uiworld.read::<InputMap>();
    let mut state = uiworld.write::<RoadEditorResource>();
    let mut imm_draw = uiworld.write::<ImmediateDraw>();
    let map = sim.map();
    let commands = &mut *uiworld.commands();

    if !matches!(*tool, Tool::RoadEditor) {
        state.inspect = None;
        return;
    }

    if let Some(id) = state.inspect.as_ref().map(|x| x.id) {
        if let Some(inter) = map.intersections().get(id) {
            let lanes = map.lanes();
            let mut points_left = 8192_usize;
            for turn in inter.turns().take(256) {
                let Some(remaining) = points_left.checked_sub(turn.points.len()) else { break };
                points_left = remaining;

                let r = common::rand::randhash(turn.id);
                let col = Color::hsv(r * 360.0, 0.8, 0.6, 0.5);

                let or_src = unwrap_cont!(lanes.get(turn.id.src)).orientation_from(inter.id);
                let or_dst = unwrap_cont!(lanes.get(turn.id.dst)).orientation_from(inter.id);

                let p: Vec<_> = std::iter::once(turn.points.first() + or_src.z0() * 0.01)
                    .chain(turn.points.iter().copied())
                    .chain(std::iter::once(turn.points.last() + or_dst.z0() * 0.01))
                    .map(|x| x.up(0.05 + r * 0.01))
                    .collect();

                imm_draw.polyline(p, 1.0, false).color(col);
                if turn.kind == TurnKind::Driving {
                    crate::gui::traffic_repair::draw_arrow(&mut imm_draw, &turn.points, col.into());
                }
            }
        } else {
            state.inspect = None;
        }
    }

    let mut proj_pos = unwrap_ret!(inp.unprojected);
    let cur_proj = map.project(proj_pos, 10.0, ProjectFilter::INTER);

    let mut proj_col;

    if let ProjectKind::Intersection(id) = cur_proj.kind {
        if Some(id) != state.inspect.as_ref().map(|x| x.id) {
            proj_pos = cur_proj.pos;
        }
        proj_col = simulation::colors().gui_primary;
    } else {
        proj_col = simulation::colors().gui_disabled;
    }

    if inp.just_act.contains(&InputAction::Select) {
        if let ProjectKind::Intersection(id) = cur_proj.kind {
            proj_col = simulation::colors().gui_success;
            proj_pos = cur_proj.pos;
            let inter = &map.intersections()[id];
            state.inspect = Some(IntersectionComponent {
                id,
                turn_policy: inter.turn_policy,
                light_policy: inter.light_policy,
            });
            state.dirty = false;
        }
    }

    imm_draw.circle(proj_pos.up(0.5), 10.0).color(proj_col);

    if state.dirty {
        if let Some(interc) = &mut state.inspect {
            let command = WorldCommand::MapUpdateIntersectionPolicy {
                inter: interc.id,
                turn: interc.turn_policy,
                light: interc.light_policy,
            };
            if preview_edit(sim, uiworld, &command) {
                commands.push(command);
            } else if let Some(inter) = map.get(interc.id) {
                interc.turn_policy = inter.turn_policy;
                interc.light_policy = inter.light_policy;
            }
        }
        state.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rendering::immediate::OrderKind;
    use geom::vec3;
    use scenarios::ScenarioCtx;
    use simulation::map::{LanePatternBuilder, MapProject};
    use simulation::SimulationOptions;

    #[test]
    fn p7_selected_junction_draws_directed_vehicle_arrowheads() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            ..SimulationOptions::LEGACY
        });
        let (junction, expected) = {
            let mut map = ctx.sim.write::<simulation::map::Map>();
            let center = vec3(1500.0, 1500.0, 0.0);
            let endpoint = |pos: geom::Vec3| MapProject {
                pos: pos.xy().z(map.environment().height(pos.xy()).unwrap()),
                kind: ProjectKind::Ground,
            };
            let center = endpoint(center);
            let east = endpoint(center.pos + vec3(140.0, 0.0, 0.0));
            let north = endpoint(center.pos + vec3(0.0, 140.0, 0.0));
            let pattern = LanePatternBuilder::default().build();
            let (junction, _) = map.make_connection(east, center, None, &pattern).unwrap();
            let center = MapProject {
                pos: center.pos,
                kind: ProjectKind::Intersection(junction),
            };
            map.make_connection(north, center, None, &pattern).unwrap();
            let expected = map.intersections()[junction]
                .turns()
                .filter(|turn| turn.kind == TurnKind::Driving)
                .count();
            assert!(expected > 0);
            (junction, expected)
        };
        let uiw = UiWorld::with_defaults();
        *uiw.write::<Tool>() = Tool::RoadEditor;
        let map = ctx.sim.map();
        let inter = &map.intersections()[junction];
        uiw.write::<RoadEditorResource>().inspect = Some(IntersectionComponent {
            id: junction,
            turn_policy: inter.turn_policy,
            light_policy: inter.light_policy,
        });
        drop(map);

        roadeditor(&ctx.sim, &uiw);
        let draw = uiw.read::<ImmediateDraw>();
        assert_eq!(
            draw.orders
                .iter()
                .filter(|order| matches!(order.kind, OrderKind::Line { .. }))
                .count(),
            expected * 2,
            "every legal driving turn needs two lines forming its arrowhead"
        );
    }
}
