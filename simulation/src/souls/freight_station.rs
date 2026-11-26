use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use geom::{Transform, Vec3};
use prototypes::{FreightStationPrototypeID, GameTime, ItemID, SECONDS_PER_REALTIME_SECOND};

use crate::economy::cargo::{CargoBook, ItemQuantity, TRAIN_CAPACITY};
use crate::map::{BuildingID, Map, PathKind};
use crate::map_dynamic::{
    BuildingInfos, DispatchID, DispatchKind, DispatchQueryTarget, Dispatcher, Itinerary,
};
use crate::utils::resources::Resources;
use crate::world::{FreightStationEnt, FreightStationID, TrainID};
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
const LOADING_SECONDS: f64 = SECONDS_PER_REALTIME_SECOND as f64;

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
    #[inspect(skip)]
    #[serde(skip)]
    pub(crate) rail_trips: RailTrips,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct RailTrips {
    waiting_items: BTreeMap<ItemID, u32>,
    trains: BTreeMap<TrainID, RailTrip>,
}

#[derive(Serialize, Deserialize)]
pub struct RailTrip {
    pub dispatch_position: Vec3,
    pub exports: Option<RailLoad>,
}

#[derive(Serialize, Deserialize)]
pub struct RailLoad {
    pub items: Vec<ItemQuantity>,
    pub unspecified_units: u32,
}

impl FreightStation {
    pub(crate) fn accept_export(&mut self, units: u32) {
        self.waiting_cargo = self.waiting_cargo.saturating_add(units);
    }

    pub(crate) fn accept_item_export(&mut self, item: ItemID, units: u32) {
        let credited = units.min(u32::MAX - self.waiting_cargo);
        self.accept_export(units);
        if credited != 0 {
            *self.rail_trips.waiting_items.entry(item).or_default() += credited;
        }
    }

    pub fn rail_trip(&self, train: TrainID) -> Option<&RailTrip> {
        self.rail_trips.trains.get(&train)
    }
}

impl RailTrips {
    pub(crate) fn is_empty(&self) -> bool {
        self.waiting_items.is_empty() && self.trains.is_empty()
    }

    fn load_exports(&mut self, waiting_cargo: &mut u32, train: TrainID, position: Vec3) {
        let units = (*waiting_cargo).min(TRAIN_CAPACITY);
        let tracked: u32 = self.waiting_items.values().copied().sum();
        let unspecified_units = units.min(waiting_cargo.saturating_sub(tracked));
        let mut remaining = units - unspecified_units;
        let mut items = Vec::new();
        while remaining > 0 {
            let Some(mut pending) = self.waiting_items.first_entry() else {
                break;
            };
            let amount = (*pending.get()).min(remaining);
            items.push(ItemQuantity {
                item: *pending.key(),
                units: amount,
            });
            *pending.get_mut() -= amount;
            if *pending.get() == 0 {
                pending.remove();
            }
            remaining -= amount;
        }
        *waiting_cargo -= units;
        let trip = self.trains.entry(train).or_insert(RailTrip {
            dispatch_position: position,
            exports: None,
        });
        trip.exports = Some(RailLoad {
            items,
            unspecified_units: unspecified_units + remaining,
        });
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
        rail_trips: RailTrips::default(),
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
    let map = resources.read::<Map>();
    let time = resources.read::<GameTime>();
    let tick = time.tick;

    for (me, f) in world.freight_stations.iter_mut() {
        let pos = f.trans;
        let station = &mut f.f;
        if !map.buildings.contains_key(station.building) {
            cbuf.kill(me);
            continue;
        }

        // update our trains, and remove the ones that are done
        let mut to_clean = vec![];
        for (trainid, state) in &mut station.trains {
            let Some(train) = world.trains.get_mut(*trainid) else {
                to_clean.push(*trainid);
                continue;
            };
            let itin = &mut train.it;

            match state {
                FreightTrainState::Arriving => {
                    if itin.has_ended(0.0) {
                        *state = FreightTrainState::Loading;
                        station.rail_trips.load_exports(
                            &mut station.waiting_cargo,
                            *trainid,
                            train.trans.pos,
                        );
                        station.wanted_cargo =
                            cargo.unload_train(SoulID::FreightStation(me), TRAIN_CAPACITY);
                        *itin = Itinerary::wait_until(time.timestamp + LOADING_SECONDS);
                    }
                }
                FreightTrainState::Loading => {
                    if itin.has_ended(time.timestamp) {
                        let Some(building) = map
                            .external_train_stations
                            .first()
                            .and_then(|&id| map.get(id))
                        else {
                            *itin = Itinerary::wait_until(time.timestamp + LOADING_SECONDS);
                            continue;
                        };
                        let bpos = building.obb.center().z(0.0);

                        *itin = if let Some(r) =
                            Itinerary::route(tick, train.trans.pos, bpos, &map, PathKind::Rail)
                        {
                            r
                        } else {
                            Itinerary::wait_until(time.timestamp + LOADING_SECONDS);
                            continue;
                        };
                        *state = FreightTrainState::Moving;
                    }
                }
                FreightTrainState::Moving => {
                    if itin.has_ended(time.timestamp) {
                        to_clean.push(*trainid);
                    }
                }
            }
        }
        for v in to_clean {
            station.trains.retain(|x| x.0 != v);
            station.rail_trips.trains.remove(&v);
            dispatch.free(v)
        }

        // Query for a train when enough exports wait or any import is on order.
        if station.trains.len() >= MAX_TRAINS_PER_STATION || map.external_train_stations.is_empty()
        {
            continue;
        }
        let ordered_by_rail = station.wanted_cargo
            > station
                .trains
                .iter()
                .filter(|(_, state)| matches!(state, FreightTrainState::Arriving))
                .count() as u32
                * TRAIN_CAPACITY;
        if station.waiting_cargo < 10 && !ordered_by_rail {
            continue;
        }

        let destination = train_stop(&pos);

        let Some(DispatchID::FreightTrain(trainid)) = dispatch.query(
            &map,
            DispatchKind::FreightTrain,
            DispatchQueryTarget::Pos(destination),
        ) else {
            continue;
        };

        let train = world.trains.get_mut(trainid).unwrap();

        let Some(route) =
            Itinerary::route(tick, train.trans.pos, destination, &map, PathKind::Rail)
        else {
            dispatch.free(trainid);
            continue;
        };
        train.it = route;

        station.trains.push((trainid, FreightTrainState::Arriving));
        station.rail_trips.trains.insert(
            trainid,
            RailTrip {
                dispatch_position: train.trans.pos,
                exports: None,
            },
        );
    }
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
