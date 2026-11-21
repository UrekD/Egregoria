use crate::map::{Lane, LaneKind, Map, ParkingSpot, ParkingSpotID, ParkingSpots};
use common::AccessCmp;
use geom::Vec3;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::option::Option::None;

pub(crate) const NEAR_PARKING_RADIUS: f32 = 100.0;

#[derive(Debug, Serialize, Deserialize)]
#[repr(transparent)]
pub struct SpotReservation(ParkingSpotID);

#[derive(Default, Serialize, Deserialize)]
pub struct ParkingManagement {
    reserved_spots: BTreeSet<ParkingSpotID>,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum ParkingReserveError {
    FindingNearestLane,
    FetchingLaneData,
    NoSpotFoundAfterSearch,
}

impl ParkingManagement {
    #[allow(unknown_lints)]
    #[allow(clippy::forget_non_drop)]
    pub fn free(&mut self, spot: SpotReservation) {
        if !self.reserved_spots.remove(&spot.0) {
            log::warn!("{:?} wasn't reserved", spot.0);
        }
        std::mem::forget(spot);
    }

    pub fn is_free(&self, spot: SpotReservation) -> bool {
        self.is_spot_free(spot.0)
    }

    pub fn is_spot_free(&self, spot: ParkingSpotID) -> bool {
        !self.reserved_spots.contains(&spot)
    }

    pub fn reserve_random_free_spot(
        &mut self,
        spots: &ParkingSpots,
        rng: u64,
    ) -> Option<SpotReservation> {
        for i_try in 0..3 {
            let Some(spot) = spots.random_spot(rng.wrapping_add(i_try)) else {
                continue;
            };
            if self.reserved_spots.insert(spot) {
                return Some(SpotReservation(spot));
            }
        }
        None
    }

    pub fn reserve_near(
        &mut self,
        near: Vec3,
        map: &Map,
    ) -> Result<SpotReservation, ParkingReserveError> {
        self.reserve_near_inner(near, map, None, 7)
    }

    pub fn reserve_near_radius(
        &mut self,
        near: Vec3,
        map: &Map,
        radius: f32,
    ) -> Result<SpotReservation, ParkingReserveError> {
        self.reserve_near_inner(near, map, Some(radius), usize::MAX)
    }

    fn reserve_near_inner(
        &mut self,
        near: Vec3,
        map: &Map,
        radius: Option<f32>,
        depth: usize,
    ) -> Result<SpotReservation, ParkingReserveError> {
        use ParkingReserveError as E;
        let lane = map
            .nearest_lane(near, LaneKind::Driving, radius)
            .ok_or(E::FindingNearestLane)?;
        let lane = map.lanes().get(lane).ok_or(E::FetchingLaneData)?;

        let idget = |l: &Lane| l.id;
        let max_dist2 = radius.map(|radius| radius * radius);

        let mut potential = BTreeSet::new();
        potential.insert(AccessCmp(lane, idget));
        let mut next = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let intersections = map.intersections();
        let roads = map.roads();
        for _ in 0..depth {
            if potential.is_empty() {
                break;
            }
            for lane in potential.iter() {
                let lane = lane.0;
                if !visited.insert(lane.id)
                    || max_dist2.is_some_and(|max| {
                        lane.points.project_2d(near.xy()).xy().distance2(near.xy()) > max
                    })
                {
                    continue;
                }

                let inter_dst = unwrap_or!(intersections.get(lane.dst), continue);
                let inter_src = unwrap_or!(intersections.get(lane.src), continue);

                next.extend(
                    inter_dst
                        .turns_from(lane.id)
                        .flat_map(|(turn, _)| Some(AccessCmp(map.lanes().get(turn.dst)?, idget))),
                );

                next.extend(
                    inter_src
                        .turns_to(lane.id)
                        .flat_map(|(turn, _)| Some(AccessCmp(map.lanes().get(turn.src)?, idget))),
                );

                let parent = unwrap_or!(roads.get(lane.parent), continue);
                let plane = unwrap_or!(parent.parking_next_to(lane), continue);

                if let Some(p_iter) = map.parking.closest_spots(plane, near) {
                    for spot in p_iter {
                        if max_dist2.is_none_or(|max| {
                            map.parking
                                .get(spot)
                                .is_some_and(|data| data.trans.pos.xy().distance2(near.xy()) <= max)
                        }) && self.reserved_spots.insert(spot)
                        {
                            return Ok(SpotReservation(spot));
                        }
                    }
                }
            }
            std::mem::swap(&mut potential, &mut next);
            next.clear();
        }
        Err(E::NoSpotFoundAfterSearch)
    }
}

impl SpotReservation {
    pub fn exists(&self, spots: &ParkingSpots) -> bool {
        spots.contains(self.0)
    }

    pub fn get<'a>(&self, spots: &'a ParkingSpots) -> Option<&'a ParkingSpot> {
        spots.get(self.0)
    }

    pub fn park_pos(&self, map: &Map) -> Option<Vec3> {
        map.parking_to_drive_pos(self.0)
    }
}
