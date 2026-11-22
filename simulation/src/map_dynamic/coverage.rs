use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fmt;

use geom::Vec3;
use ordered_float::OrderedFloat;
use prototypes::{try_prototype, ServiceID, ServiceKind};
use serde::de::{MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use slotmapd::SecondaryMap;

use crate::map::{BuildingID, BuildingKind, IntersectionID, LaneKind, Map, Road, RoadID};
use crate::{Simulation, SoulID};

use super::{BuildingInfos, ElectricityFlow};

#[derive(Default)]
pub struct CoverageGrid {
    pub by_kind: BTreeMap<ServiceKind, BTreeMap<BuildingID, f32>>,
    started_providers: BTreeSet<BuildingID>,
    revisions: BTreeMap<ServiceKind, u64>,
}

impl CoverageGrid {
    pub fn coverage(&self, building: BuildingID, kind: ServiceKind) -> f32 {
        self.by_kind
            .get(&kind)
            .and_then(|receivers| receivers.get(&building))
            .copied()
            .unwrap_or_default()
    }

    pub fn revision(&self, kind: ServiceKind) -> u64 {
        self.revisions.get(&kind).copied().unwrap_or_default()
    }
}

// Keep the existing service kind discriminants in the Bincode map. The extra
// record is written only for cities that have started at least one provider.
#[derive(Serialize, Deserialize)]
enum SnapshotKey {
    Health,
    Fire,
    Police,
    School,
    Garbage,
    Park,
    WaterPump,
    SewageOutlet,
    StartedProviders,
}

impl Serialize for CoverageGrid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Records<'a>(&'a CoverageGrid);

        impl Serialize for Records<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let has_providers = !self.0.started_providers.is_empty();
                let mut map = serializer
                    .serialize_map(Some(self.0.by_kind.len() + usize::from(has_providers)))?;
                for (kind, coverage) in &self.0.by_kind {
                    map.serialize_entry(kind, coverage)?;
                }
                if has_providers {
                    map.serialize_entry(&SnapshotKey::StartedProviders, &self.0.started_providers)?;
                }
                map.end()
            }
        }

        let mut state = serializer.serialize_struct("CoverageGrid", 1)?;
        state.serialize_field("by_kind", &Records(self))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CoverageGrid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct SavedGrid {
            #[serde(deserialize_with = "read_records")]
            by_kind: CoverageGrid,
        }

        fn read_records<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<CoverageGrid, D::Error> {
            struct Records;
            impl<'de> Visitor<'de> for Records {
                type Value = CoverageGrid;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("coverage snapshots")
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut grid = CoverageGrid::default();
                    while let Some(key) = map.next_key()? {
                        let kind = match key {
                            SnapshotKey::Health => ServiceKind::Health,
                            SnapshotKey::Fire => ServiceKind::Fire,
                            SnapshotKey::Police => ServiceKind::Police,
                            SnapshotKey::School => ServiceKind::School,
                            SnapshotKey::Garbage => ServiceKind::Garbage,
                            SnapshotKey::Park => ServiceKind::Park,
                            SnapshotKey::WaterPump => ServiceKind::WaterPump,
                            SnapshotKey::SewageOutlet => ServiceKind::SewageOutlet,
                            SnapshotKey::StartedProviders => {
                                grid.started_providers = map.next_value()?;
                                continue;
                            }
                        };
                        grid.by_kind.insert(kind, map.next_value()?);
                    }
                    Ok(grid)
                }
            }

            deserializer.deserialize_map(Records)
        }

        Ok(SavedGrid::deserialize(deserializer)?.by_kind)
    }
}

const COVERAGE_KINDS: [ServiceKind; 6] = [
    ServiceKind::Health,
    ServiceKind::Fire,
    ServiceKind::Police,
    ServiceKind::School,
    ServiceKind::Garbage,
    ServiceKind::Park,
];
const REFRESH_INTERVAL: u64 = 6000;
const STARTUP_INTERVAL: u64 = 300;
const STARTUP_STEP: u64 = STARTUP_INTERVAL / COVERAGE_KINDS.len() as u64;

#[derive(Clone, Copy)]
struct RoadPosition {
    along: f32,
    offset: f32,
}

impl RoadPosition {
    fn at_door(road: &Road, door: Vec3) -> Self {
        let points = road.points();
        let (point, segment_end) = points.project_segment_2d(door.xy());
        let prefix = &points.as_slice()[..segment_end];
        let along = prefix
            .windows(2)
            .map(|pair| pair[0].distance(pair[1]))
            .sum::<f32>()
            + prefix.last().map_or(0.0, |start| start.distance(point));
        Self {
            along: along.min(road.length()),
            offset: door.distance(point),
        }
    }
}

struct Provider {
    building: BuildingID,
    road: RoadID,
    door: Vec3,
    radius: f32,
    capacity: f32,
}

struct Receiver {
    building: BuildingID,
    position: RoadPosition,
    people: f32,
    unserved: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Direction {
    Forward,
    Backward,
}

impl Direction {
    fn exit(self, road: &Road) -> IntersectionID {
        match self {
            Self::Forward => road.dst,
            Self::Backward => road.src,
        }
    }
}

struct CoverageRoad<'a> {
    road: &'a Road,
    directions: [bool; 2],
    receivers: Vec<Receiver>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NodeId {
    Intersection(IntersectionID),
    Road(RoadID, Direction),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    distance: OrderedFloat<f32>,
    building: BuildingID,
    road: RoadID,
    receiver: usize,
}

fn road_directions(map: &Map, road: &Road, walking: bool) -> [bool; 2] {
    if walking {
        let has_sidewalk = road
            .lanes_iter()
            .any(|(id, kind)| kind == LaneKind::Walking && map.lanes().contains_key(id));
        return [has_sidewalk; 2];
    }
    let mut directions = [false; 2];
    for (id, kind) in road.lanes_iter() {
        if kind == LaneKind::Driving {
            let Some(lane) = map.lanes().get(id) else {
                continue;
            };
            let direction = if lane.src == road.src {
                Direction::Forward
            } else {
                Direction::Backward
            };
            directions[direction as usize] = true;
        }
    }
    directions
}

pub fn coverage_update(sim: &mut Simulation) {
    let tick = sim.get_tick();
    let offset = tick % REFRESH_INTERVAL;
    let scheduled = offset % 600 == 300;
    let startup_offset = tick % STARTUP_INTERVAL;
    let index = if scheduled {
        offset / 600
    } else if startup_offset % STARTUP_STEP == STARTUP_STEP / 2 {
        // Stagger first snapshots and keep them off the other city refresh ticks.
        startup_offset / STARTUP_STEP
    } else {
        return;
    };
    let Some(&kind) = COVERAGE_KINDS.get(index as usize) else {
        return;
    };

    profiling::scope!("map_dynamic::coverage_update");
    let mut providers = active_providers(sim, kind);
    if !scheduled {
        let grid = sim.read::<CoverageGrid>();
        if providers
            .iter()
            .all(|provider| grid.started_providers.contains(&provider.building))
        {
            return;
        }
    }

    let coverage = recompute(sim, kind, &mut providers);
    let map = sim.map();
    let mut grid = sim.write::<CoverageGrid>();
    grid.started_providers
        .retain(|id| map.buildings().contains_key(*id));
    grid.started_providers
        .extend(providers.iter().map(|provider| provider.building));
    grid.by_kind.insert(kind, coverage);
    let revision = grid.revisions.entry(kind).or_default();
    *revision = revision.wrapping_add(1);
}

fn active_providers(sim: &Simulation, kind: ServiceKind) -> Vec<Provider> {
    let map = sim.map();
    let electricity = sim.read::<ElectricityFlow>();
    let infos = sim.read::<BuildingInfos>();
    sim.world
        .companies
        .iter()
        .filter_map(|(id, company)| {
            let proto = try_prototype(ServiceID::from(company.comp.proto))?;
            if proto.service_kind != kind {
                return None;
            }
            let building = map.get(company.comp.building)?;
            let road = building.connected_road?;
            if infos.owner(building.id) != Some(SoulID::GoodsCompany(id))
                || map
                    .electricity
                    .net_id(building.id)
                    .is_some_and(|net| electricity.blackout(net))
            {
                return None;
            }
            let capacity =
                proto.capacity as f32 * company.raw_productivity(proto, building.zone.as_ref());
            (capacity > 0.0).then_some(Provider {
                building: building.id,
                road,
                door: building.door_pos,
                radius: proto.radius_m,
                capacity,
            })
        })
        .collect()
}

fn recompute(
    sim: &Simulation,
    kind: ServiceKind,
    providers: &mut [Provider],
) -> BTreeMap<BuildingID, f32> {
    let mut coverage = BTreeMap::new();
    if providers.is_empty() {
        return coverage;
    }
    providers.sort_unstable_by_key(|provider| provider.building);

    let map = sim.map();
    let infos = sim.read::<BuildingInfos>();
    let walking = matches!(kind, ServiceKind::School | ServiceKind::Park);
    let mut roads = SecondaryMap::with_capacity(map.roads().len());
    for (id, road) in map.roads() {
        let directions = road_directions(&map, road, walking);
        if directions == [false; 2] {
            continue;
        }
        let receivers = road
            .connected_buildings
            .iter()
            .filter_map(|&id| {
                let building = map.get(id)?;
                let people = match building.kind {
                    BuildingKind::House => infos.get(id)?.residents.len(),
                    BuildingKind::GoodsCompany(proto) if proto.is_growable() => {
                        let SoulID::GoodsCompany(company) = infos.owner(id)? else {
                            return None;
                        };
                        sim.get(company)?.workers.0.len()
                    }
                    _ => return None,
                } as f32;
                (people > 0.0).then(|| Receiver {
                    building: id,
                    position: RoadPosition::at_door(road, building.door_pos),
                    people,
                    unserved: people,
                })
            })
            .collect();
        roads.insert(
            id,
            CoverageRoad {
                road,
                directions,
                receivers,
            },
        );
    }

    let mut search = RoadSearch::default();
    for (index, provider) in providers.iter().enumerate() {
        search.serve(&map, &mut roads, provider, index + 1, &mut coverage);
    }
    coverage
}

#[derive(Default)]
struct RoadSearch {
    queue: BinaryHeap<Reverse<(OrderedFloat<f32>, NodeId)>>,
    junction_distances: SecondaryMap<IntersectionID, (usize, f32)>,
    candidates: BinaryHeap<Reverse<Candidate>>,
}

impl RoadSearch {
    fn serve(
        &mut self,
        map: &Map,
        roads: &mut SecondaryMap<RoadID, CoverageRoad<'_>>,
        provider: &Provider,
        pass: usize,
        coverage: &mut BTreeMap<BuildingID, f32>,
    ) {
        let Some(origin) = roads.get(provider.road) else {
            return;
        };
        self.queue.clear();
        self.candidates.clear();

        let source = RoadPosition::at_door(origin.road, provider.door);
        let [forward, backward] = origin.directions;
        for (index, receiver) in origin.receivers.iter().enumerate() {
            let delta = receiver.position.along - source.along;
            if (forward && delta >= 0.0) || (backward && delta <= 0.0) {
                self.add_candidate(
                    provider.road,
                    index,
                    receiver,
                    source.offset + delta.abs(),
                    provider.radius,
                );
            }
        }
        if forward {
            let distance = source.offset + (origin.road.length() - source.along);
            self.enqueue(origin.road.dst, distance, provider.radius, pass);
        }
        if backward {
            let distance = source.offset + source.along;
            self.enqueue(origin.road.src, distance, provider.radius, pass);
        }

        let mut remaining = provider.capacity;
        while remaining > 0.0 {
            if let Some(&Reverse(candidate)) = self.candidates.peek() {
                // Finish all graph nodes at this distance before breaking receiver ties by ID.
                if self
                    .queue
                    .peek()
                    .is_none_or(|Reverse((distance, _))| *distance > candidate.distance)
                {
                    self.candidates.pop();
                    let receiver = &mut roads[candidate.road].receivers[candidate.receiver];
                    // A receiver can be reached from both ends of its road.
                    let served = remaining.min(receiver.unserved);
                    if served > 0.0 {
                        remaining -= served;
                        receiver.unserved -= served;
                        let falloff = 1.0 - candidate.distance.0 / provider.radius;
                        let value = 100.0 * falloff * (served / receiver.people);
                        let previous = coverage.entry(candidate.building).or_insert(0.0_f32);
                        *previous = previous.max(value);
                    }
                    continue;
                }
            }

            let Some(Reverse((OrderedFloat(distance), node))) = self.queue.pop() else {
                break;
            };
            match node {
                NodeId::Intersection(id) => {
                    if distance > self.junction_distances[id].1 {
                        continue;
                    }
                    for &road_id in &map.intersections()[id].roads {
                        let Some(road) = roads.get(road_id) else {
                            continue;
                        };
                        let direction = if road.road.src == id {
                            Direction::Forward
                        } else {
                            Direction::Backward
                        };
                        if road.directions[direction as usize] {
                            self.queue.push(Reverse((
                                OrderedFloat(distance),
                                NodeId::Road(road_id, direction),
                            )));
                        }
                    }
                }
                NodeId::Road(id, direction) => {
                    let road = &roads[id];
                    let length = road.road.length();
                    for (index, receiver) in road.receivers.iter().enumerate() {
                        let along = match direction {
                            Direction::Forward => receiver.position.along,
                            Direction::Backward => length - receiver.position.along,
                        };
                        self.add_candidate(id, index, receiver, distance + along, provider.radius);
                    }
                    self.enqueue(
                        direction.exit(road.road),
                        distance + length,
                        provider.radius,
                        pass,
                    );
                }
            }
        }
    }

    fn add_candidate(
        &mut self,
        road: RoadID,
        index: usize,
        receiver: &Receiver,
        road_distance: f32,
        radius: f32,
    ) {
        let distance = road_distance + receiver.position.offset;
        if distance < radius && receiver.unserved > 0.0 {
            self.candidates.push(Reverse(Candidate {
                distance: OrderedFloat(distance),
                building: receiver.building,
                road,
                receiver: index,
            }));
        }
    }

    fn enqueue(&mut self, id: IntersectionID, distance: f32, radius: f32, pass: usize) {
        if distance >= radius {
            return;
        }
        let best = self
            .junction_distances
            .entry(id)
            .unwrap()
            .or_insert((0, f32::INFINITY));
        if best.0 != pass || distance < best.1 {
            *best = (pass, distance);
            self.queue
                .push(Reverse((OrderedFloat(distance), NodeId::Intersection(id))));
        }
    }
}

#[cfg(test)]
#[path = "../tests/p2a_coverage.rs"]
mod tests;
