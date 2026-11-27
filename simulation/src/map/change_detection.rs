//! This module contains the change detection system for the map.
//! This should not be used inside the simulation as change subscribers are not serialized.
//! It is mostly for rendering purposes by decoupling it from the simulation.

use crate::map::{Building, Intersection, Lot, Road, TerrainChunkID, WaterDirtyChunks};
use common::{ChunkID, ChunkID_1024};
use geom::Vec2;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

// CanonicalPosition is a trait that describes the canonical position of an object.
// It is used to determine which chunk an object belongs to.
pub trait CanonicalPosition {
    fn canonical_position(&self) -> Vec2;
}

pub type SubscriberChunkID = ChunkID_1024;

bitflags::bitflags! {
    #[derive(Debug, Copy, Clone)]
    pub struct UpdateType: u8 {
        const Road = 1;
        const Building = 1 << 1;
        const Terrain = 1 << 2;
        const Water = 1 << 3;
        const RoadSupport = 1 << 4;
        const Land = 1 << 5;
    }
}

#[derive(Default)]
pub struct MapSubscribers(Mutex<Vec<MapSubscriber>>);

impl MapSubscribers {
    pub fn subscribe(&self, filter: UpdateType) -> MapSubscriber {
        self.add(MapSubscriber::new(filter))
    }

    pub fn subscribe_limited(&self, filter: UpdateType, limit: usize) -> MapSubscriber {
        let mut sub = MapSubscriber::new(filter);
        sub.limit = Some(limit);
        self.add(sub)
    }

    fn add(&self, sub: MapSubscriber) -> MapSubscriber {
        self.0.lock().unwrap().push(sub.clone());
        sub
    }

    pub fn dispatch_all(&self, chunks: impl Iterator<Item = SubscriberChunkID>) {
        let mut me = self.0.lock().unwrap();
        for chunk in chunks.collect::<BTreeSet<_>>() {
            for sub in me.iter_mut() {
                sub.dispatch(UpdateType::all(), chunk);
            }
        }
    }

    /// Derive subscriber buckets from existing terrain chunks, including odd outer rows/columns.
    pub fn dispatch_all_terrain(&self, chunks: impl Iterator<Item = TerrainChunkID>) {
        let buckets: BTreeSet<SubscriberChunkID> = chunks.map(TerrainChunkID::convert_up).collect();
        for sub in self.0.lock().unwrap().iter() {
            let mut inner = sub.inner.lock().unwrap();
            inner.cleared = true;
            if sub.limit.is_some() {
                inner.updated_chunks.clear();
            } else if sub.filter.intersects(UpdateType::all()) {
                inner.updated_chunks.clone_from(&buckets);
            }
        }
    }

    pub fn dispatch_water(&mut self, dirty: &WaterDirtyChunks) {
        for (kind, chunks) in [
            (UpdateType::Water, dirty.water()),
            (UpdateType::Terrain, dirty.terrain()),
            (UpdateType::RoadSupport, dirty.road_support()),
        ] {
            let buckets: BTreeSet<SubscriberChunkID> = chunks
                .iter()
                .copied()
                .map(TerrainChunkID::convert_up)
                .collect();
            self.dispatch_chunks(kind, buckets.into_iter());
        }
    }

    pub fn dispatch_clear(&self) {
        let mut me = self.0.lock().unwrap();
        for sub in me.iter_mut() {
            sub.inner.lock().unwrap().cleared = true;
        }
    }

    pub fn dispatch(&mut self, update_type: UpdateType, p: &impl CanonicalPosition) {
        let chunk_id = SubscriberChunkID::new(p.canonical_position());
        self.dispatch_chunk(update_type, chunk_id);
    }

    pub fn dispatch_chunk<const LEVEL: u16>(
        &mut self,
        update_type: UpdateType,
        chunk_id: ChunkID<LEVEL>,
    ) {
        self.dispatch_chunks(update_type, chunk_id.convert());
    }

    pub fn dispatch_chunks(
        &mut self,
        update_type: UpdateType,
        chunks: impl Iterator<Item = SubscriberChunkID>,
    ) {
        let mut me = self.0.lock().unwrap();
        for chunk in chunks {
            for sub in me.iter_mut() {
                sub.dispatch(update_type, chunk);
            }
        }
    }
}

#[derive(Default)]
pub struct MapSubscriberInner {
    pub updated_chunks: BTreeSet<SubscriberChunkID>,
    pub cleared: bool,
}

/// Describes a subscriber to a specific UpdateType
#[derive(Clone)]
pub struct MapSubscriber {
    filter: UpdateType,
    limit: Option<usize>,
    inner: Arc<Mutex<MapSubscriberInner>>,
}

impl MapSubscriber {
    pub fn new(update_type: UpdateType) -> Self {
        Self {
            filter: update_type,
            limit: None,
            inner: Default::default(),
        }
    }

    /// Drain a render journal as one boundary. A subscriber that has fallen
    /// behind can use the full source instead of carrying an unbounded delta.
    pub fn take_changes(&mut self, limit: usize) -> (bool, BTreeSet<SubscriberChunkID>) {
        let mut inner = self.inner.lock().unwrap();
        let cleared = std::mem::take(&mut inner.cleared);
        let chunks = std::mem::take(&mut inner.updated_chunks);
        if cleared || chunks.len() > limit {
            (true, BTreeSet::new())
        } else {
            (false, chunks)
        }
    }

    pub fn take_updated_chunks(&mut self) -> impl Iterator<Item = SubscriberChunkID> {
        let mut inner = self.inner.lock().unwrap();
        std::mem::take(&mut inner.updated_chunks).into_iter()
    }

    pub fn take_one_updated_chunk(&mut self) -> Option<SubscriberChunkID> {
        let mut inner = self.inner.lock().unwrap();
        inner.updated_chunks.pop_first()
    }

    pub fn take_cleared(&mut self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        std::mem::take(&mut inner.cleared)
    }

    pub fn dispatch(&mut self, update_type: UpdateType, chunk_id: SubscriberChunkID) {
        if !self.filter.intersects(update_type) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if self.limit.is_some() && inner.cleared {
            return;
        }
        inner.updated_chunks.insert(chunk_id);
        if self
            .limit
            .is_some_and(|limit| inner.updated_chunks.len() > limit)
        {
            inner.updated_chunks.clear();
            inner.cleared = true;
        }
    }
}

impl CanonicalPosition for Intersection {
    fn canonical_position(&self) -> Vec2 {
        self.pos.xy()
    }
}

impl CanonicalPosition for Road {
    fn canonical_position(&self) -> Vec2 {
        self.points.first().xy()
    }
}

impl CanonicalPosition for Building {
    fn canonical_position(&self) -> Vec2 {
        self.obb.center()
    }
}

impl CanonicalPosition for Lot {
    fn canonical_position(&self) -> Vec2 {
        self.shape.center()
    }
}

#[cfg(test)]
mod water_w1_tests {
    use super::*;

    #[test]
    fn w1_water_full_updates_cover_odd_rectangular_terrain_and_clear_empty_world() {
        let subscribers = MapSubscribers::default();
        let mut water = subscribers.subscribe(UpdateType::Water);
        let mut road_support = subscribers.subscribe(UpdateType::RoadSupport);
        subscribers.dispatch_all_terrain(
            (0..5).flat_map(|x| (0..3).map(move |y| TerrainChunkID::new_i16(x, y))),
        );
        let expected = (0..3)
            .flat_map(|x| (0..2).map(move |y| SubscriberChunkID::new_i16(x, y)))
            .collect::<Vec<_>>();
        assert!(water.take_cleared());
        assert_eq!(water.take_updated_chunks().collect::<Vec<_>>(), expected);
        assert_eq!(
            road_support.take_updated_chunks().collect::<Vec<_>>(),
            expected
        );
        subscribers.dispatch_all_terrain(std::iter::once(TerrainChunkID::new_i16(2, 2)));
        subscribers.dispatch_all_terrain(std::iter::empty());
        assert!(water.take_cleared());
        assert!(water.take_updated_chunks().next().is_none());
    }
}
