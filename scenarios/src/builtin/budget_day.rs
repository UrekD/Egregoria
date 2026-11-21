use crate::builtin::smoke;
use crate::{Scenario, ScenarioCtx};
use simulation::economy::{Budget, BudgetCategory, Government, TICKS_PER_BUDGET_DAY};
use simulation::SimulationOptions;

pub const SCENARIO: (Scenario, SimulationOptions) = (
    Scenario {
        name: "budget_day",
        run,
    },
    smoke::OPTIONS,
);

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let opening = i128::from(ctx.sim.read::<Government>().money.inner());
    let construction_before = ctx.sim.read::<Budget>().total(BudgetCategory::Construction);
    smoke::run_for(ctx, TICKS_PER_BUDGET_DAY)?;
    let closing = i128::from(ctx.sim.read::<Government>().money.inner());
    let construction =
        ctx.sim.read::<Budget>().total(BudgetCategory::Construction) - construction_before;
    let operating_balance = closing - opening - construction;
    if operating_balance < 0 {
        return Err(format!(
            "daily operating balance was {operating_balance} in Money units"
        ));
    }
    ctx.samples.last_mut().unwrap().extra.insert(
        "operating_balance_cents".to_string(),
        operating_balance as f64 / 100.0,
    );
    Ok(())
}
