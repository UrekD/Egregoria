use scenarios::{builtin::SCENARIOS, ScenarioCtx};

fn run_fixture(name: &str) {
    let (scenario, options) = SCENARIOS
        .iter()
        .find(|(scenario, _)| scenario.name == name)
        .expect("registered P9 scenario");
    (scenario.run)(&mut ScenarioCtx::new(*options)).unwrap();
}

#[test]
fn p9_resume_exercises_active_response_and_clearance_checkpoints() {
    run_fixture("p9_fire_resume");
}

#[test]
fn p9_repair_restores_the_directed_response_circuit() {
    run_fixture("p9_fire_repair");
}
