use crate::map::{Map, RoadID};

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
    pub(crate) fn sync_map(&mut self, _map: &Map) {}

    pub fn record_entry(&mut self, _road: RoadID, _kind: RoadUserKind, _tick: u64) {}

    /// Recent entries in car, truck, bus and pedestrian order.
    pub fn recent(&self, _road: RoadID, _now: u64) -> [u32; 4] {
        [0; 4]
    }
}
