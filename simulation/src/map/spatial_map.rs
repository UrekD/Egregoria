use crate::map::{
    spend_water_work, Building, BuildingID, CanonicalPosition, Intersection, IntersectionID, Lot,
    LotID, Map, Road, RoadID, WaterEditError,
};
use common::{FastMap, FastSet};
use derive_more::From;
use flat_spatial::aabbgrid::{AABBGridHandle, AABBGridObjects, StoreObject};
use flat_spatial::cell::AABBGridCell;
use flat_spatial::storage::{CellIdx, SparseStorage};
use flat_spatial::AABBGrid;
use geom::{Circle, Intersect, Shape, ShapeEnum, Vec2, AABB};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::{BitOr, Neg, Sub};

// A generated lot is at most 40 m square and can span at most 3x3 cells.
const GENERATED_LOT_CELLS: usize = 9;

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

// The broad grid uses the same storage and handles as AABBGrid, whose public
// API only supports removing entries one at a time.
struct BroadGrid {
    storage: SparseStorage<AABBGridCell>,
    objects: AABBGridObjects<ProjectKind, AABB>,
    #[cfg(test)]
    removal_scan_entries: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellRange {
    first: CellIdx,
    last: CellIdx,
}

impl CellRange {
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
        let handle = self.objects.insert(StoreObject { obj, aabb });
        self.insert_cells(handle, self.cell_range(aabb));
        handle
    }

    fn insert_cells(&mut self, handle: AABBGridHandle, cells: CellRange) {
        let single_cell = cells.first == cells.last;
        for cell in cells.iter() {
            self.storage
                .cell_mut_unchecked(cell)
                .objs
                .push((handle, single_cell));
        }
    }

    fn remove_cell_entry(&mut self, cell: CellIdx, handle: AABBGridHandle) {
        let cell = self.storage.cell_mut_unchecked(cell);
        let index = cell.objs.iter().position(|&(entry, _)| entry == handle);
        #[cfg(test)]
        {
            self.removal_scan_entries += index.map_or(cell.objs.len(), |index| index + 1);
        }
        if let Some(index) = index {
            cell.objs.swap_remove(index);
        }
    }

    fn remove_cells(&mut self, handle: AABBGridHandle, cells: CellRange) {
        for cell in cells.iter() {
            self.remove_cell_entry(cell, handle);
        }
    }

    fn remove(&mut self, handle: AABBGridHandle) {
        if let Some(object) = self.objects.remove(handle) {
            self.remove_cells(handle, self.cell_range(object.aabb));
        }
    }

    fn remove_many(&mut self, handles: impl Iterator<Item = AABBGridHandle>) {
        let mut by_cell: FastMap<CellIdx, Vec<AABBGridHandle>> = FastMap::default();
        for handle in handles {
            let object = self.objects.remove(handle).expect("indexed lot");
            for cell in self.cell_range(object.aabb).iter() {
                by_cell.entry(cell).or_default().push(handle);
            }
        }
        for (cell, handles) in by_cell {
            if let [handle] = handles.as_slice() {
                self.remove_cell_entry(cell, *handle);
                continue;
            }
            let cell = self.storage.cell_mut_unchecked(cell);

            // Find the removed entries in one scan, then replay swap-removals
            // in input order so surviving objects keep their query order.
            let mut positions: FastMap<_, _> =
                handles.iter().map(|&handle| (handle, usize::MAX)).collect();
            #[cfg(test)]
            {
                self.removal_scan_entries += cell.objs.len();
            }
            for (index, &(handle, _)) in cell.objs.iter().enumerate() {
                if let Some(position) = positions.get_mut(&handle) {
                    *position = index;
                }
            }
            for handle in handles {
                let index = positions[&handle];
                let moved = cell.objs.last().unwrap().0;
                cell.objs.swap_remove(index);
                if let Some(position) = positions.get_mut(&moved) {
                    *position = index;
                }
            }
        }
    }

    fn set_aabb(&mut self, handle: AABBGridHandle, aabb: AABB) {
        let old_cells = self.cell_range(self.objects[handle].aabb);
        let new_cells = self.cell_range(aabb);
        self.objects[handle].aabb = aabb;
        if old_cells != new_cells {
            self.remove_cells(handle, old_cells);
            self.insert_cells(handle, new_cells);
        }
    }

    fn query(&self, bounds: AABB) -> impl Iterator<Item = (AABBGridHandle, &AABB, &ProjectKind)> {
        let cells = self.cell_range(bounds);
        let mut seen = FastSet::default();
        cells
            .iter()
            .filter_map(|cell| self.storage.cell(cell))
            .flat_map(|cell| cell.objs.iter().copied())
            .filter_map(move |(handle, single_cell)| {
                if cells.first != cells.last && !single_cell && !seen.insert(handle) {
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
                storage: SparseStorage::new(50),
                objects: Default::default(),
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
        let ids = &mut self.ids;
        let near = &mut self.near;
        // Lots have no entries in the occupancy or cut influence grids.
        self.broad.remove_many(lots.into_iter().filter_map(|id| {
            let kind = ProjectKind::Lot(id);
            let handle = ids.remove(&kind)?;
            near.remove(&kind);
            Some(handle)
        }));
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

    pub fn query<'a>(
        &'a self,
        shape: impl Intersect<ShapeEnum> + Intersect<AABB> + Clone + 'a,
        filter: ProjectFilter,
    ) -> impl Iterator<Item = ProjectKind> + 'a {
        self.broad
            .query(shape.bbox())
            .filter(move |&(_, _, p)| filter.test(p))
            .filter_map(move |(_, _, p)| shape.intersects(self.near.get(p)?).then_some(*p))
    }

    pub(super) fn reserve_lot_query(
        &self,
        bounds: AABB,
        per_lot: usize,
        changed_objects: usize,
        new_lots: usize,
        work: &mut usize,
    ) -> Result<(), WaterEditError> {
        let cells = self.broad.cell_range(bounds);
        let storage = &self.broad.storage;
        // Pay for the cell count and both queries, including empty cells and
        // repeated grid entries.
        // An added or updated road/junction can contribute one entry per cell.
        // Generated lots are at most 40 m square and span at most 3x3 cells.
        // Count each new membership for the live query, removal bookkeeping,
        // and the removal scan.
        spend_water_work(
            work,
            cells
                .len()
                .saturating_mul(changed_objects.saturating_add(3))
                .saturating_add(new_lots.saturating_mul(per_lot.saturating_add(3 * GENERATED_LOT_CELLS))),
        )?;
        for cell in cells.iter() {
            if let Some(cell) = storage.cell(cell) {
                spend_water_work(work, cell.objs.len().saturating_mul(2))?;
            }
        }
        let mut removal_cells = FastSet::default();
        // Lots can straddle cells outside the query. Budget the batch's
        // bookkeeping per membership and one scan per affected cell.
        for (_, lot_bounds, object) in self.broad.query(bounds) {
            if object.to_lot().is_some() {
                spend_water_work(work, per_lot)?;
                self.reserve_lot_removal_cells(
                    self.broad.cell_range(*lot_bounds),
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
                self.broad.cell_range(possible),
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
        // Newly inserted handles add memberships to both the removal lists
        // and the affected cell scans.
        spend_water_work(work, new_lots.saturating_mul(1 + 2 * GENERATED_LOT_CELLS))?;
        for id in ids {
            spend_water_work(work, 1)?;
            if let Some(&handle) = self.ids.get(&ProjectKind::Lot(id)) {
                self.reserve_lot_removal_cells(
                    self.broad.cell_range(self.broad.objects[handle].aabb),
                    changed_objects,
                    &mut removal_cells,
                    work,
                )?;
            }
        }
        for bounds in generated_regions {
            self.reserve_lot_removal_cells(
                self.broad.cell_range(bounds),
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
                let entries = self.broad.storage.cell(cell).map_or(0, |cell| cell.objs.len());
                spend_water_work(work, entries.saturating_add(changed_objects))?;
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
            .query(bounds)
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
