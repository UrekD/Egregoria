use crate::builders::{fill_houses, grid_city, place_company, seed_households};
use crate::ScenarioCtx;
use geom::vec2;
use simulation::map_dynamic::BuildingInfos;
use simulation::{SimulationOptions, SoulID};

pub const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 10,
    save_replay: false,
    ..SimulationOptions::LEGACY
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
    let mut companies = Vec::new();
    for (company, offset) in [
        ("bakery", vec2(80.0, -20.0)),
        ("supermarket", vec2(240.0, -50.0)),
    ] {
        let building = place_company(ctx, company, origin + offset)
            .ok_or_else(|| format!("smoke could not place {company}"))?;
        companies.push((company, building));
    }
    let houses = fill_houses(ctx, 48);
    if houses.len() < 20 {
        return Err(format!(
            "smoke built {} houses, expected at least 20",
            houses.len()
        ));
    }
    seed_households(&mut ctx.sim, &houses)?;

    let setup_ticks = ctx.sim.get_tick() - start;
    let remaining = ticks.checked_sub(setup_ticks).ok_or_else(|| {
        format!("smoke setup took {setup_ticks} ticks, exceeding its {ticks}-tick budget")
    })?;
    ctx.run_ticks(remaining);

    let sample = ctx.sample("final");
    let world = ctx.sim.world();
    let infos = ctx.sim.read::<BuildingInfos>();
    let map = ctx.sim.map();
    for house in houses {
        if !matches!(infos.owner(house), Some(SoulID::Human(id)) if world.humans.contains_key(id)) {
            return Err(format!("smoke house {house:?} has no live human owner"));
        }
    }
    for (company, building) in companies {
        if !map.buildings().contains_key(building) {
            return Err(format!("smoke {company} building {building:?} is missing"));
        }
        if !matches!(infos.owner(building), Some(SoulID::GoodsCompany(id)) if world.companies.contains_key(id))
        {
            return Err(format!(
                "smoke {company} building {building:?} has no live company owner"
            ));
        }
    }
    if sample.extra["employed"] == 0.0 {
        return Err("smoke has no employed humans".to_string());
    }
    Ok(())
}
