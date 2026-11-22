use prototypes::{GameTime, GoodsCompanyID, Money, Tick};
use scenarios::builtin::{determinism, smoke};
use scenarios::ScenarioCtx;
use simulation::economy::{Budget, BudgetCategory, Government};
use simulation::map::BuildingKind;
use simulation::world_command::WorldCommand;
use simulation::SimulationOptions;
use std::process::Command;

fn final_projection(ctx: &mut ScenarioCtx) -> u64 {
    let mut dump = Vec::new();
    ctx.run_with_hashes("test", &mut dump, |_| Ok(())).unwrap();
    let checkpoint: serde_json::Value = serde_json::from_slice(&dump).unwrap();
    checkpoint["projection"]
        .as_u64()
        .expect("missing projection hash")
}

#[test]
fn projection_ignores_an_unserialized_resource() {
    struct UnrelatedResource(u32);
    let mut without = ScenarioCtx::new(smoke::OPTIONS);
    let mut with = ScenarioCtx::new(smoke::OPTIONS);
    with.sim.insert(UnrelatedResource(42));
    smoke::run_for(&mut without, 500).unwrap();
    smoke::run_for(&mut with, 500).unwrap();

    assert_eq!(with.sim.read::<UnrelatedResource>().0, 42);
    assert_eq!(final_projection(&mut without), final_projection(&mut with));
}

#[test]
fn projection_changes_when_a_human_moves() {
    let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
    smoke::run_for(&mut ctx, 500).unwrap();

    let before = final_projection(&mut ctx);
    let human = ctx
        .sim
        .world_mut_unchecked()
        .humans
        .values_mut()
        .next()
        .unwrap();
    human.trans.pos.x += 0.02;
    assert_ne!(before, final_projection(&mut ctx));
}

#[test]
fn checkpoint_cli_advertises_paths() {
    let output = Command::new(env!("CARGO_BIN_EXE_scenarios"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let usage = String::from_utf8(output.stdout).unwrap();
    assert!(usage.contains("--hashes"), "{usage}");
    assert!(usage.contains("--check-projection"), "{usage}");
}

#[test]
fn hashes_cli_rejects_missing_or_option_like_paths() {
    for args in [
        &["smoke", "--hashes"][..],
        &["smoke", "--hashes", "--twice"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_scenarios"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains("--hashes requires an output path"),
            "{error}"
        );
    }
}

#[test]
fn hashes_cli_rejects_duplicate_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_scenarios"))
        .args(["smoke", "--hashes", "out.jsonl", "--hashes", "--help"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("unexpected argument \"--hashes\""),
        "{error}"
    );
}

#[test]
fn perf_10k_cli_is_opt_in() {
    let scenarios = env!("CARGO_BIN_EXE_scenarios");
    let help = Command::new(scenarios).arg("--help").output().unwrap();
    assert!(help.status.success());
    let usage = String::from_utf8(help.stdout).unwrap();
    assert!(usage.contains("perf_10k"), "{usage}");
    assert!(usage.contains("--gate"), "{usage}");

    let output = Command::new(scenarios)
        .args(["all", "--gate"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("--gate requires perf_10k"), "{error}");
}

#[test]
fn perf_10k_cli_rejects_option_like_json_paths() {
    let scenarios = env!("CARGO_BIN_EXE_scenarios");
    // The trailing argument keeps the benchmark from running if parsing regresses.
    let output = Command::new(scenarios)
        .args(["perf_10k", "--json", "--gate", "--help"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("--json requires an output path"), "{error}");
}

#[test]
fn perf_10k_cli_rejects_twice_with_gate() {
    let scenarios = env!("CARGO_BIN_EXE_scenarios");
    for (first, second) in [("--twice", "--gate"), ("--gate", "--twice")] {
        let output = Command::new(scenarios)
            .args(["perf_10k", first, second, "--help"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains("--twice cannot be used with --gate"),
            "{error}"
        );
    }
}

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
fn smoke_money_reconciles_with_ledger() {
    let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
    let start_tick = ctx.sim.get_tick();
    smoke::run(&mut ctx).unwrap();

    assert_eq!(ctx.sim.get_tick() - start_tick, 50_000);
    let final_money = ctx.sim.read::<Government>().money;
    let budget = ctx.sim.read::<Budget>();
    assert!(budget.total(BudgetCategory::Construction) < 0);
    let ledger: i128 = budget
        .days()
        .values()
        .flat_map(|day| day.values())
        .sum();
    assert_eq!(
        i128::from(Government::default().money.inner()) + ledger,
        i128::from(final_money.inner())
    );
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
fn smoke_resumed_trajectory_matches_continuous_run() {
    determinism::verify_resumed(2_000).unwrap();
}

#[test]
fn determinism_detects_a_difference_at_a_checkpoint() {
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        terrain_size: 0,
        save_replay: false,
    });
    let checkpoint_tick = ctx.sim.get_tick() + 10_000;
    let mut first = true;

    let error = ctx
        .verify_twice(|run| {
            let money = run.sim.read::<Government>().money;
            if first {
                run.sim.write::<Government>().money += Money::new_bucks(1);
            }
            run.run_ticks(10_000);
            run.sim.write::<Government>().money = money;
            run.run_ticks(1);
            first = false;
            Ok(())
        })
        .unwrap_err();

    assert!(
        error.contains(&format!("tick {checkpoint_tick}")),
        "{error}"
    );
    assert!(error.contains("government"), "{error}");
}

#[test]
fn determinism_captures_checkpoint_when_game_clock_jumps() {
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        terrain_size: 0,
        save_replay: false,
    });
    let mut first = true;

    let error = ctx
        .verify_twice(|run| {
            run.submit(&[WorldCommand::SetGameTime(GameTime::new(Tick(0)))]);
            run.run_ticks(9_999);
            assert_eq!(run.sim.get_tick(), 9_999);

            let money = run.sim.read::<Government>().money;
            if first {
                run.sim.write::<Government>().money += Money::new_bucks(1);
            }
            run.submit(&[WorldCommand::SetGameTime(GameTime::new(Tick(10_000)))]);
            run.run_ticks(1);
            assert_eq!(run.sim.get_tick(), 10_001);

            run.sim.write::<Government>().money = money;
            run.run_ticks(1);
            first = false;
            Ok(())
        })
        .unwrap_err();

    assert!(error.contains("tick 10001"), "{error}");
    assert!(error.contains("government"), "{error}");
}
