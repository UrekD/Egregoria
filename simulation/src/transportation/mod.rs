use std::fmt::{Debug, Formatter};
use std::ops::Deref;

use flat_spatial::grid::GridHandle;
use serde::{Deserialize, Serialize};

use egui_inspect::InspectVec2Rotation;
use geom::{Transform, Vec2};
pub use pedestrian::*;
pub use vehicle::*;

use crate::map::BuildingID;
use crate::utils::resources::Resources;
use crate::world::{HumanID, VehicleID};
use crate::{Simulation, World};

pub mod pedestrian;
pub mod road;
pub mod testing_vehicles;
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
}

impl<I> Default for ActiveColliders<I> {
    fn default() -> Self {
        Self {
            refresh: true,
            entries: Vec::new(),
        }
    }
}

impl<I> ActiveColliders<I> {
    fn refresh_from(&mut self, entries: impl Iterator<Item = (I, Transporter)>) {
        if std::mem::take(&mut self.refresh) {
            self.entries.clear();
            self.entries.extend(entries);
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
        self.grid.remove(handle)
    }

    pub fn remove_maintain(&mut self, handle: GridHandle) -> Option<TransportState> {
        self.grid.remove_maintain(handle)
    }

    pub fn clear(&mut self) -> impl Iterator<Item = (Vec2, TransportState)> {
        self.humans.refresh = true;
        self.vehicles.refresh = true;
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

    fn synchronize_owners(&mut self, world: &World) {
        let Self {
            grid,
            humans,
            vehicles,
            pending_positions,
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
                return false;
            };
            synchronize_collider(grid, coll, &h.trans, &h.speed, None, *pending_positions);
            true
        });
    }
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
