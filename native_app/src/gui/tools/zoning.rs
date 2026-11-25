use crate::gui::{ErrorTooltip, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::MapRenderer;
use crate::session::SessionState;
use crate::uiworld::UiWorld;
use geom::{vec2, LinearColor, Vec2, AABB, OBB};
use goryak::{
    button_secondary, fixed_spacer, icon, is_hovered, mincolumn, minrow, on_secondary_container,
    padxy, selectable_label_primary, textc, tooltip,
};
use simulation::economy::progression::{can_build, BuildCapability, LockedUntil};
use simulation::map::{Density, EligiblePaintCells, LotLayout, ZoneGrid, ZoneKind, ZONE_CELL_SIZE};
use simulation::world_command::WorldCommand;
use simulation::Simulation;
use std::sync::Arc;
use yakui::{colored_box_container, Color};

mod zone_color;
pub use zone_color::{zone_cell_color, zone_color};

const MAX_BRUSH_RADIUS: u8 = 5;
const DENSITIES: [Density; 3] = [Density::Low, Density::Medium, Density::High];
const LOT_LAYOUTS: [(LotLayout, &str, &str); 2] = [
    (
        LotLayout::Varied,
        "Varied",
        "Plots of 20, 30 and 40 m mixed along each road",
    ),
    (
        LotLayout::Uniform,
        "Uniform",
        "30 m plots that line up evenly along both sides of each road",
    ),
];
const LOT_LAYOUT_NOTE: &str = "Switching redraws empty plots; buildings and zoning stay";

pub fn zone_name(zone: ZoneKind) -> &'static str {
    match zone {
        ZoneKind::Residential => "Residential",
        ZoneKind::Commercial => "Commercial",
        ZoneKind::Industrial => "Industrial",
        ZoneKind::Office => "Office",
        ZoneKind::MixedUse => "Mixed Use",
    }
}

pub(crate) fn density_name(density: Density) -> &'static str {
    match density {
        Density::Low => "Low",
        Density::Medium => "Medium",
        Density::High => "High",
    }
}

pub(crate) struct ZoningPreview<'a> {
    pub stamp: &'a EligiblePaintCells,
    pub pointer: Option<Vec2>,
    pub hover_radius: f32,
    pub pitch: f32,
    pub held: bool,
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
    overlay_grid: Option<(u64, Arc<ZoneGrid>)>,
    preview_areas: Vec<OBB>,
    lot_layout: LotLayout,
}

impl Default for ZoningState {
    fn default() -> Self {
        Self {
            kind: Some(ZoneKind::Residential),
            density: Density::Low,
            density_locks: [None; 3],
            rectangle: false,
            detail: false,
            brush_radius: 1,
            drag_start: None,
            last_brush: None,
            over_controls: false,
            overlay_grid: None,
            preview_areas: Vec::new(),
            lot_layout: LotLayout::default(),
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

    fn brush_side(&self) -> f32 {
        (2.0 * f32::from(self.brush_radius) + 1.0) * self.pitch()
    }

    pub(crate) fn select_kind(&mut self, kind: Option<ZoneKind>) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        if kind == Some(ZoneKind::MixedUse) && self.density == Density::Low {
            self.density = Density::Medium;
        }
        self.cancel_stroke();
    }

    fn selected_lock(&self) -> Option<LockedUntil> {
        self.kind?;
        self.density_locks[usize::from(self.density.ordinal())]
    }

    fn refresh_density_locks(&mut self, sim: &Simulation) {
        let previous = self.selected_lock();
        self.density_locks =
            DENSITIES.map(|density| can_build(sim, BuildCapability::ZoneDensity(density)).err());
        if previous != self.selected_lock() {
            self.cancel_stroke();
        }
        if self.kind != Some(ZoneKind::MixedUse) && self.selected_lock().is_some() {
            self.density = Density::Low;
            self.cancel_stroke();
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

fn zone_swatch(color: LinearColor) {
    colored_box_container(Color::from_linear(<[f32; 4]>::from(color).into()), || {
        fixed_spacer((12.0, 12.0));
    });
}

fn density_picker(state: &mut ZoningState, kind: ZoneKind) {
    for density in DENSITIES
        .into_iter()
        .filter(|&density| kind.supports_density(density))
    {
        let label = density_name(density);
        if let Some(lock) = state.density_locks[usize::from(density.ordinal())] {
            tooltip(format!("Unlocks at {} citizens", lock.population), || {
                minrow(2.0, || {
                    icon(Color::rgb(188, 190, 192), "lock");
                    let mut button = button_secondary(label);
                    button.style.fill = if state.density == density {
                        Color::from_linear(<[f32; 4]>::from(zone_cell_color(kind, density)).into())
                    } else {
                        Color::rgb(64, 66, 69)
                    };
                    button.style.text.color = Color::rgb(188, 190, 192);
                    button.hover_style = button.style.clone();
                    button.down_style = button.style.clone();
                    button.show();
                });
            });
        } else {
            minrow(4.0, || {
                zone_swatch(zone_cell_color(kind, density));
                if tooltip("Density applies to new growth", || {
                    selectable_label_primary(state.density == density, label)
                })
                .clicked
                {
                    state.density = density;
                    state.cancel_stroke();
                }
            });
        }
    }
}

/// "Plots: Varied | Uniform" buttons. `select` runs when a different layout is clicked.
pub(crate) fn lot_layout_picker(current: LotLayout, mut select: impl FnMut(LotLayout)) {
    minrow(5.0, || {
        textc(on_secondary_container(), "Plots:");
        for (layout, label, hint) in LOT_LAYOUTS {
            if tooltip(format!("{hint}. {LOT_LAYOUT_NOTE}."), || {
                selectable_label_primary(current == layout, label)
            })
            .clicked
                && current != layout
            {
                select(layout);
            }
        }
    });
}

fn zone_row(state: &mut ZoningState, kinds: &[ZoneKind], unzone: bool) {
    minrow(5.0, || {
        for &kind in kinds {
            minrow(4.0, || {
                zone_swatch(zone_color(kind));
                if selectable_label_primary(state.kind == Some(kind), zone_name(kind)).clicked {
                    state.select_kind(Some(kind));
                }
            });
        }
        if unzone && selectable_label_primary(state.kind.is_none(), "Unzone").clicked {
            state.select_kind(None);
        }
    });
}

fn zone_picker(state: &mut ZoningState, width: f32) {
    let registry = prototypes::prototypes();
    let mixed_ready = [Density::Medium, Density::High]
        .into_iter()
        .all(|density| registry.mixed_use_pair(density).is_some());
    let standard = ZoneKind::TAXABLE.as_slice();
    let palette = if mixed_ready { &ZoneKind::ALL } else { standard };

    if width >= 740.0 {
        zone_row(state, palette, true);
    } else {
        let per_row = if width >= 520.0 { standard.len() } else { 2 };
        for row in standard.chunks(per_row) {
            zone_row(state, row, false);
        }
        zone_row(state, &palette[standard.len()..], true);
    }
}

pub fn properties(uiworld: &UiWorld, width: f32) {
    let narrow = width < 830.0;
    let brush_hint = |state: &ZoningState| {
        let side = state.brush_side() as u32;
        textc(
            on_secondary_container(),
            format!("{side} × {side} m · Ctrl + wheel"),
        );
    };
    let mut state = uiworld.write::<ZoningState>();
    state.over_controls = is_hovered(|| {
        padxy(12.0, 8.0, || {
            mincolumn(3.0, || {
                zone_picker(&mut state, width);
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
                lot_layout_picker(state.lot_layout, |layout| {
                    if uiworld.can_author() {
                        state.lot_layout = layout;
                        uiworld.commands().push(WorldCommand::SetLotLayout(layout));
                    }
                });
            });
        });
    })
    .hovered;
}

pub fn zoning(sim: &Simulation, uiworld: &UiWorld) {
    let tool = *uiworld.read::<Tool>();
    let mut state = uiworld.write::<ZoningState>();
    if tool != Tool::Zoning {
        state.cancel_gesture();
        if tool.show_lots() {
            let camera = uiworld.camera();
            MapRenderer::draw_zoned_lots(
                &sim.map(),
                &sim.read::<ZoneGrid>(),
                &mut uiworld.write::<ImmediateDraw>(),
                camera.camera.pos.xy(),
                camera.camera.dist,
            );
        }
        return;
    }
    state.refresh_density_locks(sim);
    state.lot_layout = sim.map().lot_layout();

    let input = uiworld.read::<InputMap>();
    let camera = uiworld.camera();
    let viewport = AABB::new_ll_ur(
        Vec2::ZERO,
        vec2(camera.camera.viewport_w, camera.camera.viewport_h),
    );
    let pointer = input
        .unprojected
        .filter(|p| p.is_finite())
        .filter(|_| {
            viewport.contains(input.screen)
                && !state.over_controls
                && !input.act.contains(&InputAction::CameraRotate)
        })
        .map(|p| p.xy());
    if pointer.is_none() {
        state.cancel_stroke();
    }
    let pressed = input.just_act.contains(&InputAction::Select);
    let held = input.act.contains(&InputAction::Select);
    let map = sim.map();
    let bounds = map.environment().bounds();
    let pitch = state.pitch();
    let mut areas = std::mem::take(&mut state.preview_areas);
    areas.clear();

    if let Some(pointer) = pointer.filter(|_| state.selected_lock().is_none()) {
        let cell = (pointer / pitch).floor();
        let mut commands = uiworld.commands();
        let kind = state.kind;
        let density = state.density;
        let mut paint = |area| {
            commands.push(WorldCommand::ZonePaint {
                area,
                kind,
                density,
            })
        };

        if state.rectangle {
            if pressed {
                state.drag_start = Some(cell);
            }
            let area = paint_area(state.drag_start.unwrap_or(cell), cell, bounds, pitch);
            if !held && state.drag_start.take().is_some() {
                if let Some(area) = area {
                    paint(area);
                }
            }
            areas.extend(area);
        } else {
            if input.just_act.contains(&InputAction::SizeUp) {
                state.brush_radius = (state.brush_radius + 1).min(MAX_BRUSH_RADIUS);
            }
            if input.just_act.contains(&InputAction::SizeDown) {
                state.brush_radius = state.brush_radius.saturating_sub(1);
            }

            let radius = Vec2::splat(f32::from(state.brush_radius));
            let stamp = |center| paint_area(center - radius, center + radius, bounds, pitch);
            if pressed {
                state.last_brush = Some(cell);
                areas.extend(stamp(cell));
                areas.iter().copied().for_each(&mut paint);
            } else if held {
                if let Some(start) = state.last_brush.filter(|&previous| previous != cell) {
                    state.last_brush = Some(cell);
                    for center in brush_centers(start, cell, state.brush_radius == 0) {
                        areas.extend(stamp(center));
                    }
                    areas.iter().copied().for_each(&mut paint);
                }
            } else {
                state.last_brush = None;
            }
            if areas.is_empty() {
                areas.extend(stamp(cell));
            }
        }
    }

    let world = uiworld.read::<SessionState>().generation;
    let zones = sim.read::<ZoneGrid>();
    let grid = state
        .overlay_grid
        .get_or_insert_with(|| (world, Arc::new((*zones).clone())));
    if grid.0 != world || grid.1.generation() != zones.generation() {
        *grid = (world, Arc::new((*zones).clone()));
    }
    let grid = grid.1.clone();
    let (stamp, denied) = ZoneGrid::preview_paint_stroke(&map, &areas, state.kind, state.density)
        .unwrap_or_else(|denied| (EligiblePaintCells::default(), Some(denied)));
    let warning = MapRenderer::queue_zoning_overlay(
        &map,
        grid,
        world,
        camera.camera.pos.xy(),
        camera.camera.dist,
        ZoningPreview {
            stamp: &stamp,
            pointer,
            hover_radius: if state.rectangle {
                pitch
            } else {
                state.brush_side()
            } + 16.0,
            pitch,
            held,
        },
    );
    state.preview_areas = areas;
    let warning = denied
        .map(|reason| crate::gui::notifications::command_message("ZonePaint", reason.into()))
        .or_else(|| warning.map(Into::into));
    if let Some(warning) = warning.filter(|_| !goryak::pointer_over_ui(input.screen.x, input.screen.y))
    {
        *uiworld.write::<ErrorTooltip>() = ErrorTooltip::new(warning);
    }
}

fn brush_centers(start: Vec2, end: Vec2, connect_corners: bool) -> impl Iterator<Item = Vec2> {
    let delta = end - start;
    let steps = delta.x.abs().max(delta.y.abs()) as u32;
    let mut previous = start;
    // A radius-zero stamp must share an edge with the next stamp, even on diagonals.
    (1..=steps).flat_map(move |step| {
        let center = (start + delta * (step as f32 / steps as f32)).round();
        let edge = (connect_corners && center.x != previous.x && center.y != previous.y)
            .then(|| vec2(center.x, previous.y));
        previous = center;
        edge.into_iter().chain([center])
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
    fn u1_zoning_default_brush_stamps_thirty_six_storage_cells() {
        let state = ZoningState::default();
        let mut grid = ZoneGrid::default();
        let radius = Vec2::splat(f32::from(state.brush_radius));
        let center = Vec2::splat(3.0);
        let bounds = AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(512.0));
        let area = paint_area(center - radius, center + radius, bounds, state.pitch()).unwrap();
        let mut map = simulation::map::Map::empty();
        assert!(map.install_environment(simulation::map::Environment::new(1, 1)));
        grid.paint_on_map(&map, &area, state.kind, state.density)
            .unwrap();
        assert_eq!(grid.cells().len(), 36);
        assert_eq!(
            grid.cells().keys().copied().collect::<Vec<_>>(),
            (4..10)
                .flat_map(|x| (4..10).map(move |y| (x, y)))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn u1_zoning_radius_zero_brush_connects_diagonal_stamps() {
        let centers = brush_centers(Vec2::ZERO, Vec2::splat(3.0), true).collect::<Vec<_>>();
        assert_eq!(centers.last(), Some(&Vec2::splat(3.0)));
        for (a, b) in std::iter::once(Vec2::ZERO)
            .chain(centers.iter().copied())
            .zip(centers.iter().copied())
        {
            let delta = b - a;
            assert_eq!(delta.x.abs() + delta.y.abs(), 1.0);
        }
    }

    #[test]
    fn u1_zoning_guided_locks_cancel_a_locked_density_stroke() {
        use simulation::economy::progression::{ProgressionMode, ProgressionState};

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new(false);
        sim.write::<ProgressionState>().mode = ProgressionMode::Guided;
        let mut state = ZoningState {
            density: Density::High,
            drag_start: Some(Vec2::ZERO),
            last_brush: Some(Vec2::ZERO),
            ..ZoningState::default()
        };
        state.refresh_density_locks(&sim);
        assert_eq!(state.density, Density::Low);
        assert!(state.gesture_is_clear());
        assert_eq!(
            state
                .density_locks
                .map(|lock| lock.map(|lock| lock.population)),
            [None, Some(100), Some(250)]
        );

        sim.write::<ProgressionState>().highest_milestone = 1;
        state.refresh_density_locks(&sim);
        assert!(state.density_locks[1].is_none());
        assert_eq!(state.density_locks[2].unwrap().population, 250);
    }

    #[test]
    fn zones2_mixed_selection_survives_guided_lock_refresh() {
        use simulation::economy::progression::{ProgressionMode, ProgressionState};

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new(false);
        sim.write::<ProgressionState>().mode = ProgressionMode::Guided;
        let mut state = ZoningState {
            drag_start: Some(Vec2::ZERO),
            ..ZoningState::default()
        };
        state.select_kind(Some(ZoneKind::MixedUse));
        assert!(state.gesture_is_clear());
        state.refresh_density_locks(&sim);
        assert_eq!(state.kind, Some(ZoneKind::MixedUse));
        assert_eq!(state.density, Density::Medium);
        assert_eq!(state.selected_lock().unwrap().population, 100);

        state.last_brush = Some(Vec2::ZERO);
        sim.write::<ProgressionState>().highest_milestone = 1;
        state.refresh_density_locks(&sim);
        assert!(state.gesture_is_clear());
        assert!(state.selected_lock().is_none());
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
            let mut map = simulation::map::Map::empty();
            assert!(map.install_environment(simulation::map::Environment::new(1, 1)));
            grid.paint_on_map(&map, &area, Some(ZoneKind::Office), Density::Medium)
                .unwrap();
            assert_eq!(grid.cells().keys().copied().collect::<Vec<_>>(), [(62, 62)]);
            grid.paint_on_map(&map, &area, None, Density::Medium)
                .unwrap();
            assert!(grid.cells().is_empty());
        }
    }

    #[test]
    fn u1_zoning_palette_separates_densities_kinds_and_grass() {
        let oklab = |LinearColor { r, g, b, .. }| {
            let l = (0.41222146 * r + 0.53633255 * g + 0.051445995 * b).cbrt();
            let m = (0.2119035 * r + 0.6806995 * g + 0.10739696 * b).cbrt();
            let s = (0.08830246 * r + 0.28171885 * g + 0.6299787 * b).cbrt();
            geom::vec3(
                0.21045426 * l + 0.7936178 * m - 0.004072047 * s,
                1.9779985 * l - 2.4285922 * m + 0.4505937 * s,
                0.025904037 * l + 0.78277177 * m - 0.80867577 * s,
            )
        };
        let grass = oklab(LinearColor::new(0.071, 0.155, 0.018, 1.0));
        for (index, kind) in ZoneKind::ALL.into_iter().enumerate() {
            let ramp: Vec<_> = DENSITIES
                .into_iter()
                .filter(|&density| kind.supports_density(density))
                .map(|density| (density, oklab(zone_cell_color(kind, density))))
                .collect();
            for pair in ramp.windows(2) {
                let [(_, lighter), (_, deeper)] = [pair[0], pair[1]];
                assert!(
                    lighter.x > deeper.x && lighter.distance(deeper) >= 0.10,
                    "{kind:?} adjacent densities need an OKLab distance of at least 0.10"
                );
            }
            for (density, color) in ramp {
                assert!(
                    color.distance(grass) >= 0.12,
                    "{kind:?} {density:?} vs grass"
                );
                assert!(
                    color.y.hypot(color.z) >= 0.07,
                    "{kind:?} {density:?} chroma"
                );
                for &other in ZoneKind::ALL[index + 1..]
                    .iter()
                    .filter(|other| other.supports_density(density))
                {
                    let other_color = oklab(zone_cell_color(other, density));
                    assert!(
                        color.distance(other_color) >= 0.085,
                        "{kind:?} vs {other:?} at {density:?}"
                    );
                }
            }
        }
    }
}
