use super::lesce::{Facilities, OPENING_POPULATION};
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

const HOURS_PER_DAY: u64 = prototypes::HOURS_PER_DAY as u64;
const CHECK_HOURS: u64 = 3 * HOURS_PER_DAY;
const SAMPLE_HOURS: u64 = 4;
const MAX_UNEMPLOYMENT_PCT: f64 = 25.0;
const MIN_POPULATION_GROWTH_PCT: usize = 1;
const MIN_PEAK_POPULATION_RETENTION_PCT: usize = 97;
const MIN_PEAK_HOME_RETENTION_PCT: usize = 95;

pub(super) fn warm_opening(
    ctx: &mut ScenarioCtx,
    facilities: &Facilities,
) -> Result<CitySample, String> {
    let population = ctx.sim.world().humans.len();
    if !OPENING_POPULATION.contains(&population) {
        return Err(format!(
            "mapped homes produced {population} residents; expected {OPENING_POPULATION:?}"
        ));
    }
    for _ in 0..80 {
        ctx.run_ticks(60);
        let sample = city_sample(&ctx.sim, 0, facilities);
        if sample.unserved_buildings == 0
            && staffing_ready(&ctx.sim, facilities, &sample)
            && sample.unemployment_pct < MAX_UNEMPLOYMENT_PCT
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
        || opening.unemployment_pct >= MAX_UNEMPLOYMENT_PCT
        || !OPENING_POPULATION.contains(&opening.population)
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
    let mut freight_completed = false;
    let mut freight = FreightRoundTrip::default();
    let station = ctx
        .sim
        .world()
        .freight_stations
        .iter()
        .find_map(|(id, station)| (station.f.building == facilities.terminal).then_some(id))
        .ok_or("reloaded slot has no freight station soul")?;
    let train = ctx
        .sim
        .world()
        .trains
        .keys()
        .next()
        .ok_or("reloaded slot has no freight train")?;
    ctx.sim.world_mut_unchecked().freight_stations[station]
        .f
        .waiting_cargo = 100;

    let ticks = CHECK_HOURS * TICKS_PER_HOUR;
    let start = Instant::now();
    eprintln!("Checking three game days ({ticks} ticks) from the reloaded slot");
    for hour in 1..=CHECK_HOURS {
        let mut remaining = TICKS_PER_HOUR;
        while !freight_completed && remaining > 0 {
            ctx.run_ticks(1);
            remaining -= 1;
            let world = ctx.sim.world();
            let terminal = world
                .freight_stations
                .get(station)
                .ok_or("freight station soul was lost")?;
            if !world.trains.contains_key(train) {
                return Err("freight train was lost before completing its round trip".into());
            }
            let state = terminal
                .f
                .trains
                .iter()
                .find_map(|&(id, state)| (id == train).then_some(state));
            freight_completed = freight.observe(state);
            if freight_completed {
                let elapsed = hour * TICKS_PER_HOUR - remaining;
                eprintln!("Headless freight round trip completed after {elapsed} ticks");
            }
        }
        ctx.run_ticks(remaining);
        if hour % SAMPLE_HOURS == 0 {
            let sample = city_sample(&ctx.sim, hour, &facilities);
            eprintln!(
                "Headless hour {hour}: {} residents, {} homes, {:.1}% unemployment, ${:.2}, {} unserved, water {:.0}/{:.0}, {:.0} ticks/s",
                sample.population, sample.homes, sample.unemployment_pct, sample.money_cents as f64 / 100.0,
                sample.unserved_buildings,
                sample.water_demand,
                sample.water_production,
                (hour * TICKS_PER_HOUR) as f64 / start.elapsed().as_secs_f64(),
            );
            if sample.unserved_buildings != 0
                || (hour == HOURS_PER_DAY && sample.unemployment_pct >= MAX_UNEMPLOYMENT_PCT)
            {
                return Err(format!(
                    "employment or utility gate failed at hour {hour}: {sample:?}"
                ));
            }
            if hour % HOURS_PER_DAY == 0 {
                let previous_day = samples.len() - (HOURS_PER_DAY / SAMPLE_HOURS) as usize;
                money_per_day_cents.push(sample.money_cents - samples[previous_day].money_cents);
            }
            samples.push(sample);
        }
    }

    if !freight_completed {
        return Err("three-day gate failed: freight round trip did not complete".into());
    }
    validate_growth(&samples)?;
    let last = samples.last().unwrap();
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

#[derive(Default)]
struct FreightRoundTrip {
    loaded: bool,
    departed: bool,
}

impl FreightRoundTrip {
    fn observe(&mut self, state: Option<FreightTrainState>) -> bool {
        use FreightTrainState::{Arriving, Loading, Moving};

        if self.departed {
            // The station can assign the same train to its next inbound leg on the
            // tick it finishes the outbound leg.
            return matches!(state, None | Some(Arriving));
        }
        match state {
            Some(Loading) => self.loaded = true,
            Some(Moving) => self.departed = self.loaded,
            _ => {}
        }
        false
    }
}

fn validate_growth(samples: &[CitySample]) -> Result<(), String> {
    let opening = &samples[0];
    let last = samples.last().unwrap();
    let peak_population = samples
        .iter()
        .map(|sample| sample.population)
        .max()
        .unwrap();
    let peak_homes = samples.iter().map(|sample| sample.homes).max().unwrap();
    if last.population * 100 < opening.population * (100 + MIN_POPULATION_GROWTH_PCT)
        || last.population * 100 < peak_population * MIN_PEAK_POPULATION_RETENTION_PCT
        || last.homes * 100 < peak_homes * MIN_PEAK_HOME_RETENTION_PCT
    {
        return Err(format!(
            "three-day growth gate failed: population {} opening, {peak_population} peak, {} final; homes {} opening, {peak_homes} peak, {} final",
            opening.population, last.population, opening.homes, last.homes,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use FreightTrainState::{Arriving, Loading, Moving};

    #[test]
    fn osm_freight_round_trip_can_finish_with_an_immediate_redispatch() {
        let mut freight = FreightRoundTrip::default();
        // The train was already outbound when the city opened.
        assert!(!freight.observe(Some(Moving)));
        assert!(!freight.observe(Some(Arriving)));
        assert!(!freight.observe(Some(Loading)));
        assert!(!freight.observe(Some(Moving)));
        assert!(freight.observe(Some(Arriving)));
    }
}
