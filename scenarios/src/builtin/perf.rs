use crate::builders::{fill_houses, place_company, seed_households};
use crate::ScenarioCtx;
use geom::vec2;
use simulation::world_command::WorldCommand;
use simulation::SimulationOptions;

pub const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 50,
    save_replay: false,
};

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
            if companies == 20 {
                break;
            }
        }
    }
    let houses = fill_houses(ctx, 5_000);
    seed_households(&mut ctx.sim, &houses)?;
    ctx.sample("setup");

    ctx.run_ticks(2_000);
    ctx.sample("warmup");
    ctx.run_ticks(10_000);
    ctx.sample("measured");
    Ok(())
}
