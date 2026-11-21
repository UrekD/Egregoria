use crate::builders::{fill_houses, grant_money, place_company, seed_households};
use crate::ScenarioCtx;
use geom::vec2;
use prototypes::Money;
use simulation::world_command::WorldCommand;
use simulation::SimulationOptions;

pub const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 50,
    save_replay: false,
};

const HOUSE_COUNT: usize = 5_000;
const COMPANY_COUNT: usize = 20;

pub fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    ctx.submit(&[WorldCommand::MapLoadParis]);
    ctx.run_ticks(1);

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
    setup_houses(ctx, HOUSE_COUNT)?;

    for (label, ticks) in [("setup", 0), ("warmup", 2_000), ("measured", 10_000)] {
        ctx.run_ticks(ticks);
        let sample = ctx.sample(label);
        if sample.humans != HOUSE_COUNT || sample.companies != COMPANY_COUNT {
            return Err(format!(
                "{label} had {} residents and {} companies, expected {HOUSE_COUNT} residents and {COMPANY_COUNT} companies",
                sample.humans, sample.companies
            ));
        }
    }
    Ok(())
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
