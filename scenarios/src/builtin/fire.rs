use geom::vec2;
use prototypes::{Tick, TICKS_PER_MINUTE};
use simulation::economy::Government;
use simulation::map::BuildingKind;
use simulation::souls::demolition::{demolition_quote, demolition_status, SiteRemnantCause};
use simulation::souls::events::{fire_incident, offer_fixture_fire, ServiceIncidents};
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SimulationOptions, SimulationReplayLoader, SnapshotRevision};

use crate::builders::{fill_houses, grid_city};
use crate::{Scenario, ScenarioCtx};

const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 1,
    save_replay: false,
    ..SimulationOptions::LEGACY
};

pub const RUINS: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_ruins",
        run: run_ruins,
    },
    OPTIONS,
);

pub const RESUME: (Scenario, SimulationOptions) = (
    Scenario {
        name: "p9_fire_resume",
        run: run_resume,
    },
    OPTIONS,
);

fn run_ruins(ctx: &mut ScenarioCtx) -> Result<(), String> {
    grid_city(ctx, vec2(150.0, 150.0), 2, 2, 160.0);
    let house = *fill_houses(ctx, 1).first().ok_or("missing fixture house")?;
    let center = ctx.sim.map().get(house).unwrap().obb.center();
    let lot_count = ctx.sim.map().lots().len();
    if !offer_fixture_fire(&ctx.sim, house) {
        return Err("fixture fire rejected".into());
    }
    ctx.run_ticks(85 * TICKS_PER_MINUTE + 1);
    let city = &ctx.sim;
    if city.map().get(house).map(|building| building.kind) != Some(BuildingKind::SiteRemnant)
        || demolition_status(city, house).map(|status| status.cause) != Some(SiteRemnantCause::Fire)
        || city.map().lots().len() != lot_count
        || city.map().ruin_land_value_penalty(center) <= 0.0
    {
        return Err(
            "an untreated incident must leave an occupied ruin and lower land value".into(),
        );
    }
    let quote = demolition_quote(city, house)
        .map_err(|e| format!("{e:?}"))?
        .quote;
    let command = WorldCommand::MapStartDemolition { site: house, quote };
    let balance = city.read::<Government>().money;
    ctx.sim.apply_commands([&command, &command]);
    if ctx.sim.read::<Government>().money != balance - quote.price {
        return Err("a repeated demolition command should charge once".into());
    }
    ctx.run_ticks(u64::from(quote.minutes) * TICKS_PER_MINUTE - 1);
    if ctx.sim.map().get(house).is_none() {
        return Err("site was cleared before paid work finished".into());
    }
    ctx.run_ticks(1);
    if ctx.sim.map().get(house).is_some()
        || ctx.sim.map().ruin_land_value_penalty(center) != 0.0
        || ctx.sim.map().lots().len() != lot_count + 1
    {
        return Err("paid clearance did not restore available land and frontage".into());
    }
    ctx.sample("cleared");
    Ok(())
}

fn run_resume(ctx: &mut ScenarioCtx) -> Result<(), String> {
    grid_city(ctx, vec2(150.0, 150.0), 2, 2, 160.0);
    let site = *fill_houses(ctx, 1).first().ok_or("missing fixture house")?;
    if !offer_fixture_fire(&ctx.sim, site)
        || ctx.sim.read::<ServiceIncidents>().pending_count() != 1
    {
        return Err("fixture must save a pending fire".into());
    }
    let mut pending = ScenarioCtx::from_sim(round_trip(&ctx.sim)?);
    pending.run_ticks(1);
    ctx.run_ticks(1);
    if pending.sim.hashes() != ctx.sim.hashes() || fire_incident(&ctx.sim, site).is_none() {
        return Err("pending offer changed across resume".into());
    }

    let quote = demolition_quote(&ctx.sim, site)
        .map_err(|e| format!("{e:?}"))?
        .quote;
    let commands = [WorldCommand::MapStartDemolition { site, quote }];
    let mut replayed = round_trip(&ctx.sim)?;
    let mut resumed = ScenarioCtx::from_sim(round_trip(&ctx.sim)?);
    let origin = Tick(ctx.sim.get_tick());
    let steps = (90 + u64::from(quote.minutes)) * TICKS_PER_MINUTE;
    let mut replay = SimulationReplayLoader::checkpoint_playback(
        origin,
        SnapshotRevision::new(1),
        steps,
        [(0, false, &commands[..])],
    )
    .map_err(|e| e.to_string())?;
    ctx.sim.apply_commands(&commands);
    resumed.sim.apply_commands(&commands);
    ctx.run_ticks(steps);
    resumed.run_ticks(steps);
    let mut schedule = Simulation::schedule();
    while replay.advance_tick(&mut replayed, &mut schedule) {}
    let expected = ctx.sim.hashes();
    if resumed.sim.hashes() != expected || replayed.hashes() != expected {
        return Err(
            "current checkpoint replay or resume changed the fire and clearance result".into(),
        );
    }
    if ctx.sim.map().get(site).is_some() {
        return Err("clearance never finished".into());
    }
    ctx.sample("resumed_and_replayed");
    Ok(())
}

fn round_trip(sim: &Simulation) -> Result<Simulation, String> {
    let bytes = sim
        .encode_slot(SnapshotRevision::new(1))
        .map_err(|e| e.to_string())?
        .into_snapshot_bytes();
    Simulation::load_compressed_snapshot(&bytes, 256 * 1024 * 1024).map_err(|e| e.to_string())
}
