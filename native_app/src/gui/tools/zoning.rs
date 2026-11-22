use crate::gui::windows::settings::Settings;
use crate::gui::{ErrorTooltip, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::MapRenderer;
use crate::uiworld::UiWorld;
use geom::{vec2, LinearColor, Vec2, AABB, OBB};
use goryak::{
    button_secondary, fixed_spacer, is_hovered, mincolumn, minrow, on_secondary_container, padxy,
    selectable_label_primary, textc, tooltip,
};
use simulation::economy::progression::{can_build, BuildCapability, LockedUntil};
use simulation::map::{
    Density, Map, ProjectFilter, ProjectKind, ZoneGrid, ZoneKind, ZONE_CELL_SIZE,
};
use simulation::world_command::WorldCommand;
use simulation::Simulation;
use yakui::{colored_box_container, Color};

mod zone_color;
pub use zone_color::{zone_cell_color, zone_color};

pub const ZONES: [ZoneKind; 4] = [
    ZoneKind::Residential,
    ZoneKind::Commercial,
    ZoneKind::Industrial,
    ZoneKind::Office,
];

const MAX_BRUSH_RADIUS: u8 = 5;
const DENSITIES: [Density; 3] = [Density::Low, Density::Medium, Density::High];

pub fn zone_name(zone: ZoneKind) -> &'static str {
    match zone {
        ZoneKind::Residential => "Residential",
        ZoneKind::Commercial => "Commercial",
        ZoneKind::Industrial => "Industrial",
        ZoneKind::Office => "Office",
    }
}

pub struct ZoningState {
    kind: Option<ZoneKind>,
    density: Density,
    density_locks: [Option<LockedUntil>; 3],
    rectangle: bool,
    detail: bool,
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
            density_locks: [None; 3],
            rectangle: false,
            detail: false,
            brush_radius: 0,
            drag_start: None,
            last_brush: None,
            over_controls: false,
        }
    }
}

impl ZoningState {
    fn pitch(&self) -> f32 {
        if self.detail {
            ZONE_CELL_SIZE
        } else {
            2.0 * ZONE_CELL_SIZE
        }
    }

    fn cancel_stroke(&mut self) {
        self.drag_start = None;
        self.last_brush = None;
    }

    pub(crate) fn cancel_gesture(&mut self) {
        self.cancel_stroke();
        self.over_controls = false;
    }
}

fn density_picker(state: &mut ZoningState, kind: ZoneKind) {
    for (density, label) in DENSITIES.into_iter().zip(["Low", "Medium", "High"]) {
        let lock = state.density_locks[usize::from(density.ordinal())];
        let color = if lock.is_some() {
            LinearColor::gray(0.23)
        } else {
            zone_cell_color(kind, density)
        };
        minrow(4.0, || {
            colored_box_container(Color::from_linear(<[f32; 4]>::from(color).into()), || {
                fixed_spacer((12.0, 12.0));
            });
            if let Some(lock) = lock {
                tooltip(format!("Unlocks at {} citizens", lock.population), || {
                    let mut button = button_secondary(label);
                    button.style.fill = Color::rgb(64, 66, 69);
                    button.style.text.color = Color::rgb(188, 190, 192);
                    button.hover_style = button.style.clone();
                    button.down_style = button.style.clone();
                    button.show();
                });
            } else if selectable_label_primary(state.density == density, label).clicked {
                state.density = density;
                state.cancel_stroke();
            }
        });
    }
}

pub fn properties(uiworld: &UiWorld) {
    let narrow = uiworld.camera().camera.viewport_w < 830.0 * uiworld.read::<Settings>().gui_scale;
    let brush_hint = |state: &ZoningState| {
        let side = (2 * u32::from(state.brush_radius) + 1) * state.pitch() as u32;
        textc(
            on_secondary_container(),
            format!("{side} × {side} m · Ctrl + wheel"),
        );
    };
    let mut state = uiworld.write::<ZoningState>();
    state.over_controls = is_hovered(|| {
        padxy(12.0, 8.0, || {
            mincolumn(3.0, || {
                minrow(5.0, || {
                    for kind in ZONES.into_iter().map(Some).chain([None]) {
                        if selectable_label_primary(
                            state.kind == kind,
                            kind.map_or("Unzone", zone_name),
                        )
                        .clicked
                        {
                            state.kind = kind;
                            state.cancel_stroke();
                        }
                    }
                });
                minrow(5.0, || {
                    if let Some(kind) = state.kind {
                        density_picker(&mut state, kind);
                    }

                    for (rectangle, label) in [(false, "Brush"), (true, "Rectangle")] {
                        if selectable_label_primary(state.rectangle == rectangle, label).clicked {
                            state.rectangle = rectangle;
                            state.cancel_stroke();
                        }
                    }

                    if selectable_label_primary(state.detail, "8 m detail").clicked {
                        state.detail = !state.detail;
                        state.cancel_stroke();
                    }

                    if !state.rectangle && !narrow {
                        brush_hint(&state);
                    }
                });
                if !state.rectangle && narrow {
                    brush_hint(&state);
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
    if active {
        state.density_locks =
            DENSITIES.map(|density| can_build(sim, BuildCapability::ZoneDensity(density)).err());
        if state.density_locks[usize::from(state.density.ordinal())].is_some() {
            state.density = Density::Low;
            state.cancel_stroke();
        }
    }

    if active || tool.show_lots() {
        let camera = uiworld.camera();
        MapRenderer::draw_zoning_overlay(
            sim,
            &mut uiworld.write::<ImmediateDraw>(),
            camera.camera.pos.xy(),
            camera.camera.dist,
            active,
        );
    }

    let input = uiworld.read::<InputMap>();
    let Some(pointer) = input.unprojected.filter(|p| p.is_finite()) else {
        state.cancel_stroke();
        return;
    };
    if !active || state.over_controls || input.act.contains(&InputAction::CameraRotate) {
        state.cancel_stroke();
        state.over_controls &= active;
        return;
    }

    let pressed = input.just_act.contains(&InputAction::Select);
    let held = input.act.contains(&InputAction::Select);
    let pitch = state.pitch();
    let cell = (pointer.xy() / pitch).floor();
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
        let area = paint_area(state.drag_start.unwrap_or(cell), cell, bounds, pitch);
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
        let stamp = |center| paint_area(center - radius, center + radius, bounds, pitch);
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
    let ll = area.corners[0].min(area.corners[2]);
    let ur = area.corners[0].max(area.corners[2]);
    let min = (ll / pitch).floor();
    let max = (ur / pitch).ceil();
    let color = state
        .kind
        .map_or(LinearColor::new(0.96, 0.28, 0.28, 1.0), |kind| {
            zone_cell_color(kind, state.density)
        });
    let alpha = if held { 0.66 } else { 0.46 };
    if state.kind.is_some() && !has_frontage(&map, (cell + Vec2::splat(0.5)) * pitch) {
        *uiworld.write::<ErrorTooltip>() = ErrorTooltip::new("Zones need road frontage to grow");
    }
    let mut draw = uiworld.write::<ImmediateDraw>();
    for x in min.x as i32..max.x as i32 {
        for y in min.y as i32..max.y as i32 {
            let cell_ll = vec2(x as f32, y as f32) * pitch;
            let tile_ll = cell_ll.max(ll);
            let tile_ur = (cell_ll + Vec2::splat(pitch)).min(ur);
            let center = (tile_ll + tile_ur) * 0.5;
            if let Some(height) = map.environment.height(center) {
                let color = if state.kind.is_some() && !has_frontage(&map, center) {
                    LinearColor::new(0.58, 0.58, 0.58, 1.0)
                } else {
                    color
                };
                let inset = Vec2::splat(0.45);
                draw.aabb(
                    AABB::new_ll_ur(tile_ll + inset, tile_ur - inset),
                    height + 0.97,
                )
                .color(color.a(alpha));
            }
        }
    }
}

fn has_frontage(map: &Map, center: Vec2) -> bool {
    map.spatial_map()
        .query(center, ProjectFilter::LOT)
        .any(|project| match project {
            ProjectKind::Lot(id) => map.get(id).is_some_and(|lot| map.get(lot.parent).is_some()),
            _ => false,
        })
}

/// Convert inclusive editor coordinates to a map-clipped area aligned to the storage grid.
fn paint_area(start: Vec2, end: Vec2, bounds: AABB, pitch: f32) -> Option<OBB> {
    let max_side = ZoneGrid::MAX_PAINT_SIDE / pitch;
    let end = start
        + (end - start)
            .max(Vec2::splat(1.0 - max_side))
            .min(Vec2::splat(max_side - 1.0));
    let ll = (start.min(end) * pitch).max((bounds.ll / ZONE_CELL_SIZE).ceil() * ZONE_CELL_SIZE);
    let ur = ((start.max(end) + Vec2::splat(1.0)) * pitch)
        .min((bounds.ur / ZONE_CELL_SIZE).floor() * ZONE_CELL_SIZE);
    let size = ur - ll;
    (size.x > 0.0 && size.y > 0.0).then(|| OBB::new((ll + ur) * 0.5, Vec2::X, size.x, size.y))
}

#[cfg(test)]
impl ZoningState {
    pub(crate) fn start_test_gesture(&mut self) {
        self.drag_start = Some(Vec2::X);
        self.last_brush = Some(Vec2::Y);
        self.over_controls = true;
    }

    pub(crate) fn gesture_is_clear(&self) -> bool {
        self.drag_start.is_none() && self.last_brush.is_none() && !self.over_controls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u1_zoning_default_brush_stamps_four_storage_cells() {
        let state = ZoningState::default();
        let mut grid = ZoneGrid::default();
        let radius = Vec2::splat(f32::from(state.brush_radius));
        let center = Vec2::splat(3.0);
        let bounds = AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(512.0));
        let area = paint_area(center - radius, center + radius, bounds, state.pitch()).unwrap();
        grid.paint(area, state.kind, state.density);
        assert_eq!(grid.cells.len(), 4);
        assert_eq!(
            grid.cells.keys().copied().collect::<Vec<_>>(),
            [(6, 6), (6, 7), (7, 6), (7, 7)]
        );
    }

    #[test]
    fn u1_zoning_pitch_preserves_drag_cap_and_clips_on_storage_boundaries() {
        let bounds = AABB::new_ll_ur(Vec2::splat(8.0), Vec2::splat(504.0));
        for pitch in [8.0, 16.0] {
            let start = Vec2::splat(2.0);
            let long = paint_area(start, Vec2::splat(100.0), bounds, pitch).unwrap();
            let [a, _, b, _] = long.corners;
            assert_eq!(a.max(b) - a.min(b), Vec2::splat(400.0));
            let edge = (bounds.ur / pitch).ceil() - Vec2::splat(1.0);
            let area = paint_area(edge, edge + Vec2::splat(5.0), bounds, pitch).unwrap();
            let mut grid = ZoneGrid::default();
            grid.paint(area, Some(ZoneKind::Office), Density::Medium);
            assert_eq!(grid.cells.keys().copied().collect::<Vec<_>>(), [(62, 62)]);
            grid.paint(area, None, Density::Medium);
            assert!(grid.cells.is_empty());
        }
    }

    #[test]
    fn u1_zoning_palette_has_distinct_lightness() {
        // OKLab lightness computed from the rendered linear RGB values.
        let lightness = |LinearColor { r, g, b, .. }| {
            let l = (0.41222146 * r + 0.53633255 * g + 0.051445995 * b).cbrt();
            let m = (0.2119035 * r + 0.6806995 * g + 0.10739696 * b).cbrt();
            let s = (0.08830246 * r + 0.28171885 * g + 0.6299787 * b).cbrt();
            0.21045426 * l + 0.7936178 * m - 0.004072047 * s
        };
        for kind in ZONES {
            let [low, medium, high] = [Density::Low, Density::Medium, Density::High]
                .map(|density| lightness(zone_cell_color(kind, density)));
            assert!(
                medium - low >= 0.15 && high - medium >= 0.15,
                "{kind:?}: {low}, {medium}, {high}"
            );
        }
    }
}
