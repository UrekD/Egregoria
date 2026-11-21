use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use geom::Vec3;
use ordered_float::OrderedFloat;
use prototypes::{try_prototype, ServiceID, ServiceKind};
use serde::{Deserialize, Serialize};
use slotmapd::SecondaryMap;

use crate::map::{BuildingID, BuildingKind, IntersectionID, LaneKind, Map, Road, RoadID};
use crate::{Simulation, SoulID};

use super::{BuildingInfos, ElectricityFlow};

#[derive(Default, Serialize, Deserialize)]
pub struct CoverageGrid {
    pub by_kind: BTreeMap<ServiceKind, BTreeMap<BuildingID, f32>>,
}

impl CoverageGrid {
    pub fn coverage(&self, building: BuildingID, kind: ServiceKind) -> f32 {
        self.by_kind
            .get(&kind)
            .and_then(|receivers| receivers.get(&building))
            .copied()
            .unwrap_or_default()
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
    people: usize,
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NodeId {
    Intersection(IntersectionID),
    Road(RoadID, Direction),
}

struct CoverageRoad<'a> {
    road: &'a Road,
    directions: [bool; 2],
    receivers: Vec<Receiver>,
}

fn road_directions(map: &Map, road: &Road, walking: bool) -> [bool; 2] {
    if walking {
        return [road.has_sidewalks(); 2];
    }
    let mut directions = [false; 2];
    for (id, kind) in road.lanes_iter() {
        if kind == LaneKind::Driving {
            let lane = &map.lanes()[id];
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
    let offset = sim.get_tick() % 6000;
    if offset % 600 != 0 {
        return;
    }
    let Some(&kind) = COVERAGE_KINDS.get((offset / 600) as usize) else {
        return;
    };

    profiling::scope!("map_dynamic::coverage_update");
    let coverage = recompute(sim, kind);
    sim.write::<CoverageGrid>().by_kind.insert(kind, coverage);
}

fn recompute(sim: &Simulation, kind: ServiceKind) -> BTreeMap<BuildingID, f32> {
    let map = sim.map();
    let electricity = sim.read::<ElectricityFlow>();
    let infos = sim.read::<BuildingInfos>();
    let mut providers: Vec<_> = sim
        .world
        .companies
        .iter()
        .filter_map(|(id, company)| {
            let proto = try_prototype(ServiceID::from(company.comp.proto))?;
            let building = map.get(company.comp.building)?;
            let road = building.connected_road?;
            if proto.service_kind != kind
                || infos.owner(building.id) != Some(SoulID::GoodsCompany(id))
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
        .collect();
    let mut coverage = BTreeMap::new();
    if providers.is_empty() {
        return coverage;
    }
    providers.sort_unstable_by_key(|provider| provider.building);

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
                };
                (people > 0).then(|| Receiver {
                    building: id,
                    position: RoadPosition::at_door(road, building.door_pos),
                    people,
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

    for provider in providers {
        let Some(origin) = roads.get(provider.road) else {
            continue;
        };
        let source = RoadPosition::at_door(origin.road, provider.door);
        let arrivals = road_arrivals(&map, &roads, &provider, source);
        let mut candidates = Vec::new();
        for (id, [forward, backward]) in arrivals {
            let road = &roads[id];
            let length = road.road.length();
            for receiver in &road.receivers {
                let along = receiver.position.along;
                let mut distance = (forward + along).min(backward + (length - along));
                // A receiver on the starting road may be reached before the first junction.
                if id == provider.road {
                    let delta = along - source.along;
                    if (road.directions[0] && delta >= 0.0) || (road.directions[1] && delta <= 0.0)
                    {
                        distance = distance.min(source.offset + delta.abs());
                    }
                }
                distance += receiver.position.offset;
                if distance < provider.radius {
                    candidates.push(Reverse((
                        OrderedFloat(distance),
                        receiver.building,
                        receiver.people,
                    )));
                }
            }
        }

        let mut remaining = provider.capacity;
        // Heapify once and only order as many receivers as the provider can serve.
        let mut candidates = BinaryHeap::from(candidates);
        while remaining > 0.0 {
            let Some(Reverse((OrderedFloat(distance), id, people))) = candidates.pop() else {
                break;
            };
            let people = people as f32;
            let served = remaining.min(people);
            remaining -= served;
            let value = 100.0 * (1.0 - distance / provider.radius) * (served / people);
            let previous = coverage.entry(id).or_insert(0.0_f32);
            *previous = previous.max(value);
        }
    }
    coverage
}

fn road_arrivals(
    map: &Map,
    roads: &SecondaryMap<RoadID, CoverageRoad<'_>>,
    provider: &Provider,
    source: RoadPosition,
) -> SecondaryMap<RoadID, [f32; 2]> {
    let origin = &roads[provider.road];
    let mut queue = BinaryHeap::new();
    let [forward, backward] = origin.directions;
    if forward {
        let distance = source.offset + (origin.road.length() - source.along);
        queue.push(Reverse((
            OrderedFloat(distance),
            NodeId::Intersection(origin.road.dst),
        )));
    }
    if backward {
        let distance = source.offset + source.along;
        queue.push(Reverse((
            OrderedFloat(distance),
            NodeId::Intersection(origin.road.src),
        )));
    }
    let mut visited = SecondaryMap::new();
    let mut arrivals = SecondaryMap::new();
    arrivals.insert(provider.road, [f32::INFINITY; 2]);
    while let Some(Reverse((OrderedFloat(distance), node))) = queue.pop() {
        if distance >= provider.radius {
            break;
        }
        match node {
            NodeId::Intersection(id) => {
                if visited.insert(id, ()).is_some() {
                    continue;
                }
                for &id_road in &map.intersections()[id].roads {
                    let Some(road) = roads.get(id_road) else {
                        continue;
                    };
                    let direction = if road.road.src == id {
                        Direction::Forward
                    } else {
                        Direction::Backward
                    };
                    if road.directions[direction as usize] {
                        queue.push(Reverse((
                            OrderedFloat(distance),
                            NodeId::Road(id_road, direction),
                        )));
                    }
                }
            }
            NodeId::Road(id, direction) => {
                let distances = arrivals.entry(id).unwrap().or_insert([f32::INFINITY; 2]);
                distances[direction as usize] = distance;
                let road = roads[id].road;
                // Entering a road is free. Its far intersection costs its full centerline length.
                queue.push(Reverse((
                    OrderedFloat(distance + road.length()),
                    NodeId::Intersection(direction.exit(road)),
                )));
            }
        }
    }
    arrivals
}

#[cfg(test)]
#[path = "../tests/p2a_coverage.rs"]
mod tests;
