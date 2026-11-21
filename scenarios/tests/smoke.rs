use prototypes::{GoodsCompanyID, Money};
use scenarios::builtin::smoke;
use scenarios::ScenarioCtx;
use simulation::economy::Government;
use simulation::map::BuildingKind;
use simulation::SimulationOptions;

#[test]
fn smoke_builds_a_working_city() {
    let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
    let start_tick = ctx.sim.get_tick();

    smoke::run_for(&mut ctx, 5_000).unwrap();

    assert_eq!(ctx.sim.get_tick() - start_tick, 5_000);
    let world = ctx.sim.world();
    assert!(!world.humans.is_empty());
    assert!(world.humans.values().any(|human| human.work.is_some()));
    assert_eq!(world.companies.len(), 2);

    let map = ctx.sim.map();
    for name in ["bakery", "supermarket"] {
        let kind = BuildingKind::GoodsCompany(GoodsCompanyID::new(name));
        assert!(map
            .buildings()
            .values()
            .any(|building| building.kind == kind));
    }
}

#[test]
fn smoke_is_deterministic() {
    let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
    let mut final_ticks = Vec::new();

    ctx.verify_twice(|run| {
        let start_tick = run.sim.get_tick();
        smoke::run_for(run, 5_000)?;
        final_ticks.push(run.sim.get_tick() - start_tick);
        Ok(())
    })
    .unwrap();

    assert_eq!(final_ticks, [5_000, 5_000]);
}

#[test]
fn determinism_detects_a_difference_at_a_checkpoint() {
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        terrain_size: 0,
        save_replay: false,
    });
    let mut first = true;

    let error = ctx
        .verify_twice(|run| {
            let money = run.sim.read::<Government>().money;
            if first {
                run.sim.write::<Government>().money += Money::new_bucks(1);
            }
            run.run_ticks(10_000 - run.sim.get_tick());
            run.sim.write::<Government>().money = money;
            run.run_ticks(1);
            first = false;
            Ok(())
        })
        .unwrap_err();

    assert!(error.contains("tick 10000"), "{error}");
    assert!(error.contains("government"), "{error}");
}
