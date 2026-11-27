use crate::gui::roadbuild::PrecisionSnap;
use crate::gui::{ErrorTooltip, PotentialCommands, Tool};
use crate::inputmap::{DeckPicker, InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use geom::{Vec2, Vec3};
use goryak::{minrow, on_secondary_container, padxy, selectable_label_primary, textc};
use simulation::map::building_parking::curb_edge;
use simulation::map::{Map, MapProject, PathEndpoint, ProjectFilter, ProjectKind};
use simulation::world_command::WorldCommand;
use simulation::Simulation;

#[derive(Default)]
pub(crate) struct FootpathTool {
    curved: bool,
    start: Option<PathEndpoint>,
    end: Option<PathEndpoint>,
}

impl FootpathTool {
    pub(crate) fn cancel(&mut self) -> bool {
        let had_start = self.start.take().is_some();
        self.end = None;
        had_start
    }
}

pub(crate) fn properties(uiw: &UiWorld) {
    let mut state = uiw.write::<FootpathTool>();
    padxy(12.0, 8.0, || {
        minrow(8.0, || {
            if selectable_label_primary(!state.curved, "Straight").clicked {
                state.curved = false;
                state.end = None;
            }
            if selectable_label_primary(state.curved, "Curved").clicked {
                state.curved = true;
            }
            let instruction = match (state.start, state.end) {
                (_, Some(_)) => "Click to set the bend",
                (Some(_), None) => "Choose the end · Hold Shift for precision snapping",
                _ => "Choose a start on the ground, a footpath, or a sidewalk",
            };
            textc(on_secondary_container(), instruction);
        });
    });
}

fn centimeters(point: Vec2) -> [i32; 2] {
    [
        (point.x * 100.0).round() as i32,
        (point.y * 100.0).round() as i32,
    ]
}

fn marker_position(map: &Map, endpoint: PathEndpoint, position: Vec3) -> Vec3 {
    if let PathEndpoint::Sidewalk(anchor) = endpoint {
        if let Some((edge, outward)) = curb_edge(map, anchor) {
            return edge - outward.z0() * 1.5;
        }
    }
    position
}

pub(crate) fn update(sim: &Simulation, uiw: &UiWorld) {
    if *uiw.read::<Tool>() != Tool::Footpath {
        return;
    }
    let mut potential = uiw.write::<PotentialCommands>();
    potential.0.clear();
    let mut state = uiw.write::<FootpathTool>();
    let input = uiw.read::<InputMap>();
    let over_ui = goryak::pointer_over_ui(input.screen.x, input.screen.y)
        || engine::pointer_over_egui(input.screen);
    if over_ui {
        return;
    }
    let map = sim.map();
    let surfaces = input.surface_hits(map.environment());
    let deck = input.ray.and_then(|ray| {
        DeckPicker::new(&map).pick(ray, &surfaces, ProjectFilter::ROAD | ProjectFilter::INTER)
    });
    let Some(pointer) = deck.map(|hit| hit.pos).or(input.unprojected) else {
        return;
    };
    let click = input.just_act.contains(&InputAction::Select) && uiw.can_author();
    let mut draw = uiw.write::<ImmediateDraw>();
    let mut snapped = pointer;
    let start = state.start.and_then(|endpoint| {
        map.path_endpoint_position(endpoint)
            .map(|pos| (endpoint, pos))
    });
    if state.start.is_some() && start.is_none() {
        state.cancel();
        *uiw.write::<ErrorTooltip>() = ErrorTooltip::new("The selected footpath start changed");
    }
    let no_snapping = input.act.contains(&InputAction::NoSnapping);
    let attachment = (!no_snapping && state.end.is_none())
        .then(|| {
            let nearby = deck.or_else(|| {
                let hit = map.project(pointer, 2.0, ProjectFilter::ROAD | ProjectFilter::INTER);
                (!hit.is_ground()).then_some(hit)
            });
            map.pick_path_attachment(nearby?.kind, pointer)
        })
        .flatten();
    if let Some((endpoint, anchor)) = start {
        if !no_snapping && attachment.is_none() && input.act.contains(&InputAction::PrecisionRoad) {
            let start = MapProject {
                pos: anchor,
                kind: endpoint
                    .road()
                    .map_or(ProjectKind::Ground, ProjectKind::Road),
            };
            let snap = PrecisionSnap::at(&map, start, anchor, pointer);
            snapped = snap
                .point
                .z(map.environment().height(snap.point).unwrap_or(pointer.z));
            snap.draw(&mut draw, anchor, snapped, 3.0);
        }
    }
    let [x, y] = centimeters(snapped.xy());
    let cursor = attachment.or_else(|| {
        if deck.is_some_and(|hit| {
            (hit.pos.z - map.environment().height(hit.pos.xy()).unwrap_or(hit.pos.z)).abs() > 0.8
        }) && !no_snapping
            && state.end.is_none()
        {
            return None;
        }
        map.pick_path_ground(x, y)
    });
    let cursor_pos = cursor.and_then(|endpoint| map.path_endpoint_position(endpoint));
    let success = simulation::colors().gui_success;
    let danger = simulation::colors().gui_danger;
    let Some((from, a)) = start else {
        if let Some((endpoint, pos)) = cursor.zip(cursor_pos) {
            let marker = marker_position(&map, endpoint, pos);
            draw.stroke_circle(marker.up(0.2), 1.4, 0.45).color(success);
            if click {
                state.start = cursor;
            }
        } else {
            draw.stroke_circle(pointer.up(0.2), 1.4, 0.45).color(danger);
        }
        return;
    };
    let Some(to) = state.end.or(cursor) else {
        return;
    };
    let Some(b) = map.path_endpoint_position(to) else {
        state.end = None;
        return;
    };
    let control = state.end.map(|_| [x, y]);
    let command = WorldCommand::MapMakeWalkingPath { from, to, control };
    let markers = [marker_position(&map, from, a), marker_position(&map, to, b)];
    drop(map);
    let preview = command.preview(sim);
    let choosing_end = state.curved && state.end.is_none();
    let color = if preview.is_ok() || choosing_end {
        success
    } else {
        danger
    };
    for marker in markers {
        draw.stroke_circle(marker.up(0.2), 1.4, 0.45).color(color);
    }
    if let Some(road) = preview
        .as_ref()
        .ok()
        .and_then(|preview| preview.road.as_ref())
    {
        draw.polyline(road.deck.as_slice().to_vec(), 3.0, false)
            .color(color.a(0.55));
    } else if a.distance2(b) > 0.01 {
        draw.line(a.up(0.25), b.up(0.25), 3.0).color(color.a(0.55));
    }
    if choosing_end {
        if click && from != to {
            state.end = Some(to);
        }
        return;
    }
    match preview {
        Ok(_) if click => {
            uiw.commands().push(command);
            state.cancel();
        }
        Ok(_) => potential.set(command),
        Err(reason) => {
            *uiw.write::<ErrorTooltip>() = ErrorTooltip::new(
                crate::gui::notifications::command_message("MapMakeWalkingPath", reason),
            );
        }
    }
}
