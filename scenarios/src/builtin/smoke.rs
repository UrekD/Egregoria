use crate::builders::{fill_houses, grid_city, place_company};
use crate::ScenarioCtx;
use geom::vec2;
use simulation::SimulationOptions;

pub const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 10,
    save_replay: false,
};

pub fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    run_for(ctx, 50_000)
}

/// Build and exercise the smoke city within the given total tick budget.
pub fn run_for(ctx: &mut ScenarioCtx, ticks: u64) -> Result<(), String> {
    let start = ctx.sim.get_tick();
    let origin = vec2(700.0, 700.0);
    let grid = grid_city(ctx, origin, 4, 4, 160.0);
    if grid.roads.len() != 24 {
        return Err(format!("smoke built {} of 24 grid roads", grid.roads.len()));
    }
    for (company, offset) in [
        ("bakery", vec2(80.0, -20.0)),
        ("supermarket", vec2(240.0, -50.0)),
    ] {
        place_company(ctx, company, origin + offset)
            .ok_or_else(|| format!("smoke could not place {company}"))?;
    }
    fill_houses(ctx, 48);

    let setup_ticks = ctx.sim.get_tick() - start;
    let remaining = ticks.checked_sub(setup_ticks).ok_or_else(|| {
        format!("smoke setup took {setup_ticks} ticks, exceeding its {ticks}-tick budget")
    })?;
    ctx.run_ticks(remaining);

    let sample = ctx.sample("final");
    if sample.humans == 0 {
        return Err("smoke has no humans".to_string());
    }
    if sample.extra["employed"] == 0.0 {
        return Err("smoke has no employed humans".to_string());
    }
    Ok(())
}
