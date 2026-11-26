use serde::{Deserialize, Serialize};

use geom::{Transform, Vec3};
use prototypes::{
    FreightStationPrototypeID, GameTime, Tick, TICKS_PER_HOUR, TICKS_PER_REALTIME_SECOND,
};

use crate::economy::cargo::{
    CallManifest, CargoBook, Endpoint, FreightCallKey, FreightFailure, FreightTrace, RailAnchor,
    RailLeg,
};
use crate::map::{BuildingID, LaneID, Map};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{
    BuildingInfos, DispatchID, DispatchKind, DispatchQueryTarget, Dispatcher, Itinerary,
};
use crate::utils::resources::Resources;
use crate::world::{FreightStationEnt, FreightStationID, TrainEnt, TrainID};
use crate::World;
use crate::{ParCommandBuffer, Simulation, SoulID};

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Inspect)]
pub enum FreightTrainState {
    /// The train is coming to the station
    Arriving,
    /// The train is waiting for the station to load goods
    Loading,
    /// The train is going to the destination
    Moving,
}

const MAX_TRAINS_PER_STATION: usize = 2;
/// Train loading is agent movement: one real second, in game seconds.
const LOADING_TICKS: u64 = TICKS_PER_REALTIME_SECOND;

/// A freight train station
/// A component that identifies freight station souls, managing freight station logic
/// and the freight trains that are associated with them.
#[derive(Serialize, Deserialize, Inspect)]
pub struct FreightStation {
    pub proto: FreightStationPrototypeID,
    pub building: BuildingID,
    pub trains: Vec<(TrainID, FreightTrainState)>,
    /// Exported units waiting for a train.
    pub waiting_cargo: u32,
    /// Imported units on order by rail. A train call unloads them into the
    /// station's import stock, which its trucks carry to the buyers.
    pub wanted_cargo: u32,
}

impl FreightStation {
    pub(crate) fn accept_export(&mut self, units: u32) {
        self.waiting_cargo = self.waiting_cargo.saturating_add(units);
    }
}

pub fn freight_station_soul(
    sim: &mut Simulation,
    building: BuildingID,
    proto: FreightStationPrototypeID,
) -> Option<FreightStationID> {
    let map = sim.map();

    let f = FreightStation {
        proto,
        building,
        trains: Vec::with_capacity(MAX_TRAINS_PER_STATION),
        waiting_cargo: 0,
        wanted_cargo: 0,
    };
    let b = map.buildings.get(building)?;

    let height = b.height;
    let obb = b.obb;
    let pos = obb.center();
    let axis = obb.axis();

    drop(map);

    let id = sim.world.insert(FreightStationEnt {
        f,
        trans: Transform::new_dir(pos.z(height), axis[1].z(0.0).normalize()),
    });

    sim.write::<BuildingInfos>()
        .set_owner(building, SoulID::FreightStation(id));

    Some(id)
}

/// Where a station's trains stop on its platform. The dispatcher looks for
/// platform track within 50 m of it.
pub fn train_stop(trans: &Transform) -> Vec3 {
    trans.pos + trans.dir * 75.0 - trans.dir.perp_up() * 40.0
}

pub fn freight_station_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("souls::freight_station_system");
    let cbuf = resources.read::<ParCommandBuffer<FreightStationEnt>>();
    let mut dispatch = resources.write::<Dispatcher>();
    let mut cargo = resources.write::<CargoBook>();
    let mut trace = resources.write::<FreightTrace>();
    let map = resources.read::<Map>();
    let time = resources.read::<GameTime>();
    let tick = time.tick;
    let now = elapsed_ticks(resources).0;
    trace.age(now);

    for (me, entity) in world.freight_stations.iter_mut() {
        let station = &mut entity.f;
        if !map.buildings.contains_key(station.building) {
            cbuf.kill(me);
            continue;
        }
        let terminal = Endpoint {
            soul: SoulID::FreightStation(me),
            building: station.building,
        };
        let policy = trace.terminal_policy(terminal).policy;
        let mut assignments = std::mem::take(&mut station.trains);
        assignments.retain_mut(|(id, state)| {
            let Some(mut call) = trace.calls.remove(id) else {
                // Let a legacy assignment clear its existing leg. Its unproved
                // history cannot move any of the terminal's cargo balances.
                if world.trains.get(*id).is_some_and(|train| {
                    !train.it.has_ended(time.timestamp) && train.it.is_wait_for_reroute().is_none()
                }) {
                    return true;
                }
                dispatch.free(*id);
                return false;
            };
            let Some(train) = world.trains.get_mut(*id) else {
                cargo.close_call(
                    &mut trace,
                    call,
                    true,
                    Some(FreightFailure::TrainRemoved),
                    now,
                );
                dispatch.free(*id);
                return false;
            };
            if !map.external_train_stations.contains(&call.key.outside_gate)
                || !call.berth.live(&map)
                || !call.gate.live(&map)
                || !call.gate_departure.live(&map)
            {
                cargo.close_call(
                    &mut trace,
                    call,
                    true,
                    Some(FreightFailure::TrackChanged),
                    now,
                );
                train.it = Itinerary::NONE;
                dispatch.free(*id);
                return false;
            }
            if call.departed && !call.departure_recorded {
                trace.mark_departed(&call, now);
                call.departure_recorded = true;
            }

            let limited = !call.departed && call.leg == RailLeg::Inbound && !trace.may_start(&call);
            if !limited && now.saturating_sub(call.last_motion) >= TICKS_PER_HOUR {
                cargo.close_call(
                    &mut trace,
                    call,
                    true,
                    Some(FreightFailure::RailStalled),
                    now,
                );
                train.it = Itinerary::NONE;
                dispatch.free(*id);
                return false;
            }
            let disposition = advance_call(
                &mut trace, &mut cargo, &mut call, train, station, &map, tick, now, limited,
            );
            match disposition {
                CallDisposition::Active => {
                    *state = match call.leg {
                        RailLeg::Positioning | RailLeg::Inbound => FreightTrainState::Arriving,
                        RailLeg::Loading { .. } => FreightTrainState::Loading,
                        RailLeg::Outbound => FreightTrainState::Moving,
                    };
                    trace.calls.insert(*id, call);
                    true
                }
                CallDisposition::Exited => {
                    cargo.close_call(&mut trace, call, true, None, now);
                    dispatch.free(*id);
                    false
                }
            }
        });

        station.trains = assignments;
        if station.trains.len() >= usize::from(policy.max_calls)
            || (trace.unreserved_exports(terminal, station.waiting_cargo) < 10
                && !cargo.can_dispatch_import(&trace, terminal))
        {
            continue;
        }
        let Some(berth) = RailAnchor::near(&map, train_stop(&entity.trans)) else {
            continue;
        };
        let Some((outside, gate, gate_departure)) =
            map.external_train_stations.iter().find_map(|&id| {
                let building = map.get(id)?;
                let anchor = RailAnchor::near(&map, building.obb.center().z(building.height))?;
                berth.route(tick, &map, anchor.position, anchor.lane)?;
                anchor.route(tick, &map, berth.position, berth.lane)?;
                Some((id, anchor, anchor.departure_point(&map)?))
            })
        else {
            continue;
        };
        let Some(DispatchID::FreightTrain(id)) = dispatch.query(
            &map,
            DispatchKind::FreightTrain,
            DispatchQueryTarget::Pos(berth.position),
        ) else {
            continue;
        };
        let Some(train) = world.trains.get_mut(id) else {
            dispatch.free(id);
            continue;
        };
        let Some(lane) = train_lane(&map, train) else {
            dispatch.free(id);
            continue;
        };
        let (target, leg) = if lane == gate.lane
            && train.trans.pos.distance(gate.position) <= 40.0
            && gate_departure.before(train.trans.pos)
        {
            (berth, RailLeg::Inbound)
        } else {
            (gate, RailLeg::Positioning)
        };
        let Some(route) = target.route(tick, &map, train.trans.pos, lane) else {
            dispatch.free(id);
            continue;
        };
        let mut call = CallManifest {
            key: FreightCallKey {
                terminal,
                train: id,
                epoch: trace.next_id(),
                outside_gate: outside,
            },
            gate,
            gate_departure,
            berth,
            leg,
            departed: false,
            reached: false,
            departure_recorded: false,
            retry_at: now + LOADING_TICKS,
            last_motion: now,
            imports: Vec::new(),
            exports: Vec::new(),
            exports_loaded: false,
        };
        cargo.reserve_call(&mut trace, &mut call, now);
        trace.reserve_exports(&mut call, station.waiting_cargo);
        if call.imports.is_empty() && call.exports.is_empty() {
            dispatch.free(id);
            continue;
        }
        train.it = route;
        trace.calls.insert(id, call);
        station.trains.push((id, FreightTrainState::Arriving));
    }
}

enum CallDisposition {
    Active,
    Exited,
}

#[allow(clippy::too_many_arguments)]
fn advance_call(
    trace: &mut FreightTrace,
    cargo: &mut CargoBook,
    call: &mut CallManifest,
    train: &mut TrainEnt,
    station: &mut FreightStation,
    map: &Map,
    tick: Tick,
    now: u64,
    limited: bool,
) -> CallDisposition {
    let ended = train.it.has_ended(0.0);
    match call.leg {
        RailLeg::Positioning if call.reached && ended => {
            if let Some(route) = call.berth.route(tick, map, train.trans.pos, call.gate.lane) {
                call.leg = RailLeg::Inbound;
                call.reached = false;
                train.it = route;
            }
        }
        RailLeg::Inbound if call.reached && call.departed && ended => {
            station.wanted_cargo = cargo.receive_call(trace, call, now);
            station.waiting_cargo -= trace.load_exports(call, now);
            call.leg = RailLeg::Loading {
                until: now + LOADING_TICKS,
            };
            call.reached = false;
            train.it = Itinerary::NONE;
            train.speed.0 = 0.0;
        }
        RailLeg::Loading { until } if now >= until && now >= call.retry_at => {
            call.retry_at = now + LOADING_TICKS;
            if let Some(route) = call.gate.route(tick, map, train.trans.pos, call.berth.lane) {
                call.leg = RailLeg::Outbound;
                call.departed = false;
                call.departure_recorded = false;
                train.it = route;
            }
        }
        RailLeg::Outbound
            if call.reached
                && call.departed
                && ended
                && crate::transportation::train::rear_clear_of_turns(&train.res) =>
        {
            return CallDisposition::Exited;
        }
        RailLeg::Positioning | RailLeg::Inbound | RailLeg::Outbound
            if !limited
                && !call.reached
                && now >= call.retry_at
                && (ended
                    || train.it.is_wait_for_reroute().is_some()
                    || train
                        .it
                        .get_travers()
                        .is_some_and(|traversal| traversal.raw_points(map).is_none())) =>
        {
            call.retry_at = now + LOADING_TICKS;
            let target = if call.leg == RailLeg::Inbound {
                call.berth
            } else {
                call.gate
            };
            if let Some(route) = train_lane(map, train)
                .and_then(|lane| target.route(tick, map, train.trans.pos, lane))
            {
                train.it = route;
            }
        }
        _ => {}
    }
    CallDisposition::Active
}

fn train_lane(map: &Map, train: &TrainEnt) -> Option<LaneID> {
    crate::transportation::train::occupied_lanes(&train.res)
        .filter_map(|lane| {
            let distance = map.get(lane)?.points.project_dist2(train.trans.pos);
            (distance <= 0.25).then_some((ordered_float::OrderedFloat(distance), lane))
        })
        .min()
        .map(|(_, lane)| lane)
}

#[cfg(test)]
mod tests {
    use geom::{vec2, vec3, OBB};
    use prototypes::{BuildingGen, FreightStationPrototypeID};

    use crate::map_dynamic::BuildingInfos;
    use crate::souls::human::{spawn_human, HumanDecisionKind};
    use crate::tests::TestCtx;
    use crate::{BuildingKind, SoulID, WorldCommand};

    #[test]
    fn test_deliver_to_freight_station_incrs_station() {
        let mut test = TestCtx::new();

        test.build_roads(&[vec3(0., 0., 0.), vec3(100., 0., 0.)]);
        let house = test.build_house_near(vec2(50.0, 50.0));
        let human = spawn_human(&mut test.g, house).unwrap();

        test.apply(&[WorldCommand::MapBuildSpecialBuilding {
            pos: OBB::new(vec2(50.0, 50.0), vec2(1.0, 0.0), 5.0, 5.0),
            kind: BuildingKind::RailFreightStation(FreightStationPrototypeID::new(
                "freight-station",
            )),
            gen: BuildingGen::NoWalkway {
                door_pos: vec2(50.0, 50.0),
            },
            zone: None,
            connected_road: None,
        }]);
        test.tick();

        let station = test
            .g
            .map()
            .buildings()
            .iter()
            .find(|(_, b)| matches!(b.kind, BuildingKind::RailFreightStation(_)))
            .unwrap()
            .0;

        test.g
            .world_mut_unchecked()
            .humans
            .get_mut(human)
            .unwrap()
            .decision
            .kind = HumanDecisionKind::DeliverAtBuilding(station);

        let binfos = test.g.read::<BuildingInfos>();
        let SoulID::FreightStation(stationsoul) = binfos.owner(station).unwrap() else {
            panic!()
        };
        drop(binfos);

        for _ in 0..100 {
            test.tick();

            if test.g.get(stationsoul).unwrap().f.waiting_cargo == 1 {
                return;
            }
        }

        panic!("should have delivered to freight station")
    }
}
