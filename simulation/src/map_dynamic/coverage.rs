use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::hash::{DefaultHasher, Hash, Hasher};

use geom::{Vec2, Vec3};
use ordered_float::OrderedFloat;
use prototypes::{try_prototype, ServiceID, ServiceKind};
use serde::{Deserialize, Serialize};
use slotmapd::SecondaryMap;

use crate::map::{Building, BuildingID, IntersectionID, LaneKind, Map, Road, RoadID};
use crate::{Simulation, SoulID};

use super::{active_business, BuildingInfos, ElectricityFlow};

#[derive(Default, Serialize, Deserialize)]
pub struct CoverageGrid {
    pub by_kind: BTreeMap<ServiceKind, BTreeMap<BuildingID, f32>>,
    started_providers: BTreeSet<BuildingID>,
    #[serde(skip)]
    revisions: BTreeMap<ServiceKind, u64>,
    // Presentation data is rebuilt by coverage passes after loading.
    #[serde(skip)]
    served_distances: BTreeMap<ServiceKind, BTreeMap<BuildingID, f32>>,
}

impl CoverageGrid {
    pub fn coverage(&self, building: BuildingID, kind: ServiceKind) -> f32 {
        self.by_kind
            .get(&kind)
            .and_then(|receivers| receivers.get(&building))
            .copied()
            .unwrap_or_default()
    }

    /// Farthest network distance at which this provider allocated any service in the last pass.
    pub fn served_distance(&self, building: BuildingID, kind: ServiceKind) -> Option<f32> {
        self.served_distances.get(&kind)?.get(&building).copied()
    }

    pub fn revision(&self, kind: ServiceKind) -> u64 {
        self.revisions.get(&kind).copied().unwrap_or_default()
    }
}

#[derive(Clone, Copy)]
pub struct ServiceReachSource {
    pub building: BuildingID,
    pub center: Vec2,
    pub served_distance_m: f32,
    pub effective_capacity: f32,
}

/// Invalidate the display when either the allocation or the set of active providers changes.
pub fn service_reach_version(sim: &Simulation, kind: ServiceKind) -> u64 {
    let mut hash = DefaultHasher::new();
    sim.read::<CoverageGrid>().revision(kind).hash(&mut hash);
    for provider in active_providers(sim, kind) {
        provider.building.hash(&mut hash);
    }
    hash.finish()
}

/// Service providers on the map, including inactive facilities for display.
pub fn service_reach_sources(sim: &Simulation, kind: ServiceKind) -> Vec<ServiceReachSource> {
    let active: BTreeMap<_, _> = active_providers(sim, kind)
        .into_iter()
        .map(|provider| (provider.building, provider.capacity))
        .collect();
    let map = sim.map();
    let coverage = sim.read::<CoverageGrid>();
    let mut sources: Vec<_> = map
        .buildings()
        .values()
        .filter_map(|building| {
            let service = try_prototype(ServiceID::from(building.kind.as_goods_company()?))?;
            if service.service_kind != kind {
                return None;
            }
            let effective_capacity = active.get(&building.id).copied().unwrap_or(0.0);
            Some(ServiceReachSource {
                building: building.id,
                center: building.obb.center(),
                served_distance_m: if effective_capacity > 0.0 {
                    coverage
                        .served_distance(building.id, kind)
                        .unwrap_or_default()
                } else {
                    0.0
                },
                effective_capacity,
            })
        })
        .collect();
    sources.sort_unstable_by_key(|source| source.building);
    sources
}

const COVERAGE_KINDS: [ServiceKind; 6] = [
    ServiceKind::Health,
    ServiceKind::Fire,
    ServiceKind::Police,
    ServiceKind::School,
    ServiceKind::Garbage,
    ServiceKind::Park,
];
// Each kind refreshes every 20 game minutes, in its own 2-minute slot.
pub(crate) const REFRESH_INTERVAL: u64 = 20 * prototypes::TICKS_PER_MINUTE;
pub(crate) const REFRESH_SLOT: u64 = 2 * prototypes::TICKS_PER_MINUTE;
const STARTUP_INTERVAL: u64 = prototypes::TICKS_PER_MINUTE;
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
    served_distance: Option<f32>,
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
    let scheduled = offset % REFRESH_SLOT == REFRESH_SLOT / 2;
    let startup_offset = tick % STARTUP_INTERVAL;
    let index = if scheduled {
        offset / REFRESH_SLOT
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
    grid.served_distances.insert(
        kind,
        providers
            .iter()
            .filter_map(|provider| Some((provider.building, provider.served_distance?)))
            .collect(),
    );
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
            let business = active_business(building, &infos, sim.world())
                .filter(|business| business.company == id)?;
            if map
                .electricity
                .net_id(building.id)
                .is_some_and(|net| electricity.blackout(net))
            {
                return None;
            }
            let productivity = if building.development().is_some() {
                business.rates().staffed_fraction as f32
            } else {
                company.raw_productivity(proto, building.zone.as_ref())
            };
            let capacity = proto.capacity as f32 * productivity;
            (capacity > 0.0).then_some(Provider {
                building: building.id,
                road,
                door: building.door_pos,
                radius: proto.radius_m,
                capacity,
                served_distance: None,
            })
        })
        .collect()
}

fn coverage_receivers(building: &Building, infos: &BuildingInfos, sim: &Simulation) -> usize {
    if building.is_abandoned_growable() {
        return 0;
    }
    let residents = infos
        .get(building.id)
        .filter(|_| building.has_residential_use())
        .map(|info| &info.residents);
    let workers = building
        .kind
        .as_goods_company()
        .filter(|proto| proto.is_growable())
        .and_then(|_| active_business(building, infos, sim.world()))
        .and_then(|business| sim.get(business.company))
        .map(|company| &company.workers.0);

    match (residents, workers) {
        (Some(residents), Some(workers)) => residents
            .iter()
            .copied()
            .chain(workers.iter().copied().map(SoulID::Human))
            .collect::<BTreeSet<_>>()
            .len(),
        (Some(residents), None) => residents.len(),
        (None, Some(workers)) => workers.len(),
        (None, None) => 0,
    }
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
                let people = coverage_receivers(building, &infos, sim) as f32;
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
    for (index, provider) in providers.iter_mut().enumerate() {
        provider.served_distance =
            search.serve(&map, &mut roads, provider, index + 1, &mut coverage);
    }
    coverage
}

#[cfg(test)]
#[test]
fn z3_accounting_coverage_counts_the_union_at_a_mixed_address() {
    use crate::map_dynamic::accounting_site;
    use crate::souls::human::spawn_human;

    let (mut test, id, company) = accounting_site(true);
    let tenant = spawn_human(&mut test.g, id).unwrap();
    spawn_human(&mut test.g, id).unwrap();
    let other_home = test.g.map().lots().values().next().unwrap().shape.center();
    let other_home = test.build_house_near(other_home);
    let commuter = spawn_human(&mut test.g, other_home).unwrap();
    test.g.world.companies[company].workers.0 = vec![tenant, commuter];
    let mut provider = {
        let map = test.g.map();
        let building = map.get(id).unwrap();
        let road = building.connected_road.unwrap();
        Provider {
            building: id,
            road,
            door: map
                .get(road)
                .unwrap()
                .points()
                .project_2d(building.door_pos.xy()),
            radius: 1_000.0,
            capacity: 1.0,
            served_distance: None,
        }
    };
    let score = |sim: &Simulation, provider: &mut Provider| {
        recompute(sim, ServiceKind::Health, std::slice::from_mut(provider))[&id]
    };
    let with_shop = score(&test.g, &mut provider);
    test.g.world.companies[company].workers.0.clear();
    let housing_only = score(&test.g, &mut provider);
    assert!(housing_only > 0.0);
    assert!((with_shop - housing_only * 2.0 / 3.0).abs() < 0.001);
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
    ) -> Option<f32> {
        let origin = roads.get(provider.road)?;
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
        let mut farthest_served = None;
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
                        // Candidates are handled in ascending network distance.
                        farthest_served = Some(candidate.distance.0);
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
        farthest_served
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

#[cfg(test)]
mod u3_tests {
    use geom::{vec2, vec3, OBB};
    use prototypes::{BuildingGen, GameTime, GoodsCompanyID};
    use slotmapd::KeyData;

    use super::*;
    use crate::map::BuildingKind;
    use crate::map_dynamic::electricity_flow_system;
    use crate::souls::goods_company::company_soul;
    use crate::tests::TestCtx;
    use crate::world::HumanID;

    #[test]
    fn u3_coverage_reach_tracks_allocated_distance_and_live_activity() {
        let mut test = TestCtx::new_fast();
        let initial: Vec<_> = test.g.map().roads().keys().collect();
        test.build_roads(&[vec3(100.0, 400.0, 0.0), vec3(600.0, 400.0, 0.0)]);
        let road = test
            .g
            .map()
            .roads()
            .keys()
            .find(|id| !initial.contains(id))
            .unwrap();
        let build = |sim: &Simulation, x, kind, gen| {
            let id = sim
                .map_mut()
                .build_special_building(
                    &OBB::new(vec2(x, 420.0), Vec2::X, 20.0, 20.0),
                    kind,
                    gen,
                    None,
                    Some(road),
                )
                .unwrap();
            sim.write::<BuildingInfos>().insert(id);
            id
        };
        let near = build(&test.g, 330.0, BuildingKind::House, BuildingGen::House);
        let far = build(&test.g, 440.0, BuildingKind::House, BuildingGen::House);
        let worker = HumanID::from(KeyData::from_ffi(1));
        for home in [near, far] {
            test.g
                .write::<BuildingInfos>()
                .get_mut(home)
                .unwrap()
                .residents
                .insert(worker.into());
        }

        let mut provider = |name, x, staff| {
            let proto = GoodsCompanyID::new(name).prototype();
            let id = build(&test.g, x, BuildingKind::GoodsCompany(proto.id), proto.bgen);
            let Some(SoulID::GoodsCompany(company)) = company_soul(&mut test.g, id, proto.id)
            else {
                panic!("missing company");
            };
            test.g.world.companies[company].workers.0 = vec![worker; staff];
            company
        };
        let school = provider("school", 280.0, 1);
        let plant = provider("coal-power-plant", 210.0, 10);
        let refresh_power = |sim: &mut Simulation| {
            let (world, resources) = sim.world_res();
            electricity_flow_system(world, resources);
        };
        let reach = |sim: &Simulation| {
            let sources = service_reach_sources(sim, ServiceKind::School);
            assert_eq!(sources.len(), 1);
            sources[0].served_distance_m
        };
        refresh_power(&mut test.g);
        assert_eq!(reach(&test.g), 0.0);
        *test.g.write::<GameTime>() = GameTime::new(prototypes::Tick(
            REFRESH_INTERVAL + 3 * REFRESH_SLOT + REFRESH_SLOT / 2,
        ));
        coverage_update(&mut test.g);
        let radius = ServiceID::new("school").prototype().radius_m;
        let coverage = test.g.read::<CoverageGrid>();
        let score = coverage.coverage(far, ServiceKind::School);
        assert!(coverage.coverage(near, ServiceKind::School) > score);
        drop(coverage);
        let served = reach(&test.g);
        let version = service_reach_version(&test.g, ServiceKind::School);
        assert!(served > 0.0 && served < radius);
        assert!((served - radius * (1.0 - score / 100.0)).abs() < 0.01);

        test.g.world.companies[plant].workers.0.clear();
        refresh_power(&mut test.g);
        assert_eq!(reach(&test.g), 0.0);
        assert_ne!(service_reach_version(&test.g, ServiceKind::School), version);
        test.g.world.companies[plant].workers.0 = vec![worker; 10];
        refresh_power(&mut test.g);
        assert_eq!(reach(&test.g), served);
        assert_eq!(service_reach_version(&test.g, ServiceKind::School), version);

        test.g.world.companies[school].workers.0.clear();
        assert_eq!(reach(&test.g), 0.0);
    }
}
