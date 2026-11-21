use scenarios::builtin::{growth_basic, perf, PERF_10K, SCENARIOS};
use scenarios::ScenarioCtx;
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::process::ExitCode;

fn usage() -> String {
    let names = SCENARIOS
        .iter()
        .map(|(scenario, _)| scenario.name)
        .chain([PERF_10K.0.name, "all"])
        .collect::<Vec<_>>()
        .join("|");
    format!("Usage: scenarios <{names}> [--json out.json] [--twice] [--gate (perf_10k only)]")
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let name = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .ok_or_else(usage)?;
    if name == "--help" || name == "-h" {
        println!("{}", usage());
        return Ok(());
    }
    let available = if name == PERF_10K.0.name {
        std::slice::from_ref(&PERF_10K)
    } else {
        SCENARIOS
    };
    if name != "all" && !available.iter().any(|(scenario, _)| scenario.name == name) {
        return Err(format!("unknown scenario {name:?}\n{}", usage()));
    }

    let mut json = None;
    let mut twice = false;
    let mut gate = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--twice") if !twice => twice = true,
            Some("--gate") if !gate => gate = true,
            Some("--json") if json.is_none() => {
                let path = args.next().ok_or("--json requires an output path")?;
                // Prototype initialization changes the process working directory.
                json = Some(std::path::absolute(Path::new(&path)).map_err(|err| err.to_string())?);
            }
            _ => return Err(format!("unexpected argument {arg:?}\n{}", usage())),
        }
    }
    if gate && name != PERF_10K.0.name {
        return Err("--gate requires perf_10k".to_string());
    }

    let mut results = BTreeMap::new();
    for (scenario, options) in available {
        if name != "all" && name != scenario.name {
            continue;
        }
        let mut ctx = ScenarioCtx::new(*options);
        let result = if twice && scenario.name == growth_basic::SCENARIO.0.name {
            growth_basic::verify_twice(&mut ctx)
        } else if twice && scenario.name != "determinism" {
            ctx.verify_twice(scenario.run)
        } else {
            (scenario.run)(&mut ctx)
        };
        result.map_err(|err| format!("{}: {err}", scenario.name))?;

        if twice || scenario.name == "determinism" {
            println!(
                "{}: both runs matched at every checkpoint and the final tick",
                scenario.name
            );
        }
        for sample in &ctx.samples {
            let employed = sample.extra.get("employed").copied().unwrap_or_default();
            println!(
                "{} [{}] tick={} humans={} employed={employed:.0} vehicles={} companies={} trains={} money_cents={} ticks/s={:.2}",
                scenario.name, sample.label, sample.tick, sample.humans, sample.vehicles,
                sample.companies, sample.trains, sample.money_cents, sample.ticks_per_sec,
            );
        }
        results.insert(scenario.name, ctx.samples);
    }

    if let Some(path) = json {
        let file = File::create(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        serde_json::to_writer_pretty(file, &results)
            .map_err(|err| format!("{}: {err}", path.display()))?;
    }

    if gate {
        let median = results[PERF_10K.0.name]
            .iter()
            .find(|sample| sample.label == "measured")
            .and_then(|sample| sample.extra.get(perf::MEDIAN_TICKS_PER_SEC))
            .ok_or("perf_10k: missing measured median throughput")?;
        if *median < perf::MIN_TICKS_PER_SEC {
            return Err(format!(
                "perf_10k: median throughput {median:.2} ticks/s is below {:.0} ticks/s",
                perf::MIN_TICKS_PER_SEC
            ));
        }
    }

    Ok(())
}
