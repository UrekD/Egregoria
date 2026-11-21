use crate::{builtin::smoke, ScenarioCtx};

const RESUMED_TICKS: u64 = 20_000;

pub fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    ctx.verify_twice(smoke::run)?;
    verify_resumed(RESUMED_TICKS)?;
    println!(
        "determinism: resumed and uninterrupted runs matched after {RESUMED_TICKS} ticks (save after {} ticks)",
        RESUMED_TICKS / 2
    );
    Ok(())
}

pub fn verify_resumed(ticks: u64) -> Result<(), String> {
    ScenarioCtx::new(smoke::OPTIONS)
        .verify_resumed(ticks, smoke::run_for)
        .map_err(|err| format!("resumed trajectory: {err}"))
}
