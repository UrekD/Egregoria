use crate::map::{Lane, LaneID, LaneKind, TraverseDirection};
use crate::souls::freight_station::train_lane;
use crate::utils::resources::Resources;
use crate::world::{TrainID, VehicleID};
use crate::{Map, World};
use derive_more::From;
use geom::Vec3;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Movement tolerance for registering a position without an occupied lane.
#[cfg(test)]
const PRECISION_RADIUS: f32 = 5.0;
#[cfg(test)]
const PRECISION_RADIUS_2: f32 = PRECISION_RADIUS * PRECISION_RADIUS;

/// Dispatcher is used to query for the closest networked entity matching a condition
/// For example:
/// - A rail freight station will query for the closest train to it that is not already used by another station
/// - A factory will query for a truck to deliver goods
/// - A hospital will query for the closest injured person
#[derive(Default, Serialize, Deserialize)]
pub struct Dispatcher {
    dispatches: BTreeMap<DispatchKind, DispatchOne>,
}

#[derive(Debug, Copy, Clone, PartialOrd, Ord, Eq, PartialEq, Serialize, Deserialize, From)]
pub enum DispatchID {
    FreightTrain(TrainID),
    SmallTruck(VehicleID),
}

impl From<DispatchID> for DispatchKind {
    fn from(id: DispatchID) -> Self {
        match id {
            DispatchID::FreightTrain(_) => DispatchKind::FreightTrain,
            DispatchID::SmallTruck(_) => DispatchKind::SmallTruck,
        }
    }
}

/// Dispatcher specialized to one kind
#[derive(Serialize, Deserialize)]
struct DispatchOne {
    positions: BTreeMap<DispatchID, DispatchPosition>,
    lanes: BTreeMap<LaneID, Vec<DispatchID>>,
    reserved_by: BTreeSet<DispatchID>,
    lanekind: LaneKind,
}

#[derive(Serialize, Deserialize)]
struct DispatchPosition {
    lane: LaneID,
    pos: Vec3,
    dist_along: f32,
}

/// DispatchKind is a component that is added to entities that can be dispatched
/// Usually constant.
#[derive(Serialize, Deserialize, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Inspect)]
pub enum DispatchKind {
    FreightTrain,
    SmallTruck,
}

impl DispatchKind {
    pub fn lane_kind(self) -> LaneKind {
        match self {
            DispatchKind::FreightTrain => LaneKind::Rail,
            DispatchKind::SmallTruck => LaneKind::Driving,
        }
    }
}

/// DispatchQueryTarget is the target that can be queried to the dispatcher
#[derive(Copy, Clone)]
pub enum DispatchQueryTarget {
    Pos(Vec3),
    Lane(LaneID),
}

impl Dispatcher {
    /// A vehicle already on the target lane must be at or before the target.
    pub(crate) fn position_is_eligible(
        lane: LaneID,
        offset: f32,
        target_lane: LaneID,
        target_offset: f32,
    ) -> bool {
        !(lane == target_lane && offset > target_offset)
    }

    /// Index trains on the same occupied lanes used to plan freight service.
    pub fn update(&mut self, map: &Map, world: &World) {
        let disp_trains = self
            .dispatches
            .entry(DispatchKind::FreightTrain)
            .or_insert_with(|| DispatchOne::new(DispatchKind::FreightTrain.lane_kind()));

        for (ent, train) in world.trains.iter() {
            let id = DispatchID::FreightTrain(ent);
            if let Some(lane) = train_lane(map, train) {
                disp_trains.register_on_lane(id, &map.lanes[lane], train.trans.pos);
            } else {
                disp_trains.remove_position(id);
            }
        }
    }

    /// Frees the entity as it is no longer used
    /// For example if a train is no longer used by a station, it should be freed so that other stations can use it
    /// It should be re-added to the cache at the next update iteration
    pub fn free(&mut self, ent: impl Into<DispatchID>) {
        let ent: DispatchID = ent.into();
        let kind: DispatchKind = ent.into();
        let Some(disp) = self.dispatches.get_mut(&kind) else {
            return;
        };
        disp.reserved_by.remove(&ent);
    }

    pub fn unregister(&mut self, id: DispatchID) {
        let kind = id.into();
        let Some(disp) = self.dispatches.get_mut(&kind) else {
            return;
        };
        disp.unregister(id);
    }

    pub(crate) fn has_available(&self, kind: DispatchKind) -> bool {
        self.dispatches.get(&kind).is_some_and(|dispatch| {
            dispatch
                .positions
                .keys()
                .any(|id| !dispatch.reserved_by.contains(id))
        })
    }

    pub(crate) fn query_map<T>(
        &mut self,
        map: &Map,
        kind: DispatchKind,
        target: DispatchQueryTarget,
        plan: impl Fn(DispatchID) -> Option<T>,
    ) -> Option<(DispatchID, T)> {
        let disp = self.dispatches.get_mut(&kind)?;
        let best = disp.query_map(map, kind, target, plan)?;
        disp.reserve(best.0);
        Some(best)
    }

    /// Reserves an entity that is closest to the target (if it is found) and returns it
    /// it takes `me` as an argument so that if `me` is killed, the reservation is cancelled
    /// If no entity is found, returns None
    pub fn query(
        &mut self,
        map: &Map,
        kind: DispatchKind,
        target: DispatchQueryTarget,
    ) -> Option<DispatchID> {
        self.query_map(map, kind, target, |_| Some(()))
            .map(|(id, ())| id)
    }
}

impl DispatchOne {
    fn new(lanekind: LaneKind) -> Self {
        Self {
            positions: Default::default(),
            lanes: Default::default(),
            reserved_by: Default::default(),
            lanekind,
        }
    }

    fn register_on_lane(&mut self, id: DispatchID, lane: &Lane, pos: Vec3) {
        let previous = self.positions.get(&id);
        if previous.is_some_and(|previous| previous.lane == lane.id && previous.pos == pos) {
            return;
        }
        if previous.is_none_or(|previous| previous.lane != lane.id) {
            self.remove_position(id);
            self.lanes.entry(lane.id).or_default().push(id);
        }

        self.positions.insert(
            id,
            DispatchPosition {
                lane: lane.id,
                pos,
                dist_along: lane.points.length_at_proj(lane.points.project(pos)),
            },
        );
    }

    #[cfg(test)]
    fn register(&mut self, id: DispatchID, map: &Map, pos: Vec3) {
        if let Some(previous) = self.positions.get_mut(&id) {
            if previous.pos.distance2(pos) < PRECISION_RADIUS_2 {
                return;
            }
            if let Some(lane) = map.lanes().get(previous.lane) {
                let projected = lane.points.project(pos);
                if projected.distance2(pos) < PRECISION_RADIUS_2 {
                    previous.dist_along = lane.points.length_at_proj(projected);
                    return;
                }
            }
        }
        if let Some(lane) = map.nearest_lane(pos, self.lanekind, Some(50.0)) {
            self.register_on_lane(id, &map.lanes[lane], pos);
        }
    }

    fn remove_position(&mut self, id: DispatchID) -> bool {
        let Some(pos) = self.positions.remove(&id) else {
            return false;
        };
        self.lanes.get_mut(&pos.lane).unwrap().retain(|e| *e != id);
        true
    }

    fn reserve(&mut self, id: DispatchID) {
        self.reserved_by.insert(id);
        if !self.remove_position(id) {
            log::error!("Dispatcher: trying to reserve an entity that is not in the cache");
        }
    }

    pub fn unregister(&mut self, id: DispatchID) {
        self.reserved_by.remove(&id);
        self.remove_position(id);
    }

    /// Finds an entity that is closest to the target and returns it
    /// If no entity is found, returns None
    #[cfg(test)]
    fn query(
        &self,
        map: &Map,
        kind: DispatchKind,
        target: DispatchQueryTarget,
    ) -> Option<DispatchID> {
        self.query_map(map, kind, target, |_| Some(()))
            .map(|(id, ())| id)
    }

    fn query_map<T>(
        &self,
        map: &Map,
        kind: DispatchKind,
        target: DispatchQueryTarget,
        plan: impl Fn(DispatchID) -> Option<T>,
    ) -> Option<(DispatchID, T)> {
        // todo: handle the case where there are few entities in the cache
        // todo: probably some kind of astar on good candidates

        let mut start_along = f32::MAX;

        if self.positions.is_empty() {
            return None;
        }

        let target_lane = match target {
            DispatchQueryTarget::Pos(pos) => {
                let lid = map.nearest_lane(pos, kind.lane_kind(), Some(50.0))?;
                let lane = map.lanes().get(lid)?;
                let proj = lane.points.project(pos);
                start_along = lane.points.length_at_proj(proj);
                lid
            }
            DispatchQueryTarget::Lane(lane) => {
                #[allow(clippy::question_mark)]
                if map.lanes().get(lane).is_none() {
                    return None;
                }
                lane
            }
        };

        let mut best_dist = f32::MAX;
        let mut best_ent = None;

        // do a backward breadth first search, looking for lanes with matching entities
        for lane in pathfinding::directed::bfs::bfs_reach(target_lane, move |&lid| {
            let l = &map.lanes[lid];
            let start_i = l.src;
            let int = &map.intersections[start_i];
            int.turns_to(lid).map(|(tid, dir)| match dir {
                TraverseDirection::Forward => tid.src,
                TraverseDirection::Backward => tid.dst,
            })
        }) {
            let Some(ents) = self.lanes.get(&lane) else {
                continue;
            };
            for ent in ents {
                if self.reserved_by.contains(ent) {
                    continue;
                }
                let pos = self.positions.get(ent).unwrap();
                let dist = -pos.dist_along; // since dist_along is from start to end, a good dist_along is one that is big
                if dist >= best_dist
                    || !Dispatcher::position_is_eligible(lane, pos.dist_along, target_lane, start_along)
                {
                    continue;
                }
                if let Some(plan) = plan(*ent) {
                    best_dist = dist;
                    best_ent = Some((*ent, plan));
                }
            }
            if best_ent.is_some() {
                break;
            }
        }

        best_ent
    }
}

pub fn dispatch_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::dispatch");

    let mut dispatcher = resources.write::<Dispatcher>();
    let map = resources.read::<Map>();
    dispatcher.update(&map, world);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{LanePatternBuilder, MapProject, ProjectKind};
    use common::rand::rand2;

    fn mk_ent(id: u64) -> DispatchID {
        DispatchID::FreightTrain(TrainID::from(KeyData::from_ffi(id)))
    }

    #[test]
    fn dispatch_one_register_one_works() {
        let mut disp = DispatchOne::new(LaneKind::Rail);
        let mut map = Map::default();

        let (_, r) = map
            .make_connection(
                MapProject::ground(Vec3::ZERO),
                MapProject::ground(Vec3::x(100.0)),
                None,
                &LanePatternBuilder::new().rail(true).build(),
            )
            .unwrap();

        let lanes: Vec<LaneID> = map.roads[r].lanes_iter().map(|(id, _)| id).collect();

        // first insert
        let ent = mk_ent(1 << 32);
        disp.register(ent, &map, Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(disp.positions.len(), 1);
        assert_eq!(disp.lanes.len(), 1);
        assert_eq!(disp.lanes.values().next().unwrap()[0], ent);
        assert!(lanes.contains(disp.lanes.keys().next().unwrap()));

        // second insert in same lane
        let ent2 = mk_ent((1 << 32) + 1);
        disp.register(ent2, &map, Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(disp.positions.len(), 2);
        assert_eq!(disp.lanes.len(), 1);
        assert_eq!(disp.lanes.values().next().unwrap(), &vec![ent, ent2]);

        // insert in another lane
        let ent3 = mk_ent((1 << 32) + 2);
        disp.register(ent3, &map, Vec3::new(100.0, 10.0, 0.0));
        assert_eq!(disp.positions.len(), 3);
        assert_eq!(disp.lanes.len(), 2);
        let mut v = disp.lanes.values();
        assert_eq!(v.next().unwrap(), &vec![ent, ent2]);
        assert_eq!(v.next().unwrap(), &vec![ent3]);

        // unregister
        disp.unregister(ent);
        assert_eq!(disp.positions.len(), 2);
        assert_eq!(disp.lanes.len(), 2);
        let mut v = disp.lanes.values();
        assert_eq!(v.next().unwrap(), &vec![ent2]);
        assert_eq!(v.next().unwrap(), &vec![ent3]);

        // unregister again
        disp.unregister(ent2);
        assert_eq!(disp.positions.len(), 1);
        assert_eq!(disp.lanes.len(), 2);
        let mut v = disp.lanes.values();
        assert_eq!(v.next().unwrap(), &vec![]);
        assert_eq!(v.next().unwrap(), &vec![ent3]);

        // ent3 moves from a lane to another
        disp.register(ent3, &map, Vec3::new(100.0, -1.0, 0.0));
        let mut v = disp.lanes.values();
        assert_eq!(v.next().unwrap(), &vec![ent3]);
        assert_eq!(v.next().unwrap(), &vec![]);

        // ent3 doesn't change lane because it's close to the old one
        disp.register(ent3, &map, Vec3::new(100.0, 1.0, 0.0));
        let mut v = disp.lanes.values();
        assert_eq!(v.next().unwrap(), &vec![ent3]);
        assert_eq!(v.next().unwrap(), &vec![]);
    }

    #[test]
    fn query_same_lane_works() {
        let mut d = Dispatcher::default();
        let mut map = Map::default();

        let (_, r) = map
            .make_connection(
                MapProject::ground(Vec3::ZERO),
                MapProject::ground(Vec3::x(100.0)),
                None,
                &LanePatternBuilder::new().one_way(true).rail(true).build(),
            )
            .unwrap();

        let (lid, _) = map.roads[r].lanes_iter().next().unwrap();

        let mut register = |id: DispatchID, pos: f32| {
            d.dispatches
                .entry(DispatchKind::FreightTrain)
                .or_insert(DispatchOne::new(DispatchKind::FreightTrain.lane_kind()))
                .register(id, &map, Vec3::x(pos))
        };

        let ent0 = mk_ent(1 << 32);
        let ent1 = mk_ent((1 << 32) + 1);
        let ent2 = mk_ent((1 << 32) + 2);

        register(ent0, 0.0);
        register(ent1, 10.0);
        register(ent2, 100.0);

        assert_eq!(
            d.query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Pos(Vec3::x(70.0)),
            ),
            Some(ent1)
        );
        d.dispatches[&DispatchKind::FreightTrain]
            .reserved_by
            .contains(&mk_ent((1 << 32) + 1));

        assert_eq!(
            d.query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Pos(Vec3::x(50.0)),
            ),
            Some(ent0)
        );
        d.dispatches[&DispatchKind::FreightTrain]
            .reserved_by
            .contains(&mk_ent(1 << 32));

        assert!(d
            .query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Pos(Vec3::x(50.0)),
            )
            .is_none());

        assert_eq!(
            d.query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Lane(lid),
            ),
            Some(ent2)
        );
    }

    #[test]
    fn query_two_lanes_bfs() {
        let mut d = Dispatcher::default();
        let mut map = Map::default();

        let (i, _) = map
            .make_connection(
                MapProject::ground(Vec3::ZERO),
                MapProject::ground(Vec3::x(100.0)),
                None,
                &LanePatternBuilder::new().one_way(true).rail(true).build(),
            )
            .unwrap();

        let (_, r2) = map
            .make_connection(
                MapProject {
                    kind: ProjectKind::Intersection(i),
                    pos: Vec3::x(100.0),
                },
                MapProject::ground(Vec3::new(200.0, 50.0, 0.0)),
                None,
                &LanePatternBuilder::new().one_way(true).rail(true).build(),
            )
            .unwrap();

        // unrelated
        map.make_connection(
            MapProject::ground(Vec3::new(0.0, 10.0, 0.0)),
            MapProject::ground(Vec3::new(100.0, 10.0, 0.0)),
            None,
            &LanePatternBuilder::new().one_way(true).rail(true).build(),
        )
        .unwrap();

        let (lid, _) = map.roads[r2].lanes_iter().next().unwrap();

        let mut register = |id: DispatchID, pos: f32| {
            d.dispatches
                .entry(DispatchKind::FreightTrain)
                .or_insert(DispatchOne::new(DispatchKind::FreightTrain.lane_kind()))
                .register(id, &map, Vec3::x(pos))
        };

        let ent0 = mk_ent(1 << 32);
        let ent1 = mk_ent((1 << 32) + 1);
        let ent2 = mk_ent((1 << 32) + 2);

        register(ent0, 0.0);
        register(ent1, 10.0);
        register(ent2, 200.0);

        assert_eq!(
            d.query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Pos(Vec3::x(70.0)),
            ),
            Some(ent1)
        );
        d.dispatches[&DispatchKind::FreightTrain]
            .reserved_by
            .contains(&mk_ent((1 << 32) + 1));

        assert_eq!(
            d.query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Pos(Vec3::x(50.0)),
            ),
            Some(ent0)
        );
        d.dispatches[&DispatchKind::FreightTrain]
            .reserved_by
            .contains(&mk_ent(1 << 32));

        assert!(d
            .query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Pos(Vec3::x(50.0)),
            )
            .is_none());

        assert_eq!(
            d.query(
                &map,
                DispatchKind::FreightTrain,
                DispatchQueryTarget::Lane(lid),
            ),
            Some(ent2)
        );
    }

    use crate::map::procgen::load_parismap;
    use easybench::bench;
    use slotmapd::KeyData;

    #[test]
    fn bench_query() {
        /* if 1 == 1 {
            return;
        }*/

        let mut m = Map::default();
        load_parismap(&mut m);

        let mut minx = f32::MAX;
        let mut maxx = f32::MIN;
        let mut miny = f32::MAX;
        let mut maxy = f32::MIN;
        for pos in m.intersections.iter().map(|i| i.1.pos) {
            minx = minx.min(pos.x);
            maxx = maxx.max(pos.x);
            miny = miny.min(pos.y);
            maxy = maxy.max(pos.y);
        }
        let w = maxx - minx;
        let h = maxy - miny;

        let mut start = DispatchOne::new(LaneKind::Driving);
        let mut i = 0;
        println!(
            "query empty: {}",
            bench(|| {
                i += 1;
                start.query(
                    &m,
                    DispatchKind::SmallTruck,
                    DispatchQueryTarget::Pos(Vec3::new(
                        minx + w * rand2(i as f32, 12.0),
                        miny + h * rand2(i as f32, 11.0),
                        0.0,
                    )),
                )
            })
        );

        for i in 0..100 {
            start.register(
                mk_ent((1 << 32) + i),
                &m,
                Vec3::new(
                    minx + w * rand2(i as f32, 2.0),
                    miny + h * rand2(i as f32, 1.0),
                    0.0,
                ),
            );
        }

        let mut i = 0;
        println!(
            "query 100: {}",
            bench(|| {
                i += 1;
                start.query(
                    &m,
                    DispatchKind::SmallTruck,
                    DispatchQueryTarget::Pos(Vec3::new(
                        minx + w * rand2(i as f32, 12.0),
                        miny + h * rand2(i as f32, 11.0),
                        0.0,
                    )),
                )
            })
        );

        for i in 100..1000 {
            start.register(
                mk_ent((1 << 32) + i),
                &m,
                Vec3::new(
                    minx + w * rand2(i as f32, 2.0),
                    miny + h * rand2(i as f32, 1.0),
                    0.0,
                ),
            );
        }

        let mut i = 0;
        println!(
            "query 1000: {}",
            bench(|| {
                i += 1;
                start.query(
                    &m,
                    DispatchKind::SmallTruck,
                    DispatchQueryTarget::Pos(Vec3::new(
                        minx + w * rand2(i as f32, 12.0),
                        miny + h * rand2(i as f32, 11.0),
                        0.0,
                    )),
                )
            })
        );

        for i in 1000..10000 {
            start.register(
                mk_ent((1 << 32) + i),
                &m,
                Vec3::new(
                    minx + w * rand2(i as f32, 2.0),
                    miny + h * rand2(i as f32, 1.0),
                    0.0,
                ),
            );
        }

        let mut i = 0;
        println!(
            "query 10000: {}",
            bench(|| {
                i += 1;
                start.query(
                    &m,
                    DispatchKind::SmallTruck,
                    DispatchQueryTarget::Pos(Vec3::new(
                        minx + w * rand2(i as f32, 12.0),
                        miny + h * rand2(i as f32, 11.0),
                        0.0,
                    )),
                )
            })
        );
    }
}
