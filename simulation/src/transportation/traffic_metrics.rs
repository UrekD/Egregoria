use std::collections::BTreeMap;

use common::{FastMap, FastSet};
use geom::{Vec3, AABB};

use crate::economy::cargo::{CargoBook, ItemQuantity};
use crate::economy::TripPurpose;
use crate::map::{BuildingID, Lane, LaneKind, Map, Road, RoadID, TraverseKind};
use crate::map_dynamic::{Itinerary, Transit};
use crate::souls::desire::WorkKind;
use crate::transportation::{Location, VehicleKind, VehicleState, TIME_TO_PARK};
use crate::{Simulation, VehicleEnt, VehicleID};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoadTraffic {
    pub vehicles: u16,
    pub rel_speed: f32,
    /// Mean vehicle speed as a fraction of each vehicle's posted lane limit.
    pub rel_limit_speed: f32,
    /// Number of usable vehicle observations in this sample.
    pub samples: u32,
}

#[derive(Default)]
pub struct RoadUsers {
    pub cars: u32,
    pub trucks: u32,
    pub buses: u32,
    pub pedestrians: u32,
    pub cargo: Vec<TruckCargoRow>,
}

pub struct TruckCargoRow {
    pub truck: VehicleID,
    pub cargo: TruckCargo,
    pub destination: Option<BuildingID>,
}

pub enum TruckCargo {
    Manifest(Vec<ItemQuantity>),
    EmptyReturn,
    Unknown,
}

#[derive(Default)]
struct Observations {
    total_speed: f64,
    total_limit_ratio: f64,
    count: u32,
}

const LANE_SEARCH_RADIUS: f32 = 8.0;

/// Identifies the map and the revision of its road network, including turns.
pub fn road_network_revision(map: &Map) -> (u64, u64) {
    (map.identity(), map.walking_topology_generation)
}

/// Whether the road uses elevated deck geometry, including inherited spans.
pub fn road_is_elevated(road: &Road) -> bool {
    road.elevated_span()
}

/// UI-owned lookup for repeatedly sampling the same roads.
pub struct RoadSampler {
    roads: FastSet<RoadID>,
    revision: (u64, u64),
    fallback_bounds: Option<AABB>,
}

impl RoadSampler {
    pub fn new(map: &Map, roads: impl Iterator<Item = RoadID>) -> Self {
        let mut sampler = Self {
            roads: roads.collect(),
            revision: road_network_revision(map),
            fallback_bounds: None,
        };
        sampler.update_bounds(map);
        sampler
    }

    fn update_bounds(&mut self, map: &Map) {
        self.revision = road_network_revision(map);
        // The spatial query for route-less vehicles starts with roads' bold lines.
        self.fallback_bounds = self
            .roads
            .iter()
            .filter_map(|&id| map.get(id))
            .map(|road| road.points.bbox().flatten().expand(road.width * 0.5))
            .reduce(AABB::union);
    }

    /// Roads without usable observations are omitted.
    pub fn sample(&mut self, sim: &Simulation) -> BTreeMap<RoadID, RoadTraffic> {
        let map = sim.map();
        if self.revision != road_network_revision(&map) {
            self.update_bounds(&map);
        }
        sample_visible(sim, &map, &self.roads, self.fallback_bounds)
    }
}

/// Sample the requested roads. Roads without usable observations are omitted.
pub fn sample(
    sim: &Simulation,
    roads: impl Iterator<Item = RoadID>,
) -> BTreeMap<RoadID, RoadTraffic> {
    sample_visible(sim, &sim.map(), &roads.collect(), None)
}

/// Snapshot of traffic occupying a single road. A bus at a stop still occupies its
/// lane. A traveler inside a junction turn is between roads.
pub fn sample_road_users(sim: &Simulation, road: RoadID) -> RoadUsers {
    profiling::scope!("traffic_metrics::sample_road_users");
    let map = sim.map();
    let Some(segment) = map.get(road) else {
        return RoadUsers::default();
    };
    let bounds = segment.points.bbox().flatten().expand(segment.width * 0.5);
    let mut users = RoadUsers::default();
    let mut truck_rows = FastMap::default();
    let cargo = sim.read::<CargoBook>();

    for (id, vehicle) in sim.world().vehicles.iter() {
        if vehicle_lane(&map, vehicle, Some(bounds), |id| id == road).is_none() {
            continue;
        }
        match vehicle.vehicle.kind {
            VehicleKind::Car => users.cars += 1,
            VehicleKind::Bus => users.buses += 1,
            VehicleKind::Truck => {
                users.trucks += 1;
                let cargo = match cargo.manifest_for(id) {
                    Some(manifest) if !manifest.items.is_empty() => {
                        TruckCargo::Manifest(manifest.items.clone())
                    }
                    _ if matches!(vehicle.vehicle.state, VehicleState::ReturningToDepot { .. }) => {
                        TruckCargo::EmptyReturn
                    }
                    _ => TruckCargo::Unknown,
                };
                truck_rows.insert(id, users.cargo.len());
                users.cargo.push(TruckCargoRow {
                    truck: id,
                    cargo,
                    destination: None,
                });
            }
        }
    }

    for human in sim.world().humans.values() {
        if human.location == Location::Outside
            && route_lane(&map, &human.it).is_some_and(|lane| {
                lane.parent == road
                    && lane.kind == LaneKind::Walking
                    && within_lane(lane, human.trans.pos)
            })
        {
            users.pedestrians += 1;
        }
        if truck_rows.is_empty() {
            continue;
        }
        if let Some(WorkKind::Driver {
            truck,
            deliver_order,
            ..
        }) = human.work.as_ref().map(|work| work.kind)
        {
            if let Some(&index) = truck_rows.get(&truck) {
                let row = &mut users.cargo[index];
                if matches!(row.cargo, TruckCargo::Unknown)
                    && human.location == Location::Vehicle(truck)
                    && human.router.trip_purpose == TripPurpose::FreightReturn
                {
                    row.cargo = TruckCargo::EmptyReturn;
                }
                if !matches!(row.cargo, TruckCargo::EmptyReturn) {
                    row.destination =
                        deliver_order.filter(|&destination| map.get(destination).is_some());
                }
            }
        }
    }
    users
}

fn route_lane<'a>(map: &'a Map, itinerary: &Itinerary) -> Option<&'a Lane> {
    let TraverseKind::Lane(id) = itinerary.get_travers()?.kind else {
        return None;
    };
    map.get(id)
}

pub(crate) fn within_lane(lane: &Lane, pos: Vec3) -> bool {
    let half_width = lane.kind.width() * 0.5;
    lane.points.project_dist2(pos) <= half_width * half_width
}

fn vehicle_lane<'a>(
    map: &'a Map,
    entity: &VehicleEnt,
    fallback_bounds: Option<AABB>,
    requested: impl Fn(RoadID) -> bool,
) -> Option<&'a Lane> {
    let pos = entity.trans.pos;
    if !pos.is_finite() {
        return None;
    }
    match &entity.vehicle.state {
        VehicleState::RoadToPark(_, _, reservation) => {
            // The animation crosses from the driving lane into a parking lane
            // on the same road. Use the adjacent driving lane's speed limit.
            let parking = map.get(reservation.get(&map.parking)?.parent)?;
            if !requested(parking.parent) {
                return None;
            }
            let road = map.get(parking.parent)?;
            let (lane, _) = road
                .outgoing_lanes_from(parking.src)
                .iter()
                .rfind(|&&(_, kind)| kind == LaneKind::Driving)?;
            map.get(*lane)
        }
        VehicleState::Driving
        | VehicleState::Panicking(_)
        | VehicleState::ReturningToDepot { .. } => {
            let lane = if entity.it.get_travers().is_some() {
                route_lane(map, &entity.it)?
            } else {
                // A route-less driver may be stopped on a travel lane.
                if fallback_bounds
                    .is_some_and(|bounds| !bounds.contains_within(pos.xy(), LANE_SEARCH_RADIUS))
                {
                    return None;
                }
                let id = map.nearest_lane(pos, LaneKind::Driving, Some(LANE_SEARCH_RADIUS))?;
                map.get(id)?
            };
            (requested(lane.parent)
                && matches!(lane.kind, LaneKind::Driving | LaneKind::Bus)
                && within_lane(lane, pos))
            .then_some(lane)
        }
        VehicleState::Parked(_) | VehicleState::ParkedAtDepot => None,
    }
}

fn sample_visible(
    sim: &Simulation,
    map: &Map,
    requested: &FastSet<RoadID>,
    fallback_bounds: Option<AABB>,
) -> BTreeMap<RoadID, RoadTraffic> {
    profiling::scope!("traffic_metrics::sample");
    if requested.is_empty() {
        return BTreeMap::new();
    }
    let transit = sim.read::<Transit>();
    let mut observations: FastMap<_, Observations> = FastMap::default();
    // Vehicle slot order fixes the order of floating-point additions.
    for (id, ent) in sim.world().vehicles.iter() {
        if matches!(ent.vehicle.kind, VehicleKind::Bus)
            && transit
                .runs
                .get(&id)
                .is_some_and(|run| run.dwell_until.is_some())
        {
            continue;
        }
        let Some(lane) = vehicle_lane(map, ent, fallback_bounds, |road| requested.contains(&road))
        else {
            continue;
        };
        let speed = match ent.vehicle.state {
            VehicleState::RoadToPark(spline, progress, _) => {
                spline.derivative(progress).mag() / TIME_TO_PARK
            }
            _ => ent.speed.0,
        };
        if !speed.is_finite() {
            continue;
        }

        // Vehicle movement applies the kind factor and individual multiplier
        // to the lane limit when computing free-flow speed.
        let limit = lane.speed_limit;
        let vehicle_limit =
            ent.vehicle.kind.speed_factor() * ent.vehicle.max_speed_multiplier * limit;
        if !limit.is_finite() || !vehicle_limit.is_finite() {
            continue;
        }
        let effective_limit = limit.min(vehicle_limit);
        if effective_limit <= 0.0 {
            continue;
        }
        let road = observations.entry(lane.parent).or_default();
        road.total_speed += (f64::from(speed) / f64::from(effective_limit)).clamp(0.0, 1.0);
        road.total_limit_ratio += (f64::from(speed) / f64::from(limit)).max(0.0);
        road.count = road.count.saturating_add(1);
    }

    observations
        .into_iter()
        .map(|(id, road)| {
            (
                id,
                RoadTraffic {
                    vehicles: road.count.min(u32::from(u16::MAX)) as u16,
                    rel_speed: (road.total_speed / f64::from(road.count)) as f32,
                    rel_limit_speed: (road.total_limit_ratio / f64::from(road.count)) as f32,
                    samples: road.count,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{LaneID, LaneKind, TraverseKind};
    use crate::map_dynamic::transit::{BusRun, LineID};
    use crate::map_dynamic::{Itinerary, Transit};
    use crate::tests::TestCtx;
    use crate::transportation::{
        spawn_driving_vehicle, spawn_parked_vehicle, VehicleKind, VehicleState,
    };
    use crate::VehicleID;
    use geom::{vec3, Transform};
    use prototypes::{GameInstant, Tick};
    use std::collections::BTreeSet;

    fn streets() -> (TestCtx, [RoadID; 2], [LaneID; 2]) {
        let ctx = TestCtx::new();
        let existing: BTreeSet<_> = ctx.g.map().roads().keys().collect();
        ctx.build_roads(&[
            vec3(60.0, 150.0, 0.0),
            vec3(240.0, 150.0, 0.0),
            vec3(430.0, 310.0, 0.0),
        ]);
        let map = ctx.g.map();
        let mut roads: Vec<_> = map
            .roads()
            .keys()
            .filter(|id| !existing.contains(id))
            .collect();
        roads.sort_by(|&a, &b| {
            let left = |road: RoadID| {
                let points = &map.get(road).unwrap().points;
                points.first().x.min(points.last().x)
            };
            left(a).total_cmp(&left(b))
        });
        let roads = roads.try_into().unwrap();
        let lanes = <[RoadID; 2]>::map(roads, |road| {
            map.get(road)
                .unwrap()
                .lanes_iter()
                .find(|&(lane, kind)| {
                    kind == LaneKind::Driving
                        && map.get(lane).unwrap().points.first().x
                            < map.get(lane).unwrap().points.last().x
                })
                .unwrap()
                .0
        });
        drop(map);
        (ctx, roads, lanes)
    }

    fn add_vehicle(ctx: &mut TestCtx, lane: LaneID, offset: f32, kind: VehicleKind) -> VehicleID {
        let pos = ctx.g.map().get(lane).unwrap().points.point_along(offset);
        let id = spawn_driving_vehicle(&mut ctx.g, kind, Transform::new(pos)).unwrap();
        ctx.g.world.vehicles[id].vehicle.max_speed_multiplier = 1.0;
        id
    }

    #[test]
    fn u5_traffic_ignores_a_route_assigned_to_an_off_lane_vehicle() {
        let (mut ctx, [road, _], [lane, _]) = streets();
        let car = add_vehicle(&mut ctx, lane, 30.0, VehicleKind::Car);
        let pos = ctx.g.world.vehicles[car].trans.pos + vec3(0.0, -9.0, 0.0);
        let route = {
            let map = ctx.g.map();
            Itinerary::route_to_lane(Tick(0), pos, &map, lane, 85.0).unwrap()
        };
        ctx.g.world.vehicles[car].it = route;
        ctx.g.world.vehicles[car].trans.pos = pos;
        assert_eq!(sample_road_users(&ctx.g, road).cars, 0);
        assert_eq!(sample(&ctx.g, [road].into_iter()).get(&road), None);
    }

    #[test]
    fn u5_traffic_includes_returning_and_parking_trucks_in_both_samples() {
        use crate::map_dynamic::ParkingManagement;

        let (mut ctx, [road, _], [lane, _]) = streets();
        let truck = add_vehicle(&mut ctx, lane, 30.0, VehicleKind::Truck);
        let pos = ctx.g.world.vehicles[truck].trans.pos;
        let reserve = |sim: &Simulation| {
            sim.write::<ParkingManagement>()
                .reserve_near(pos, &sim.map())
                .unwrap()
        };
        ctx.g.world.vehicles[truck].vehicle.state = VehicleState::ReturningToDepot {
            spot: reserve(&ctx.g),
            stopped_ticks: 0,
        };
        ctx.g.world.vehicles[truck].speed.0 = 3.0;
        assert_eq!(sample_road_users(&ctx.g, road).trucks, 1);
        assert_eq!(
            sample(&ctx.g, [road].into_iter())
                .get(&road)
                .map(|t| t.samples),
            Some(1)
        );

        let spot = reserve(&ctx.g);
        let target = spot.get(&ctx.g.map().parking).unwrap().trans;
        ctx.g.world.vehicles[truck].begin_parking(target, spot);
        assert_eq!(sample_road_users(&ctx.g, road).trucks, 1);
        assert_eq!(
            sample(&ctx.g, [road].into_iter())
                .get(&road)
                .map(|t| t.samples),
            Some(1)
        );
    }

    #[test]
    fn p4e_traffic_stopped_drivers_count_but_parked_and_dwelling_do_not() {
        let (mut ctx, [road, empty], [lane, _]) = streets();
        add_vehicle(&mut ctx, lane, 20.0, VehicleKind::Car);
        let truck = add_vehicle(&mut ctx, lane, 36.0, VehicleKind::Truck);
        ctx.g.world.vehicles[truck].vehicle.state = VehicleState::ParkedAtDepot;
        let bus = add_vehicle(&mut ctx, lane, 54.0, VehicleKind::Bus);
        ctx.g.write::<Transit>().runs.insert(
            bus,
            BusRun {
                line: LineID::default(),
                next_stop_idx: 0,
                dwell_until: Some(Tick(100)),
                leg_started: Tick(0),
                failures: 0,
            },
        );
        let pos = ctx.g.map().get(lane).unwrap().points.point_along(85.0);
        spawn_parked_vehicle(&mut ctx.g, VehicleKind::Car, pos).unwrap();
        let distant = add_vehicle(&mut ctx, lane, 100.0, VehicleKind::Car);
        ctx.g.world.vehicles[distant].trans.pos.y += 40.0;

        let traffic = sample(&ctx.g, [road, empty].into_iter());
        assert_eq!(
            traffic.get(&road),
            Some(&RoadTraffic {
                vehicles: 1,
                rel_speed: 0.0,
                rel_limit_speed: 0.0,
                samples: 1
            })
        );
        assert_eq!(traffic.get(&empty), None);

        ctx.g.map_mut().remove_road(road).unwrap();
        assert!(sample(&ctx.g, [road, empty].into_iter()).is_empty());
    }

    #[test]
    fn u5_occupancy_includes_dwelling_buses_and_excludes_parked_vehicles_and_passengers() {
        use crate::map::PathKind;
        use crate::souls::desire::Work;
        use crate::souls::human::spawn_human;

        let (mut ctx, [road, empty], [lane, _]) = streets();
        add_vehicle(&mut ctx, lane, 20.0, VehicleKind::Car);
        let truck = add_vehicle(&mut ctx, lane, 36.0, VehicleKind::Truck);
        let bus = add_vehicle(&mut ctx, lane, 54.0, VehicleKind::Bus);
        let parked = add_vehicle(&mut ctx, lane, 72.0, VehicleKind::Truck);
        ctx.g.world.vehicles[parked].vehicle.state = VehicleState::ParkedAtDepot;
        ctx.g.write::<Transit>().runs.insert(
            bus,
            BusRun {
                line: LineID::default(),
                next_stop_idx: 0,
                dwell_until: Some(Tick(100)),
                leg_started: Tick(0),
                failures: 0,
            },
        );

        let house = ctx.build_house_near(vec3(125.0, 150.0, 0.0).xy());
        let walker = spawn_human(&mut ctx.g, house).unwrap();
        let passenger = spawn_human(&mut ctx.g, house).unwrap();
        for (id, location) in [
            (walker, Location::Outside),
            (passenger, Location::Vehicle(truck)),
        ] {
            let (pos, itinerary) = {
                let map = ctx.g.map();
                let sidewalk = map
                    .get(road)
                    .unwrap()
                    .lanes_iter()
                    .find_map(|(id, kind)| (kind == LaneKind::Walking).then_some(id))
                    .unwrap();
                let points = &map.get(sidewalk).unwrap().points;
                let pos = points.point_along(25.0);
                let itinerary = Itinerary::route(
                    Tick(0),
                    pos,
                    points.point_along(50.0),
                    &map,
                    PathKind::Pedestrian,
                )
                .unwrap();
                (pos, itinerary)
            };
            let human = &mut ctx.g.world.humans[id];
            human.location = location;
            human.trans.pos = pos;
            human.it = itinerary;
        }
        ctx.g.world.humans[passenger].work = Some(Work::new(
            house,
            WorkKind::Driver {
                truck,
                deliver_order: Some(house),
                load: None,
            },
            0.0,
        ));

        let current = sample_road_users(&ctx.g, road);
        assert_eq!(
            (
                current.cars,
                current.trucks,
                current.buses,
                current.pedestrians
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(current.cargo.len(), 1);
        assert_eq!(current.cargo[0].truck, truck);
        assert!(matches!(current.cargo[0].cargo, TruckCargo::Unknown));
        assert_eq!(current.cargo[0].destination, Some(house));
        assert_eq!(sample(&ctx.g, [road].into_iter())[&road].samples, 2);
        assert_eq!(sample_road_users(&ctx.g, empty).cars, 0);

        ctx.g.world.humans[passenger].router.trip_purpose = TripPurpose::FreightReturn;
        let returning = sample_road_users(&ctx.g, road);
        assert!(matches!(returning.cargo[0].cargo, TruckCargo::EmptyReturn));
        assert_eq!(returning.cargo[0].destination, None);
    }

    #[test]
    fn p4e_traffic_aggregation_is_deterministic_and_excludes_junction_turns() {
        let (mut ctx, [from, to], [from_lane, to_lane]) = streets();
        let truck = add_vehicle(&mut ctx, to_lane, 20.0, VehicleKind::Truck);
        let turning = add_vehicle(&mut ctx, from_lane, 30.0, VehicleKind::Car);
        let invalid = add_vehicle(&mut ctx, to_lane, 50.0, VehicleKind::Car);
        let (route, position, limit) = {
            let map = ctx.g.map();
            let from = map.get(from_lane).unwrap();
            let to = map.get(to_lane).unwrap();
            let mut route =
                Itinerary::route_between_lanes(Tick(0), &map, (from_lane, 30.0), (to_lane, 30.0))
                    .unwrap();
            let mut position = from.points.point_along(30.0);
            for _ in 0..200 {
                position = route.update(position, 1.0, Tick(0), 0, &map);
                if matches!(route.get_travers().unwrap().kind, TraverseKind::Turn(_)) {
                    break;
                }
            }
            assert!(matches!(
                route.get_travers().unwrap().kind,
                TraverseKind::Turn(turn) if turn.dst == to_lane
            ));
            (route, position, to.speed_limit)
        };
        ctx.g.world.vehicles[turning].it = route;
        ctx.g.world.vehicles[turning].trans.pos = position;
        ctx.g.world.vehicles[turning].vehicle.state = VehicleState::Panicking(GameInstant(Tick(0)));
        ctx.g.world.vehicles[turning].vehicle.max_speed_multiplier = 1.05;
        ctx.g.world.vehicles[turning].speed.0 = limit * 0.5;
        ctx.g.world.vehicles[truck].speed.0 = limit * 0.8;
        ctx.g.world.vehicles[invalid].speed.0 = f32::NAN;

        let traffic = sample(&ctx.g, [from, to].into_iter());
        assert_eq!(traffic, sample(&ctx.g, [to, from, to].into_iter()));
        assert_eq!(traffic.get(&from), None);
        assert_eq!(
            traffic.get(&to),
            Some(&RoadTraffic {
                vehicles: 1,
                rel_speed: 1.0,
                rel_limit_speed: 0.8,
                samples: 1
            })
        );
        assert!(sample(&ctx.g, [from].into_iter()).is_empty());
        assert_eq!(sample_road_users(&ctx.g, from).cars, 0);
        let users = sample_road_users(&ctx.g, to);
        assert_eq!(users.cars, 1); // The car with an invalid speed still occupies its lane.
        assert_eq!(users.trucks, 1);
    }
}
