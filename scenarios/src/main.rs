use scenarios::builtin::{demo_cities, growth_basic, perf, PERF_10K, SCENARIOS};
use scenarios::runner::{PerfTimings, ProjectionCheckpoint, MEDIAN_TICKS_PER_SEC, P99_MS_PER_TICK};
use scenarios::{MetricSample, ScenarioCtx};
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Serialize)]
struct JsonSample {
    #[serde(flatten)]
    metrics: MetricSample,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    timings: Option<PerfTimings>,
}

fn usage() -> String {
    let names = SCENARIOS
        .iter()
        .map(|(scenario, _)| scenario.name)
        .chain([PERF_10K.0.name, "all"])
        .collect::<Vec<_>>()
        .join("|");
    format!("Usage: scenarios <{names}> [--json out.json] [--hashes out.jsonl] [--check-projection baseline.json] [--twice | --gate (perf_10k only)]\n       scenarios demo-cities --save-root <dir>\n       scenarios osm-city --input <overpass.json> --save-root <dir> [--replace] [--check]")
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

fn path_arg(
    args: &mut impl Iterator<Item = OsString>,
    flag: &str,
    kind: &str,
) -> Result<PathBuf, String> {
    let path = args
        .next()
        .filter(|path| !path.as_encoded_bytes().starts_with(b"--"))
        .ok_or_else(|| format!("{flag} requires {kind}"))?;
    // Prototype initialization changes the process working directory.
    std::path::absolute(Path::new(&path)).map_err(|err| err.to_string())
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
    if name == "osm-city" {
        let mut input = None;
        let mut root = None;
        let mut replace = false;
        let mut check = false;
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--input") if input.is_none() => {
                    input = Some(path_arg(&mut args, "--input", "an Overpass JSON path")?)
                }
                Some("--save-root") if root.is_none() => {
                    root = Some(path_arg(&mut args, "--save-root", "a directory")?)
                }
                Some("--replace") if !replace => replace = true,
                Some("--check") if !check => check = true,
                _ => return Err(format!("unexpected osm-city argument {arg:?}")),
            }
        }
        let report = scenarios::osm_city::generate_with_options(
            &input.ok_or("osm-city requires --input")?,
            &root.ok_or("osm-city requires --save-root")?,
            replace,
            check,
        )?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if name == "demo-cities" {
        if args.next().as_deref() != Some(std::ffi::OsStr::new("--save-root")) {
            return Err("usage: scenarios demo-cities --save-root <dir>".into());
        }
        let root = path_arg(&mut args, "--save-root", "a directory")?;
        if let Some(arg) = args.next() {
            return Err(format!("unexpected demo-cities argument {arg:?}"));
        }
        return demo_cities::generate(&root);
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
    let mut hashes = None;
    let mut projection = None;
    let mut twice = false;
    let mut gate = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--twice") if !twice => twice = true,
            Some("--gate") if !gate => gate = true,
            Some("--json") if json.is_none() => {
                json = Some(path_arg(&mut args, "--json", "an output path")?)
            }
            Some("--hashes") if hashes.is_none() => {
                hashes = Some(path_arg(&mut args, "--hashes", "an output path")?)
            }
            Some("--check-projection") if projection.is_none() => {
                projection = Some(path_arg(
                    &mut args,
                    "--check-projection",
                    "a baseline path",
                )?)
            }
            _ => return Err(format!("unexpected argument {arg:?}\n{}", usage())),
        }
        if twice && gate {
            return Err("--twice cannot be used with --gate".to_string());
        }
    }
    if gate && name != PERF_10K.0.name {
        return Err("--gate requires perf_10k".to_string());
    }
    if hashes.is_some() && hashes == json {
        return Err("--hashes and --json require different output paths".to_string());
    }
    if projection.is_some() && (projection == hashes || projection == json) {
        return Err(
            "--check-projection requires a path distinct from the output paths".to_string(),
        );
    }

    let baseline: Option<BTreeMap<String, Vec<ProjectionCheckpoint>>> = projection
        .as_ref()
        .map(|path| {
            let file = File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
            serde_json::from_reader(BufReader::new(file))
                .map_err(|err| format!("{}: {err}", path.display()))
        })
        .transpose()?;
    let mut hash_output = hashes
        .as_ref()
        .map(|path| {
            File::create(path)
                .map(BufWriter::new)
                .map_err(|err| format!("{}: {err}", path.display()))
        })
        .transpose()?;
    let mut results = BTreeMap::new();
    for (scenario, options) in available {
        if name != "all" && name != scenario.name {
            continue;
        }
        let expected = match &baseline {
            Some(baseline) => Some(baseline.get(scenario.name).map(Vec::as_slice).ok_or_else(
                || {
                    format!(
                        "projection baseline has no checkpoints for {}",
                        scenario.name
                    )
                },
            )?),
            None => None,
        };
        let mut ctx = ScenarioCtx::new(*options);
        if json.is_some() && (scenario.name == "perf" || scenario.name == PERF_10K.0.name) {
            ctx.enable_perf_timings();
        }
        let run = |ctx: &mut ScenarioCtx| {
            if twice && scenario.name == growth_basic::SCENARIO.0.name {
                growth_basic::verify_twice(ctx)
            } else if twice && scenario.name != "determinism" {
                ctx.verify_twice(scenario.run)
            } else {
                (scenario.run)(ctx)
            }
        };
        let result = if hash_output.is_some() || expected.is_some() {
            let out = hash_output.as_mut().map(|out| out as &mut dyn Write);
            ctx.run_with_checkpoints(scenario.name, out, expected, run)
        } else {
            run(&mut ctx)
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
            let timing = match (
                sample.extra.get(MEDIAN_TICKS_PER_SEC),
                sample.extra.get(P99_MS_PER_TICK),
            ) {
                (Some(median), Some(p99)) => {
                    format!(" median ticks/s={median:.2} p99={p99:.2}ms/tick")
                }
                _ => String::new(),
            };
            println!(
                "{} [{}] tick={} humans={} employed={employed:.0} vehicles={} companies={} trains={} money_cents={} ticks/s={:.2}{timing}",
                scenario.name, sample.label, sample.tick, sample.humans, sample.vehicles,
                sample.companies, sample.trains, sample.money_cents, sample.ticks_per_sec,
            );
        }
        let mut timings = ctx.take_measured_timings();
        let samples = ctx
            .samples
            .into_iter()
            .map(|metrics| JsonSample {
                timings: if metrics.label == "measured" {
                    timings.take()
                } else {
                    None
                },
                metrics,
            })
            .collect::<Vec<_>>();
        results.insert(scenario.name, samples);
    }

    if let Some((mut out, path)) = hash_output.zip(hashes) {
        out.flush()
            .map_err(|err| format!("{}: {err}", path.display()))?;
    }
    if let Some(path) = json {
        let file = File::create(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        serde_json::to_writer_pretty(file, &results)
            .map_err(|err| format!("{}: {err}", path.display()))?;
    }

    if gate {
        let median = results[PERF_10K.0.name]
            .iter()
            .map(|sample| &sample.metrics)
            .find(|sample| sample.label == "measured")
            .and_then(|sample| sample.extra.get(MEDIAN_TICKS_PER_SEC))
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
