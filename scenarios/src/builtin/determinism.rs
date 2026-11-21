use crate::{builtin::smoke, ScenarioCtx};

pub fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    ctx.verify_twice(smoke::run)
}
