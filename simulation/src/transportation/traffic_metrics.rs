use std::collections::BTreeMap;

use common::{FastMap, FastSet};

use crate::map::{Lane, LaneKind, Map, RoadID};
use crate::map_dynamic::Transit;
use crate::transportation::{VehicleKind, VehicleState};
use crate::{Simulation, VehicleEnt};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoadTraffic {
    pub vehicles: u16,
    pub rel_speed: f32,
    /// Number of usable vehicle observations in this sample.
    pub samples: u32,
}

#[derive(Default)]
struct Observations {
    total_speed: f64,
    count: u32,
}

/// Sample the requested roads. Roads without usable observations are omitted.
pub fn sample(
    sim: &Simulation,
    roads: impl Iterator<Item = RoadID>,
) -> BTreeMap<RoadID, RoadTraffic> {
    profiling::scope!("traffic_metrics::sample");
    let requested: FastSet<_> = roads.collect();
    if requested.is_empty() {
        return BTreeMap::new();
    }

    let map = sim.map();
    let transit = sim.read::<Transit>();
    let mut observations: FastMap<_, Observations> = FastMap::default();
    // Vehicle slot order fixes the order of floating-point additions.
    for (id, ent) in sim.world().vehicles.iter() {
        if !matches!(
            ent.vehicle.state,
            VehicleState::Driving | VehicleState::Panicking(_)
        ) || !ent.speed.0.is_finite()
            || !ent.trans.pos.is_finite()
            || matches!(ent.vehicle.kind, VehicleKind::Bus)
                && transit
                    .runs
                    .get(&id)
                    .is_some_and(|run| run.dwell_until.is_some())
        {
            continue;
        }

        let Some(lane) = current_lane(&map, ent) else {
            continue;
        };
        if !requested.contains(&lane.parent) {
            continue;
        }

        // Vehicle movement applies the kind factor and the individual multiplier
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
        road.total_speed += (f64::from(ent.speed.0) / f64::from(effective_limit)).clamp(0.0, 1.0);
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
                    samples: road.count,
                },
            )
        })
        .collect()
}

fn current_lane<'a>(map: &'a Map, entity: &VehicleEnt) -> Option<&'a Lane> {
    if let Some(traversable) = entity.it.get_travers() {
        return map.get(traversable.destination_lane());
    }

    // A route-less car can wait on a road. Bound the projection to avoid
    // assigning a vehicle in a yard or on another level to a distant lane.
    const LANE_SEARCH_RADIUS: f32 = 8.0;
    let pos = entity.trans.pos;
    let lane = map.get(map.nearest_lane(pos, LaneKind::Driving, Some(LANE_SEARCH_RADIUS))?)?;
    (lane.points.project_dist2(pos) <= LANE_SEARCH_RADIUS * LANE_SEARCH_RADIUS).then_some(lane)
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
                samples: 1
            })
        );
        assert_eq!(traffic.get(&empty), None);

        ctx.g.map_mut().remove_road(road).unwrap();
        assert!(sample(&ctx.g, [road, empty].into_iter()).is_empty());
    }

    #[test]
    fn p4e_traffic_aggregation_is_deterministic_and_turns_use_destination_lane() {
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
                vehicles: 2,
                rel_speed: 0.75,
                samples: 2
            })
        );
        assert!(sample(&ctx.g, [from].into_iter()).is_empty());
    }
}
