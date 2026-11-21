use crate::builders::{fill_houses, grant_money, place_company, seed_households};
use crate::ScenarioCtx;
use geom::vec2;
use prototypes::{try_prototype, GoodsCompanyID, Money};
use simulation::map::BuildingKind;
use simulation::world_command::WorldCommand;
use simulation::SimulationOptions;

pub const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 50,
    save_replay: false,
};

pub const MEDIAN_TICKS_PER_SEC: &str = "median_ticks_per_sec";
pub const MIN_TICKS_PER_SEC: f64 = 150.0;

pub struct PerfOptions {
    pub houses: usize,
    pub minimum_employment_percent: Option<usize>,
    pub setup_services_and_transit: Option<fn(&mut ScenarioCtx) -> Result<(), String>>,
}

const PERF_OPTIONS: PerfOptions = PerfOptions {
    houses: 5_000,
    minimum_employment_percent: None,
    setup_services_and_transit: None,
};

const PERF_10K_OPTIONS: PerfOptions = PerfOptions {
    houses: 10_000,
    minimum_employment_percent: Some(50),
    setup_services_and_transit: None,
};

pub fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    run_with_options(ctx, PERF_OPTIONS)
}

pub fn run_10k(ctx: &mut ScenarioCtx) -> Result<(), String> {
    run_with_options(ctx, PERF_10K_OPTIONS)
}

fn run_with_options(ctx: &mut ScenarioCtx, options: PerfOptions) -> Result<(), String> {
    ctx.submit(&[WorldCommand::MapLoadParis]);
    ctx.run_ticks(1);

    let company_count = match options.minimum_employment_percent {
        Some(percent) => setup_employers(ctx, options.houses, percent)?,
        None => setup_bakeries(ctx)?,
    };
    setup_houses(ctx, options.houses)?;
    if let Some(setup) = options.setup_services_and_transit {
        setup(ctx)?;
    }

    for (label, ticks) in [("setup", 0), ("warmup", 2_000), ("measured", 10_000)] {
        ctx.run_ticks(ticks);
        let houses = ctx
            .sim
            .map()
            .buildings()
            .values()
            .filter(|building| building.kind == BuildingKind::House)
            .count();
        let sample = if options.minimum_employment_percent.is_some() {
            ctx.sample_perf(label, houses)
        } else {
            ctx.sample(label)
        };
        if houses != options.houses || sample.companies != company_count {
            return Err(format!(
                "{label} had {houses} houses and {} companies, expected {} houses and {company_count} companies",
                sample.companies, options.houses
            ));
        }

        if let Some(percent) = options.minimum_employment_percent.filter(|_| ticks != 0) {
            let minimum_population = (options.houses * 95).div_ceil(100);
            if sample.humans < minimum_population {
                return Err(format!(
                    "{label} had {} residents, expected at least {minimum_population}",
                    sample.humans
                ));
            }
            let minimum_employed = (sample.humans.max(options.houses) * percent).div_ceil(100);
            let employed = sample.extra["employed"] as usize;
            if employed < minimum_employed {
                return Err(format!(
                    "{label} had {employed} employed residents, expected at least {minimum_employed}"
                ));
            }
        } else if sample.humans != options.houses {
            return Err(format!(
                "{label} had {} residents, expected {}",
                sample.humans, options.houses
            ));
        }
    }
    Ok(())
}

fn setup_bakeries(ctx: &mut ScenarioCtx) -> Result<usize, String> {
    const COMPANY_COUNT: usize = 20;
    let mut companies = 0;
    for i in 0..36 {
        let near = vec2(
            11_450.0 + (i % 6) as f32 * 220.0,
            7_450.0 + (i / 6) as f32 * 220.0,
        );
        if place_company(ctx, "bakery", near).is_some() {
            companies += 1;
            if companies == COMPANY_COUNT {
                break;
            }
        }
    }
    if companies != COMPANY_COUNT {
        return Err(format!("built {companies} of {COMPANY_COUNT} companies"));
    }
    Ok(companies)
}

fn setup_employers(ctx: &mut ScenarioCtx, houses: usize, percent: usize) -> Result<usize, String> {
    const EMPLOYER: &str = "clothes-store";
    let proto = try_prototype(GoodsCompanyID::new(EMPLOYER))
        .filter(|proto| proto.n_workers > 0)
        .ok_or_else(|| format!("missing employer with jobs: {EMPLOYER}"))?;
    let jobs = (houses * percent).div_ceil(100);
    let company_count = jobs.div_ceil(proto.n_workers as usize);
    grant_money(ctx, proto.price * company_count as i64);

    // Use the same lot order as fill_houses to spread jobs across the residential area.
    let mut sites: Vec<_> = ctx
        .sim
        .map()
        .lots()
        .values()
        .map(|lot| (lot.id, lot.shape.center()))
        .collect();
    sites.sort_unstable_by_key(|&(id, _)| id);

    let mut companies = 0;
    let stride = (houses / company_count).max(1);
    for (_, near) in sites.into_iter().step_by(stride) {
        if place_company(ctx, EMPLOYER, near).is_some() {
            companies += 1;
            if companies == company_count {
                return Ok(companies);
            }
        }
    }
    Err(format!("built {companies} of {company_count} employers"))
}

fn setup_houses(ctx: &mut ScenarioCtx, house_count: usize) -> Result<(), String> {
    // A house costs $100. Cover the default run and the 10,000-house comparison.
    grant_money(ctx, Money::new_bucks(1_000_000));
    let houses = fill_houses(ctx, house_count);
    if houses.len() != house_count {
        return Err(format!("built {} of {house_count} houses", houses.len()));
    }

    // P0 spawned one resident per house automatically. P1 gates immigration on
    // demand, so seed one resident in each house regardless of its capacity.
    seed_households(&mut ctx.sim, &houses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builders::grid_city, builtin::smoke};
    use simulation::{economy::Government, map::BuildingKind};

    #[test]
    fn perf_setup_houses_funds_and_seeds_the_requested_workload() {
        let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
        grid_city(&mut ctx, vec2(700.0, 700.0), 2, 2, 160.0);
        assert!(ctx.sim.map().lots().len() >= 4);
        ctx.sim.write::<Government>().money = Money::ZERO;

        setup_houses(&mut ctx, 4).unwrap();

        let houses = ctx
            .sim
            .map()
            .buildings()
            .values()
            .filter(|building| building.kind == BuildingKind::House)
            .count();
        assert_eq!(houses, 4);
        assert_eq!(ctx.sim.world().humans.len(), 4);
    }
}
