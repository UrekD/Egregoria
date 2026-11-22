use crate::map::{Map, RoadID};
use crate::utils::resources::Resources;
use crate::World;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoadUserKind {
    Car,
    Truck,
    Bus,
    Pedestrian,
}

#[derive(Default)]
pub struct RoadThroughput;

impl RoadThroughput {
    pub fn record_entry(&mut self, _road: RoadID, _kind: RoadUserKind, _tick: u64) {}

    /// Recent entries in car, truck, bus and pedestrian order.
    pub fn recent(&self, _road: RoadID, _now: u64) -> [u32; 4] {
        [0; 4]
    }

    pub fn prune(&mut self, _map: &Map, _now: u64) {}
}

pub fn road_throughput_update(_world: &mut World, _resources: &mut Resources) {}
