use crate::{Scenario, ScenarioCtx};
use geom::{vec2, Vec2};
use simulation::map::building_parking::{parking_entrance_near, BuildingParkingKind, GarageKind};
use simulation::map::{
    BuildingID, Density, LanePatternBuilder, MapProject, ProjectFilter, ProjectKind, ZoneCell,
    ZoneKind,
};
use simulation::map_dynamic::Destination;
use simulation::souls::human::spawn_human;
use simulation::transportation::{Location, VehicleState};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{HumanID, Simulation, SimulationOptions, VehicleID};
use std::collections::BTreeMap;
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

pub const SCALE: (Scenario, SimulationOptions) = (
    Scenario {
        name: "pathpark_scale",
        run: run_scale,
    },
    SimulationOptions {
        preset: simulation::MapPreset::Flat,
        ..super::perf::OPTIONS
    },
);

fn setup(sim: &mut Simulation, round_trip: bool) -> Result<Vec<BuildingID>, String> {
    setup_at(sim, round_trip, Vec2::ZERO)
}

fn setup_at(
    sim: &mut Simulation,
    round_trip: bool,
    offset: Vec2,
) -> Result<Vec<BuildingID>, String> {
    let point = |x, y| (offset + vec2(x, y)).z(0.0);
    let ground = |x, y| MapProject {
        pos: point(x, y),
        kind: ProjectKind::Ground,
    };
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: ground(75.0, 100.0),
        to: ground(450.0, 100.0),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }]);
    if round_trip {
        let path = [
            point(450.0, 100.0),
            point(450.0, 280.0),
            point(75.0, 280.0),
            point(75.0, 100.0),
        ];
        for pair in path.windows(2) {
            let [from, to] = {
                let map = sim.map();
                std::array::from_fn(|index| map.project(pair[index], 0.0, ProjectFilter::ALL))
            };
            sim.apply_commands([&WorldCommand::MapMakeConnection {
                from,
                to,
                inter: None,
                pat: LanePatternBuilder::default().build(),
            }]);
            let outcomes = sim.read::<CommandOutcomes>();
            if outcomes.accepted.len() != 1 {
                return Err(format!(
                    "tagged return loop at {pair:?}: {:?}",
                    outcomes.last
                ));
            }
        }
    }
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
            .filter(|lot| {
                kind != ZoneKind::Commercial || {
                    let axes = lot.shape.axis().map(|axis| axis.mag());
                    let center = lot.shape.center() - offset;
                    axes[0] >= 29.5
                        && axes[1] >= 29.5
                        && (35.0..100.0).contains(&center.y)
                        && (175.0..310.0).contains(&center.x)
                }
            })
            .min_by(|a, b| {
                let area_order = if kind == ZoneKind::Commercial {
                    let area = |lot: &simulation::map::Lot| {
                        let [width, depth] = lot.shape.axis().map(|axis| axis.mag());
                        width * depth
                    };
                    area(b).total_cmp(&area(a))
                } else {
                    std::cmp::Ordering::Equal
                };
                area_order
                    .then_with(|| {
                        a.shape
                            .center()
                            .distance2(point + offset)
                            .total_cmp(&b.shape.center().distance2(point + offset))
                    })
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
    for (kind, preferred) in [
        (GarageKind::MultiStorey, point(150., 118.)),
        (GarageKind::Underground, point(364., 81.)),
    ] {
        let map = sim.map();
        let command = [
            preferred,
            point(374., 81.),
            point(400., 81.),
            point(300., 261.),
            point(180., 261.),
            point(300., 298.),
            point(180., 298.),
        ]
        .into_iter()
        .filter_map(|pick| parking_entrance_near(&map, pick))
        .map(|entrance| WorldCommand::MapPlaceParkingGarage { kind, entrance })
        .find(|command| command.validate_and_price(sim).is_ok())
        .ok_or_else(|| format!("no feasible {kind:?} garage entrance"))?;
        drop(map);
        sim.apply_commands([&command]);
        let outcome = sim
            .read::<CommandOutcomes>()
            .parking_garage(0)
            .ok_or("missing garage receipt")?;
        sites.push(outcome.map_err(|error| format!("{kind:?} placement: {error:?}"))?);
    }
    Ok(sites)
}

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let sites = setup(&mut ctx.sim, true)?;
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
    if !summaries
        .iter()
        .any(|parking| parking.kind == BuildingParkingKind::RearLot)
    {
        return Err("no shop rear lot was commissioned".into());
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
    let (driver, home, car) = ctx
        .sim
        .world()
        .humans
        .iter()
        .find_map(|(driver, person)| {
            let Location::Building(home) = person.location else {
                return None;
            };
            let parking = ctx.sim.building_parking(home)?;
            let car = person.router.personal_car?;
            (parking.kind == BuildingParkingKind::Driveway && parking.occupied != 0)
                .then_some((driver, home, car))
        })
        .ok_or("no tagged driver at an occupied driveway")?;
    let garage = *sites.get(4).ok_or("missing tagged public garage")?;
    ctx.sim.world_mut_unchecked().edit_human(driver, |person| {
        person.router.go_to(Destination::Building(garage));
    });
    let mut occupied = false;
    for _ in 0..4_000 {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(driver, |person| person.decision.wait = u8::MAX);
        ctx.run_ticks(1);
        let person = &ctx.sim.world().humans[driver];
        if person.location == Location::Building(garage) && ctx.sim.car_stored_inside_garage(car) {
            occupied = true;
            break;
        }
    }
    if !occupied
        || ctx
            .sim
            .building_parking(home)
            .is_none_or(|parking| parking.occupied != 0)
    {
        return Err(
            "tagged driver did not drive from the driveway to the public garage door".into(),
        );
    }
    ctx.sample("tagged_garage_arrival");
    ctx.submit(&[WorldCommand::SetBuildingParkingOpen {
        building: garage,
        open: false,
    }]);
    let mut drained = false;
    for _ in 0..10_000 {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(driver, |person| person.decision.wait = u8::MAX);
        ctx.run_ticks(1);
        let person = &ctx.sim.world().humans[driver];
        if person.location == Location::Building(home)
            && ctx
                .sim
                .building_parking(home)
                .is_some_and(|parking| parking.occupied == 1)
            && ctx
                .sim
                .building_parking(garage)
                .is_some_and(|parking| parking.occupied == 0)
            && person.router.personal_car == Some(car)
        {
            drained = true;
            break;
        }
    }
    if !drained {
        let person = &ctx.sim.world().humans[driver];
        return Err(format!(
            "tagged return stopped at {:?}, step {:?}, error {:?}, home {:?}, garage {:?}, car {:?}",
            person.location,
            person.router.cur_step(),
            person.router.last_error,
            ctx.sim.building_parking(home),
            ctx.sim.building_parking(garage),
            ctx.sim.world().vehicles.get(car).map(|actor| (&actor.vehicle.state, actor.trans)),
        ));
    }
    ctx.sample("property_live");
    Ok(())
}

struct ScaleTrip {
    driver: HumanID,
    car: VehicleID,
    home: BuildingID,
    destination: Destination,
}

fn run_scale(ctx: &mut ScenarioCtx) -> Result<(), String> {
    const DISTRICTS: usize = 128;
    const POPULATION: usize = 10_000;
    ctx.submit(&[WorldCommand::MapLoadParis]);
    ctx.run_ticks(1);
    super::perf::setup_employers(ctx, POPULATION, 50)?;
    let garage_funds = (GarageKind::MultiStorey.cost() + GarageKind::Underground.cost())
        * DISTRICTS as i64;
    crate::builders::grant_money(ctx, garage_funds + prototypes::Money::new_bucks(1_000_000));
    let mut districts = Vec::new();
    for index in 0..DISTRICTS {
        let offset = vec2((index % 16) as f32 * 550.0, (index / 16) as f32 * 400.0);
        districts.push(
            setup_at(&mut ctx.sim, true, offset)
                .map_err(|error| format!("parking district {index}: {error}"))?,
        );
    }
    let mut offers = Vec::new();
    for (index, sites) in districts.iter().enumerate() {
        for (driver, person) in ctx.sim.world().humans.iter() {
            let Some(car) = person.router.personal_car else {
                continue;
            };
            let home = person.home.house;
            if sites[..3].contains(&home) {
                let destination = if index % 2 == 0 {
                    Destination::Building(sites[4])
                } else {
                    Destination::Outside(ctx.sim.map().get(sites[3]).unwrap().door_pos)
                };
                offers.push(ScaleTrip {
                    driver,
                    car,
                    home,
                    destination,
                });
            }
        }
    }
    offers.sort_unstable_by_key(|trip| trip.driver);
    if offers.len() * 2 < 500 {
        return Err(format!(
            "scaled opening commissioned only {} tagged drivers",
            offers.len()
        ));
    }
    let initial_residents = ctx.sim.world().humans.len();
    super::perf::setup_houses(ctx, POPULATION - initial_residents)?;
    if ctx.sim.world().humans.len() != POPULATION {
        return Err("mixed-supply opening did not commission 10,000 residents".into());
    }
    let garages: Vec<_> = districts
        .iter()
        .flat_map(|sites| [sites[4], sites[5]])
        .collect();
    ctx.sample("setup");
    let mut workload = crate::runner::ParkingWorkload::new(&ctx.sim);
    for trip in &offers {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(trip.driver, |person| {
                person.router.go_to(trip.destination);
            });
    }
    let advance = |run: &mut ScenarioCtx, ticks, workload: &mut crate::runner::ParkingWorkload| {
        freeze_scale_drivers(run, &offers);
        run.run_ticks_observed(ticks, |sim| workload.observe(sim));
    };
    advance(ctx, 8, &mut workload);
    let holds = garages
        .iter()
        .filter_map(|&garage| ctx.sim.building_parking(garage))
        .map(|parking| usize::from(parking.held))
        .sum::<usize>();
    if holds < 2 {
        return Err("mixed-supply save cut did not contain simultaneous garage holds".into());
    }
    let mut resumed = ScenarioCtx::from_sim(ctx.round_tripped()?);
    freeze_scale_drivers(&mut resumed, &offers);
    resumed.run_ticks(128);
    advance(ctx, 128, &mut workload);
    if ctx.sim.hashes() != resumed.sim.hashes() {
        return Err("mixed-supply resumed parking holds diverged".into());
    }
    drop(resumed);
    for _ in 0..30 {
        advance(ctx, 200, &mut workload);
    }
    let mut curb = 0;
    let mut garage = 0;
    for trip in &offers {
        let human = &ctx.sim.world().humans[trip.driver];
        let vehicle = &ctx.sim.world().vehicles[trip.car];
        match trip.destination {
            Destination::Building(building)
                if human.location == Location::Building(building)
                    && ctx.sim.car_stored_inside_garage(trip.car) =>
            {
                garage += 1
            }
            Destination::Outside(door)
                if human.location == Location::Outside
                    && human.trans.pos.distance2(door) < 4.0
                    && matches!(vehicle.vehicle.state, VehicleState::Parked(_)) =>
            {
                curb += 1
            }
            _ => {}
        }
    }
    let mut extra = workload.finish(&ctx.sim);
    extra.extend(BTreeMap::from([
        ("tagged_offers".to_owned(), offers.len() as f64),
        ("tagged_garage_arrivals".to_owned(), f64::from(garage)),
        ("tagged_curb_arrivals".to_owned(), f64::from(curb)),
        ("simultaneous_garage_holds".to_owned(), holds as f64),
    ]));
    ctx.sample_perf_with_extra("measured", POPULATION, extra, true);
    if curb == 0 || garage == 0 || (curb + garage) * 100 < offers.len() as i32 * 90 {
        return Err(format!("mixed-supply departures reached {curb} curb and {garage} garage destinations from {} tagged offers", offers.len()));
    }
    let close: Vec<_> = garages
        .iter()
        .map(|&building| WorldCommand::SetBuildingParkingOpen {
            building,
            open: false,
        })
        .collect();
    ctx.submit(&close);
    for trip in &offers {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(trip.driver, |person| {
                person.router.go_to(Destination::Building(trip.home));
            });
    }
    for _ in 0..50 {
        freeze_scale_drivers(ctx, &offers);
        ctx.run_ticks(200);
    }
    let returned = offers
        .iter()
        .filter(|trip| {
            let human = &ctx.sim.world().humans[trip.driver];
            let vehicle = &ctx.sim.world().vehicles[trip.car];
            human.location == Location::Building(trip.home)
                && human.router.personal_car == Some(trip.car)
                && vehicle.vehicle.state.is_parked()
        })
        .count();
    if returned != offers.len()
        || garages.iter().any(|&garage| {
            ctx.sim
                .building_parking(garage)
                .is_none_or(|parking| parking.occupied + parking.held != 0)
        })
    {
        return Err(format!(
            "only {returned}/{} original cars returned after drain",
            offers.len()
        ));
    }
    ctx.sample_perf_with_extra(
        "drained",
        POPULATION,
        BTreeMap::from([("tagged_returns".to_owned(), returned as f64)]),
        true,
    );
    Ok(())
}

fn freeze_scale_drivers(ctx: &mut ScenarioCtx, offers: &[ScaleTrip]) {
    for trip in offers {
        ctx.sim
            .world_mut_unchecked()
            .edit_human(trip.driver, |person| {
                person.decision.wait = u8::MAX;
            });
    }
}

pub fn generate(root: &Path) -> Result<(), String> {
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    let mut ctx = ScenarioCtx::new(OPTIONS);
    setup(&mut ctx.sim, false)?;
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
