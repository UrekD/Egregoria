use geom::{Color, Vec2, Vec3};
use ordered_float::OrderedFloat;
use simulation::map::{LaneKind, Map, ProjectFilter, ProjectKind};
use simulation::map_dynamic::transit::preview_stop;
use simulation::map_dynamic::{StopID, Transit};
use simulation::world_command::WorldCommand;
use simulation::Simulation;

use crate::gui::overlays::land_tiles::{preview_edit, show_edit_error};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;

struct StopPreview {
    berth: Vec3,
    dir: Vec2,
    walk_pos: Option<Vec3>,
}

impl StopPreview {
    fn on_road(map: &Map, pos: Vec3) -> Option<Self> {
        let project = map.project(pos, 0.0, ProjectFilter::ROAD | ProjectFilter::INTER);
        let ProjectKind::Road(road_id) = project.kind else {
            return None;
        };
        let road = map.get(road_id)?;
        let driving_lanes = || {
            road.lanes_iter()
                .filter(|&(_, kind)| kind == LaneKind::Driving)
                .filter_map(|(id, _)| map.get(id))
                .filter(|lane| lane.points.n_points() >= 2)
        };

        // The side of the road under the pointer picks the direction. On
        // multi-lane roads use the driving lane closest to that sidewalk.
        let nearest = driving_lanes()
            .min_by_key(|lane| (OrderedFloat(lane.points.project_dist2(pos)), lane.id))?;
        let sidewalk = road
            .sidewalks(nearest.src)
            .outgoing
            .and_then(|id| map.get(id));
        let lane = sidewalk
            .and_then(|sidewalk| {
                let sidewalk_point = sidewalk.points.project(pos);
                driving_lanes()
                    .filter(|lane| lane.src == nearest.src)
                    .min_by_key(|lane| {
                        (
                            OrderedFloat(lane.points.project_dist2(sidewalk_point)),
                            lane.id,
                        )
                    })
            })
            .unwrap_or(nearest);
        let (berth, _, dir) = lane.points.project_segment_dir(pos);
        Some(Self {
            berth,
            dir: dir.xy().try_normalize()?,
            walk_pos: sidewalk.map(|lane| lane.points.project(berth)),
        })
    }

    fn draw(&self, draw: &mut ImmediateDraw, color: Color) {
        let berth = self.berth.up(0.8);
        draw.stroke_circle(berth, 2.2, 0.8).color(color);
        let end = berth + self.dir.z0() * 7.0;
        let back = end - self.dir.z0() * 2.5;
        let wing = self.dir.perpendicular().z0() * 1.6;
        draw.line(berth, end, 0.9).color(color);
        draw.line(back + wing, end, 0.9).color(color);
        draw.line(back - wing, end, 0.9).color(color);

        if let Some(walk_pos) = self.walk_pos {
            let walk_pos = walk_pos.up(0.8);
            draw.line(berth, walk_pos, 0.8).color(color);
            draw.circle(walk_pos, 2.6).color(color);
        }
    }
}

pub(super) fn stop_at(transit: &Transit, pos: Vec3) -> Option<StopID> {
    transit
        .stops
        .values()
        .filter_map(|stop| {
            let distance = pos
                .xy()
                .distance2(stop.walk_pos.xy())
                .min(pos.xy().distance2(stop.berth.xy()));
            (distance <= 10.0 * 10.0).then_some((OrderedFloat(distance), stop.id))
        })
        .min()
        .map(|(_, stop)| stop)
}

pub(super) fn draw_stops(
    transit: &Transit,
    draw: &mut ImmediateDraw,
    hovered: Option<StopID>,
    hover_color: Color,
) {
    for (id, stop) in &transit.stops {
        let color = if hovered == Some(id) {
            hover_color
        } else {
            Color::WHITE
        };
        draw.circle(stop.walk_pos.up(0.8), 3.2).color(color);
        draw.stroke_circle(stop.berth.up(0.8), 2.0, 0.7)
            .color(color);
    }
}

pub(super) fn update(sim: &Simulation, uiworld: &UiWorld, removing: bool) {
    let input = uiworld.read::<InputMap>();
    let mut draw = uiworld.write::<ImmediateDraw>();
    let transit = sim.read::<Transit>();
    let hovered = input
        .unprojected
        .filter(|_| removing)
        .and_then(|pos| stop_at(&transit, pos));
    let colors = simulation::colors();
    let selected = input.just_act.contains(&InputAction::Select);
    let removal = hovered.map(|stop| WorldCommand::TransitRemoveStop { stop });
    let can_remove = removal
        .as_ref()
        .is_some_and(|command| preview_edit(sim, uiworld, command));
    let hover_color = if can_remove {
        colors.gui_danger
    } else {
        colors.gui_disabled
    };
    draw_stops(&transit, &mut draw, hovered, hover_color);

    if removing {
        if let Some(command) = removal.filter(|_| selected && can_remove) {
            uiworld.commands().push(command);
        }
        return;
    }
    let Some(pos) = input.unprojected else {
        return;
    };

    let Some(preview) = StopPreview::on_road(&sim.map(), pos) else {
        draw.stroke_circle(pos.up(0.8), 3.2, 0.8)
            .color(colors.gui_danger);
        return;
    };
    let command = WorldCommand::TransitAddStop {
        pos: preview.berth,
        dir: preview.dir,
    };
    match preview_stop(sim, preview.berth, preview.dir) {
        Ok((walk, berth, dir)) => {
            StopPreview {
                berth,
                dir,
                walk_pos: Some(walk),
            }
            .draw(&mut draw, colors.gui_primary);
            if selected {
                uiworld.commands().push(command);
            }
        }
        Err(reason) => {
            preview.draw(&mut draw, colors.gui_danger);
            show_edit_error(uiworld, "TransitAddStop", reason);
        }
    }
}
