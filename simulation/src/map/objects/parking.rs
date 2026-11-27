use crate::map::{Lane, LaneID, LaneKind, CROSSWALK_WIDTH};
use flat_spatial::Grid;
use geom::{Transform, Vec2, Vec3};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use slotmapd::{new_key_type, Key, SecondaryMap, SlotMap};
use std::sync::atomic::{AtomicU64, Ordering};

new_key_type! {
    pub struct ParkingSpotID;
}

pub const PARKING_SPOT_LENGTH: f32 = 6.0;

#[cfg(test)]
thread_local! {
    static CLOSEST_SPOT_INSPECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct ParkingSpot {
    pub parent: LaneID,
    pub trans: Transform,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ParkingSpots {
    pub(crate) spots: SlotMap<ParkingSpotID, ParkingSpot>,
    pub(crate) lane_spots: SecondaryMap<LaneID, Vec<ParkingSpotID>>,
    pub(crate) reuse_spot: Grid<ParkingSpotID, Vec2>,
    #[serde(skip, default = "next_revision")]
    revision: u64,
    #[serde(skip)]
    surface_count: usize,
}

fn next_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Default for ParkingSpots {
    fn default() -> Self {
        Self {
            spots: Default::default(),
            lane_spots: Default::default(),
            reuse_spot: Grid::new(10),
            revision: next_revision(),
            surface_count: 0,
        }
    }
}

impl ParkingSpots {
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub fn get(&self, spot: ParkingSpotID) -> Option<&ParkingSpot> {
        self.spots.get(spot)
    }

    pub fn contains(&self, spot: ParkingSpotID) -> bool {
        self.spots.contains_key(spot)
    }

    pub fn remove_spots(&mut self, lane: LaneID) {
        if let Some(spots) = self.lane_spots.remove(lane) {
            if !spots.is_empty() {
                self.revision = next_revision();
            }
            for spot in spots {
                self.spots.remove(spot);
            }
        }
    }

    pub fn random_spot(&self, rng: u64) -> Option<ParkingSpotID> {
        let count = self.spots.len() - self.surface_count;
        if count == 0 {
            return None;
        }
        if self.surface_count == 0 {
            return self.spots.keys().nth(rng as usize % count);
        }
        self.spots
            .iter()
            .filter_map(|(id, spot)| (!spot.parent.is_null()).then_some(id))
            .nth(rng as usize % count)
    }

    pub(in crate::map) fn insert_surface(&mut self, trans: Transform) -> ParkingSpotID {
        self.surface_count += 1;
        self.spots.insert(ParkingSpot {
            parent: LaneID::null(),
            trans,
        })
    }

    pub(in crate::map) fn remove_surface(&mut self, id: ParkingSpotID) {
        if self.spots.remove(id).is_some() {
            self.surface_count -= 1;
        }
    }

    pub(in crate::map) fn restore_surface_count(&mut self, owned: usize) -> bool {
        self.surface_count = self
            .spots
            .values()
            .filter(|spot| spot.parent.is_null())
            .count();
        self.surface_count == owned
    }

    pub fn clean_reuse(&mut self) -> u32 {
        let mut has_reused = 0;
        for (_, spot) in self.reuse_spot.clear() {
            self.spots.remove(spot);
            has_reused += 1;
        }
        if has_reused != 0 {
            self.revision = next_revision();
        }
        has_reused
    }

    pub fn remove_to_reuse(&mut self, lane: LaneID) {
        if let Some(spots) = self.lane_spots.remove(lane) {
            for spot_id in spots {
                let spot = unwrap_cont!(self.spots.get(spot_id));
                self.reuse_spot.insert(spot.trans.pos.xy(), spot_id);
            }
        }
    }

    pub fn generate_spots(&mut self, lane: &Lane) {
        debug_assert!(matches!(lane.kind, LaneKind::Parking));
        if self.lane_spots.contains_key(lane.id) {
            self.remove_to_reuse(lane.id);
        }

        let gap = CROSSWALK_WIDTH + 1.0;
        let l = lane.points.length() - gap * 2.0;
        let n_spots = (l / PARKING_SPOT_LENGTH) as i32;
        if n_spots <= 0 {
            self.lane_spots.insert(lane.id, vec![]);
            return;
        }
        let step = l / n_spots as f32;

        let parent = lane.id;
        let spots = &mut self.spots;
        let reuse = &mut self.reuse_spot;
        let mut moved = false;
        let spots = lane
            .points
            .points_dirs_along((0..n_spots).map(|x| (x as f32 + 0.5) * step + gap))
            .map(|(pos, dir)| {
                let mut iter = reuse.query_around(pos.xy(), 3.0);
                if let Some((h, _)) = iter.next() {
                    if let Some((_, spot_id)) = reuse.get(h) {
                        let spot_id = *spot_id;
                        drop(iter);

                        reuse.remove_maintain(h);
                        if let Some(p) = spots.get_mut(spot_id) {
                            let trans = Transform::new_dir(pos, dir);
                            moved |= p.trans != trans;
                            *p = ParkingSpot { parent, trans };
                            return spot_id;
                        } else {
                            log::error!("found a spot in reuse that doesn't exist anymore");
                        }
                    }
                }

                spots.insert(ParkingSpot {
                    parent,
                    trans: Transform::new_dir(pos, dir),
                })
            })
            .collect();

        self.lane_spots.insert(lane.id, spots);
        if moved {
            self.revision = next_revision();
        }
    }

    pub fn clear(&mut self) {
        self.revision = next_revision();
        self.spots.clear();
        self.surface_count = 0;
        self.lane_spots.clear();
        for _ in self.reuse_spot.clear() {}
    }

    pub fn spots(&self, lane: LaneID) -> Option<impl Iterator<Item = &ParkingSpot> + '_> {
        self.lane_spots
            .get(lane)
            .map(move |x| x.iter().flat_map(move |spot| self.spots.get(*spot)))
    }

    pub fn all_spots(&self) -> impl Iterator<Item = (ParkingSpotID, &ParkingSpot)> + '_ {
        self.spots.iter().filter(|(_, spot)| !spot.parent.is_null())
    }

    pub(crate) fn curb_spots_near(
        &self,
        lane: &Lane,
        near: Vec3,
        remaining: &mut usize,
    ) -> Option<impl Iterator<Item = ParkingSpotID> + '_> {
        let spots = self.lane_spots.get(lane.id)?;
        let last = spots.len().checked_sub(1)?;
        let points = &lane.points;
        let segments = points.len().checked_sub(1)?;
        // Project to the lane and recover its station before indexing the evenly
        // spaced bays. Charge both geometry passes before reading the segments.
        *remaining = remaining.checked_sub(segments.saturating_mul(2))?;
        let (projected, end) = points.project_segment_2d(near.xy());
        let start = end.checked_sub(1)?;
        let station = points
            .segments()
            .take(start)
            .map(|segment| segment.src.distance(segment.dst))
            .sum::<f32>()
            + points[start].distance(projected);
        let gap = CROSSWALK_WIDTH + 1.0;
        let spacing = (points.length() - gap * 2.0) / spots.len() as f32;
        let closest = (((station - gap) / spacing - 0.5).round() as usize).min(last);
        let furthest = closest.max(last - closest);
        Some((0..=furthest * 2).filter_map(move |step| {
            let distance = step.div_ceil(2);
            let index = if step % 2 == 0 {
                closest.checked_sub(distance)?
            } else {
                closest + distance
            };
            spots.get(index).copied()
        }))
    }

    /// Iterate in spiral around the projected `near`
    pub fn closest_spots(
        &self,
        lane: LaneID,
        near: Vec3,
    ) -> Option<impl Iterator<Item = ParkingSpotID> + '_> {
        let spots = &self.spots;
        let lspots = self.lane_spots.get(lane)?;
        let (closest, _) = lspots.iter().copied().enumerate().min_by_key(|&(_, x)| {
            #[cfg(test)]
            CLOSEST_SPOT_INSPECTIONS.with(|count| count.set(count.get() + 1));
            let p = unwrap_ret!(spots.get(x), OrderedFloat(f32::INFINITY));
            OrderedFloat(p.trans.pos.distance2(near))
        })?;

        let closest = closest as i32;

        Some(
            (0..lspots.len() as i32 * 2)
                .map(|i| if i & 1 == 0 { i >> 1 } else { -(i >> 1) })
                .filter_map(move |offset| {
                    let i = unwrap_orr!(usize::try_from(closest + offset), return None);
                    lspots.get(i)
                })
                .copied(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map_dynamic::ParkingManagement;
    use crate::tests::TestCtx;
    use geom::vec3;

    #[test]
    fn pathpark_curb_discovery_does_not_scan_entire_lanes() {
        let ctx = TestCtx::new();
        ctx.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(900.0, 100.0, 0.0)]);
        let parking = ctx.g.read::<ParkingManagement>();
        let map = ctx.g.map();
        CLOSEST_SPOT_INSPECTIONS.set(0);
        let point = vec3(500.0, 100.0, 0.3);
        let mut work = 1_024;
        let (near, _) = parking.property_curb_candidates(&map, point, &mut work);
        assert!(!near.is_empty());
        assert_eq!(CLOSEST_SPOT_INSPECTIONS.get(), 0, "unbudgeted curb scan");
        assert!(work < 1_024);

        let mut work = 1;
        let (near, street) = parking.property_curb_candidates(&map, point, &mut work);
        assert!(near.is_empty() && street.is_empty());
        assert_eq!(work, 0);
    }
}
