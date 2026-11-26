use crate::gui::zoning::{density_name, zone_cell_color, zone_color, zone_name, ZoningPreview};
use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::map_rendering::placeholder_color::placeholder_color;
use crate::rendering::MapRenderOptions;
use crate::service_icon::ServiceIcon;
use common::{ChunkID_32, FastMap, FastSet};
use engine::earcut::earcut;
use engine::MeshBuilder;
use engine::{
    Drawable, FrameContext, GfxContext, InstancedMeshBuilder, Material, Mesh, MeshInstance,
    MeshVertex, MetallicRoughness, SpriteBatch, SpriteBatchBuilder, Tesselator,
};
use geom::{
    minmax, vec2, vec3, Camera, Circle, Color, Intersect, LinearColor, PolyLine3, Polygon, Radians,
    Shape, Vec2, Vec3, AABB, OBB,
};
use itertools::{EitherOrBoth, Itertools};
use prototypes::{
    try_prototype, FreightStationPrototype, GoodsCompanyID, GoodsCompanyPrototype,
    GrowableCompanyID, GrowableCompanyPrototype, RenderAsset,
};
use simulation::map::{
    Building, BuildingKind, CanonicalPosition, Density, EligiblePaintCells, Environment,
    Intersection, LaneKind, Lanes, LotID, LotKind, Map, MapSubscriber, ProjectFilter, ProjectKind,
    PylonPosition, Road, Roads, SubscriberChunkID, Turn, TurnKind, UpdateType, ZoneCell, ZoneGrid,
    ZoneKind, CROSSWALK_WIDTH, ROAD_Z_OFFSET, ZONE_CELL_SIZE,
};
use simulation::souls::growth::{check_growable_lot, GrowableLotError};
use simulation::Simulation;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::ops::{Mul, Neg, Range};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "growable_mesh.rs"]
mod growable_mesh;

// A true result consumes the building before the legacy renderer.
fn append_profiled_building(mesh: &mut MeshBuilder<false>, building: &Building) -> bool {
    let Some(site) = building.development() else {
        return false;
    };
    if let Some(shell) = crate::gui::selectable::PreparedShell::new(building) {
        growable_mesh::append(mesh, &shell, site);
    }
    true
}

fn append_building_face(
    face: &[Vec3],
    color: LinearColor,
    vertices: &mut Vec<MeshVertex>,
    add_index: &mut dyn FnMut(u32),
) {
    let Some((start, &origin)) = face.iter().enumerate().min_by(|(_, a), (_, b)| {
        a.x.total_cmp(&b.x)
            .then_with(|| a.y.total_cmp(&b.y))
            .then_with(|| a.z.total_cmp(&b.z))
    }) else {
        return;
    };
    let ring = || face[start..].iter().chain(&face[..start]).copied();
    // Sum from the same vertex for every rotation. Reverse the boundary normal
    // to preserve the wall winding.
    let area_normal = ring()
        .zip(ring().cycle().skip(1))
        .map(|(a, b)| (b - origin).cross(a - origin))
        .sum::<Vec3>();
    let mut normal = unwrap_ret!(area_normal.try_normalize());
    if normal.z < 0.0 {
        normal = -normal;
    }
    let axis = if normal.x.abs() > 0.8 {
        Vec3::Y
    } else {
        Vec3::X
    };
    let u = unwrap_ret!(axis.cross(normal).try_normalize());
    let v = normal.cross(u);

    // Keep one projected point per mesh vertex so Earcut indices match.
    let mut projected = Vec::with_capacity(face.len());
    for &position in face {
        let offset = position - origin;
        projected.push(vec2(offset.dot(u), offset.dot(v)));

        vertices.push(MeshVertex {
            position: position.into(),
            normal,
            uv: [0.0; 2],
            color: color.into(),
            tangent: [0.0; 4],
        });
    }

    earcut(&projected, &[], |a, b, c| {
        add_index(a as u32);
        add_index(b as u32);
        add_index(c as u32);
    });
}

#[cfg(test)]
mod house_mesh_tests {
    use super::*;

    #[test]
    fn merged_house_roof_keeps_rendered_area_after_vertex_rotation() {
        // The merged roof face of the 30 m house at (131, 150), seed 1041579720.
        let mut face = [
            vec3(128.31593, 148.32973, 10.643467),
            vec3(128.31593, 150.95515, 10.643467),
            vec3(128.31593, 157.91232, 10.643467),
            vec3(134.558, 157.91232, 4.4014063),
            vec3(134.558, 151.47673, 4.4014063),
            vec3(129.86346, 146.7822, 9.095937),
        ];
        // This 45-degree roof face covers 57.2582 square metres in plan.
        let expected_area = 57.2582 * 2.0_f32.sqrt();
        for shift in 0..face.len() {
            let mut vertices = Vec::new();
            let mut indices = Vec::new();
            append_building_face(&face, LinearColor::WHITE, &mut vertices, &mut |index| {
                indices.push(index as usize);
            });
            let mut area = 0.0;
            for triangle in indices.chunks(3) {
                let [a, b, c] = [triangle[0], triangle[1], triangle[2]].map(|i| vertices[i]);
                let cross = (Vec3::from(b.position) - Vec3::from(a.position))
                    .cross(Vec3::from(c.position) - Vec3::from(a.position));
                assert!(a.normal.z > 0.0 && cross.dot(a.normal) >= 0.0);
                area += cross.mag() * 0.5;
            }
            assert!(
                (area - expected_area).abs() < 1e-3,
                "rotation {shift} rendered {area} square metres, expected {expected_area}"
            );
            face.rotate_left(1);
        }
    }
}

/// This is the main struct that handles the map rendering.
/// It is responsible for generating the meshes and sprites for the map
/// That is, the mostly static things (roads, intersections, lights, buildings).
pub struct MapMeshHandler {
    builders: MapBuilders,
    cache: FastMap<SubscriberChunkID, CachedObj>,
    road_sub: MapSubscriber,
    building_sub: MapSubscriber,
    map_version: Option<u64>,
    rebuilt_version: Option<u64>,
    zoning: ZoningMeshes,
}

#[derive(Default)]
struct CachedObj {
    road: Vec<Arc<Mesh>>,
    build: Vec<Arc<dyn Drawable>>,
    lots: Option<Mesh>,
    arrows: Option<SpriteBatch>,
}

impl CachedObj {
    fn is_empty(&self) -> bool {
        self.road.is_empty()
            && self.lots.is_none()
            && self.arrows.is_none()
            && self.build.is_empty()
    }
}

struct MapBuilders {
    buildsprites: FastMap<BuildingKind, SpriteBatchBuilder<false>>,
    buildmeshes: FastMap<BuildingKind, InstancedMeshBuilder<false>>,
    houses_mesh: MeshBuilder<false>,
    zonemeshes: FastMap<BuildingKind, (MeshBuilder<false>, InstancedMeshBuilder<false>, bool)>,
    arrow_builder: SpriteBatchBuilder<false>,
    crosswalk_builder: MeshBuilder<false>,
    mesh_map: MeshBuilder<false>,
    mesh_lots: MeshBuilder<false>,
}

const ZONING_OVERVIEW_DISTANCE: f32 = 900.0;
const ZONING_FILL_LIFT: f32 = 0.20;
const ZONING_REBUILD_BUDGET: Duration = Duration::from_millis(2);
// Leave time for the last small chunk's tessellation and buffer uploads.
const ZONING_CHUNK_HEADROOM: Duration = Duration::from_millis(1);

type ZoningChunk = ChunkID_32;
type ZonedLot = (ZoneCell, Result<(), GrowableLotError>);

#[derive(PartialEq)]
struct ZoningPreviewMesh {
    fill: Option<ZoningPreviewFill>,
    outlines: Vec<LotOutline>,
}

#[derive(PartialEq)]
struct ZoningPreviewFill {
    tiles: Vec<AABB>,
    color: [f32; 4],
}

#[derive(Default)]
struct PreviewLots {
    outlines: Vec<LotOutline>,
    replaced: FastSet<LotID>,
}

#[derive(PartialEq)]
struct LotOutline {
    corners: [Vec2; 4],
    width: f32,
    color: [f32; 4],
}

struct ZoningFrame {
    world: u64,
    grid: Arc<ZoneGrid>,
    view: ZoningView,
    preview: ZoningPreviewMesh,
    replaced_lots: FastSet<LotID>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ZoningMode {
    Overview,
    Coarse,
    Detail,
}

impl ZoningMode {
    fn new(distance: f32, detail: bool) -> Self {
        if distance >= ZONING_OVERVIEW_DISTANCE {
            Self::Overview
        } else if detail {
            Self::Detail
        } else {
            Self::Coarse
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct ZoningView {
    center: ZoningChunk,
    bounds: AABB,
    mode: ZoningMode,
}

impl ZoningView {
    fn new(center: Vec2, distance: f32, map_bounds: AABB, mode: ZoningMode) -> Self {
        let radius = Vec2::splat(distance * 1.5);
        let size = ZoningChunk::SIZE_F32;
        let bounds = AABB::new_ll_ur(
            ((center - radius) / size).floor() * size,
            ((center + radius) / size).ceil() * size,
        )
        .intersection(map_bounds);
        Self {
            center: ZoningChunk::new(center),
            bounds,
            mode,
        }
    }

    fn chunks(self) -> impl Iterator<Item = ZoningChunk> {
        zoning_tiles(self.bounds, ZoningChunk::SIZE_F32)
            .map(|(x, y)| ZoningChunk::new_i16(x as i16, y as i16))
    }
}

thread_local! {
    // UI systems have the simulation and pointer. The renderer consumes their latest request
    // on the same thread, where it has access to the GPU. No request means no zoning mesh.
    static ZONING_FRAME: RefCell<Option<ZoningFrame>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct ZoningChunkState {
    cells: usize,
    grid_revision: u64,
    lot_revision: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct ZoningChunkRevision {
    grid: u64,
    lots: u64,
    map: u64,
}

struct ZoningChunkMesh {
    revision: ZoningChunkRevision,
    fill: Option<Arc<Mesh>>,
    lots: Option<Arc<Mesh>>,
    lot_ranges: Vec<(LotID, Range<u32>)>,
}

impl ZoningChunkMesh {
    fn visible_lots(&self, replaced: &FastSet<LotID>) -> Option<Arc<Mesh>> {
        let mesh = self.lots.as_ref()?;
        if replaced.is_empty() {
            return Some(mesh.clone());
        }
        let mut excluded = self
            .lot_ranges
            .iter()
            .filter(|(id, _)| replaced.contains(id))
            .map(|(_, range)| range.clone())
            .peekable();
        if excluded.peek().is_none() {
            return Some(mesh.clone());
        }

        // Restrict the cached index ranges while retaining the same GPU buffers.
        let mut mesh = (**mesh).clone();
        let (material, all) = mesh.lods[0].primitives[0].clone();
        mesh.lods[0].primitives = zoning_visible_ranges(all, excluded)
            .map(|range| (material, range))
            .collect();
        (!mesh.lods[0].primitives.is_empty()).then(|| Arc::new(mesh))
    }
}

fn zoning_visible_ranges(
    all: Range<u32>,
    excluded: impl Iterator<Item = Range<u32>>,
) -> impl Iterator<Item = Range<u32>> {
    let mut start = all.start;
    excluded
        .chain(std::iter::once(all.end..all.end))
        .filter_map(move |range| {
            let visible = start..range.start;
            start = range.end;
            (!visible.is_empty()).then_some(visible)
        })
}

struct ZoningMeshes {
    updates: MapSubscriber,
    world: Option<u64>,
    grid: Option<Arc<ZoneGrid>>,
    chunk_states: FastMap<ZoningChunk, ZoningChunkState>,
    map_revision: u64,
    view: Option<ZoningView>,
    visible: Vec<ZoningChunk>,
    chunks: FastMap<(ZoningChunk, ZoningMode), ZoningChunkMesh>,
    preview: Option<Arc<Mesh>>,
    preview_key: Option<ZoningPreviewMesh>,
    builder: MeshBuilder<false>,
}

impl ZoningMeshes {
    fn draw(&mut self, map: &Map, ctx: &mut FrameContext<'_>) {
        let Some(frame) = ZONING_FRAME.take() else {
            return;
        };
        profiling::scope!("zoning_overlay");
        let cleared = self.updates.take_cleared();
        let updated = self.updates.take_updated_chunks().next().is_some();
        if self.world != Some(frame.world) || cleared {
            self.world = Some(frame.world);
            self.grid = None;
            self.chunk_states.clear();
            self.chunks.clear();
            self.preview_key = None;
        }
        if updated {
            // Subscriber locations are canonical positions. Long roads and building zones can
            // affect clipping or eligibility well beyond their subscriber chunk.
            self.map_revision = self.map_revision.wrapping_add(1);
            self.preview_key = None;
        }

        let grid_changed = !self
            .grid
            .as_ref()
            .is_some_and(|grid| Arc::ptr_eq(grid, &frame.grid));
        if grid_changed {
            update_zone_chunks(
                map,
                &mut self.chunk_states,
                self.grid.as_deref(),
                &frame.grid,
            );
            self.grid = Some(frame.grid.clone());
        }

        if self.view != Some(frame.view)
            || (grid_changed && frame.view.mode == ZoningMode::Overview)
        {
            self.visible.clear();
            if frame.view.mode == ZoningMode::Overview {
                self.visible
                    .extend(self.chunk_states.iter().filter_map(|(&chunk, state)| {
                        (state.cells != 0
                            && frame.view.bounds.intersection(chunk.bbox()).area() > 0.0)
                            .then_some(chunk)
                    }));
            } else {
                self.visible.extend(frame.view.chunks());
            }
            self.visible.sort_unstable_by_key(|chunk| {
                let dx = i32::from(chunk.0) - i32::from(frame.view.center.0);
                let dy = i32::from(chunk.1) - i32::from(frame.view.center.1);
                dx * dx + dy * dy
            });
            self.view = Some(frame.view);
        }

        self.draw_chunks(map, &frame, ctx);
        self.draw_preview(map, frame.preview, ctx);
    }

    fn draw_chunks(&mut self, map: &Map, frame: &ZoningFrame, ctx: &mut FrameContext<'_>) {
        if frame.grid.cells().is_empty() {
            return;
        }
        profiling::scope!("zoning chunks");
        let rebuild_until = Instant::now() + ZONING_REBUILD_BUDGET - ZONING_CHUNK_HEADROOM;
        let mode = frame.view.mode;
        let mut lots = Vec::new();
        for &chunk in &self.visible {
            let state = self.chunk_states.get(&chunk);
            let has_cells = state.is_some_and(|state| state.cells != 0);
            if mode == ZoningMode::Overview && !has_cells {
                continue;
            }
            let revision = ZoningChunkRevision {
                grid: state.map_or(0, |state| state.grid_revision),
                lots: if mode == ZoningMode::Overview {
                    0
                } else {
                    state.map_or(0, |state| state.lot_revision)
                },
                map: self.map_revision,
            };
            let key = (chunk, mode);
            let cached = match self.chunks.entry(key) {
                Entry::Occupied(entry) if entry.get().revision == revision => entry.into_mut(),
                entry if Instant::now() < rebuild_until => {
                    let rebuilt = build_zoning_chunk(
                        &mut self.builder,
                        map,
                        frame,
                        chunk,
                        revision,
                        has_cells,
                        ctx.gfx,
                    );
                    entry.insert_entry(rebuilt).into_mut()
                }
                _ => continue,
            };
            if let Some(fill) = &cached.fill {
                ctx.draw(fill.clone());
            }
            lots.extend(cached.visible_lots(&frame.replaced_lots));
        }
        // A lot outline can extend into its neighbors' fills. Draw all marks last.
        if !lots.is_empty() {
            ctx.draw(lots);
        }
    }

    fn draw_preview(&mut self, map: &Map, preview: ZoningPreviewMesh, ctx: &mut FrameContext<'_>) {
        if self.preview_key.as_ref() != Some(&preview) {
            self.builder.clear();
            if let Some(fill) = &preview.fill {
                for &tile in &fill.tiles {
                    add_zoning_tile(
                        map,
                        tile,
                        fill.color,
                        ZONING_FILL_LIFT + 0.04,
                        &mut self.builder,
                    );
                }
            }
            let mut tess = self.builder.mk_tess();
            for mark in &preview.outlines {
                draw_lot_outline(
                    map.environment(),
                    &mut tess,
                    mark.corners,
                    mark.width,
                    mark.color,
                );
            }
            self.preview = build_zoning_mesh(&mut self.builder, ctx.gfx).map(Arc::new);
            self.preview_key = Some(preview);
        }
        if let Some(mesh) = &self.preview {
            ctx.draw(mesh.clone());
        }
    }
}

fn update_zone_chunks(
    map: &Map,
    chunks: &mut FastMap<ZoningChunk, ZoningChunkState>,
    previous: Option<&ZoneGrid>,
    grid: &ZoneGrid,
) {
    let Some(previous) = previous else {
        for &(x, y) in grid.cells().keys() {
            let chunk = ZoningChunk::new(zone_cell_center(x, y));
            chunks.entry(chunk).or_default().cells += 1;
        }
        return;
    };

    // The simulation exposes a global generation. Diff its sorted cells to recover exact
    // chunk revisions, including unzones and paints received while the tool was closed.
    let mut changed: FastMap<ZoningChunk, Vec<Vec2>> = FastMap::default();
    for change in previous
        .cells()
        .iter()
        .merge_join_by(grid.cells(), |a, b| a.0.cmp(b.0))
    {
        let ((x, y), delta) = match change {
            EitherOrBoth::Left((&key, _)) => (key, -1),
            EitherOrBoth::Right((&key, _)) => (key, 1),
            EitherOrBoth::Both((&key, old), (_, new)) if old != new => (key, 0),
            EitherOrBoth::Both(_, _) => continue,
        };
        let center = zone_cell_center(x, y);
        let chunk = ZoningChunk::new(center);
        let state = chunks.entry(chunk).or_default();
        state.cells = state
            .cells
            .checked_add_signed(delta)
            .expect("zone chunk cell count");
        state.grid_revision = grid.generation();
        changed.entry(chunk).or_default().push(center);
    }

    // A lot is drawn in its center chunk, but its vote can use cells across the boundary.
    for (chunk, cells) in changed {
        for id in map
            .spatial_map()
            .query(chunk.bbox(), ProjectFilter::LOT)
            .filter_map(ProjectKind::to_lot)
        {
            let lot = &map.lots()[id];
            let owner = ZoningChunk::new(lot.shape.center());
            if owner != chunk && cells.iter().any(|&cell| lot.shape.contains(cell)) {
                chunks.entry(owner).or_default().lot_revision = grid.generation();
            }
        }
    }
}

fn zone_cell_center(x: i32, y: i32) -> Vec2 {
    (vec2(x as f32, y as f32) + Vec2::splat(0.5)) * ZONE_CELL_SIZE
}

fn build_zoning_chunk(
    builder: &mut MeshBuilder<false>,
    map: &Map,
    frame: &ZoningFrame,
    chunk: ZoningChunk,
    revision: ZoningChunkRevision,
    has_cells: bool,
    gfx: &GfxContext,
) -> ZoningChunkMesh {
    profiling::scope!("build zoning chunk");
    builder.clear();
    let mode = frame.view.mode;
    if has_cells {
        build_painted_cells(map, &frame.grid, chunk, mode, builder);
    }

    let mut lot_ranges = Vec::new();
    if mode != ZoningMode::Overview {
        let mut tess = builder.mk_tess();
        for id in map
            .spatial_map()
            .query(chunk.bbox(), ProjectFilter::LOT)
            .filter_map(ProjectKind::to_lot)
        {
            let lot = &map.lots()[id];
            if ZoningChunk::new(lot.shape.center()) != chunk {
                continue;
            }
            let zoned = frame
                .grid
                .lot_zone(&lot.shape)
                .map(|zone| (zone, check_growable_lot(map, lot, zone).map(|_| ())));
            if let Some((width, color)) = lot_outline(zoned, false) {
                let start = tess.indices.len() as u32;
                draw_lot_outline(
                    map.environment(),
                    &mut tess,
                    lot.shape.corners,
                    width,
                    color.into(),
                );
                let end = tess.indices.len() as u32;
                if start < end {
                    lot_ranges.push((id, start..end));
                }
            }
        }
    }
    // Both layers share one upload and retain their original blending order.
    let (fill, lots) = match (build_zoning_mesh(builder, gfx), lot_ranges.first()) {
        (Some(mut mesh), Some((_, indices))) if indices.start > 0 => {
            let mut fill = mesh.clone();
            fill.lods[0].primitives[0].1.end = indices.start;
            mesh.lods[0].primitives[0].1.start = indices.start;
            (Some(Arc::new(fill)), Some(Arc::new(mesh)))
        }
        (mesh, Some(_)) => (None, mesh.map(Arc::new)),
        (mesh, None) => (mesh.map(Arc::new), None),
    };
    ZoningChunkMesh {
        revision,
        fill,
        lots,
        lot_ranges,
    }
}

fn build_zoning_mesh(builder: &mut MeshBuilder<false>, gfx: &GfxContext) -> Option<Mesh> {
    let mut mesh = builder.build(gfx)?;
    mesh.skip_depth = true;
    Some(mesh)
}

impl super::MapRenderer {
    pub(crate) fn queue_zoning_overlay(
        map: &Map,
        grid: Arc<ZoneGrid>,
        world: u64,
        center: Vec2,
        distance: f32,
        stroke: ZoningPreview<'_>,
    ) -> Option<String> {
        let ZoningPreview {
            stamp,
            pointer,
            hover_radius,
            pitch,
            held,
        } = stroke;
        profiling::scope!("zoning_preview");
        let mode = ZoningMode::new(distance, pitch == ZONE_CELL_SIZE);
        let overview = mode == ZoningMode::Overview;
        let view = ZoningView::new(center, distance, map.environment().bounds(), mode);
        let fill = (!stamp.cells().is_empty()).then(|| ZoningPreviewFill {
            tiles: preview_tiles(stamp, mode),
            color: stamp
                .replacement()
                .map_or(LinearColor::new(0.96, 0.28, 0.28, 1.0), |zone| {
                    zone_cell_color(zone.kind, zone.density)
                })
                .a(if held { 0.66 } else { 0.50 })
                .into(),
        });
        let (lots, warning) = pointer.map_or_else(
            || (PreviewLots::default(), None),
            |pointer| preview_lots(map, &grid, stamp, pointer, hover_radius, overview),
        );
        ZONING_FRAME.replace(Some(ZoningFrame {
            world,
            grid,
            view,
            preview: ZoningPreviewMesh {
                fill,
                outlines: lots.outlines,
            },
            replaced_lots: lots.replaced,
        }));
        warning
    }

    pub(crate) fn draw_zoned_lots(
        map: &Map,
        zones: &ZoneGrid,
        draw: &mut ImmediateDraw,
        center: Vec2,
        distance: f32,
    ) {
        if zones.cells().is_empty() {
            return;
        }
        for project in map
            .spatial_map()
            .query_around(center, distance * 1.5, ProjectFilter::LOT)
        {
            let ProjectKind::Lot(id) = project else {
                continue;
            };
            let lot = &map.lots()[id];
            if let Some(zone) = zones.lot_zone(&lot.shape) {
                draw.obb(lot.shape, lot.height + 0.32)
                    .color(zone_color(zone.kind).a(0.65));
            }
        }
    }
}

fn preview_lots(
    map: &Map,
    grid: &ZoneGrid,
    stamp: &EligiblePaintCells,
    pointer: Vec2,
    hover_radius: f32,
    overview: bool,
) -> (PreviewLots, Option<String>) {
    let hover = Circle::new(pointer, hover_radius);
    let affected = stamp
        .cells()
        .iter()
        .map(|&(x, y)| tile_bounds(x, y, ZONE_CELL_SIZE))
        .reduce(AABB::union);
    let pointer_bounds = if overview {
        AABB::new_ll_ur(pointer, pointer)
    } else {
        hover.bbox()
    };
    let lots_in = |bounds| map.spatial_map().query(bounds, ProjectFilter::LOT);
    let nearby = lots_in(pointer_bounds).filter(|&project| match project {
        ProjectKind::Lot(id) => {
            !affected.is_some_and(|area| area.intersects(&map.lots()[id].shape))
        }
        _ => false,
    });
    let mut marks = PreviewLots::default();
    let mut hovered = None;
    let mut blocked = None;
    let mut frontage = false;
    let mut eligible = false;
    for project in affected.into_iter().flat_map(lots_in).chain(nearby) {
        let ProjectKind::Lot(id) = project else {
            continue;
        };
        let lot = &map.lots()[id];
        let under_pointer = lot.shape.contains(pointer);
        let near_brush = !overview && lot.shape.intersects(&hover);
        let intersects_paint = affected.is_some_and(|area| area.intersects(&lot.shape));
        if !intersects_paint && !under_pointer && !near_brush {
            continue;
        }
        let before = grid.lot_zone(&lot.shape);
        let (touched, zone) = if intersects_paint {
            grid.lot_zone_after_prepared_paint(&lot.shape, stamp)
        } else {
            (false, before)
        };
        let zoned = zone
            .filter(|_| touched || under_pointer)
            .map(|zone| (zone, check_growable_lot(map, lot, zone).map(|_| ())));
        if touched {
            frontage |= map.get(lot.parent).is_some();
            eligible |= zoned.is_some_and(|(_, status)| status.is_ok());
            if blocked.is_none() {
                blocked = zoned.filter(|(_, status)| status.is_err());
            }
        }
        if under_pointer && hovered.is_none() {
            hovered = zoned.filter(|(_, status)| status.is_err());
        }
        if !overview && (zone != before || zone.is_none()) {
            if before.is_some() {
                marks.replaced.insert(id);
            }
            if let Some((width, color)) = lot_outline(zoned, near_brush) {
                marks.outlines.push(LotOutline {
                    corners: lot.shape.corners,
                    width,
                    color: color.into(),
                });
            }
        }
    }
    let warning = hovered.map(lot_warning).or_else(|| {
        if stamp.replacement().is_none() || stamp.cells().is_empty() || eligible {
            return None;
        }
        if let Some(blocked) = blocked {
            return Some(lot_warning(blocked));
        }
        Some(if frontage {
            "Cover at least half of a roadside lot to zone it".into()
        } else {
            "Zones need road frontage to grow".into()
        })
    });
    (marks, warning)
}

fn lot_warning((zone, status): ZonedLot) -> String {
    match status.expect_err("only blocked lots produce warnings") {
        GrowableLotError::MissingLot => "Lot is no longer available".into(),
        GrowableLotError::MissingRoad => "Lot needs road frontage to grow".into(),
        GrowableLotError::UnsupportedDensity => "Mixed Use requires Medium or High density".into(),
        GrowableLotError::MixedUseDisabled => "Mixed Use development is unavailable".into(),
        GrowableLotError::Wet => "Lot must be dry to grow".into(),
        GrowableLotError::InfeasibleGrade => "Lot grade is too steep to grow".into(),
        GrowableLotError::TooSmall => format!(
            "Lot too small for {} {}",
            density_name(zone.density),
            zone_name(zone.kind),
        ),
        GrowableLotError::Obstructed => "Lot is obstructed".into(),
        GrowableLotError::MissingPrototype => "No growable for this zone and density".into(),
        GrowableLotError::LandDenied(_) => "This lot requires editable land and access".into(),
    }
}

fn lot_outline(zone: Option<ZonedLot>, near_brush: bool) -> Option<(f32, LinearColor)> {
    match zone {
        None => near_brush.then_some((0.3, LinearColor::gray(0.6).a(0.45))),
        Some((_, Ok(()))) => Some((0.35, LinearColor::new(0.05, 0.05, 0.05, 0.45))),
        Some((_, Err(GrowableLotError::MissingRoad))) => {
            Some((0.9, LinearColor::new(1.0, 0.22, 0.36, 0.95)))
        }
        Some((_, Err(_))) => Some((0.9, LinearColor::new(1.0, 0.55, 0.10, 0.95))),
    }
}

fn draw_lot_outline(
    terrain: &Environment,
    tess: &mut Tesselator<'_>,
    corners: [Vec2; 4],
    width: f32,
    color: [f32; 4],
) {
    let mut points = [Vec3::ZERO; 25];
    let mut len = 0;
    for i in 0..4 {
        let from = corners[i];
        let to = corners[(i + 1) % 4];
        let steps = ((to - from).mag() / ZONE_CELL_SIZE).ceil().clamp(1.0, 6.0) as usize;
        for step in 0..steps {
            let xy = from + (to - from) * (step as f32 / steps as f32);
            let Some(height) = terrain.true_height(xy) else {
                return;
            };
            points[len] = xy.z(height + ROAD_Z_OFFSET * 0.85);
            len += 1;
        }
    }
    points[len] = points[0];
    let [r, g, b, a] = color;
    tess.set_color(LinearColor::new(r, g, b, a));
    tess.draw_polyline(&points[..=len], width, true);
}

fn density_inset(density: Density) -> f32 {
    match density {
        Density::Low => 1.2,
        Density::Medium => 0.5,
        Density::High => 0.1,
    }
}

fn zoning_tiles(bounds: AABB, pitch: f32) -> impl Iterator<Item = (i32, i32)> {
    let min = (bounds.ll / pitch).floor();
    let max = (bounds.ur / pitch).ceil();
    (min.x as i32..max.x as i32)
        .flat_map(move |x| (min.y as i32..max.y as i32).map(move |y| (x, y)))
}

fn tile_bounds(x: i32, y: i32, pitch: f32) -> AABB {
    AABB::new_ll_size(vec2(x as f32, y as f32) * pitch, Vec2::splat(pitch))
}

fn inset_tile(tile: AABB, inset: f32) -> AABB {
    AABB::new_ll_ur(tile.ll + Vec2::splat(inset), tile.ur - Vec2::splat(inset))
}

fn uniform_block(zones: &ZoneGrid, x: i32, y: i32, zone: ZoneCell) -> bool {
    [(x, y), (x + 1, y), (x, y + 1), (x + 1, y + 1)]
        .iter()
        .all(|key| zones.cells().get(key) == Some(&zone))
}

fn preview_tiles(stamp: &EligiblePaintCells, mode: ZoningMode) -> Vec<AABB> {
    let cells = stamp.cells();
    let density = stamp
        .replacement()
        .map_or(Density::Low, |zone| zone.density);
    let inset = if mode == ZoningMode::Overview {
        0.0
    } else {
        density_inset(density)
    };
    cells
        .iter()
        .filter_map(|&(x, y)| {
            if mode == ZoningMode::Coarse {
                let (left, bottom) = (x & !1, y & !1);
                if [
                    (left, bottom),
                    (left + 1, bottom),
                    (left, bottom + 1),
                    (left + 1, bottom + 1),
                ]
                .iter()
                .all(|key| cells.binary_search(key).is_ok())
                {
                    return ((x, y) == (left, bottom)).then(|| {
                        inset_tile(
                            tile_bounds(left / 2, bottom / 2, 2.0 * ZONE_CELL_SIZE),
                            inset,
                        )
                    });
                }
            }
            Some(inset_tile(tile_bounds(x, y, ZONE_CELL_SIZE), inset))
        })
        .collect()
}

fn build_painted_cells(
    map: &Map,
    grid: &ZoneGrid,
    chunk: ZoningChunk,
    mode: ZoningMode,
    mesh: &mut MeshBuilder<false>,
) {
    let min = chunk.corner() / ZONE_CELL_SIZE;
    let side = (ZoningChunk::SIZE_F32 / ZONE_CELL_SIZE) as i32;
    let (min_x, min_y) = (min.x as i32, min.y as i32);
    for x in min_x..min_x + side {
        for (&(x, y), &zone) in grid.cells().range((x, min_y)..(x, min_y + side)) {
            let color = zone_cell_color(zone.kind, zone.density).a(0.88);
            let tile = painted_tile(grid, mode, x, y, zone);
            add_zoning_tile(map, tile, color.into(), ZONING_FILL_LIFT, mesh);
        }
    }
}

fn painted_tile(grid: &ZoneGrid, mode: ZoningMode, x: i32, y: i32, zone: ZoneCell) -> AABB {
    let tile = tile_bounds(x, y, ZONE_CELL_SIZE);
    let inset = density_inset(zone.density);
    if mode == ZoningMode::Overview {
        // Shared XY boundaries are sampled at exactly the same positions in overview.
        tile
    } else if mode == ZoningMode::Coarse && uniform_block(grid, x & !1, y & !1, zone) {
        // Keep the terrain samples at 8 m pitch even inside a visually solid 16 m tile.
        let block = inset_tile(
            tile_bounds(x.div_euclid(2), y.div_euclid(2), 2.0 * ZONE_CELL_SIZE),
            inset,
        );
        tile.intersection(block)
    } else {
        inset_tile(tile, inset)
    }
}

fn add_zoning_tile(
    map: &Map,
    tile: AABB,
    color: [f32; 4],
    lift: f32,
    mesh: &mut MeshBuilder<false>,
) {
    let roads = ProjectFilter::ROAD | ProjectFilter::INTER;
    if map.spatial_map().query(tile, roads).next().is_some() {
        // Trim at the road footprint on slopes too, where terrain and road heights can diverge.
        let size = tile.size();
        if size.x.max(size.y) <= 2.0 {
            return;
        }
        let center = tile.center();
        for (ll, ur) in [
            (tile.ll, center),
            (vec2(center.x, tile.ll.y), vec2(tile.ur.x, center.y)),
            (vec2(tile.ll.x, center.y), vec2(center.x, tile.ur.y)),
            (center, tile.ur),
        ] {
            add_zoning_tile(map, AABB::new_ll_ur(ll, ur), color, lift, mesh);
        }
        return;
    }

    // Use the same 8 m terrain sampling for coarse preview tiles and committed cells.
    for (x, y) in zoning_tiles(tile, ZONE_CELL_SIZE) {
        let part = tile.intersection(tile_bounds(x, y, ZONE_CELL_SIZE));
        let [ll, lr, ur, ul] = [
            part.ll,
            vec2(part.ur.x, part.ll.y),
            part.ur,
            vec2(part.ll.x, part.ur.y),
        ];
        let bounds = map.environment().bounds();
        let sample = |p: Vec2| {
            map.environment()
                .true_height(p.min(bounds.ur - Vec2::splat(0.01)))
        };
        let [Some(hll), Some(hlr), Some(hur), Some(hul)] = [ll, lr, ur, ul].map(sample) else {
            continue;
        };
        mesh.extend(
            None,
            &[(ll, hll), (lr, hlr), (ur, hur), (ul, hul)].map(|(p, height)| MeshVertex {
                position: p.z(height + lift).into(),
                color,
                ..Default::default()
            }),
            &[0, 1, 3, 1, 2, 3],
        );
    }
}

#[cfg(test)]
mod zoning_tests {
    use super::*;

    #[test]
    fn u1_zoning_pan_reuses_an_overlapping_chunk() {
        let map = AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(2048.0));
        let first = ZoningView::new(Vec2::splat(128.0), 300.0, map, ZoningMode::Coarse);
        let panned = ZoningView::new(vec2(256.0, 128.0), 300.0, map, ZoningMode::Coarse);
        assert!(first.bounds != panned.bounds);
        let keys = |view: ZoningView| {
            view.chunks()
                .map(|chunk| (chunk, view.mode))
                .collect::<FastSet<_>>()
        };
        let (first, panned) = (keys(first), keys(panned));
        assert!(first.intersection(&panned).count() > 0);
        assert!(panned.difference(&first).count() > 0);
    }

    #[test]
    fn w0_water_coarse_zoning_ghost_covers_every_prepared_cell() {
        let mut terrain = Environment::new(1, 1);
        terrain
            .fill_lake(
                &geom::Polygon::centered_rect(vec2(8.0, 8.0), 14.0, 14.0),
                50,
                1.0,
            )
            .unwrap();
        let mut map = Map::default();
        assert!(map.install_environment(terrain));
        let area = OBB::new(vec2(16.0, 8.0), Vec2::X, 32.0, 16.0);
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut sim = simulation::Simulation::new_authored_flat(1, 0).unwrap();
        *sim.write::<Map>() = map;
        for (kind, expected) in [(Some(ZoneKind::MixedUse), 4), (None, 8)] {
            let map = sim.map();
            let grid = sim.read::<ZoneGrid>();
            let stamp =
                ZoneGrid::eligible_paint_stroke(&map, &[area], kind, Density::Medium).unwrap();
            let vote = grid.lot_zone_after_prepared_paint(&area, &stamp);
            super::super::MapRenderer::queue_zoning_overlay(
                &map,
                Arc::new((*grid).clone()),
                1,
                Vec2::ZERO,
                300.0,
                ZoningPreview {
                    stamp: &stamp,
                    pointer: None,
                    hover_radius: 32.0,
                    pitch: 16.0,
                    held: true,
                },
            );
            let fill = ZONING_FRAME.take().unwrap().preview.fill.unwrap();
            assert_eq!(stamp.cells().len(), expected);
            for x in 0..4 {
                for y in 0..2 {
                    assert_eq!(
                        fill.tiles
                            .iter()
                            .any(|tile| tile.contains(tile_bounds(x, y, 8.0).center())),
                        stamp.cells().contains(&(x, y)),
                    );
                }
            }
            drop(grid);
            drop(map);
            sim.apply_commands([&simulation::world_command::WorldCommand::ZonePaint {
                area,
                kind,
                density: Density::Medium,
            }]);
            let grid = sim.read::<ZoneGrid>();
            assert_eq!(grid.lot_zone(&area), vote.1);
            assert_eq!(grid.cells().len(), if kind.is_some() { 4 } else { 0 });
        }
    }

    #[test]
    fn u1_zoning_preview_covers_interpolated_stamps_without_filling_the_gap() {
        let paints = [
            OBB::new(vec2(24.0, 24.0), Vec2::X, 16.0, 16.0),
            OBB::new(vec2(40.0, 24.0), Vec2::X, 16.0, 16.0),
            OBB::new(vec2(40.0, 40.0), Vec2::X, 16.0, 16.0),
        ];
        let mut map = Map::default();
        assert!(map.install_environment(Environment::new(1, 1)));
        let stamp = ZoneGrid::eligible_paint_stroke(
            &map,
            &paints,
            Some(ZoneKind::Residential),
            Density::Low,
        )
        .unwrap();
        super::super::MapRenderer::queue_zoning_overlay(
            &map,
            Arc::new(ZoneGrid::default()),
            1,
            Vec2::ZERO,
            300.0,
            ZoningPreview {
                stamp: &stamp,
                pointer: None,
                hover_radius: 32.0,
                pitch: 16.0,
                held: true,
            },
        );
        let fill = ZONING_FRAME.take().unwrap().preview.fill.unwrap();
        let covers = |point| fill.tiles.iter().any(|tile| tile.contains(point));
        assert!(covers(vec2(24.0, 24.0)), "first stamp is visible");
        assert!(covers(vec2(40.0, 24.0)));
        assert!(covers(vec2(40.0, 40.0)));
        assert!(!covers(vec2(24.0, 40.0)));
        assert_eq!(
            ZoneGrid::eligible_paint_stroke(&map, &[paints[0]; 2], None, Density::Low)
                .unwrap()
                .cells()
                .len(),
            4
        );
    }

    #[test]
    fn u1_zoning_paint_revises_only_changed_chunks() {
        let mut map = Map::default();
        assert!(map.install_environment(Environment::new(1, 1)));
        let mut grid = ZoneGrid::default();
        let left = OBB::new(vec2(60.0, 4.0), Vec2::X, 8.0, 8.0);
        let right = OBB::new(vec2(68.0, 4.0), Vec2::X, 8.0, 8.0);
        let left_chunk = ZoningChunk::new(left.center());
        let right_chunk = ZoningChunk::new(right.center());
        grid.paint_on_map(&map, &left, Some(ZoneKind::Office), Density::Low)
            .unwrap();
        grid.paint_on_map(&map, &right, Some(ZoneKind::Office), Density::Low)
            .unwrap();
        let mut chunks = FastMap::default();
        update_zone_chunks(&map, &mut chunks, None, &grid);

        let previous = grid.clone();
        grid.paint_on_map(&map, &left, Some(ZoneKind::Office), Density::High)
            .unwrap();
        grid.paint_on_map(&map, &right, Some(ZoneKind::Office), Density::Low)
            .unwrap();
        update_zone_chunks(&map, &mut chunks, Some(&previous), &grid);
        assert_eq!(chunks[&left_chunk].grid_revision, grid.generation());
        assert_eq!(chunks[&left_chunk].cells, 1);
        assert_eq!(chunks[&right_chunk].grid_revision, 0);

        let previous = grid.clone();
        grid.paint_on_map(&map, &left, None, Density::Low).unwrap();
        update_zone_chunks(&map, &mut chunks, Some(&previous), &grid);
        assert_eq!(chunks[&left_chunk].grid_revision, grid.generation());
        assert_eq!(chunks[&left_chunk].cells, 0);
        assert_eq!(chunks[&right_chunk].grid_revision, 0);
    }

    #[test]
    fn u1_zoning_prospective_marks_exclude_committed_lot_indices() {
        assert_eq!(
            zoning_visible_ranges(0..60, [12..24, 24..36, 48..60].into_iter()).collect::<Vec<_>>(),
            [0..12, 36..48]
        );
        assert_eq!(zoning_visible_ranges(0..24, [0..24].into_iter()).count(), 0);
    }

    #[test]
    fn u1_zoning_cross_chunk_lot_and_erase_preview() {
        use simulation::map::{LanePatternBuilder, LotLayout, MapProject};

        let mut map = Map::default();
        assert!(map.install_environment(Environment::new(1, 1)));
        map.set_lot_layout(LotLayout::Uniform);
        let ground = |y| {
            let point = vec2(49.0, y);
            MapProject::ground(point.z(map.environment().height(point).unwrap()))
        };
        let (from, to) = (ground(16.0), ground(176.0));
        let (_, road) = map
            .make_connection(from, to, None, &LanePatternBuilder::new().build())
            .unwrap();
        let area = OBB::new(vec2(60.0, 52.0), Vec2::X, 8.0, 8.0);
        let lot = map
            .lots()
            .values()
            .find(|lot| lot.parent == road && lot.shape.contains(area.center()))
            .expect("the road provides frontage across the chunk boundary");
        let owner = ZoningChunk::new(lot.shape.center());
        let painted = ZoningChunk::new(area.center());
        assert_ne!(painted, owner);

        let mut grid = ZoneGrid::default();
        grid.paint_on_map(&map, &lot.shape, Some(ZoneKind::Commercial), Density::High)
            .unwrap();
        assert!(grid.lot_zone(&lot.shape).is_some());
        let stamp =
            ZoneGrid::eligible_paint_stroke(&map, &[lot.shape], None, Density::High).unwrap();
        let (preview, _) = preview_lots(&map, &grid, &stamp, lot.shape.center(), 2.0, false);
        assert!(preview.replaced.contains(&lot.id));
        assert_eq!(preview.outlines.len(), 1);
        assert_eq!(preview.outlines[0].width, 0.3);

        let mut chunks = FastMap::default();
        update_zone_chunks(&map, &mut chunks, None, &grid);
        let previous = grid.clone();
        grid.paint_on_map(&map, &area, None, Density::Low).unwrap();
        update_zone_chunks(&map, &mut chunks, Some(&previous), &grid);
        assert_eq!(chunks[&painted].grid_revision, grid.generation());
        assert_eq!(chunks[&owner].lot_revision, grid.generation());
        assert_eq!(chunks[&owner].grid_revision, 0);
    }

    #[test]
    fn u1_zoning_outlines_distinguish_blocked_lots_from_hover_halos() {
        let zone = ZoneCell {
            kind: ZoneKind::Industrial,
            density: Density::High,
        };
        assert!(lot_outline(None, false).is_none());
        assert_eq!(lot_outline(None, true).unwrap().0, 0.3);
        assert_eq!(lot_outline(Some((zone, Ok(()))), false).unwrap().0, 0.35);
        let (width, amber) =
            lot_outline(Some((zone, Err(GrowableLotError::TooSmall))), false).unwrap();
        let (_, rose) =
            lot_outline(Some((zone, Err(GrowableLotError::MissingRoad))), false).unwrap();
        assert_eq!(width, 0.9);
        assert_eq!(amber.g, 0.55);
        assert!(rose.g < amber.g);
        assert_eq!(
            lot_warning((zone, Err(GrowableLotError::TooSmall))),
            "Lot too small for High Industrial"
        );
    }

    #[test]
    fn u1_zoning_tiles_share_edges_in_overview_and_uniform_coarse_blocks() {
        let zone = ZoneCell {
            kind: ZoneKind::Residential,
            density: Density::Low,
        };
        let mut grid = ZoneGrid::default();
        let mut map = Map::default();
        assert!(map.install_environment(Environment::new(1, 1)));
        grid.paint_on_map(
            &map,
            &OBB::new(Vec2::splat(8.0), Vec2::X, 16.0, 16.0),
            Some(zone.kind),
            zone.density,
        )
        .unwrap();
        let coarse = ZoningMode::Coarse;
        let detail = ZoningMode::Detail;
        let overview = ZoningMode::Overview;
        for key in [coarse, overview] {
            assert_eq!(
                painted_tile(&grid, key, 0, 0, zone).ur.x,
                painted_tile(&grid, key, 1, 0, zone).ll.x
            );
        }
        assert_eq!(painted_tile(&grid, coarse, 0, 0, zone).ll.x, 1.2);
        assert_eq!(painted_tile(&grid, overview, 0, 0, zone).ll.x, 0.0);
        assert!(painted_tile(&grid, detail, 0, 0, zone).ur.x < 8.0);

        let cell = OBB::new(vec2(12.0, 4.0), Vec2::X, 8.0, 8.0);
        grid.paint_on_map(&map, &cell, Some(zone.kind), Density::High)
            .unwrap();
        assert!(painted_tile(&grid, coarse, 0, 0, zone).ur.x < 8.0);
    }
}

/// Sprite direction and size so that its top faces the top of the screen as closely as
/// the footprint allows at the default camera yaw.
fn upright_sprite([a, b]: [Vec2; 2]) -> (Vec2, (f32, f32)) {
    let screen_up = -Camera::new(Vec3::ZERO, 1.0, 1.0).yaw.vec2();
    let (up, across) = [(a, b), (-a, b), (b, a), (-b, a)]
        .into_iter()
        .max_by(|(p, _), (q, _)| {
            let p = p.normalize().dot(screen_up);
            p.total_cmp(&q.normalize().dot(screen_up))
        })
        .unwrap();
    let right = vec2(up.y, -up.x).normalize();
    (right, (across.mag(), up.mag()))
}

fn growable_company(id: GoodsCompanyID) -> Option<&'static GrowableCompanyPrototype> {
    try_prototype(GrowableCompanyID::from(id)).filter(|proto| proto.growable)
}

impl MapMeshHandler {
    pub fn new(gfx: &mut GfxContext, sim: &Simulation) -> Self {
        let arrow_builder = SpriteBatchBuilder::from_path(gfx, "assets/sprites/arrow_one_way.png");

        let mut buildsprites = FastMap::default();
        let mut buildmeshes = FastMap::default();
        let mut zonemeshes = FastMap::default();

        let mut roof_icons = FastMap::default();
        for descr in GoodsCompanyPrototype::iter() {
            if descr.zone.is_some() || growable_company(descr.id).is_some() {
                continue;
            }
            let RenderAsset::Sprite { path } = &descr.asset else {
                continue;
            };

            let sprite = if let Some(icon) =
                ServiceIcon::for_company(descr).filter(|_| placeholder_color(descr.id).is_some())
            {
                let texture = roof_icons.entry(icon).or_insert_with(|| icon.texture(gfx));
                SpriteBatchBuilder::new(texture, gfx)
            } else {
                SpriteBatchBuilder::new(&gfx.texture(path, "goods_company_tex"), gfx)
            };
            buildsprites.insert(BuildingKind::GoodsCompany(descr.id), sprite);
        }

        for (asset, bkind) in GoodsCompanyPrototype::iter()
            .filter(|descr| growable_company(descr.id).is_none())
            .map(|descr| (&descr.asset, BuildingKind::GoodsCompany(descr.id)))
            .chain(
                FreightStationPrototype::iter()
                    .map(|descr| (&descr.asset, BuildingKind::RailFreightStation(descr.id))),
            )
            .chain([(
                &RenderAsset::Mesh {
                    path: "external_trading.glb".into(),
                },
                BuildingKind::ExternalTrading,
            )])
        {
            let RenderAsset::Mesh { path } = asset else {
                continue;
            };
            let m = match gfx.mesh(path) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("Failed to load mesh {}: {:?}", asset, e);
                    continue;
                }
            };

            buildmeshes.insert(bkind, InstancedMeshBuilder::new_ref(&m));
        }

        for descr in GoodsCompanyPrototype::iter() {
            let Some(ref z) = descr.zone else { continue };
            let floor = &z.floor;
            let filler = &z.filler;

            let floor_tex = gfx.texture(floor, "zone_floor_tex");
            let floor_mat = gfx.register_material(Material::new(
                gfx,
                &floor_tex,
                MetallicRoughness {
                    metallic: 0.0,
                    roughness: 1.0,
                    tex: None,
                },
                None,
            ));
            let floor_mesh = MeshBuilder::new(floor_mat);

            let m = match gfx.mesh(filler.as_ref()) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("Failed to load mesh for zone {}: {:?}", filler, e);
                    continue;
                }
            };

            let filler_mesh = InstancedMeshBuilder::new_ref(&m);

            zonemeshes.insert(
                BuildingKind::GoodsCompany(descr.id),
                (floor_mesh, filler_mesh, z.randomize_filler),
            );
        }

        let crosswalk_tex = gfx.texture("assets/sprites/crosswalk.png", "crosswalk");
        let crosswalk_mat = gfx.register_material(Material::new(
            gfx,
            &crosswalk_tex,
            MetallicRoughness {
                metallic: 0.0,
                roughness: 1.0,
                tex: None,
            },
            None,
        ));
        let houses_mat = gfx.register_material(Material::new(
            gfx,
            &gfx.palette(),
            MetallicRoughness {
                metallic: 0.0,
                roughness: 1.0,
                tex: None,
            },
            None,
        ));
        let builders = MapBuilders {
            arrow_builder,
            buildsprites,
            crosswalk_builder: MeshBuilder::new(crosswalk_mat),
            mesh_map: MeshBuilder::new(gfx.tess_material),
            houses_mesh: MeshBuilder::new(houses_mat),
            buildmeshes,
            zonemeshes,
            mesh_lots: MeshBuilder::new(gfx.tess_material),
        };

        Self {
            builders,
            cache: Default::default(),
            road_sub: sim
                .map()
                .subscribe(UpdateType::Road | UpdateType::RoadSupport),
            building_sub: sim.map().subscribe(UpdateType::Building),
            map_version: None,
            rebuilt_version: None,
            zoning: ZoningMeshes {
                updates: sim
                    .map()
                    .subscribe(UpdateType::Road | UpdateType::Building | UpdateType::Terrain),
                world: None,
                grid: None,
                chunk_states: Default::default(),
                map_revision: 0,
                view: None,
                visible: Vec::new(),
                chunks: Default::default(),
                preview: None,
                preview_key: None,
                builder: MeshBuilder::new(gfx.tess_material),
            },
        }
    }

    /// Tag this frame's map snapshot for the automation barrier.
    pub fn set_map_version(&mut self, tick: u64) {
        self.map_version = Some(tick);
    }

    pub fn rebuilt_version(&self) -> Option<u64> {
        self.rebuilt_version
    }

    pub fn latest_mesh(
        &mut self,
        map: &Map,
        options: MapRenderOptions,
        ctx: &mut FrameContext<'_>,
    ) {
        profiling::scope!("draw map mesh");
        for chunk in self.road_sub.take_updated_chunks() {
            profiling::scope!("build road chunk");
            let b = &mut self.builders;
            b.map_mesh(map, chunk);

            let cached = self.cache.entry(chunk).or_default();

            cached.road.clear();
            cached.road.reserve(2);

            if let Some(mesh) = b.mesh_map.build(ctx.gfx) {
                cached.road.push(Arc::new(mesh));
            }
            if let Some(mesh) = b.crosswalk_builder.build(ctx.gfx) {
                cached.road.push(Arc::new(mesh));
            }

            cached.lots = b.mesh_lots.build(ctx.gfx);
            cached.arrows = b.arrow_builder.build(ctx.gfx);

            if cached.is_empty() {
                self.cache.remove(&chunk);
            }
        }

        for chunk in self.building_sub.take_updated_chunks() {
            profiling::scope!("build building chunk");

            let b = &mut self.builders;
            b.buildings_mesh(map, chunk);

            let cached = self.cache.entry(chunk).or_default();

            cached.build.clear();
            cached.build.reserve(4);

            let buildmeshes = b
                .buildmeshes
                .values_mut()
                .flat_map(|x| x.build(ctx.gfx))
                .collect::<Vec<_>>();

            if !buildmeshes.is_empty() {
                cached.build.push(Arc::new(buildmeshes));
            }

            if let Some(mesh) = b.houses_mesh.build(ctx.gfx) {
                cached.build.push(Arc::new(mesh));
            }

            // Sprites don't write depth, so they go after the placeholder boxes they sit on.
            let sprites = b
                .buildsprites
                .values_mut()
                .flat_map(|x| x.build(ctx.gfx))
                .collect::<Vec<_>>();

            if !sprites.is_empty() {
                cached.build.push(Arc::new(sprites));
            }

            let zonemeshes = b
                .zonemeshes
                .values_mut()
                .flat_map(|(a, b, _)| a.build(ctx.gfx).zip(b.build(ctx.gfx)))
                .collect::<Vec<_>>();
            if !zonemeshes.is_empty() {
                cached.build.push(Arc::new(zonemeshes));
            }

            if cached.is_empty() {
                self.cache.remove(&chunk);
            }
        }
        self.rebuilt_version = self.map_version.take();

        profiling::scope!("prepare map mesh");
        for v in self.cache.values() {
            ctx.draw(v.build.clone());
            ctx.draw(v.road.clone());
            if options.show_arrows {
                if let Some(ref x) = v.arrows {
                    ctx.draw(x.clone());
                }
            }
            if options.show_lots {
                if let Some(ref x) = v.lots {
                    ctx.draw(x.clone());
                }
            }
        }
        self.zoning.draw(map, ctx);
    }
}

impl MapBuilders {
    fn arrows(arrow_builder: &mut SpriteBatchBuilder<false>, road: &Road, lanes: &Lanes) {
        let has_forward = road
            .outgoing_lanes_from(road.src)
            .iter()
            .filter(|(_, kind)| kind.needs_arrows())
            .count()
            > 0;
        let has_backward = road
            .outgoing_lanes_from(road.dst)
            .iter()
            .filter(|(_, kind)| kind.needs_arrows())
            .count()
            > 0;
        let is_two_way = has_forward && has_backward;

        if is_two_way {
            return;
        }

        let n_arrows = ((road.length() / 50.0) as i32).max(1);

        let fade =
            (road.length() - 5.0 - road.interface_from(road.src) - road.interface_from(road.dst))
                .mul(0.2)
                .clamp(0.0, 1.0);

        for (id, _) in road.lanes_iter().filter(|(_, kind)| kind.needs_arrows()) {
            let lane = &lanes[id];
            let l = lane.points.length();
            for i in 0..n_arrows {
                let (mid, dir) = lane
                    .points
                    .point_dir_along(l * (1.0 + i as f32) / (1.0 + n_arrows as f32));

                arrow_builder.push(
                    mid.up(0.03),
                    dir,
                    LinearColor::gray(0.3 + fade * 0.1),
                    (4.0, 4.0),
                );
            }
        }
    }

    fn crosswalks(crosswalk_builder: &mut MeshBuilder<false>, inter: &Intersection, lanes: &Lanes) {
        const WALKING_W: f32 = LaneKind::Walking.width();

        for turn in inter.turns() {
            let id = turn.id;

            if matches!(turn.kind, TurnKind::Crosswalk) {
                let from = lanes[id.src].get_inter_node_pos(inter.id).up(0.01);
                let to = lanes[id.dst].get_inter_node_pos(inter.id).up(0.01);

                let l = (to - from).mag();

                if l < WALKING_W {
                    continue;
                }

                let dir = (to - from) / l;
                let perp = dir.perp_up() * CROSSWALK_WIDTH * 0.5;
                let pos = from + dir * WALKING_W * 0.5;
                let height = l - WALKING_W;

                crosswalk_builder.extend_with(None, |vertices, add_index| {
                    let mk_v = |position: Vec3, uv: Vec2| MeshVertex {
                        position: position.into(),
                        uv: uv.into(),
                        normal: Vec3::Z,
                        color: [1.0; 4],
                        tangent: [0.0; 4],
                    };

                    vertices.push(mk_v(pos - perp, Vec2::ZERO));
                    vertices.push(mk_v(pos + perp, Vec2::ZERO));
                    vertices.push(mk_v(pos + perp + dir * height, Vec2::x(height)));
                    vertices.push(mk_v(pos - perp + dir * height, Vec2::x(height)));

                    add_index(0);
                    add_index(1);
                    add_index(2);

                    add_index(0);
                    add_index(2);
                    add_index(3);
                });
            }
        }
    }

    fn buildings_mesh(&mut self, map: &Map, chunk: SubscriberChunkID) {
        for v in self.buildsprites.values_mut() {
            v.clear();
        }
        for v in self.buildmeshes.values_mut() {
            v.instances.clear();
        }
        for v in self.zonemeshes.values_mut() {
            v.0.clear();
            v.1.instances.clear();
        }
        self.houses_mesh.clear();

        let buildings = &map.buildings();
        for building in map
            .spatial_map()
            .query(chunk.bbox(), ProjectFilter::BUILDING)
            .map(|p| {
                if let ProjectKind::Building(b) = p {
                    b
                } else {
                    unreachable!()
                }
            })
        {
            let building = &buildings[building];
            if SubscriberChunkID::new(building.canonical_position()) != chunk {
                continue;
            }
            if append_profiled_building(&mut self.houses_mesh, building) {
                continue;
            }
            let mut sprite_z = building.height;
            let mut placeholder = false;
            if let BuildingKind::GoodsCompany(id) = building.kind {
                if let Some(proto) = growable_company(id) {
                    let tint = zone_color(proto.zone_kind);
                    self.building_box(building, tint, proto.level);
                    continue;
                }
                // Service and warehouse shells carry their icon on the roof.
                if self.buildsprites.contains_key(&building.kind) {
                    if let Some(color) = placeholder_color(id) {
                        sprite_z = self.building_box(building, color, 1);
                        placeholder = true;
                    }
                }
            }
            if !placeholder {
                self.zone_mesh(building);
                self.houses_mesh(building);
            }

            if let Some(x) = self.buildsprites.get_mut(&building.kind) {
                let axis = building.obb.axis();
                let c = building.obb.center();
                let (dir, scale) = if placeholder {
                    let (dir, (width, height)) = upright_sprite(axis);
                    let size = width.min(height);
                    (dir, (size, size))
                } else {
                    let w = axis[0].mag();
                    (axis[0] / w, (w, axis[1].mag()))
                };
                x.push(c.z(sprite_z + 0.1), dir.z0(), LinearColor::WHITE, scale);
            } else if let Some(x) = self.buildmeshes.get_mut(&building.kind) {
                let pos = building.obb.center().z(building.height);
                let dir = building.obb.axis()[0].normalize().z0();

                x.instances.push(MeshInstance {
                    pos,
                    dir,
                    tint: LinearColor::WHITE,
                });
            } else if !building
                .mesh
                .faces
                .iter()
                .flat_map(|(face, _)| face)
                .any(|point| point.z > building.height + 0.2)
            {
                self.building_box(building, LinearColor::new(0.55, 0.48, 0.40, 1.0), 1);
            }
        }
    }

    /// Returns the roof height.
    fn building_box(&mut self, building: &Building, tint: LinearColor, level: u8) -> f32 {
        let [width, depth] = building.obb.axis().map(Vec2::mag);
        let height = (width.min(depth) * 0.45).clamp(3.0, 18.0)
            * (1.0 + f32::from(level.saturating_sub(1)) * 0.45);
        let mut corners = building.obb.corners;
        if (corners[1] - corners[0])
            .z0()
            .cross((corners[2] - corners[0]).z0())
            .z
            < 0.0
        {
            corners.reverse();
        }
        let base = corners.map(|p| p.z(building.height));
        let roof = corners.map(|p| p.z(building.height + height));
        let mut push_face = |face: [Vec3; 4], color: LinearColor| {
            let Some(normal) = (face[1] - face[0]).cross(face[2] - face[0]).try_normalize() else {
                return;
            };
            self.houses_mesh.extend_with(None, |vertices, add_index| {
                vertices.extend(face.map(|position| MeshVertex {
                    position: position.into(),
                    normal,
                    uv: [0.0; 2],
                    color: color.into(),
                    tangent: [0.0; 4],
                }));
                for index in [0, 1, 2, 0, 2, 3] {
                    add_index(index);
                }
            });
        };
        push_face(roof, tint);
        for i in 0..4 {
            let next = (i + 1) % 4;
            push_face(
                [base[i], base[next], roof[next], roof[i]],
                LinearColor::new(tint.r * 0.78, tint.g * 0.75, tint.b * 0.73, tint.a),
            );
        }
        building.height + height
    }

    fn zone_mesh(&mut self, building: &Building) {
        let Some(bzone) = &building.zone else {
            return;
        };
        let Some((zone_mesh, filler, randomize)) = self.zonemeshes.get_mut(&building.kind) else {
            return;
        };
        let zone = &bzone.poly;
        let randomize = *randomize;

        let mut hull = building
            .mesh
            .faces
            .iter()
            .flat_map(|x| x.0.iter())
            .map(|x| x.xy())
            .collect::<Polygon>()
            .convex_hull();
        hull.simplify();
        hull.scale_from(hull.barycenter(), 1.8);

        let Some(filldir) = bzone.unit_fill_direction() else {
            return;
        };
        let principal_axis = building.obb.axis()[0].normalize().rotated_by(filldir);

        let Some((min, max)) = minmax(zone.iter().map(|x| x.rotated_by(principal_axis.flipy())))
        else {
            return;
        };
        let extent = max - min;
        if !extent.is_finite() {
            return;
        }
        let min = min.rotated_by(principal_axis);
        let secondary_axis = -principal_axis.perpendicular();

        // No valid zone can span more than half its perimeter in any direction.
        let limit = simulation::map::MAX_ZONE_PERIMETER * 0.5;
        for principal_offset in (0..=extent.x.min(limit) as i32).step_by(4) {
            for secondary_offset in (0..=extent.y.min(limit) as i32).step_by(4) {
                let mut pos = min
                    + principal_axis * principal_offset as f32
                    + secondary_axis * secondary_offset as f32;
                if randomize {
                    pos = pos
                        + vec2(
                            common::rand::rand3(pos.x, pos.y, 10.0),
                            common::rand::rand3(pos.x, pos.y, 20.0),
                        ) * 2.0
                        - 1.0 * Vec2::XY;
                }

                if !zone.contains(pos) {
                    continue;
                }
                if zone.distance(pos) < 3.0 {
                    continue;
                }
                if hull.contains(pos) {
                    continue;
                }

                filler.instances.push(MeshInstance {
                    pos: pos.z(building.height),
                    dir: principal_axis.perpendicular().z0(),
                    tint: LinearColor::WHITE,
                });
            }
        }

        let avg = -zone.0.iter().sum::<Vec2>() / zone.len() as f32;

        zone_mesh.extend_with(None, |vertices, add_index| {
            for p in &zone.0 {
                vertices.push(MeshVertex {
                    position: p.z(building.height + 0.05).into(),
                    normal: Vec3::Z,
                    uv: ((*p + avg) * 0.05).into(),
                    color: [1.0; 4],
                    tangent: [0.0; 4],
                });
            }

            earcut(&zone.0, &[], |a, b, c| {
                add_index(a as u32);
                add_index(b as u32);
                add_index(c as u32);
            });
        })
    }

    fn houses_mesh(&mut self, building: &Building) {
        let tint =
            (building.kind == BuildingKind::House).then(|| zone_color(ZoneKind::Residential));
        for (face, col) in &building.mesh.faces {
            let mut color: LinearColor = (*col).into();
            if let Some(tint) = tint {
                color.r = color.r * 0.65 + tint.r * 0.35;
                color.g = color.g * 0.65 + tint.g * 0.35;
                color.b = color.b * 0.65 + tint.b * 0.35;
            }
            self.houses_mesh.extend_with(None, |vertices, add_index| {
                append_building_face(face, color, vertices, add_index);
            });
        }
    }

    fn draw_rail(tess: &mut Tesselator, cut: &PolyLine3, off: f32, _limits: bool) {
        tess.set_color(Color::gray(0.5));
        tess.draw_polyline_full(
            cut.as_slice().iter().map(|v| vec3(v.x, v.y, v.z + 0.02)),
            unwrap_ret!(cut.first_dir()).xy(),
            unwrap_ret!(cut.last_dir()).xy(),
            0.1,
            off + 0.6,
        );
        tess.draw_polyline_full(
            cut.as_slice().iter().map(|v| vec3(v.x, v.y, v.z + 0.02)),
            unwrap_ret!(cut.first_dir()).xy(),
            unwrap_ret!(cut.last_dir()).xy(),
            0.1,
            off - 0.6,
        );
        //for (v, dir) in cut.equipoints_dir(1.0, !limits) {
        //    let up = vec3(v.x, v.y, v.z + 0.04);
        //    tess.draw_polyline_full(
        //        [up, up + dir * 0.1].into_iter(),
        //        dir.xy(),
        //        dir.xy(),
        //        2.0,
        //        off,
        //    );
        //}
    }

    fn map_mesh(&mut self, map: &Map, chunk: SubscriberChunkID) {
        self.arrow_builder.clear();
        self.crosswalk_builder.clear();
        self.mesh_map.clear();
        self.mesh_lots.clear();

        let mut tess_map = self.mesh_map.mk_tess();
        let mut tess_lots = self.mesh_lots.mk_tess();

        let low_col: LinearColor = simulation::colors().road_low_col.into();
        let mid_col: LinearColor = simulation::colors().road_mid_col.into();
        let hig_col: LinearColor = simulation::colors().road_hig_col.into();
        let line_col: LinearColor = simulation::colors().road_line_col.into();

        let objs = map.spatial_map().query(
            chunk.bbox(),
            ProjectFilter::ROAD | ProjectFilter::LOT | ProjectFilter::INTER,
        );

        let mut chunk_roads = Vec::new();
        let mut chunk_lots = Vec::new();
        let mut chunk_inters = Vec::new();

        for obj in objs {
            if SubscriberChunkID::new(obj.canonical_position(map)) != chunk {
                continue;
            }
            match obj {
                ProjectKind::Road(road) => chunk_roads.push(road),
                ProjectKind::Lot(lot) => chunk_lots.push(lot),
                ProjectKind::Intersection(inter) => chunk_inters.push(inter),
                _ => {}
            }
        }

        let inters = map.intersections();
        let lanes = map.lanes();
        let roads = map.roads();
        let lots = map.lots();
        let env = map.environment();

        for road in chunk_roads {
            let road = &roads[road];

            Self::arrows(&mut self.arrow_builder, road, lanes);

            let cut = road.interfaced_points();
            let first_dir = unwrap_cont!(cut.first_dir());
            let last_dir = unwrap_cont!(cut.last_dir());

            road_pylons(&mut tess_map, env, road);

            tess_map.normal.z = -1.0;
            tess_map.draw_polyline_full(
                cut.iter().map(|x| x.up(-0.3)),
                first_dir.xy(),
                last_dir.xy(),
                road.width,
                0.0,
            );
            tess_map.normal.z = 1.0;

            let draw_off = |tess: &mut Tesselator, col: LinearColor, w, off| {
                tess.set_color(col);
                tess.draw_polyline_full(
                    cut.as_slice().iter().copied(),
                    first_dir.xy(),
                    last_dir.xy(),
                    w,
                    off,
                );
            };

            let mut start = true;
            for l in road.lanes_iter().flat_map(|(l, _)| lanes.get(l)) {
                if l.kind.is_rail() {
                    let off = l.dist_from_bottom - road.width * 0.5 + LaneKind::Rail.width() * 0.5;
                    draw_off(&mut tess_map, mid_col, LaneKind::Rail.width(), off);
                    Self::draw_rail(&mut tess_map, cut, off, true);
                    start = true;
                    continue;
                }
                if start {
                    draw_off(
                        &mut tess_map,
                        line_col,
                        0.25,
                        l.dist_from_bottom - road.width * 0.5,
                    );
                    start = false;
                }
                draw_off(
                    &mut tess_map,
                    match l.kind {
                        LaneKind::Walking => hig_col,
                        LaneKind::Parking => low_col,
                        _ => mid_col,
                    },
                    l.kind.width() - 0.25,
                    l.dist_from_bottom - road.width * 0.5 + l.kind.width() * 0.5,
                );
                draw_off(
                    &mut tess_map,
                    line_col,
                    0.25,
                    l.dist_from_bottom - road.width * 0.5 + l.kind.width(),
                );
            }
        }

        // Intersections
        let mut p = Vec::with_capacity(8);
        let mut ppoly = unsafe { PolyLine3::new_unchecked(vec![]) };
        for inter in chunk_inters {
            let inter = &inters[inter];

            let interpos = inter.pos.up(ROAD_Z_OFFSET);

            if inter.roads.is_empty() {
                tess_map.set_color(line_col);
                tess_map.draw_circle(interpos, 5.5);

                tess_map.set_color(mid_col);
                tess_map.draw_circle(interpos, 5.0);
                continue;
            }

            Self::crosswalks(&mut self.crosswalk_builder, inter, lanes);

            inter_pylon(&mut tess_map, env, inter, roads);
            intersection_mesh(&mut tess_map, &hig_col, inter, roads);

            // Walking corners
            for turn in inter
                .turns()
                .filter(|turn| matches!(turn.kind, TurnKind::WalkingCorner))
            {
                tess_map.set_color(line_col);
                let id = turn.id;

                let w = lanes[id.src].kind.width();

                let first_dir = -lanes[id.src].orientation_from(id.parent);
                let last_dir = lanes[id.dst].orientation_from(id.parent);

                p.clear();
                p.extend_from_slice(turn.points.as_slice());

                tess_map.draw_polyline_full(p.iter().copied(), first_dir, last_dir, 0.25, w * 0.5);
                tess_map.draw_polyline_full(p.iter().copied(), first_dir, last_dir, 0.25, -w * 0.5);

                tess_map.set_color(hig_col);

                p.clear();
                p.extend_from_slice(turn.points.as_slice());

                tess_map.draw_polyline_with_dir(&p, first_dir, last_dir, w - 0.25);
            }

            // Rail turns
            for turn in inter
                .turns()
                .filter(|turn| matches!(turn.kind, TurnKind::Rail))
            {
                ppoly.clear_extend(turn.points.as_slice());
                Self::draw_rail(&mut tess_map, &ppoly, 0.0, false);
            }
        }

        // Lots
        for lot in chunk_lots {
            let lot = &lots[lot];
            let col = match lot.kind {
                LotKind::Unassigned => simulation::colors().lot_unassigned_col,
                LotKind::Residential => simulation::colors().lot_residential_col,
            };
            tess_lots.set_color(col);
            tess_lots.draw_filled_polygon(&lot.shape.corners, lot.height + 0.28);
        }
    }
}

fn add_polyon(
    mut tess: &mut Tesselator,
    w: f32,
    PylonPosition {
        terrain_height,
        pos,
        dir,
    }: PylonPosition,
) {
    let color = LinearColor::from(simulation::colors().road_pylon_col);
    let color: [f32; 4] = color.into();

    let up = pos.up(-0.2);
    let down = pos.xy().z(terrain_height - 20.0);
    let dirp = dir.perp_up();
    let d2 = dir.xy().z0();
    let d2p = d2.perp_up();
    let d2 = d2 * w * 0.5;
    let d2p = d2p * w * 0.5;
    let dir = dir * w * 0.5;
    let dirp = dirp * w * 0.5;
    // down rect
    // 2 --- 1 -> dir
    // |     |
    // |     |
    // 3-----0
    // | dirp
    // v

    // up rect
    // 6 --- 5
    // |     |
    // |     |
    // 7-----4
    let verts = [
        down + d2 + d2p, // 0
        down + d2 - d2p, // 1
        down - d2 - d2p, // 2
        down - d2 + d2p, // 3
        up + dir + dirp, // 4
        up + dir - dirp, // 5
        up - dir - dirp, // 6
        up - dir + dirp, // 7
    ];

    let mr = &mut tess;
    let mut quad = move |a, b, c, d, nor| {
        mr.extend_with(move |vertices, add_idx| {
            let mut pvert = move |p: Vec3, normal: Vec3| {
                vertices.push(MeshVertex {
                    position: p.into(),
                    normal,
                    uv: [0.0; 2],
                    color,
                    tangent: [0.0; 4],
                })
            };

            pvert(verts[a], nor);
            pvert(verts[b], nor);
            pvert(verts[c], nor);
            pvert(verts[d], nor);

            add_idx(0);
            add_idx(1);
            add_idx(2);

            add_idx(1);
            add_idx(3);
            add_idx(2);
        });
    };
    quad(0, 1, 4, 5, d2);
    quad(1, 2, 5, 6, -d2p);
    quad(2, 3, 6, 7, -d2);
    quad(3, 0, 7, 4, d2p);
}

fn road_pylons(meshb: &mut Tesselator, env: &Environment, road: &Road) {
    for pylon in Road::pylons_positions(road.interfaced_points(), env) {
        add_polyon(meshb, road.width * 0.5, pylon);
    }
}

fn inter_pylon(tess: &mut Tesselator, env: &Environment, inter: &Intersection, roads: &Roads) {
    let interpos = inter.pos.up(ROAD_Z_OFFSET);

    let h = unwrap_ret!(env.true_height(inter.pos.xy()));
    if (h - interpos.z).abs() <= 2.0 {
        return;
    }

    let mut maxw = 3.0f32;
    let mut avgp = Vec3::ZERO;

    for &road in &inter.roads {
        let r = &roads[road];
        maxw = maxw.max(r.width * 0.5);
        avgp += r.interface_point(inter.id);
    }
    if !inter.roads.is_empty() {
        avgp /= inter.roads.len() as f32;
    } else {
        avgp = interpos;
    }

    add_polyon(
        tess,
        maxw,
        PylonPosition {
            terrain_height: h,
            pos: avgp,
            dir: Vec3::X,
        },
    );
}

pub(crate) fn intersection_mesh(
    tess: &mut Tesselator,
    center_col: &LinearColor,
    inter: &Intersection,
    roads: &Roads,
) {
    let interpos = inter.pos.up(ROAD_Z_OFFSET);
    let id = inter.id;

    let getw = |road: &Road| {
        if road.sidewalks(id).outgoing.is_some() {
            road.width * 0.5 - LaneKind::Walking.width()
        } else {
            road.width * 0.5
        }
    };

    let mut polygon = Polygon::default();

    for (i, &road) in inter.roads.iter().enumerate() {
        #[allow(clippy::indexing_slicing)]
        let road = &roads[road];

        #[allow(clippy::indexing_slicing)]
        let next_road = &roads[inter.roads[(i + 1) % inter.roads.len()]];

        let ip = road.interfaced_points();

        let firstp;
        let firstdir;
        if road.dst == inter.id {
            firstp = ip.last();
            firstdir = ip.last_dir().map(Vec3::neg);
        } else {
            firstp = ip.first();
            firstdir = ip.first_dir();
        }

        let src_orient = -unwrap_cont!(firstdir).xy();

        let left = firstp.xy() + src_orient.perpendicular() * getw(road);

        let ip = next_road.interfaced_points();

        let firstp;
        let firstdir;
        if next_road.dst == inter.id {
            firstp = ip.last();
            firstdir = ip.last_dir().map(Vec3::neg);
        } else {
            firstp = ip.first();
            firstdir = ip.first_dir();
        }

        let dst_orient = unwrap_cont!(firstdir).xy();
        let next_right = firstp.xy() + dst_orient.perpendicular() * getw(next_road);

        if inter.is_roundabout() {
            if let Some(rp) = inter.turn_policy.roundabout {
                let center = interpos.xy();

                let ang = (left - center)
                    .normalize()
                    .angle((next_right - center).normalize())
                    .abs();
                if ang >= Radians::from_deg(21.0).0 {
                    polygon.extend(Turn::gen_roundabout(
                        left.z(0.0),
                        next_right.z(0.0),
                        src_orient,
                        dst_orient,
                        rp.radius + 3.0,
                        center,
                    ));

                    tess.set_color(center_col);
                    tess.draw_circle(center.z(interpos.z + 0.01), rp.radius * 0.5);

                    continue;
                }
            }
        }

        let spline = Turn::spline(left, next_right, src_orient, dst_orient);

        polygon.extend(spline.smart_points(1.0, 0.0, 1.0));
    }

    polygon.simplify();

    let col = LinearColor::from(simulation::colors().road_mid_col).into();
    tess.extend_with(move |vertices, add_idx| {
        vertices.extend(polygon.iter().map(|pos| MeshVertex {
            position: pos.z(interpos.z - 0.001).into(),
            normal: Vec3::Z,
            uv: [0.0; 2],
            color: col,
            tangent: [0.0; 4],
        }));
        earcut(&polygon.0, &[], |a, b, c| {
            add_idx(a as u32);
            add_idx(b as u32);
            add_idx(c as u32);
            add_idx(c as u32);
            add_idx(b as u32);
            add_idx(a as u32);
        });
    });
}
