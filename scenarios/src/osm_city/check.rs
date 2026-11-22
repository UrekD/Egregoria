use super::lesce::Facilities;
use super::{CitySample, HeadlessReport};
use crate::ScenarioCtx;
use prototypes::TICKS_PER_HOUR;
use simulation::economy::Government;
use simulation::map::BuildingKind;
use simulation::map_dynamic::{BuildingInfos, ElectricityFlow, UtilityFlow};
use simulation::souls::{freight_station::FreightTrainState, residence::Residences};
use simulation::Simulation;
use std::collections::BTreeMap;
use std::time::Instant;

pub(super) fn warm_opening(
    ctx: &mut ScenarioCtx,
    facilities: &Facilities,
) -> Result<CitySample, String> {
    let population = ctx.sim.world().humans.len();
    if !(2500..=3500).contains(&population) {
        return Err(format!(
            "mapped homes produced {population} residents; expected 2,500–3,500"
        ));
    }
    for _ in 0..80 {
        ctx.run_ticks(60);
        let sample = city_sample(&ctx.sim, 0, facilities);
        if sample.unserved_buildings == 0
            && staffing_ready(&ctx.sim, facilities, &sample)
            && sample.unemployment_pct < 25.0
        {
            return Ok(sample);
        }
    }
    let sample = city_sample(&ctx.sim, 0, facilities);
    Err(format!(
        "opening did not commission utilities and jobs: {sample:?}"
    ))
}

fn city_sample(sim: &Simulation, hours: u64, facilities: &Facilities) -> CitySample {
    let world = sim.world();
    let population = world.humans.len();
    let unemployed = world
        .humans
        .values()
        .filter(|human| human.work.is_none())
        .count();
    let map = sim.map();
    let infos = sim.read::<BuildingInfos>();
    let flow = sim.read::<UtilityFlow>();
    let electricity = sim.read::<ElectricityFlow>();
    let utility_totals = map
        .buildings()
        .values()
        .find(|building| building.kind == BuildingKind::House)
        .and_then(|building| flow.status(&map, &sim.read::<Residences>(), building.id))
        .and_then(|status| status.totals)
        .unwrap_or_default();
    let unserved_buildings = map
        .buildings()
        .values()
        .filter(|building| {
            building.id != facilities.external
                && (!(flow.has_water(building.id) && flow.has_sewage(building.id))
                    || map
                        .electricity
                        .net_id(building.id)
                        .is_none_or(|net| electricity.blackout(net)))
        })
        .count();
    let utility_workers = facilities
        .utilities
        .iter()
        .map(|&id| {
            let proto = map.buildings()[id]
                .kind
                .as_goods_company()
                .unwrap()
                .prototype();
            let workers = match infos.owner(id) {
                Some(simulation::SoulID::GoodsCompany(owner)) => world
                    .companies
                    .get(owner)
                    .map_or(0, |company| company.workers.0.len()),
                _ => 0,
            };
            (format!("{} {id:?}", proto.label), workers)
        })
        .collect();
    CitySample {
        hours,
        population,
        unemployment_pct: 100.0 * unemployed as f64 / population.max(1) as f64,
        homes: map
            .buildings()
            .values()
            .filter(|b| b.kind == BuildingKind::House)
            .count(),
        money_cents: sim.read::<Government>().money.cents(),
        unserved_buildings,
        water_demand: utility_totals.water_demand,
        water_production: utility_totals.water_production,
        sewage_capacity: utility_totals.sewage_capacity,
        utility_workers,
    }
}

fn staffing_ready(sim: &Simulation, facilities: &Facilities, sample: &CitySample) -> bool {
    let map = sim.map();
    facilities.utilities.iter().all(|&id| {
        let proto = map.buildings()[id]
            .kind
            .as_goods_company()
            .unwrap()
            .prototype();
        sample.utility_workers[&format!("{} {id:?}", proto.label)] * 2 >= proto.n_workers as usize
    })
}

pub(super) fn validate_opening(
    sim: &Simulation,
    facilities: &Facilities,
) -> Result<CitySample, String> {
    let opening = city_sample(sim, 0, facilities);
    if opening.unserved_buildings != 0
        || !staffing_ready(sim, facilities, &opening)
        || !(2500..=3500).contains(&opening.population)
    {
        return Err(format!(
            "reloaded opening lost population, utility service or staffing: {opening:?}"
        ));
    }
    Ok(opening)
}

pub(super) fn check_days(
    sim: Simulation,
    facilities: Facilities,
) -> Result<HeadlessReport, String> {
    let mut ctx = ScenarioCtx::from_sim(sim);
    let opening = validate_opening(&ctx.sim, &facilities)?;
    let mut samples = vec![opening];
    let mut money_per_day_cents = Vec::new();
    let mut freight_departed = false;
    let mut freight_completed = false;
    let get_freight = |sim: &Simulation| {
        sim.world()
            .freight_stations
            .values()
            .find(|station| station.f.building == facilities.terminal)
            .map(|station| {
                (
                    station
                        .f
                        .trains
                        .iter()
                        .any(|(_, state)| matches!(state, FreightTrainState::Moving)),
                    station.f.trains.is_empty(),
                )
            })
            .ok_or("freight station soul was lost")
    };
    ctx.sim
        .world_mut_unchecked()
        .freight_stations
        .values_mut()
        .find(|station| station.f.building == facilities.terminal)
        .ok_or("reloaded slot has no freight station soul")?
        .f
        .waiting_cargo = 100;

    let ticks = 72 * TICKS_PER_HOUR;
    let start = Instant::now();
    eprintln!("Checking three game days ({ticks} ticks) from the reloaded slot");
    for hour in 1..=72 {
        if freight_completed {
            ctx.run_ticks(TICKS_PER_HOUR);
        } else {
            for _ in 0..(TICKS_PER_HOUR / 300) {
                ctx.run_ticks(300);
                let (moving, empty) = get_freight(&ctx.sim)?;
                freight_departed |= moving;
                freight_completed |= freight_departed && empty;
            }
        }
        if hour % 4 == 0 {
            let sample = city_sample(&ctx.sim, hour, &facilities);
            eprintln!(
                "Headless hour {hour}: {} residents, {:.1}% unemployment, ${:.2}, {} unserved, water {:.0}/{:.0}, {:.0} ticks/s",
                sample.population, sample.unemployment_pct, sample.money_cents as f64 / 100.0,
                sample.unserved_buildings,
                sample.water_demand,
                sample.water_production,
                (hour * TICKS_PER_HOUR) as f64 / start.elapsed().as_secs_f64(),
            );
            if sample.unserved_buildings != 0 || (hour == 24 && sample.unemployment_pct >= 25.0) {
                return Err(format!(
                    "employment or utility gate failed at hour {hour}: {sample:?}"
                ));
            }
            if hour % 24 == 0 {
                money_per_day_cents
                    .push(sample.money_cents - samples[samples.len() - 6].money_cents);
            }
            samples.push(sample);
        }
    }

    let last = &samples[samples.len() - 1];
    if last.unserved_buildings > 0 || !freight_completed {
        return Err(format!(
            "three-day gate failed | day 3: {last:?}; freight completed: {freight_completed}"
        ));
    }
    let mut developed = BTreeMap::from([("residential".to_owned(), last.homes)]);
    for building in ctx.sim.map().buildings().values() {
        if let Some(company) = building
            .kind
            .as_goods_company()
            .and_then(|id| prototypes::try_prototype(prototypes::GrowableCompanyID::from(id)))
        {
            *developed
                .entry(format!("{:?}", company.zone_kind).to_lowercase())
                .or_default() += 1;
        }
    }
    let ticks_per_second = ticks as f64 / start.elapsed().as_secs_f64();
    Ok(HeadlessReport {
        ticks,
        ticks_per_second,
        samples,
        money_per_day_cents,
        developed,
        below_perf_baseline: ticks_per_second < crate::builtin::perf::MIN_TICKS_PER_SEC,
        freight_completed,
    })
}
