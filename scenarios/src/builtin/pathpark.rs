use crate::{Scenario, ScenarioCtx};
use geom::{vec2, vec3};
use simulation::map::building_parking::{parking_entrance_near, BuildingParkingKind, GarageKind};
use simulation::map::{
    BuildingID, Density, LanePatternBuilder, MapProject, ProjectKind, ZoneCell, ZoneKind,
};
use simulation::souls::human::spawn_human;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{Simulation, SimulationOptions};
use std::path::Path;

const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 1,
    save_replay: false,
    ..SimulationOptions::LEGACY
};

pub const GROWABLES: (Scenario, SimulationOptions) = (
    Scenario {
        name: "pathpark_growables",
        run,
    },
    OPTIONS,
);

fn setup(sim: &mut Simulation) -> Result<Vec<BuildingID>, String> {
    let ground = |x, y| MapProject {
        pos: vec3(x, y, 0.0),
        kind: ProjectKind::Ground,
    };
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: ground(75.0, 100.0),
        to: ground(450.0, 100.0),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }]);
    let mut sites = Vec::new();
    for (point, kind, density) in [
        (vec2(238.0, 135.0), ZoneKind::Residential, Density::Low),
        (vec2(285.0, 135.0), ZoneKind::Residential, Density::Low),
        (vec2(330.0, 135.0), ZoneKind::Residential, Density::High),
        (vec2(265.0, 65.0), ZoneKind::Commercial, Density::Low),
    ] {
        let lot = sim
            .map()
            .lots()
            .values()
            .min_by(|a, b| {
                a.shape
                    .center()
                    .distance2(point)
                    .total_cmp(&b.shape.center().distance2(point))
                    .then(a.id.cmp(&b.id))
            })
            .ok_or("no parcel")?
            .id;
        let building = sim
            .place_zoned_growable(lot, ZoneCell { kind, density })
            .map_err(|error| format!("build {kind:?} {density:?} at {point:?}: {error:?}"))?
            .building;
        if kind == ZoneKind::Residential {
            spawn_human(sim, building).ok_or("no resident spawned")?;
        }
        sites.push(building);
    }
    for (kind, pick) in [
        (GarageKind::MultiStorey, vec3(150., 118., 0.)),
        (GarageKind::Underground, vec3(364., 81., 0.)),
    ] {
        let entrance = parking_entrance_near(&sim.map(), pick).ok_or("no garage entrance")?;
        sim.apply_commands([&WorldCommand::MapPlaceParkingGarage { kind, entrance }]);
        let outcome = sim
            .read::<CommandOutcomes>()
            .parking_garage(0)
            .ok_or("missing garage receipt")?;
        sites.push(outcome.map_err(|error| format!("{kind:?} placement: {error:?}"))?);
    }
    Ok(sites)
}

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let sites = setup(&mut ctx.sim)?;
    let summaries: Vec<_> = sites
        .iter()
        .filter_map(|&id| ctx.sim.building_parking(id))
        .collect();
    let house = summaries
        .iter()
        .find(|parking| parking.kind == BuildingParkingKind::Driveway)
        .ok_or("no physical driveway was commissioned")?;
    if house.occupied == 0 {
        return Err("no resident used their driveway".into());
    }
    if !summaries
        .iter()
        .any(|parking| parking.kind == BuildingParkingKind::IntegratedGarage)
    {
        return Err("no integrated garage was commissioned".into());
    }
    let capacity: u64 = summaries
        .iter()
        .map(|parking| u64::from(parking.capacity))
        .sum();
    let occupied: u64 = summaries
        .iter()
        .map(|parking| u64::from(parking.occupied))
        .sum();
    println!(
        "PathPark property spaces {occupied}/{capacity} occupied across {} facilities",
        summaries.len()
    );
    ctx.sample("property_opening");
    ctx.run_ticks(220);
    ctx.sample("property_live");
    Ok(())
}

pub fn generate(root: &Path) -> Result<(), String> {
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    let mut ctx = ScenarioCtx::new(OPTIONS);
    setup(&mut ctx.sim)?;
    super::demo_cities::publish(&ctx.sim, &root, "pathpark_growables", "PathPark Garages")
}

/// Compare new household parking on the same opening parcels of a published city.
pub fn audit(root: &Path, slot: &simulation::SlotId) -> Result<(), String> {
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    ScenarioCtx::initialize();
    let mut candidate =
        Simulation::load_from_slot(&root, slot).map_err(|error| error.to_string())?;
    let stock_facilities = candidate
        .map()
        .buildings()
        .keys()
        .filter(|&id| candidate.building_parking(id).is_some())
        .count();
    let lots: Vec<_> = candidate.map().lots().keys().collect();
    let zone = ZoneCell {
        kind: ZoneKind::Residential,
        density: Density::Low,
    };
    let mut houses = Vec::new();
    let mut built = Vec::new();
    for lot in lots.into_iter().take(512) {
        if let Ok(site) = candidate.place_zoned_growable(lot, zone) {
            built.push((lot, site.building));
            if candidate.building_parking(site.building).is_some() {
                houses.push(site.building);
                if houses.len() == 16 {
                    break;
                }
            }
        }
    }
    if houses.is_empty() {
        return Err(format!(
            "{slot}: no new driveway fit in the first 512 vacant parcels"
        ));
    }

    let mut control = Simulation::load_from_slot(&root, slot).map_err(|error| error.to_string())?;
    for (lot, expected) in built {
        let building = control
            .place_zoned_growable(lot, zone)
            .map_err(|error| format!("control growth on {lot:?}: {error:?}"))?
            .building;
        if building != expected {
            return Err("opening building IDs differ".into());
        }
    }
    for &building in &houses {
        control.apply_commands([&WorldCommand::SetBuildingParkingOpen {
            building,
            open: false,
        }]);
    }
    let observe = |mut sim: Simulation| -> Result<serde_json::Value, String> {
        let mut drivers = Vec::new();
        for &building in &houses {
            let human = spawn_human(&mut sim, building).ok_or("could not fill a new home")?;
            if let Some(car) = sim.world().humans[human].router.personal_car {
                drivers.push(car);
            }
        }
        let driveway_cars: u16 = houses
            .iter()
            .filter_map(|&building| sim.building_parking(building))
            .map(|parking| parking.occupied)
            .sum();
        let curb_cars = drivers.len() - usize::from(driveway_cars);
        let mut city = ScenarioCtx::from_sim(sim);
        city.run_ticks(500);
        let stranded = drivers
            .iter()
            .filter(|&&car| {
                city.sim.world().vehicles.get(car).is_none_or(|car| {
                    !car.vehicle.state.is_parked() && car.it.is_none_or_wait() && car.speed.0 < 0.05
                })
            })
            .count();
        Ok(serde_json::json!({
            "cars": drivers.len(),
            "opening_driveway_cars": driveway_cars,
            "opening_curb_cars": curb_cars,
            "cars_missing_or_without_route_after_500_ticks": stranded,
        }))
    };
    let with_parking = observe(candidate)?;
    let closed = observe(control)?;
    println!(
        "{}",
        serde_json::json!({
            "slot": slot.as_str(),
            "stock_facilities": stock_facilities,
            "new_houses": houses.len(),
            "property_parking": with_parking,
            "driveways_closed": closed,
        })
    );
    Ok(())
}
