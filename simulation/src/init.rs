use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Once;

#[allow(unused_imports)]
use common::saveload::{Bincode, Encoder, JSONPretty, JSON};
use flat_spatial::cell::{CellObject, GridCell};
use flat_spatial::grid::GridObjects;
use flat_spatial::storage::CellIdx;
use geom::Vec2;
use prototypes::{GameTime, Tick};

use crate::economy::{citystats_update, market_update, CityStats, EcoStats, Government, Market};
use crate::map::Map;
use crate::map_dynamic::{
    coverage_update, dispatch_system, electricity_flow_system, env_update, itinerary_update,
    routing_changed_system, routing_update_system, utility_flow_update, BuildingInfos,
    CoverageGrid, Dispatcher, ElectricityFlow, EnvGrid, ParkingManagement, UtilityFlow,
};
use crate::multiplayer::MultiplayerState;
use crate::souls::events::{events_update, Fires};
use crate::souls::freight_station::freight_station_system;
use crate::souls::goods_company::company_system;
use crate::souls::happiness::{happiness_update, Happiness};
use crate::souls::human::update_decision_system;
use crate::transportation::pedestrian_decision_system;
use crate::transportation::road::{vehicle_decision_system, vehicle_state_update_system};
use crate::transportation::testing_vehicles::{random_vehicles_update, RandomVehicles};
use crate::transportation::train::{
    locomotive_system, train_reservations_update, TrainReservations,
};
use crate::transportation::{transport_grid_synchronize, TransportGrid, TransportState};
use crate::utils::resources::Resources;
use crate::world::{CompanyEnt, FreightStationEnt, HumanEnt, TrainEnt, VehicleEnt, WagonEnt};
use crate::World;
use crate::{
    add_souls_to_empty_buildings, utils, ParCommandBuffer, RandProvider, Replay, RunnableSystem,
    Simulation, SimulationOptions, INITIAL_TICK, RNG_SEED,
};

pub fn init() {
    static LOAD_PROTOTYPES: Once = Once::new();
    LOAD_PROTOTYPES.call_once(|| {
        // SAFETY: Initialization loads the registry once, before simulations use it.
        unsafe { load_prototypes() };
    });
    register_simulation();
}

/// Load the full prototype registry.
///
/// # Safety
/// No other thread may access the prototype registry while it is being replaced.
pub(crate) unsafe fn load_prototypes() {
    #[cfg(not(test))]
    let base = "./";
    #[cfg(test)]
    let base = "../";

    unsafe { prototypes::load_prototypes(base) }
        .unwrap_or_else(|err| panic!("Error loading prototypes: {err}"));
}

pub(crate) fn register_simulation() {
    static INIT: Once = Once::new();
    INIT.call_once(register_systems_and_resources);
}

fn register_systems_and_resources() {
    register_system("dispatch_system", dispatch_system);
    register_system("update_decision_system", update_decision_system);
    register_system("company_system", company_system);
    register_system("pedestrian_decision_system", pedestrian_decision_system);
    register_system("transport_grid_synchronize", transport_grid_synchronize);
    register_system("locomotive_system", locomotive_system);
    register_system("vehicle_decision_system", vehicle_decision_system);
    register_system("vehicle_state_update_system", vehicle_state_update_system);
    register_system("routing_changed_system", routing_changed_system);
    register_system("routing_update_system", routing_update_system);
    register_system("itinerary_update", itinerary_update);
    register_system("market_update", market_update);
    register_system("budget_update", crate::economy::budget_update);
    register_system("demand_update", crate::economy::demand_update);
    register_system_sim(
        "residence_update",
        crate::souls::residence::residence_update,
    );
    register_system_sim("growth_update", crate::souls::growth::growth_update);
    register_system("electricity_flow_system", electricity_flow_system);
    register_system_sim("utility_flow_update", utility_flow_update);
    register_system_sim("coverage_update", coverage_update);
    register_system_sim("env_update", env_update);
    register_system_sim("happiness_update", happiness_update);
    register_system_sim("events_update", events_update);
    register_system("train_reservations_update", train_reservations_update);
    register_system("freight_station", freight_station_system);
    register_system("random_vehicles", random_vehicles_update);
    register_system("update_map", |_, res| res.write::<Map>().update());

    register_system_sim("add_souls_to_empty_buildings", add_souls_to_empty_buildings);
    register_system_sim("citystats_update", citystats_update);

    register_resource_noserialize::<crate::world_command::CommandOutcomes>();
    register_resource_noserialize::<ParCommandBuffer<VehicleEnt>>();
    register_resource_noserialize::<ParCommandBuffer<TrainEnt>>();
    register_resource_noserialize::<ParCommandBuffer<HumanEnt>>();
    register_resource_noserialize::<ParCommandBuffer<WagonEnt>>();
    register_resource_noserialize::<ParCommandBuffer<FreightStationEnt>>();
    register_resource_noserialize::<ParCommandBuffer<CompanyEnt>>();
    register_resource_saveload::<SimulationOptions, Bincode>("simoptions", false);

    register_resource_default::<ElectricityFlow, Bincode>("electricity_flow");
    register_resource_optional_on_load::<CoverageGrid, Bincode>("coverage_grid");
    register_resource_optional_on_load::<UtilityFlow, Bincode>("utility_flow");
    register_resource_optional_on_load::<EnvGrid, Bincode>("env_grid");
    register_resource_optional_on_load::<Happiness, Bincode>("happiness");
    register_resource_optional_on_load::<Fires, Bincode>("fires");
    register_resource_default::<Market, Bincode>("market");
    register_resource_default::<EcoStats, Bincode>("ecostats");
    register_resource_default::<CityStats, Bincode>("citystats");
    register_resource_default::<MultiplayerState, Bincode>("multiplayer_state");
    register_resource_default::<RandomVehicles, Bincode>("random_vehicles");
    register_resource_default::<Map, Bincode>("map");
    register_resource_default::<TrainReservations, Bincode>("train_reservations");
    register_resource_default::<Government, Bincode>("government");
    register_resource_default::<crate::map::ZoneGrid, Bincode>("zone_grid");
    register_resource_default::<crate::souls::residence::Residences, Bincode>("residences");
    register_resource_default::<crate::economy::ZoneDemand, Bincode>("zone_demand");
    register_resource_default::<crate::economy::Budget, Bincode>("budget");
    register_resource_default::<ParkingManagement, Bincode>("pmanagement");
    register_resource_default::<BuildingInfos, Bincode>("binfos");
    register_resource::<GameTime, Bincode>("game_time", || GameTime::new(Tick(INITIAL_TICK)));
    register_transport_grid();
    register_resource::<RandProvider, Bincode>("randprovider", || RandProvider::new(RNG_SEED));
    register_resource_default::<Dispatcher, Bincode>("dispatcher");
    register_resource_default::<Replay, crate::utils::replay::ReplayJson>("replay");
}

// Match flat_spatial::Grid's Bincode layout while ordering the storage cells by
// coordinate. Cell contents and the slot map keep their original order and handles.
#[derive(Serialize, Deserialize)]
struct SavedTransportGrid {
    storage: SavedTransportCells,
    objects: GridObjects<TransportState, Vec2>,
    to_relocate: Vec<CellObject<Vec2>>,
    _phantom: PhantomData<Vec2>,
}

#[derive(Serialize, Deserialize)]
struct SavedTransportCells {
    cell_size: i32,
    cells: BTreeMap<CellIdx, GridCell<Vec2>>,
}

fn register_transport_grid() {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(|sim| sim.insert(TransportGrid::new(100))),
        });
        SAVELOAD_FUNCS.push(SaveLoadFunc {
            name: "transport_grid",
            optional_on_load: false,
            save: Box::new(|sim| {
                let data = Bincode::encode(&*sim.read::<TransportGrid>()).unwrap();
                let sorted: SavedTransportGrid = Bincode::decode(&data).unwrap();
                Bincode::encode(&sorted).unwrap()
            }),
            load: Box::new(|sim, data| {
                sim.insert(Bincode::decode::<TransportGrid>(data)?);
                Ok(())
            }),
        });
    }
}

pub struct InitFunc {
    pub f: Box<dyn Fn(&mut Simulation) + 'static>,
}

pub(crate) struct SaveLoadFunc {
    pub name: &'static str,
    pub optional_on_load: bool,
    pub save: Box<dyn Fn(&Simulation) -> Vec<u8> + 'static>,
    pub load: Box<dyn Fn(&mut Simulation, &[u8]) -> std::io::Result<()> + 'static>,
}

pub(crate) struct GSystem {
    pub(crate) s: Box<dyn Fn() -> Box<dyn RunnableSystem>>,
}

pub(crate) static mut INIT_FUNCS: Vec<InitFunc> = Vec::new();
pub(crate) static mut SAVELOAD_FUNCS: Vec<SaveLoadFunc> = Vec::new();
pub(crate) static mut GSYSTEMS: Vec<GSystem> = Vec::new();

/*fn register_init(s: fn(&mut World, &mut Resources)) {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(move |sim| s(&mut sim.world, &mut sim.resources)),
        });
    }
}*/

fn register_system(name: &'static str, s: fn(&mut World, &mut Resources)) {
    unsafe {
        GSYSTEMS.push(GSystem {
            s: Box::new(move || {
                Box::new(utils::scheduler::RunnableFn {
                    f: move |sim| s(&mut sim.world, &mut sim.resources),
                    name,
                })
            }),
        });
    }
}

fn register_system_sim(name: &'static str, s: fn(&mut Simulation)) {
    unsafe {
        GSYSTEMS.push(GSystem {
            s: Box::new(move || Box::new(utils::scheduler::RunnableFn { f: s, name })),
        });
    }
}

fn register_resource_noserialize<T: 'static + Default + Send + Sync>() {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(|uiw| uiw.insert(T::default())),
        });
    }
}

fn register_resource_default<
    T: 'static + Send + Sync + Serialize + DeserializeOwned + Default,
    E: Encoder,
>(
    name: &'static str,
) {
    register_resource::<T, E>(name, T::default);
}

// Same-version saves from before this resource was registered keep its initialized default.
fn register_resource_optional_on_load<
    T: 'static + Send + Sync + Serialize + DeserializeOwned + Default,
    E: Encoder,
>(
    name: &'static str,
) {
    register_resource_noserialize::<T>();
    register_resource_saveload::<T, E>(name, true);
}

fn register_resource<T: 'static + Send + Sync + Serialize + DeserializeOwned, E: Encoder>(
    name: &'static str,
    initializer: impl Fn() -> T + 'static,
) {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(move |uiw| uiw.insert(initializer())),
        });
        register_resource_saveload::<T, E>(name, false);
    }
}

fn register_resource_saveload<T: 'static + Send + Sync + Serialize + DeserializeOwned, E: Encoder>(
    name: &'static str,
    optional_on_load: bool,
) {
    unsafe {
        SAVELOAD_FUNCS.push(SaveLoadFunc {
            name,
            optional_on_load,
            save: Box::new(move |uiworld| E::encode(&*uiworld.read::<T>()).unwrap()),
            load: Box::new(move |sim, data| {
                sim.insert(E::decode::<T>(data)?);
                Ok(())
            }),
        });
    }
}
