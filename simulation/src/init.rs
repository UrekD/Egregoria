use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Once;

use common::saveload::{save_dir, sweep_stale_tmp};
#[allow(unused_imports)]
use common::saveload::{Bincode, Encoder, JSONPretty, JSON};
use flat_spatial::cell::{CellObject, GridCell};
use flat_spatial::grid::GridObjects;
use flat_spatial::storage::CellIdx;
use geom::Vec2;
use prototypes::{GameTime, Tick};

use crate::economy::cargo::{cargo_update, CargoBook};
use crate::economy::progression::{progression_update, ProgressionState};
use crate::economy::{
    citystats_update, market_update, CityStats, DailyEconomyMetrics, EcoStats, Government, Market, TransitStats,
    TripStats,
};
use crate::map::Map;
use crate::map_dynamic::transit::{
    transit_board_update, transit_dispatch_update, transit_stop_update,
};
use crate::map_dynamic::{
    coverage_update, dispatch_system, electricity_flow_system, env_update, itinerary_update,
    routing_changed_system, routing_update_system, utility_flow_update, BuildingInfos,
    CoverageGrid, Dispatcher, ElectricityFlow, EnvGrid, ParkingManagement, Transit, UtilityFlow,
};
use crate::multiplayer::MultiplayerState;
use crate::souls::events::{events_update, Fires};
use crate::souls::freight_station::freight_station_system;
use crate::souls::goods_company::company_system;
use crate::souls::happiness::{happiness_update, Happiness};
use crate::souls::household::{household_update, HouseholdClaims};
use crate::souls::human::update_decision_system;
use crate::transportation::pedestrian_decision_system;
use crate::transportation::road::{vehicle_decision_system, vehicle_state_update_system};
use crate::transportation::road_throughput::{road_throughput_update, RoadThroughput};
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
    Simulation, SimulationOptions, INITIAL_TICK,
};

pub fn init() {
    static STARTUP: Once = Once::new();
    STARTUP.call_once(|| {
        let dir = save_dir();
        if let Err(error) = sweep_stale_tmp(dir, std::time::Duration::from_secs(3600)) {
            log::warn!(
                "could not sweep stale staging files in {}: {error}",
                dir.display()
            );
        }
        // SAFETY: Initialization loads the registry once, before simulations use it.
        unsafe { load_prototypes() };
    });
    register_simulation();
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    #[test]
    fn startup_sweeps_old_staging_files_from_the_configured_save_dir() {
        const SAVE_DIR_ENV: &str = "EGR_TEST_STARTUP_SWEEP_SAVE_DIR";
        if let Some(root) = std::env::var_os(SAVE_DIR_ENV) {
            common::saveload::set_save_dir(PathBuf::from(root)).unwrap();
            super::init();
            return;
        }

        let root = std::env::temp_dir().join(format!(
            "egr-startup-sweep-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir(&root).unwrap();
        let stale = root.join(format!("bindings.json.{}.1.tmp", i32::MAX));
        let recent = root.join(format!("world.zip.{}.2.tmp", i32::MAX));
        let live = root.join(format!("world.zip.{}.3.tmp", std::process::id()));
        let save = root.join("world.zip");
        let now = SystemTime::now();
        let live_age = if cfg!(unix) { 90_000 } else { 7200 };
        for (path, age) in [
            (&stale, 90_000),
            (&recent, 3300),
            (&live, live_age),
            (&save, 90_000),
        ] {
            File::create(path)
                .unwrap()
                .set_modified(now - Duration::from_secs(age))
                .unwrap();
        }

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "init::tests::startup_sweeps_old_staging_files_from_the_configured_save_dir",
            ])
            .env(SAVE_DIR_ENV, &root)
            .output();
        let remaining = (
            stale.exists(),
            recent.exists(),
            live.exists(),
            save.exists(),
        );
        fs::remove_dir_all(root).unwrap();
        let output = output.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(remaining, (false, true, true, true));
    }
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
    register_system("electricity_flow_system", electricity_flow_system);
    register_system("dispatch_system", dispatch_system);
    register_system("update_decision_system", update_decision_system);
    register_system("household_update", household_update);
    register_system("company_system", company_system);
    register_system("pedestrian_decision_system", pedestrian_decision_system);
    register_system("transport_grid_synchronize", transport_grid_synchronize);
    register_system("locomotive_system", locomotive_system);
    register_system("vehicle_decision_system", vehicle_decision_system);
    register_system("vehicle_state_update_system", vehicle_state_update_system);
    register_system("transit_stop_update", transit_stop_update);
    register_system("transit_dispatch_update", transit_dispatch_update);
    register_system("transit_board_update", transit_board_update);
    register_system("routing_changed_system", routing_changed_system);
    register_system("routing_update_system", routing_update_system);
    register_system("itinerary_update", itinerary_update);
    register_system("road_throughput_update", road_throughput_update);
    register_system("market_update", market_update);
    register_system("cargo_update", cargo_update);
    register_system("budget_update", crate::economy::budget_update);
    register_system("accrue_transit_upkeep", |world, resources| {
        crate::economy::accrue_transit_upkeep(world, resources);
    });
    register_system("demand_update", crate::economy::demand_update);
    register_system_sim(
        "residence_update",
        crate::souls::residence::residence_update,
    );
    register_system_sim("growth_update", crate::souls::growth::growth_update);
    register_system(
        "refresh_electricity_flow_system",
        crate::map_dynamic::refresh_electricity_flow_system,
    );
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
    register_system_sim("progression_update", progression_update);

    register_resource_noserialize::<crate::world_command::CommandOutcomes>();
    register_resource_noserialize::<ParCommandBuffer<VehicleEnt>>();
    register_resource_noserialize::<ParCommandBuffer<TrainEnt>>();
    register_resource_noserialize::<ParCommandBuffer<HumanEnt>>();
    register_resource_noserialize::<ParCommandBuffer<WagonEnt>>();
    register_resource_noserialize::<ParCommandBuffer<FreightStationEnt>>();
    register_resource_noserialize::<ParCommandBuffer<CompanyEnt>>();
    register_resource_noserialize::<crate::economy::WalkingComponents>();
    register_resource_noserialize::<crate::economy::JobRosters>();
    register_resource_noserialize::<RoadThroughput>();
    register_resource_noserialize::<crate::souls::growth::GrowthCache>();
    // The registry is populated once before any simulation uses it.
    unsafe {
        (*std::ptr::addr_of_mut!(INIT_FUNCS)).push(InitFunc {
            f: Box::new(|sim| sim.insert(HouseholdClaims::from_world(sim.world()))),
        });
    }
    register_resource_noinit::<SimulationOptions, Bincode>("simoptions");

    register_resource_default::<ElectricityFlow, Bincode>("electricity_flow");
    register_resource_default::<CoverageGrid, Bincode>("coverage_grid");
    register_resource_default::<UtilityFlow, Bincode>("utility_flow");
    register_resource_default::<EnvGrid, Bincode>("env_grid");
    register_resource_default::<Happiness, Bincode>("happiness");
    register_resource_default::<Fires, Bincode>("fires");
    register_resource_default::<Market, Bincode>("market");
    register_resource_default::<EcoStats, Bincode>("ecostats");
    register_resource_default::<CityStats, Bincode>("citystats");
    register_resource_default::<MultiplayerState, Bincode>("multiplayer_state");
    register_resource_default::<RandomVehicles, Bincode>("random_vehicles");
    register_resource_default::<Map, Bincode>("map");
    register_resource_default::<TrainReservations, Bincode>("train_reservations");
    register_resource_noinit::<Government, Bincode>("government");
    register_resource_default::<crate::map::ZoneGrid, Bincode>("zone_grid");
    register_resource_default::<crate::souls::residence::Residences, Bincode>("residences");
    register_resource_default::<crate::economy::ZoneDemand, Bincode>("zone_demand");
    register_resource_default::<crate::economy::Budget, Bincode>("budget");
    register_resource_default::<ParkingManagement, Bincode>("pmanagement");
    register_resource_default::<BuildingInfos, Bincode>("binfos");
    register_resource::<GameTime, Bincode>("game_time", || GameTime::new(Tick(INITIAL_TICK)));
    register_transport_grid();
    register_resource_noinit::<RandProvider, Bincode>("randprovider");
    register_resource_default::<Dispatcher, Bincode>("dispatcher");
    register_resource_default::<Replay, crate::utils::replay::ReplayJson>("replay");
    register_resource_default::<Transit, Bincode>("transit");
    register_resource_default::<TripStats, Bincode>("trip_stats");
    register_resource_default::<TransitStats, Bincode>("transit_stats");
    register_resource_default::<ProgressionState, Bincode>("progression");
    register_resource_default::<CargoBook, Bincode>("cargo_book");
    register_resource_default::<DailyEconomyMetrics, Bincode>("daily_economy_metrics");
    register_lot_layout();
    register_road_frontage();
    register_cargo_progress();
}

// The layout lives on the map, which sizes lots as roads are built. A separate entry keeps the
// map encoding unchanged, and saves from before the setting load with the varied layout.
// Registered after "map" so loading overrides the decoded map's default.
fn register_lot_layout() {
    unsafe {
        SAVELOAD_FUNCS.push(SaveLoadFunc {
            name: "lot_layout",
            optional: true,
            save: Box::new(|sim| Bincode::encode(&sim.map().lot_layout).unwrap()),
            load: Box::new(|sim, data| {
                sim.map_mut().lot_layout = Bincode::decode(data)?;
                Ok(())
            }),
        });
    }
}

// Roads the importer left without lots. A separate entry keeps the map encoding unchanged,
// and saves from before it treat every street as frontage.
// Registered after "map" so loading overrides the decoded map's default.
fn register_road_frontage() {
    unsafe {
        SAVELOAD_FUNCS.push(SaveLoadFunc {
            name: "road_frontage",
            optional: true,
            save: Box::new(|sim| Bincode::encode(&sim.map().no_frontage).unwrap()),
            load: Box::new(|sim, data| {
                sim.map_mut().no_frontage = Bincode::decode(data)?;
                Ok(())
            }),
        });
    }
}

// Stall timers of loaded trucks. A separate entry keeps the cargo book encoding unchanged,
// and saves from before it restart the timers at load.
// Registered after "cargo_book" so loading overrides the decoded book's default.
fn register_cargo_progress() {
    unsafe {
        SAVELOAD_FUNCS.push(SaveLoadFunc {
            name: "cargo_progress",
            optional: true,
            save: Box::new(|sim| Bincode::encode(&sim.read::<CargoBook>().progress).unwrap()),
            load: Box::new(|sim, data| {
                sim.write::<CargoBook>().progress = Bincode::decode(data)?;
                Ok(())
            }),
        });
    }
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
            optional: false,
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
    /// Saves without this entry keep the value set by the init functions.
    pub optional: bool,
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

fn register_resource<T: 'static + Send + Sync + Serialize + DeserializeOwned, E: Encoder>(
    name: &'static str,
    initializer: impl Fn() -> T + 'static,
) {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(move |uiw| uiw.insert(initializer())),
        });
        register_resource_noinit::<T, E>(name);
    }
}

fn register_resource_noinit<T: 'static + Send + Sync + Serialize + DeserializeOwned, E: Encoder>(
    name: &'static str,
) {
    unsafe {
        SAVELOAD_FUNCS.push(SaveLoadFunc {
            name,
            optional: false,
            save: Box::new(move |uiworld| E::encode(&*uiworld.read::<T>()).unwrap()),
            load: Box::new(move |sim, data| {
                sim.insert(E::decode::<T>(data)?);
                Ok(())
            }),
        });
    }
}
