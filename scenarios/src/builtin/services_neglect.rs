use super::services_basic::{assess, reference_demand, run_to, OPTIONS, TICKS};
use crate::{Scenario, ScenarioCtx};
use simulation::SimulationOptions;

pub const SCENARIO: (Scenario, SimulationOptions) = (
    Scenario {
        name: "services_neglect",
        run,
    },
    OPTIONS,
);

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let (basic_tick, basic_demand) = reference_demand()?;
    ctx.verify_resumed(TICKS, |ctx, ticks| run_to(ctx, ticks, false))?;
    let metrics = assess(ctx)?;
    if metrics.tick != basic_tick
        || metrics.happiness > 45.0
        || metrics.residential_demand >= basic_demand
    {
        return Err(format!(
            "expected happiness of at most 45 and R demand below basic ({basic_demand:.2} at tick {basic_tick}). Observed {} at tick {}",
            metrics.summary(), metrics.tick,
        ));
    }
    metrics.sample(ctx, SCENARIO.0.name);
    ctx.samples
        .last_mut()
        .unwrap()
        .extra
        .insert("basic_residential_demand".into(), f64::from(basic_demand));
    Ok(())
}
