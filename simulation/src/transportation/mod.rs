use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};
use std::ops::Deref;

use flat_spatial::grid::GridHandle;
use serde::{Deserialize, Serialize};

use egui_inspect::InspectVec2Rotation;
use geom::{Transform, Vec2};
pub use pedestrian::*;
pub use vehicle::*;

use crate::map::{BuildingID, Map, Traversable, TraverseDirection, TraverseKind};
use crate::map_dynamic::Itinerary;
use crate::utils::resources::Resources;
use crate::world::{HumanID, TrainID, VehicleID};
use crate::{Simulation, World};
use common::FastMap;

pub mod pedestrian;
pub(crate) mod junction_admission;
pub mod queue_diagnostics;
pub mod road;
pub mod road_throughput;
pub use road_throughput::{RoadThroughput, RoadUserKind};
pub mod testing_vehicles;
pub mod traffic_metrics;
pub mod train;
mod vehicle;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    Outside,
    Vehicle(VehicleID),
    Building(BuildingID),
    Transit(VehicleID),
}
debug_inspect_impl!(Location);

#[derive(Clone, Default, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Speed(pub f32);
debug_inspect_impl!(Speed);

impl Debug for Speed {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1}m/s", self.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Inspect)]
pub enum TransportationGroup {
    Unknown,
    Vehicles,
    Pedestrians,
}

#[derive(Copy, Clone, Serialize, Deserialize, Inspect)]
pub struct TransportState {
    #[inspect(proxy_type = "InspectVec2Rotation")]
    pub dir: Vec2,
    pub speed: f32,
    pub radius: f32,
    pub height: f32,
    pub group: TransportationGroup,
    pub flag: u64,
}

impl Default for TransportState {
    fn default() -> Self {
        Self {
            dir: Vec2::X,
            speed: 0.0,
            radius: 1.0,
            height: 0.0,
            group: TransportationGroup::Unknown,
            flag: 0,
        }
    }
}

type SpatialGrid = flat_spatial::Grid<TransportState, Vec2>;

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransportGrid {
    grid: SpatialGrid,
    #[serde(skip)]
    humans: ActiveColliders<HumanID>,
    #[serde(skip)]
    vehicles: ActiveColliders<VehicleID>,
    #[serde(skip)]
    traversal_users: Option<TraversalUsers>,
    #[serde(skip)]
    explicit_traversal_watch: bool,
    #[serde(skip, default = "positions_may_be_pending")]
    pending_positions: bool,
}

fn positions_may_be_pending() -> bool {
    // A snapshot can contain unflushed calls to Grid::set_position.
    true
}

struct ActiveColliders<I> {
    refresh: bool,
    entries: Vec<(I, Transporter)>,
    by_handle: FastMap<GridHandle, I>,
}

impl<I> Default for ActiveColliders<I> {
    fn default() -> Self {
        Self {
            refresh: true,
            entries: Vec::new(),
            by_handle: FastMap::default(),
        }
    }
}

impl<I: Copy> ActiveColliders<I> {
    fn refresh_from(&mut self, entries: impl Iterator<Item = (I, Transporter)>) {
        if std::mem::take(&mut self.refresh) {
            self.entries.clear();
            self.entries.extend(entries);
            self.by_handle.clear();
            self.by_handle.extend(
                self.entries
                    .iter()
                    .map(|&(owner, collider)| (collider.0, owner)),
            );
        }
    }
}

impl Deref for TransportGrid {
    type Target = SpatialGrid;

    fn deref(&self) -> &Self::Target {
        &self.grid
    }
}

impl Clone for TransportGrid {
    fn clone(&self) -> Self {
        Self {
            grid: self.grid.clone(),
            humans: ActiveColliders::default(),
            vehicles: ActiveColliders::default(),
            traversal_users: self
                .traversal_users
                .as_ref()
                .map(|_| TraversalUsers::default()),
            explicit_traversal_watch: self.explicit_traversal_watch,
            pending_positions: self.pending_positions,
        }
    }
}

impl TransportGrid {
    pub fn new(cell_size: i32) -> Self {
        Self {
            grid: SpatialGrid::new(cell_size),
            humans: ActiveColliders::default(),
            vehicles: ActiveColliders::default(),
            traversal_users: None,
            explicit_traversal_watch: false,
            pending_positions: false,
        }
    }

    pub fn insert(&mut self, pos: Vec2, state: TransportState) -> GridHandle {
        match state.group {
            TransportationGroup::Pedestrians => self.humans.refresh = true,
            TransportationGroup::Vehicles => self.vehicles.refresh = true,
            TransportationGroup::Unknown => {
                self.humans.refresh = true;
                self.vehicles.refresh = true;
            }
        }
        self.grid.insert(pos, state)
    }

    pub fn set_position(&mut self, handle: GridHandle, pos: Vec2) {
        self.pending_positions = true;
        self.grid.set_position(handle, pos);
    }

    pub fn get_mut(&mut self, handle: GridHandle) -> Option<(Vec2, &mut TransportState)> {
        self.grid.get_mut(handle)
    }

    pub fn remove(&mut self, handle: GridHandle) -> Option<TransportState> {
        self.forget_collider(handle);
        self.grid.remove(handle)
    }

    pub fn remove_maintain(&mut self, handle: GridHandle) -> Option<TransportState> {
        self.forget_collider(handle);
        self.grid.remove_maintain(handle)
    }

    pub fn clear(&mut self) -> impl Iterator<Item = (Vec2, TransportState)> {
        self.humans.refresh = true;
        self.vehicles.refresh = true;
        self.humans.by_handle.clear();
        self.vehicles.by_handle.clear();
        self.traversal_users = self
            .traversal_users
            .as_ref()
            .map(|_| TraversalUsers::default());
        self.pending_positions = false;
        self.grid.clear()
    }

    pub fn maintain(&mut self) {
        self.pending_positions = false;
        self.grid.maintain();
    }

    pub fn maintain_deterministic(&mut self) {
        self.pending_positions = false;
        self.grid.maintain_deterministic();
    }

    fn forget_collider(&mut self, handle: GridHandle) {
        self.humans.by_handle.remove(&handle);
        self.vehicles.by_handle.remove(&handle);
    }

    /// Looks up a generation-bearing handle at the last collider synchronization.
    pub fn collider_owner(&self, world: &World, collider: GridHandle) -> ColliderOwnerLookup {
        let Some((_, state)) = self.grid.get(collider) else {
            return ColliderOwnerLookup::Missing;
        };
        let owner = match state.group {
            TransportationGroup::Vehicles => {
                if self.vehicles.refresh {
                    return ColliderOwnerLookup::Deferred;
                }
                self.vehicles
                    .by_handle
                    .get(&collider)
                    .copied()
                    .filter(|&id| {
                        world
                            .vehicles
                            .get(id)
                            .is_some_and(|v| v.collider == Some(Transporter(collider)))
                    })
                    .map(TraversalOwner::Vehicle)
            }
            TransportationGroup::Pedestrians => {
                if self.humans.refresh {
                    return ColliderOwnerLookup::Deferred;
                }
                self.humans
                    .by_handle
                    .get(&collider)
                    .copied()
                    .filter(|&id| {
                        world
                            .humans
                            .get(id)
                            .is_some_and(|h| h.collider == Some(Transporter(collider)))
                    })
                    .map(TraversalOwner::Human)
            }
            TransportationGroup::Unknown => return ColliderOwnerLookup::Missing,
        };
        owner.map_or(ColliderOwnerLookup::Deferred, ColliderOwnerLookup::Owned)
    }

    /// Enables current directed-use observations at the next itinerary update.
    /// This derived index is cold after cloning, loading or a topology change.
    pub fn watch_traversal_users(&mut self) {
        self.explicit_traversal_watch = true;
        self.traversal_users
            .get_or_insert_with(TraversalUsers::default);
    }

    pub(crate) fn watch_queue_traversals(&mut self, enabled: bool) {
        if enabled {
            self.traversal_users
                .get_or_insert_with(TraversalUsers::default);
        } else if !self.explicit_traversal_watch {
            self.traversal_users = None;
        }
    }

    pub fn traversal_users(
        &self,
        map: &Map,
        traversal: Traversable,
    ) -> Option<TraversalUserSnapshot<'_>> {
        let index = self.traversal_users.as_ref()?;
        let (identity, topology, elapsed_tick) = index.revision?;
        if (identity, topology) != (map.identity(), map.walking_topology_generation) {
            return None;
        }
        Some(TraversalUserSnapshot {
            users: index.by_traversal.get(&traversal_key(traversal)),
            elapsed_tick,
            truncated: false,
        })
    }

    pub(crate) fn begin_traversal_update(
        &mut self,
        map: &Map,
        elapsed_tick: u64,
    ) -> Option<TraversalUsers> {
        let mut index = self.traversal_users.take()?;
        let map_revision = (map.identity(), map.walking_topology_generation);
        if index.epoch == u64::MAX
            || index
                .revision
                .is_some_and(|(identity, topology, _)| (identity, topology) != map_revision)
        {
            index = TraversalUsers::default();
        }
        index.epoch += 1;
        index.revision = Some((map_revision.0, map_revision.1, elapsed_tick));
        Some(index)
    }

    pub(crate) fn publish_traversal_users(&mut self, mut snapshot: TraversalUsers) {
        snapshot.remove_departed();
        self.traversal_users = Some(snapshot);
    }

    fn synchronize_owners(&mut self, world: &World) {
        let Self {
            grid,
            humans,
            vehicles,
            pending_positions,
            ..
        } = self;
        vehicles.refresh_from(
            world
                .vehicles
                .iter()
                .filter_map(|(id, v)| Some((id, v.collider?))),
        );
        humans.refresh_from(
            world
                .humans
                .iter()
                .filter_map(|(id, h)| Some((id, h.collider?))),
        );

        // Cell maintenance sorts relocations by handle independently of the owner index.
        vehicles.entries.retain(|&(id, coll)| {
            let Some(v) = world.vehicles.get(id).filter(|v| v.collider == Some(coll)) else {
                vehicles.by_handle.remove(&coll.0);
                return false;
            };
            synchronize_collider(
                grid,
                coll,
                &v.trans,
                &v.speed,
                Some(v.vehicle.flag),
                *pending_positions,
            );
            true
        });
        humans.entries.retain(|&(id, coll)| {
            let Some(h) = world.humans.get(id).filter(|h| h.collider == Some(coll)) else {
                humans.by_handle.remove(&coll.0);
                return false;
            };
            synchronize_collider(grid, coll, &h.trans, &h.speed, None, *pending_positions);
            true
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TraversalOwner {
    Human(HumanID),
    Vehicle(VehicleID),
    Train(TrainID),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColliderOwnerLookup {
    Owned(TraversalOwner),
    Missing,
    Deferred,
}

pub struct TraversalUserSnapshot<'a> {
    users: Option<&'a BTreeSet<TraversalOwner>>,
    pub elapsed_tick: u64,
    pub truncated: bool,
}

impl TraversalUserSnapshot<'_> {
    pub fn len(&self) -> usize {
        self.users.map_or(0, BTreeSet::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn users(&self) -> impl Iterator<Item = TraversalOwner> + '_ {
        self.users.into_iter().flatten().copied()
    }
}

type TraversalKey = (TraverseKind, bool);
type TraversalIndex = BTreeMap<TraversalKey, BTreeSet<TraversalOwner>>;

#[derive(Default)]
pub(crate) struct TraversalUsers {
    revision: Option<(u64, u64, u64)>,
    epoch: u64,
    by_traversal: TraversalIndex,
    // Last movement pass that observed the owner, including moves that kept
    // the same directed traversal.
    by_owner: BTreeMap<TraversalOwner, (TraversalKey, u64)>,
}

impl TraversalUsers {
    pub(crate) fn record(&mut self, map: &Map, owner: TraversalOwner, itinerary: &Itinerary) {
        let Some(&traversal) = itinerary.get_travers() else {
            return;
        };
        if traversal.raw_points(map).is_none() {
            return;
        }
        let key = traversal_key(traversal);
        if let Some((previous, _)) = self.by_owner.insert(owner, (key, self.epoch)) {
            if previous == key {
                return;
            }
            Self::remove_owner(&mut self.by_traversal, previous, owner);
        }
        self.by_traversal.entry(key).or_default().insert(owner);
    }

    fn remove_departed(&mut self) {
        let index = &mut self.by_traversal;
        self.by_owner.retain(|&owner, &mut (key, seen)| {
            if seen != self.epoch {
                Self::remove_owner(index, key, owner);
                return false;
            }
            true
        });
    }

    fn remove_owner(index: &mut TraversalIndex, key: TraversalKey, owner: TraversalOwner) {
        if let Some(users) = index.get_mut(&key) {
            users.remove(&owner);
            if users.is_empty() {
                index.remove(&key);
            }
        }
    }
}

fn traversal_key(traversal: Traversable) -> TraversalKey {
    (traversal.kind, traversal.dir == TraverseDirection::Backward)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transporter(pub GridHandle);
debug_inspect_impl!(Transporter);

impl Transporter {
    pub fn destroy(self) -> impl FnOnce(&mut Simulation) {
        move |sim| {
            let cw = &mut sim.write::<TransportGrid>();
            cw.remove_maintain(self.0);
        }
    }
}

pub fn transport_grid_synchronize(world: &mut World, resources: &mut Resources) {
    profiling::scope!("physics::transport_grid_synchronize");
    let mut transport_grid = resources.write::<TransportGrid>();

    let freshly_indexed = transport_grid.humans.refresh && transport_grid.vehicles.refresh;
    transport_grid.synchronize_owners(world);
    // Replacing a world can reassign existing handles without an insert. After
    // pruning, every unaccounted handle may have an owner in the new world.
    if !freshly_indexed
        && transport_grid.humans.entries.len() + transport_grid.vehicles.entries.len()
            != transport_grid.len()
    {
        transport_grid.humans.refresh = true;
        transport_grid.vehicles.refresh = true;
        transport_grid.synchronize_owners(world);
    }

    transport_grid.maintain_deterministic();
}

fn synchronize_collider(
    grid: &mut SpatialGrid,
    collider: Transporter,
    trans: &Transform,
    speed: &Speed,
    flag: Option<u64>,
    pending_positions: bool,
) {
    let (pos, state) = grid
        .get_mut(collider.0)
        .expect("collider missing from transport grid");
    let same = |a: f32, b: f32| a.to_bits() == b.to_bits();
    if !same(state.dir.x, trans.dir.x) || !same(state.dir.y, trans.dir.y) {
        state.dir = trans.dir.xy();
    }
    if !same(state.speed, speed.0) {
        state.speed = speed.0;
    }
    if !same(state.height, trans.pos.z) {
        state.height = trans.pos.z;
    }
    if let Some(flag) = flag.filter(|&flag| flag != state.flag) {
        state.flag = flag;
    }
    if pending_positions || !same(pos.x, trans.pos.x) || !same(pos.y, trans.pos.y) {
        grid.set_position(collider.0, trans.pos.xy());
    }
}
