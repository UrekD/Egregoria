use crate::map::{
    spend_water_work, Building, BuildingID, CanonicalPosition, Intersection, IntersectionID, Lot,
    LotID, Map, Road, RoadID, WaterEditError,
};
use common::{FastMap, FastSet};
use derive_more::From;
use flat_spatial::aabbgrid::{AABBGridHandle, AABBGridObjects, StoreObject};
use flat_spatial::storage::{CellIdx, SparseStorage};
use flat_spatial::AABBGrid;
use geom::{Circle, Intersect, Shape, ShapeEnum, Vec2, AABB};
use itertools::Either;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::{BitOr, Neg, Sub};

// A generated lot is at most 40 m square and can span at most 3x3 cells.
const GENERATED_LOT_CELLS: usize = 9;
const CELL_SIZE: i32 = 50;
const MAX_LOT_CELLS: usize = 256;
pub(super) const MAX_LOT_MEMBERSHIPS: usize = 4_194_304;

pub(super) fn validated_lot_memberships(bounds: AABB) -> Option<usize> {
    let count = CellRange::checked(bounds)?.len();
    (count <= MAX_LOT_CELLS).then_some(count)
}

#[derive(Copy, Clone, Debug, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize, From)]
pub enum ProjectKind {
    Intersection(IntersectionID),
    Road(RoadID),
    Building(BuildingID),
    Lot(LotID),
    Ground,
}

impl ProjectKind {
    pub fn to_lot(self) -> Option<LotID> {
        if let ProjectKind::Lot(id) = self {
            Some(id)
        } else {
            None
        }
    }

    pub fn canonical_position(&self, map: &Map) -> Vec2 {
        match *self {
            ProjectKind::Intersection(id) => map
                .intersections
                .get(id)
                .map_or(Vec2::ZERO, CanonicalPosition::canonical_position),
            ProjectKind::Road(id) => map
                .roads
                .get(id)
                .map_or(Vec2::ZERO, CanonicalPosition::canonical_position),
            ProjectKind::Building(id) => map
                .buildings
                .get(id)
                .map_or(Vec2::ZERO, CanonicalPosition::canonical_position),
            ProjectKind::Lot(id) => map
                .lots
                .get(id)
                .map_or(Vec2::ZERO, CanonicalPosition::canonical_position),
            ProjectKind::Ground => Vec2::ZERO,
        }
    }

    pub fn check_valid(&self, map: &Map) -> bool {
        match *self {
            ProjectKind::Intersection(id) => map.intersections.contains_key(id),
            ProjectKind::Road(id) => map.roads.contains_key(id),
            ProjectKind::Building(id) => map.buildings.contains_key(id),
            ProjectKind::Lot(id) => map.lots.contains_key(id),
            ProjectKind::Ground => true,
        }
    }

    pub fn as_building(&self) -> Option<BuildingID> {
        match *self {
            Self::Building(b) => Some(b),
            _ => None,
        }
    }

    pub fn is_ground(&self) -> bool {
        matches!(self, ProjectKind::Ground)
    }
}

pub trait SpatialMapObject {
    fn kind(&self) -> ProjectKind;
    fn shape(&self) -> ShapeEnum;

    fn additional_occupancy(&self) -> Option<AABB> {
        None
    }

    fn cut_influence(&self) -> Option<AABB> {
        None
    }
}

pub struct SpatialMap {
    broad: BroadGrid,
    occupancy: AABBGrid<ProjectKind, AABB>,
    occupancy_ids: BTreeMap<ProjectKind, AABBGridHandle>,
    cuts: AABBGrid<ProjectKind, AABB>,
    cut_ids: BTreeMap<ProjectKind, AABBGridHandle>,
    near: BTreeMap<ProjectKind, ShapeEnum>,
    ids: BTreeMap<ProjectKind, AABBGridHandle>,
}

struct BroadGrid {
    storage: SparseStorage<BroadCell>,
    objects: AABBGridObjects<ProjectKind, AABB>,
    positions: FastMap<(AABBGridHandle, CellIdx), usize>,
    #[cfg(test)]
    removal_scan_entries: usize,
}

#[derive(Default)]
struct BroadCell {
    entries: Vec<(AABBGridHandle, bool)>,
    // The position is the mixed-grid query order, including swap-removals.
    lots: BTreeMap<usize, AABBGridHandle>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellRange {
    first: CellIdx,
    last: CellIdx,
}

impl CellRange {
    fn checked(bounds: AABB) -> Option<Self> {
        // Keep the same boundary convention as SparseStorage::cell_id.
        const COORD_LIMIT: f32 = 4_194_304.0;
        let [left, bottom, right, top] = [bounds.ll.x, bounds.ll.y, bounds.ur.x, bounds.ur.y];
        let valid = |value: f32| value.abs() <= COORD_LIMIT;
        if ![left, bottom, right, top].into_iter().all(valid) || left > right || bottom > top {
            return None;
        }
        let cell = |value: f32| value as i32 / CELL_SIZE - i32::from(value < 0.0);
        Some(Self {
            first: (cell(left), cell(bottom)),
            last: (cell(right), cell(top)),
        })
    }

    fn len(self) -> usize {
        let (left, bottom) = self.first;
        let (right, top) = self.last;
        (right.abs_diff(left) as usize + 1).saturating_mul(top.abs_diff(bottom) as usize + 1)
    }

    fn iter(self) -> impl Iterator<Item = CellIdx> {
        let (left, bottom) = self.first;
        let (right, top) = self.last;
        (bottom..=top).flat_map(move |y| (left..=right).map(move |x| (x, y)))
    }
}

impl BroadGrid {
    fn cell_range(&self, bounds: AABB) -> CellRange {
        CellRange {
            first: self.storage.cell_id(bounds.ll),
            last: self.storage.cell_id(bounds.ur),
        }
    }

    fn insert(&mut self, aabb: AABB, obj: ProjectKind) -> AABBGridHandle {
        let cells = self.object_cells(obj, aabb);
        let handle = self.objects.insert(StoreObject { obj, aabb });
        self.insert_cells(handle, cells);
        handle
    }

    fn object_cells(&self, object: ProjectKind, bounds: AABB) -> CellRange {
        if object.to_lot().is_some() {
            CellRange::checked(bounds)
                .filter(|range| range.len() <= MAX_LOT_CELLS)
                .expect("validated lot footprint")
        } else {
            self.cell_range(bounds)
        }
    }

    fn insert_cells(&mut self, handle: AABBGridHandle, cells: CellRange) {
        let single_cell = cells.first == cells.last;
        let is_lot = self.objects[handle].obj.to_lot().is_some();
        for cell_id in cells.iter() {
            let cell = self.storage.cell_mut_unchecked(cell_id);
            let index = cell.entries.len();
            if is_lot {
                cell.lots.insert(index, handle);
            }
            self.positions.insert((handle, cell_id), index);
            cell.entries.push((handle, single_cell));
        }
    }

    fn remove_cell_entry(&mut self, cell_id: CellIdx, handle: AABBGridHandle) {
        let cell = self.storage.cell_mut_unchecked(cell_id);
        let index = self
            .positions
            .remove(&(handle, cell_id))
            .expect("indexed spatial membership");
        #[cfg(test)]
        {
            self.removal_scan_entries += 1;
        }
        cell.lots.remove(&index);
        let last = cell.entries.len() - 1;
        cell.entries.swap_remove(index);
        if index != last {
            let moved = cell.entries[index].0;
            if cell.lots.remove(&last).is_some() {
                cell.lots.insert(index, moved);
            }
            *self
                .positions
                .get_mut(&(moved, cell_id))
                .expect("indexed spatial membership") = index;
        }
    }

    fn remove_cells(&mut self, handle: AABBGridHandle, cells: CellRange) {
        for cell in cells.iter() {
            self.remove_cell_entry(cell, handle);
        }
    }

    fn remove(&mut self, handle: AABBGridHandle) {
        if let Some(object) = self.objects.remove(handle) {
            self.remove_cells(handle, self.object_cells(object.obj, object.aabb));
        }
    }

    fn set_aabb(&mut self, handle: AABBGridHandle, aabb: AABB) {
        let object = &self.objects[handle];
        let old_cells = self.object_cells(object.obj, object.aabb);
        let new_cells = self.object_cells(object.obj, aabb);
        self.objects[handle].aabb = aabb;
        if old_cells != new_cells {
            self.remove_cells(handle, old_cells);
            self.insert_cells(handle, new_cells);
        }
    }

    fn query(
        &self,
        bounds: AABB,
        lots_only: bool,
    ) -> impl Iterator<Item = (AABBGridHandle, &AABB, &ProjectKind)> {
        let cells = if lots_only {
            CellRange::checked(bounds)
        } else {
            Some(self.cell_range(bounds))
        };
        let mut seen = FastSet::default();
        cells
            .into_iter()
            .flat_map(CellRange::iter)
            .filter_map(|cell| self.storage.cell(cell))
            .flat_map(move |cell| {
                if lots_only {
                    Either::Left(cell.lots.keys().map(|&index| cell.entries[index]))
                } else {
                    Either::Right(cell.entries.iter().copied())
                }
            })
            .filter_map(move |(handle, single_cell)| {
                if cells.is_some_and(|range| range.first != range.last)
                    && !single_cell
                    && !seen.insert(handle)
                {
                    return None;
                }
                let object = &self.objects[handle];
                bounds
                    .intersects(&object.aabb)
                    .then_some((handle, &object.aabb, &object.obj))
            })
    }
}

impl Default for SpatialMap {
    fn default() -> Self {
        Self {
            broad: BroadGrid {
                storage: SparseStorage::new(CELL_SIZE),
                objects: Default::default(),
                positions: Default::default(),
                #[cfg(test)]
                removal_scan_entries: 0,
            },
            occupancy: AABBGrid::new(512),
            occupancy_ids: Default::default(),
            cuts: AABBGrid::new(512),
            cut_ids: Default::default(),
            near: Default::default(),
            ids: Default::default(),
        }
    }
}

impl SpatialMap {
    #[cfg(test)]
    pub(super) fn removal_scan_entries(&self) -> usize {
        self.broad.removal_scan_entries
    }

    pub fn insert(&mut self, obj: &impl SpatialMapObject) {
        let kind = obj.kind();
        let shape = obj.shape();
        let handle = self.broad.insert(shape.bbox(), kind);
        if let Some(old_handle) = self.ids.insert(kind, handle) {
            self.broad.remove(old_handle);
        }
        self.update_auxiliary(obj);
        self.near.insert(kind, shape);
    }

    pub fn remove(&mut self, kind: impl Into<ProjectKind>) {
        let kind = kind.into();
        if let Some(id) = self.ids.remove(&kind) {
            self.broad.remove(id);
            if let Some(handle) = self.occupancy_ids.remove(&kind) {
                self.occupancy.remove(handle);
            }
            if let Some(handle) = self.cut_ids.remove(&kind) {
                self.cuts.remove(handle);
            }
            self.near.remove(&kind);
        } else {
            warn!(
                "trying to remove {:?} from spatial map but it wasn't present",
                kind
            )
        }
    }

    pub(super) fn remove_lots(&mut self, lots: impl IntoIterator<Item = LotID>) {
        // Lots have no entries in the occupancy or cut influence grids.
        for id in lots {
            let kind = ProjectKind::Lot(id);
            if let Some(handle) = self.ids.remove(&kind) {
                self.broad.remove(handle);
                self.near.remove(&kind);
            }
        }
    }

    pub fn update(&mut self, obj: &impl SpatialMapObject) {
        let kind = obj.kind();
        let shape = obj.shape();
        if let Some(id) = self.ids.get(&kind) {
            self.broad.set_aabb(*id, shape.bbox());
            self.update_auxiliary(obj);
            self.near.insert(kind, shape);
        } else {
            warn!(
                "trying to update shape {:?} from spatial map but it wasn't present",
                kind
            )
        }
    }

    pub(super) fn update_auxiliary(&mut self, obj: &impl SpatialMapObject) {
        let kind = obj.kind();
        self.set_additional_occupancy(kind, obj.additional_occupancy());
        match (self.cut_ids.get(&kind).copied(), obj.cut_influence()) {
            (Some(handle), Some(bounds)) => self.cuts.set_aabb(handle, bounds),
            (None, Some(bounds)) => {
                self.cut_ids.insert(kind, self.cuts.insert(bounds, kind));
            }
            (Some(handle), None) => {
                self.cut_ids.remove(&kind);
                self.cuts.remove(handle);
            }
            (None, None) => {}
        }
    }

    pub(super) fn set_additional_occupancy(&mut self, kind: ProjectKind, bounds: Option<AABB>) {
        match (self.occupancy_ids.get(&kind).copied(), bounds) {
            (Some(handle), Some(bounds)) => self.occupancy.set_aabb(handle, bounds),
            (None, Some(bounds)) => {
                self.occupancy_ids
                    .insert(kind, self.occupancy.insert(bounds, kind));
            }
            (Some(handle), None) => {
                self.occupancy_ids.remove(&kind);
                self.occupancy.remove(handle);
            }
            (None, None) => {}
        }
    }

    pub(super) fn update_cut_influence(&mut self, obj: &impl SpatialMapObject) {
        if let Some(&handle) = self.cut_ids.get(&obj.kind()) {
            self.cuts
                .set_aabb(handle, obj.cut_influence().expect("indexed cut influence"));
        }
    }

    pub(super) fn cut_candidates(&self, bounds: AABB) -> impl Iterator<Item = ProjectKind> + '_ {
        self.cuts.query(bounds).map(|(_, _, &object)| object)
    }

    pub fn query_around(
        &self,
        center: Vec2,
        radius: f32,
        filter: ProjectFilter,
    ) -> impl Iterator<Item = ProjectKind> + '_ {
        self.query(Circle { center, radius }, filter)
    }

    /// Find buildings whose zone or physical footprint bounds intersect the query.
    pub fn building_candidates(&self, bounds: AABB) -> impl Iterator<Item = BuildingID> + '_ {
        self.construction_candidates(bounds, ProjectFilter::BUILDING)
            .filter_map(|project| project.as_building())
    }

    pub fn query<'a>(
        &'a self,
        shape: impl Intersect<ShapeEnum> + Intersect<AABB> + Clone + 'a,
        filter: ProjectFilter,
    ) -> impl Iterator<Item = ProjectKind> + 'a {
        self.broad
            .query(shape.bbox(), filter.0 == ProjectFilter::LOT.0)
            .filter(move |&(_, _, p)| filter.test(p))
            .filter_map(move |(_, _, p)| shape.intersects(self.near.get(p)?).then_some(*p))
    }

    pub(crate) fn reserve_apron_query(&self, bounds: AABB, work: &mut usize) -> Result<(), WaterEditError> {
        let cells = CellRange::checked(bounds).ok_or(WaterEditError::Limit)?;
        spend_water_work(work, cells.len())?;
        for cell in cells.iter().filter_map(|id| self.broad.storage.cell(id)) {
            // Charge raw memberships before deduplication or exact testing.
            spend_water_work(work, cell.entries.len().saturating_mul(2))?;
        }
        Ok(())
    }

    pub(super) fn reserve_lot_query(
        &self,
        bounds: AABB,
        per_lot: usize,
        changed_objects: usize,
        new_lots: usize,
        work: &mut usize,
    ) -> Result<(), WaterEditError> {
        let cells = CellRange::checked(bounds).ok_or(WaterEditError::Limit)?;
        let storage = &self.broad.storage;
        // Reserve both lot queries, including empty cells and repeated lot
        // memberships. Generated lots can span at most 3x3 cells.
        spend_water_work(
            work,
            cells
                .len()
                .saturating_mul(changed_objects.saturating_add(3))
                .saturating_add(new_lots.saturating_mul(per_lot.saturating_add(3 * GENERATED_LOT_CELLS))),
        )?;
        for cell in cells.iter() {
            if let Some(cell) = storage.cell(cell) {
                spend_water_work(work, cell.lots.len().saturating_mul(2))?;
            }
        }
        let mut removal_cells = FastSet::default();
        // Lots can straddle cells outside the query. Charge every membership.
        for (_, lot_bounds, object) in self.broad.query(bounds, true) {
            if object.to_lot().is_some() {
                spend_water_work(work, per_lot)?;
                self.reserve_lot_removal_cells(
                    CellRange::checked(*lot_bounds).expect("indexed lot footprint"),
                    changed_objects,
                    &mut removal_cells,
                    work,
                )?;
            }
        }
        if new_lots > 0 {
            // A merge can clear lots generated during the live connection.
            // Allow a rounded-up lot diagonal beyond the query for those lots.
            let possible = bounds.expand(super::MAX_LOT_SIZE * 1.5);
            self.reserve_lot_removal_cells(
                CellRange::checked(possible).ok_or(WaterEditError::Limit)?,
                changed_objects,
                &mut removal_cells,
                work,
            )?;
        }
        Ok(())
    }

    pub(super) fn reserve_lot_removals(
        &self,
        ids: impl IntoIterator<Item = LotID>,
        generated_regions: impl IntoIterator<Item = AABB>,
        new_lots: usize,
        changed_objects: usize,
        work: &mut usize,
    ) -> Result<(), WaterEditError> {
        let mut removal_cells = FastSet::default();
        // Include the memberships that would be created before a staged merge.
        spend_water_work(work, new_lots.saturating_mul(1 + 2 * GENERATED_LOT_CELLS))?;
        for id in ids {
            spend_water_work(work, 1)?;
            if let Some(&handle) = self.ids.get(&ProjectKind::Lot(id)) {
                self.reserve_lot_removal_cells(
                    CellRange::checked(self.broad.objects[handle].aabb)
                        .expect("indexed lot footprint"),
                    changed_objects,
                    &mut removal_cells,
                    work,
                )?;
            }
        }
        for bounds in generated_regions {
            self.reserve_lot_removal_cells(
                CellRange::checked(bounds).ok_or(WaterEditError::Limit)?,
                changed_objects,
                &mut removal_cells,
                work,
            )?;
        }
        Ok(())
    }

    fn reserve_lot_removal_cells(
        &self,
        range: CellRange,
        changed_objects: usize,
        seen: &mut FastSet<CellIdx>,
        work: &mut usize,
    ) -> Result<(), WaterEditError> {
        spend_water_work(work, range.len())?;
        for cell in range.iter() {
            if seen.insert(cell) {
                spend_water_work(work, changed_objects.saturating_add(1))?;
            }
        }
        Ok(())
    }

    pub(crate) fn reserve_construction_query(
        &self,
        bounds: AABB,
        work: &mut usize,
    ) -> Result<(), WaterEditError> {
        let cells = CellRange::checked(bounds).ok_or(WaterEditError::Limit)?;
        let occupancy = self.occupancy.storage();
        let occupied_cells = CellRange {
            first: occupancy.cell_id(bounds.ll),
            last: occupancy.cell_id(bounds.ur),
        };
        spend_water_work(
            work,
            cells
                .len()
                .saturating_add(occupied_cells.len())
                .saturating_mul(3),
        )?;
        for key in cells.iter() {
            if let Some(cell) = self.broad.storage.cell(key) {
                spend_water_work(work, cell.entries.len().saturating_mul(2))?;
            }
        }
        for key in occupied_cells.iter() {
            if let Some(cell) = occupancy.cell(key) {
                spend_water_work(work, cell.objs.len().saturating_mul(2))?;
            }
        }
        Ok(())
    }

    /// A zoned building has separate bounds for its polygon and physical footprint.
    pub(crate) fn construction_candidates(
        &self,
        bounds: AABB,
        filter: ProjectFilter,
    ) -> impl Iterator<Item = ProjectKind> + '_ {
        let additional = self.occupancy.query(bounds).filter(move |&(_, _, object)| {
            let primary = self.broad.objects[self.ids[object]].aabb;
            !primary.intersects(&bounds)
        });
        self.broad
            .query(bounds, filter.0 == ProjectFilter::LOT.0)
            .chain(additional)
            .filter(move |&(_, _, object)| filter.test(object))
            .map(|(_, _, object)| *object)
    }

    pub fn debug_grid(&self) -> impl Iterator<Item = AABB> + '_ {
        self.broad
            .objects
            .values()
            .chain(
                self.occupancy
                    .handles()
                    .filter_map(move |x| self.occupancy.get(x)),
            )
            .map(|obj| obj.aabb)
    }

    pub fn contains<T: Into<ProjectKind>>(&self, p: T) -> bool {
        let kind = p.into();

        let v = unwrap_ret!(self.ids.get(&kind), false);
        self.broad.objects.contains_key(*v) && self.near.get(&kind).is_some()
    }

    pub fn objects(&self) -> impl Iterator<Item = &ProjectKind> + '_ {
        self.ids.keys()
    }
}

impl SpatialMapObject for Intersection {
    fn kind(&self) -> ProjectKind {
        ProjectKind::Intersection(self.id)
    }

    fn shape(&self) -> ShapeEnum {
        self.bcircle().into()
    }

    fn cut_influence(&self) -> Option<AABB> {
        let mut circle = self.bcircle();
        circle.radius *= 2.0;
        Some(circle.bbox())
    }
}

impl SpatialMapObject for Road {
    fn kind(&self) -> ProjectKind {
        ProjectKind::Road(self.id)
    }

    fn shape(&self) -> ShapeEnum {
        self.boldline().into()
    }

    fn additional_occupancy(&self) -> Option<AABB> {
        let bounds = super::road_support_bounds(self);
        let primary = self.points().bbox().flatten().expand(self.width * 0.5);
        (!primary.contains(bounds.ll) || !primary.contains(bounds.ur)).then_some(bounds)
    }

    fn cut_influence(&self) -> Option<AABB> {
        Some(
            self.points()
                .bbox()
                .flatten()
                .expand(13.0 + self.width * 3.0),
        )
    }
}

impl SpatialMapObject for Lot {
    fn kind(&self) -> ProjectKind {
        ProjectKind::Lot(self.id)
    }

    fn shape(&self) -> ShapeEnum {
        self.shape.into()
    }
}

impl SpatialMapObject for Building {
    fn kind(&self) -> ProjectKind {
        ProjectKind::Building(self.id)
    }

    fn shape(&self) -> ShapeEnum {
        if let Some(ref z) = self.zone {
            return z.poly.clone().into();
        }
        self.obb.into()
    }

    fn additional_occupancy(&self) -> Option<AABB> {
        self.zone.as_ref().map(|_| self.obb.bbox())
    }

    fn cut_influence(&self) -> Option<AABB> {
        Some(self.obb.expand(25.0).bbox())
    }
}

#[derive(Copy, Clone)]
pub struct ProjectFilter(u8);

impl ProjectFilter {
    pub const INTER: Self = Self(1);
    pub const ROAD: Self = Self(2);
    pub const BUILDING: Self = Self(4);
    pub const LOT: Self = Self(8);
    pub const ALL: Self = Self(!0);

    pub fn test(self, p: &ProjectKind) -> bool {
        match p {
            ProjectKind::Intersection(_) => (self.0 & Self::INTER.0) != 0,
            ProjectKind::Road(_) => (self.0 & Self::ROAD.0) != 0,
            ProjectKind::Building(_) => (self.0 & Self::BUILDING.0) != 0,
            ProjectKind::Lot(_) => (self.0 & Self::LOT.0) != 0,
            ProjectKind::Ground => true,
        }
    }
}

impl BitOr for ProjectFilter {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl Sub for ProjectFilter {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 & !rhs.0)
    }
}

impl Neg for ProjectFilter {
    type Output = Self;

    #[inline]
    fn neg(self) -> Self::Output {
        Self(!self.0)
    }
}

#[test]
fn p7_spatial_projection_order_and_allocator() {
    use crate::map::{LanePatternBuilder, MapProject};
    use geom::{vec3, OBB};

    let mut map = Map::empty();
    let (_, road) = map
        .make_connection(
            MapProject::ground(vec3(60.0, 150.0, 0.0)),
            MapProject::ground(vec3(240.0, 150.0, 0.0)),
            None,
            &LanePatternBuilder::new().build(),
        )
        .unwrap();
    let pos = map.roads()[road].points().middle();
    assert_eq!(
        map.project(pos, 1.0, ProjectFilter::ALL).kind,
        ProjectKind::Road(road)
    );
    let shape = OBB::new(pos.xy(), Vec2::X, 4.0, 4.0);
    let [first, middle, last] = [0; 3].map(|_| Lot::insert(&mut map, road, shape, pos.z));
    assert_eq!(
        map.project(pos, 1.0, ProjectFilter::ALL).kind,
        ProjectKind::Lot(first)
    );
    map.spatial_map.remove(first);
    map.remove_lot_record(first).unwrap();

    // The stock mixed grid swapped the last entry into the removed position.
    let hits = map
        .spatial_map
        .query(pos.xy(), ProjectFilter::LOT)
        .collect::<Vec<_>>();
    assert_eq!(hits, [ProjectKind::Lot(last), ProjectKind::Lot(middle)]);
    assert_eq!(
        map.project(pos, 1.0, ProjectFilter::ALL).kind,
        ProjectKind::Lot(last)
    );
    let reused = Lot::insert(&mut map, road, shape, pos.z);
    assert_ne!(reused, first);
    assert!(map.get(first).is_none());
    assert_eq!(
        map.project(pos, 1.0, ProjectFilter::ALL).kind,
        ProjectKind::Lot(last)
    );
}
