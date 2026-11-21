use crate::gui::Tool;
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::MapRenderer;
use crate::uiworld::UiWorld;
use geom::{vec2, LinearColor, Vec2, AABB, OBB};
use goryak::{is_hovered, minrow, on_secondary_container, padxy, selectable_label_primary, textc};
use simulation::map::{Density, ZoneKind, ZONE_CELL_SIZE};
use simulation::world_command::WorldCommand;
use simulation::Simulation;

pub const ZONES: [ZoneKind; 4] = [
    ZoneKind::Residential,
    ZoneKind::Commercial,
    ZoneKind::Industrial,
    ZoneKind::Office,
];

const MAX_SIDE_CELLS: f32 = 400.0 / ZONE_CELL_SIZE;
const MAX_BRUSH_RADIUS: u8 = 5;

pub fn zone_name(zone: ZoneKind) -> &'static str {
    match zone {
        ZoneKind::Residential => "Residential",
        ZoneKind::Commercial => "Commercial",
        ZoneKind::Industrial => "Industrial",
        ZoneKind::Office => "Office",
    }
}

pub fn zone_color(zone: ZoneKind) -> LinearColor {
    match zone {
        ZoneKind::Residential => LinearColor::new(0.22, 0.78, 0.32, 1.0),
        ZoneKind::Commercial => LinearColor::new(0.19, 0.55, 0.99, 1.0),
        ZoneKind::Industrial => LinearColor::new(0.98, 0.63, 0.13, 1.0),
        ZoneKind::Office => LinearColor::new(0.69, 0.38, 0.94, 1.0),
    }
}

pub struct ZoningState {
    kind: Option<ZoneKind>,
    density: Density,
    rectangle: bool,
    brush_radius: u8,
    drag_start: Option<Vec2>,
    last_brush: Option<Vec2>,
    over_controls: bool,
}

impl Default for ZoningState {
    fn default() -> Self {
        Self {
            kind: Some(ZoneKind::Residential),
            density: Density::Low,
            rectangle: false,
            brush_radius: 1,
            drag_start: None,
            last_brush: None,
            over_controls: false,
        }
    }
}

pub fn properties(uiworld: &UiWorld) {
    let mut state = uiworld.write::<ZoningState>();
    state.over_controls = is_hovered(|| {
        padxy(12.0, 8.0, || {
            minrow(5.0, || {
                for kind in ZONES.into_iter().map(Some).chain([None]) {
                    if selectable_label_primary(
                        state.kind == kind,
                        kind.map_or("Unzone", zone_name),
                    )
                    .clicked
                    {
                        state.kind = kind;
                        state.drag_start = None;
                        state.last_brush = None;
                    }
                }

                for (rectangle, label) in [(false, "Brush"), (true, "Rectangle")] {
                    if selectable_label_primary(state.rectangle == rectangle, label).clicked {
                        state.rectangle = rectangle;
                        state.drag_start = None;
                        state.last_brush = None;
                    }
                }

                if state.kind.is_some() {
                    for (density, label) in [
                        (Density::Low, "Low density"),
                        (Density::High, "High density"),
                    ] {
                        if selectable_label_primary(state.density == density, label).clicked {
                            state.density = density;
                        }
                    }
                }

                if !state.rectangle {
                    let side = 2 * state.brush_radius + 1;
                    textc(
                        on_secondary_container(),
                        format!("{side}×{side} · Ctrl + wheel"),
                    );
                }
            });
        });
    })
    .hovered;
}

pub fn zoning(sim: &Simulation, uiworld: &UiWorld) {
    let tool = *uiworld.read::<Tool>();
    let mut state = uiworld.write::<ZoningState>();
    let active = tool == Tool::Zoning;

    if active || tool.show_lots() {
        let camera = uiworld.camera();
        MapRenderer::draw_zone_lots(
            sim,
            &mut uiworld.write::<ImmediateDraw>(),
            camera.camera.pos.xy(),
            camera.camera.dist * 1.5,
            active,
        );
    }

    let input = uiworld.read::<InputMap>();
    let Some(pointer) = input.unprojected.filter(|p| p.is_finite()) else {
        state.drag_start = None;
        state.last_brush = None;
        return;
    };
    if !active
        || state.over_controls
        || input.just_act.contains(&InputAction::Close)
        || input.act.contains(&InputAction::CameraRotate)
    {
        state.drag_start = None;
        state.last_brush = None;
        state.over_controls &= active;
        return;
    }

    let pressed = input.just_act.contains(&InputAction::Select);
    let held = input.act.contains(&InputAction::Select);
    let cell = (pointer.xy() / ZONE_CELL_SIZE).floor();
    let map = sim.map();
    let bounds = map.environment.bounds();
    let mut commands = uiworld.commands();
    let command_kind = state.kind;
    let command_density = state.density;
    let mut paint = |area| {
        commands.push(WorldCommand::ZonePaint {
            area,
            kind: command_kind,
            density: command_density,
        });
    };

    let area = if state.rectangle {
        if pressed {
            state.drag_start = Some(cell);
        }
        let area = paint_area(state.drag_start.unwrap_or(cell), cell, bounds);
        if !held && state.drag_start.take().is_some() {
            if let Some(area) = area {
                paint(area);
            }
        }
        area
    } else {
        if input.just_act.contains(&InputAction::SizeUp) {
            state.brush_radius = (state.brush_radius + 1).min(MAX_BRUSH_RADIUS);
        }
        if input.just_act.contains(&InputAction::SizeDown) {
            state.brush_radius = state.brush_radius.saturating_sub(1);
        }

        let radius = Vec2::splat(f32::from(state.brush_radius));
        let stamp = |center| paint_area(center - radius, center + radius, bounds);
        let area = stamp(cell);
        if pressed {
            state.last_brush = Some(cell);
            if let Some(area) = area {
                paint(area);
            }
        } else if held {
            if let Some(start) = state.last_brush.filter(|&previous| previous != cell) {
                state.last_brush = Some(cell);
                let delta = cell - start;
                let steps = delta.x.abs().max(delta.y.abs()) as u32;
                // Fill gaps between frames without repainting the previous stamp.
                for step in 1..=steps {
                    let center = (start + delta * (step as f32 / steps as f32)).round();
                    if let Some(area) = stamp(center) {
                        paint(area);
                    }
                }
            }
        } else {
            state.last_brush = None;
        }
        area
    };

    let Some(area) = area else {
        return;
    };
    let min = area.corners[0].min(area.corners[2]) / ZONE_CELL_SIZE;
    let max = area.corners[0].max(area.corners[2]) / ZONE_CELL_SIZE;
    let color = state
        .kind
        .map_or(LinearColor::new(0.96, 0.28, 0.28, 1.0), zone_color);
    let alpha = if held { 0.66 } else { 0.46 };
    let mut draw = uiworld.write::<ImmediateDraw>();
    for x in min.x as i32..max.x as i32 {
        for y in min.y as i32..max.y as i32 {
            let ll = vec2(x as f32, y as f32) * ZONE_CELL_SIZE;
            let center = ll + Vec2::splat(ZONE_CELL_SIZE * 0.5);
            if let Some(height) = map.environment.height(center) {
                let ur = ll + Vec2::splat(ZONE_CELL_SIZE - 0.45);
                draw.aabb(
                    AABB::new_ll_ur(ll + Vec2::splat(0.45), ur),
                    height.max(0.0) + 0.65,
                )
                .color(color.a(alpha));
            }
        }
    }
}

/// Convert inclusive cell coordinates to a map-clipped area of at most 50 × 50 cells.
fn paint_area(start: Vec2, end: Vec2, bounds: AABB) -> Option<OBB> {
    let end = start
        + (end - start)
            .max(Vec2::splat(1.0 - MAX_SIDE_CELLS))
            .min(Vec2::splat(MAX_SIDE_CELLS - 1.0));
    let ll =
        (start.min(end) * ZONE_CELL_SIZE).max((bounds.ll / ZONE_CELL_SIZE).ceil() * ZONE_CELL_SIZE);
    let ur = ((start.max(end) + Vec2::splat(1.0)) * ZONE_CELL_SIZE)
        .min((bounds.ur / ZONE_CELL_SIZE).floor() * ZONE_CELL_SIZE);
    let size = ur - ll;
    (size.x > 0.0 && size.y > 0.0).then(|| OBB::new((ll + ur) * 0.5, Vec2::X, size.x, size.y))
}
