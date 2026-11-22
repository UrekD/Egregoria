use super::services_basic::{reference_metrics, run_verified, OPTIONS};
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
    let basic = reference_metrics()?;
    let metrics = run_verified(ctx, false)?;
    // Job availability can offset the demand penalty from missing services.
    if metrics.tick != basic.tick
        || metrics.happiness > 45.0
        || metrics.residential_happiness_driver >= basic.residential_happiness_driver
    {
        return Err(format!(
            "expected happiness of at most 45 and R demand's happiness contribution below basic ({:.2} at tick {}). Observed {} at tick {}",
            basic.residential_happiness_driver, basic.tick, metrics.summary(), metrics.tick,
        ));
    }
    metrics.sample(ctx, SCENARIO.0.name);
    ctx.samples.last_mut().unwrap().extra.insert(
        "basic_residential_demand".into(),
        f64::from(basic.residential_demand),
    );
    Ok(())
}
