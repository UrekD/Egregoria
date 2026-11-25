use super::water_fixtures::{
    add_bank_roads, install_water, remove, station_at, Bodies, BANK, COAST, DOWNSTREAM, FAR_INLAND,
    LAKE, SEAM_STAMP, UPSTREAM,
};
use crate::{Scenario, ScenarioCtx};
use geom::{Ray3, Vec3};
use simulation::economy::balance::DEMAND_INTERVAL_TICKS;
use simulation::map::{
    Density, RiverBank, TerrainChunkID, WaterBodyId, WaterKind, ZoneGrid, ZoneKind,
};
use simulation::map_dynamic::{shoreline_value_bonus, UtilityFlow};
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand};
use simulation::{MapPreset, Simulation, SimulationOptions};

pub const SCENARIO: (Scenario, SimulationOptions) = (
    Scenario {
        name: "water_basic",
        run,
    },
    SimulationOptions {
        preset: MapPreset::Flat,
        terrain_size: 16,
        ..SimulationOptions::LEGACY
    },
);

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let tick = ctx.sim.get_tick();
    let bodies = install_water(&mut ctx.sim)?;
    if ctx.sim.get_tick() != tick || !ctx.sim.read::<UtilityFlow>().water_pending(&ctx.sim.map()) {
        return Err(
            "paused water edits must leave the clock unchanged and utility sampling pending".into(),
        );
    }
    check_water(&ctx.sim, &bodies)?;
    add_bank_roads(&mut ctx.sim)?;
    check_bank_paint(&mut ctx.sim)?;

    let before = ctx.sim.hashes();
    ctx.sim.apply_commands([&station_at(LAKE)]);
    let after = ctx.sim.hashes();
    let outcomes = ctx.sim.read::<CommandOutcomes>();
    if outcomes.rejections_for(0) != [("MapBuildSpecialBuilding", Rejection::WetFootprint)] {
        return Err(format!("wet building placement: {:?}", outcomes.last));
    }
    drop(outcomes);
    // Legacy construction commands record the attempt before validating it.
    if before
        .iter()
        .any(|(key, hash)| key != "replay" && after.get(key) != Some(hash))
    {
        return Err("wet building placement changed gameplay state".into());
    }
    ctx.sample("wet");

    ctx.run_ticks(DEMAND_INTERVAL_TICKS + 1);
    if ctx.sim.read::<UtilityFlow>().water_pending(&ctx.sim.map()) {
        return Err("utility refresh did not consume the water revision".into());
    }
    check_water(&ctx.sim, &bodies)?;
    ctx.sample("refreshed");

    // Own the post-reload commands so the second half is exercised by both runs.
    ctx.reload()?;
    check_water(&ctx.sim, &bodies)?;
    remove(&mut ctx.sim, bodies.lake)?;
    {
        let map = ctx.sim.map();
        let env = map.environment();
        if env.is_water(LAKE)
            || env.height(LAKE) != Some(0.0)
            || !env.base_height(LAKE).is_some_and(|height| height < 0.0)
            || env.water_body_kind(COAST) != Some(WaterKind::Sea)
            || env.water_at(UPSTREAM).map(|sample| sample.body) != Some(bodies.river)
            || !env.strict_water_siting()
        {
            return Err(
                "draining must expose the negative bed and preserve sea, river and strict siting"
                    .into(),
            );
        }
    }
    ctx.run_ticks(1);
    ctx.sample("drained");
    Ok(())
}

pub(super) fn check_water(sim: &Simulation, bodies: &Bodies) -> Result<(), String> {
    let map = sim.map();
    let env = map.environment();
    let sample = |point, kind, body, level| {
        env.water_at(point)
            .filter(|sample| {
                sample.kind == kind
                    && sample.body == body
                    && sample.surface_z == level
                    && sample.depth_m >= 0.25
            })
            .ok_or_else(|| format!("expected {kind:?} {body:?} at {point:?} with level {level}"))
    };
    let sea = sample(COAST, WaterKind::Sea, WaterBodyId::SEA, 0.0)?;
    let upstream = sample(UPSTREAM, WaterKind::River, bodies.river, 0.0)?;
    let downstream = sample(DOWNSTREAM, WaterKind::River, bodies.river, 0.0)?;
    let lake = sample(LAKE, WaterKind::Lake, bodies.lake, 1.0)?;
    let reach = upstream
        .reach_m
        .ok_or("river sample is missing its reach")?;
    if downstream
        .reach_m
        .is_none_or(|downstream| downstream <= reach)
        || sea.reach_m.is_some()
        || lake.reach_m.is_some()
    {
        return Err("reach must increase downstream and exist only on rivers".into());
    }

    let left = env.river_bank_near(bodies.river, reach, RiverBank::Left, 24);
    let right = env.river_bank_near(bodies.river, reach, RiverBank::Right, 24);
    if !matches!((left, right), (Some(a), Some(b))
        if a.point.y > UPSTREAM.y && b.point.y < UPSTREAM.y
            && a.dry_tile != b.dry_tile
            && !env.is_water(a.dry_tile.center()) && !env.is_water(b.dry_tile.center()))
    {
        return Err("river banks do not match the eastward reach".into());
    }

    for x in [1, 2] {
        let river_in_chunk = env
            .water_tiles(TerrainChunkID::new_i16(x, 1))
            .any(|tile| tile.sample.body == bodies.river);
        if !river_in_chunk {
            return Err(format!("river does not cross chunk ({x}, 1)"));
        }
    }
    let ray = Ray3::new(LAKE.z(32.0), -Vec3::Z);
    if env
        .raycast_water(ray, 64.0)
        .is_none_or(|hit| hit.sample.body != bodies.lake || hit.point.z != 1.0)
    {
        return Err("lake cannot be picked at its surface".into());
    }
    if env.is_water(BANK)
        || env.is_water(FAR_INLAND)
        || shoreline_value_bonus(&map, BANK) <= shoreline_value_bonus(&map, FAR_INLAND)
    {
        return Err("shoreline value must fall off across dry land".into());
    }
    Ok(())
}

fn check_bank_paint(sim: &mut Simulation) -> Result<(), String> {
    let kind = Some(ZoneKind::Residential);
    let preview = ZoneGrid::eligible_paint_cells(&sim.map(), &SEAM_STAMP, kind, Density::Low)
        .map_err(|error| format!("zone preview: {error:?}"))?;
    let all = ZoneGrid::eligible_paint_cells(&sim.map(), &SEAM_STAMP, None, Density::Low)
        .map_err(|error| format!("erase preview: {error:?}"))?;
    if preview.cells().is_empty() || preview.cells().len() >= all.cells().len() {
        return Err("seam stamp must contain both wet and dry cells".into());
    }
    sim.apply_commands([&WorldCommand::ZonePaint {
        area: SEAM_STAMP,
        kind,
        density: Density::Low,
    }]);
    let painted: Vec<_> = sim.read::<ZoneGrid>().cells().keys().copied().collect();
    if sim.read::<CommandOutcomes>().accepted != [0] || painted != preview.cells() {
        return Err("zone paint must commit the previewed dry subset".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn p6_water_scenario_is_registered() {
        assert_eq!(
            super::super::SCENARIOS
                .iter()
                .filter(|(scenario, _)| scenario.name == "water_basic")
                .count(),
            1
        );
    }
}
