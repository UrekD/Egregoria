use super::comparison::compare_builtin_repair_resumed;
use super::natural::StagedTraffic;
use super::staging::{read_member, verify_runtime_data};
use super::{
    compare_builtin_repair, stage_published_slots, FixtureArm, FixtureClass, StageOptions,
};
use crate::ScenarioCtx;
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use simulation::world_command::WorldCommand;

type Args = BTreeMap<String, Option<OsString>>;

pub fn cli(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let operation = args
        .next()
        .ok_or("traffic-repair requires compare, stage, soak or perf")?;
    let mut flags = Args::new();
    while let Some(arg) = args.next() {
        let flag = arg.to_str().ok_or("invalid traffic-repair flag")?;
        let value = match flag {
            "--twice" | "--resume" | "--replay" => None,
            "--fixtures" | "--workers" | "--json" | "--manifest" | "--class" | "--out"
            | "--bundle-root" | "--run-source" | "--root" | "--edit" | "--days"
            | "--agents" | "--layout" | "--warmup" | "--ticks" | "--pairs" | "--modes" => Some(
                args.next()
                    .filter(|arg| !arg.as_encoded_bytes().starts_with(b"--"))
                    .ok_or_else(|| format!("{flag} requires a value"))?,
            ),
            _ => return Err(format!("unexpected traffic-repair flag {flag}")),
        };
        if flags.insert(flag.to_owned(), value).is_some() {
            return Err(format!("duplicate traffic-repair flag {flag}"));
        }
    }
    match operation.to_str() {
        Some("compare") => compare(flags),
        Some("stage") => stage(flags),
        Some("soak") => soak(flags),
        Some("perf") => perf(flags),
        _ => Err(format!("unknown traffic-repair operation {operation:?}")),
    }
}

fn compare(mut flags: Args) -> Result<(), String> {
    if flags.contains_key("--root") {
        return compare_staged(flags);
    }
    let fixtures = text(&mut flags, "--fixtures")?;
    let unsupported_workers = serial_workers(&mut flags)?;
    let json = flags.remove("--json").flatten().map(absolute).transpose()?;
    let twice = flags.remove("--twice").is_some();
    let resume = flags.remove("--resume").is_some();
    let replay = flags.remove("--replay").is_some();
    reject_unused(&flags)?;

    let mut cases = BTreeMap::new();
    for id in fixtures.split(',') {
        let arm = match id {
            "repair-cohort" => FixtureArm::Repair,
            "repair-no-loss" => FixtureArm::NoLoss,
            _ => return Err(format!("unknown comparison fixture {id:?}")),
        };
        if cases.insert(id, arm).is_some() {
            return Err(format!("duplicate comparison fixture {id}"));
        }
    }

    initialize()?;
    let cancelled = AtomicBool::new(false);
    let mut reports = BTreeMap::new();
    for (id, arm) in cases {
        let report = compare_builtin_repair(arm, &cancelled).map_err(|err| err.to_string())?;
        if twice {
            let repeat = compare_builtin_repair(arm, &cancelled).map_err(|err| err.to_string())?;
            if report != repeat {
                return Err(format!("{id} comparison changed on an independent repeat"));
            }
        }
        if resume {
            let resumed = compare_builtin_repair_resumed(arm, &cancelled, true)
                .map_err(|err| err.to_string())?;
            if report != resumed {
                return Err(format!("{id} comparison changed after checkpoint reload"));
            }
        }
        if replay {
            let replayed = super::comparison::compare_builtin_repair_replayed(arm, &cancelled)
                .map_err(|err| err.to_string())?;
            if report != replayed {
                return Err(format!("{id} comparison changed with command replay"));
            }
        }
        reports.insert(id, report);
    }

    if unsupported_workers.is_empty() {
        write_json(json, &reports)
    } else {
        write_json(json, &serde_json::json!({
            "cases": reports,
            "unsupported_workers": unsupported_workers,
        }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Verification {
    IndependentRepeat,
    CheckpointReload,
    CommandReplay,
}

fn verify_staged<T: PartialEq>(
    id: &str,
    reference: &T,
    requested: [bool; 3],
    mut run: impl FnMut(Verification) -> Result<T, super::ComparisonError>,
) -> Result<Vec<Verification>, String> {
    let mut verified = Vec::new();
    for (mode, enabled) in [
        Verification::IndependentRepeat,
        Verification::CheckpointReload,
        Verification::CommandReplay,
    ]
    .into_iter()
    .zip(requested)
    {
        if enabled {
            let report = run(mode).map_err(|error| format!("{id} {mode:?}: {error}"))?;
            if *reference != report {
                return Err(format!("{id} traffic report changed on {mode:?}"));
            }
            verified.push(mode);
        }
    }
    Ok(verified)
}

fn compare_staged(mut flags: Args) -> Result<(), String> {
    let root = path(&mut flags, "--root")?;
    let fixtures = text(&mut flags, "--fixtures")?;
    let edit = path(&mut flags, "--edit")?;
    let edit: WorldCommand = serde_json::from_slice(&read_member(&edit, 64 * 1024)?)
        .map_err(|error| error.to_string())?;
    let unsupported_workers = serial_workers(&mut flags)?;
    let json = flags.remove("--json").flatten().map(absolute).transpose()?;
    let twice = flags.remove("--twice").is_some();
    let resume = flags.remove("--resume").is_some();
    let replay = flags.remove("--replay").is_some();
    reject_unused(&flags)?;
    initialize()?;
    let cancelled = AtomicBool::new(false);
    let mut reports = BTreeMap::new();
    for id in fixtures.split(',') {
        let input = StagedTraffic::open(&root, id)?;
        let report = input.compare(&edit, false, &cancelled).map_err(|err| err.to_string())?;
        let verified = verify_staged(id, &report, [twice, resume, replay], |mode| match mode {
            Verification::IndependentRepeat => input.compare(&edit, false, &cancelled),
            Verification::CheckpointReload => input.compare(&edit, true, &cancelled),
            Verification::CommandReplay => input.compare_replayed(&edit, &cancelled),
        })?;
        if reports
            .insert(
                id,
                serde_json::json!({
                    "fingerprint": input.fingerprint,
                    "comparison": report,
                    "verified": verified,
                }),
            )
            .is_some()
        {
            return Err(format!("duplicate comparison fixture {id}"));
        }
    }
    write_json(json, &serde_json::json!({
        "cases": reports,
        "unsupported_workers": unsupported_workers,
    }))
}

fn soak(mut flags: Args) -> Result<(), String> {
    let root = path(&mut flags, "--root")?;
    let fixtures = text(&mut flags, "--fixtures")?;
    let days = number(&mut flags, "--days")?;
    let unsupported_workers = serial_workers(&mut flags)?;
    let json = flags.remove("--json").flatten().map(absolute).transpose()?;
    let twice = flags.remove("--twice").is_some();
    let resume = flags.remove("--resume").is_some();
    let replay = flags.remove("--replay").is_some();
    reject_unused(&flags)?;
    initialize()?;
    let cancelled = AtomicBool::new(false);
    let mut reports = BTreeMap::new();
    let mut passed = true;
    for id in fixtures.split(',') {
        let input = StagedTraffic::open(&root, id)?;
        let report = input.soak(days, false, &cancelled).map_err(|err| err.to_string())?;
        let verified = verify_staged(id, &report, [twice, resume, replay], |mode| match mode {
            Verification::IndependentRepeat => input.soak(days, false, &cancelled),
            Verification::CheckpointReload => input.soak(days, true, &cancelled),
            Verification::CommandReplay => input.soak_replayed(days, &cancelled),
        })?;
        let failures = input.soak_failures(&report);
        let row = serde_json::json!({
            "gates": input.gates,
            "passed": failures.is_empty(),
            "failures": failures,
            "fingerprint": input.fingerprint,
            "traffic": report,
            "independent_repeat_verified": verified.contains(&Verification::IndependentRepeat),
            "checkpoint_reload_verified": verified.contains(&Verification::CheckpointReload),
            "replay_verified": verified.contains(&Verification::CommandReplay),
        });
        passed &= failures.is_empty();
        if reports.insert(id, row).is_some() {
            return Err(format!("duplicate soak fixture {id}"));
        }
    }
    write_json(json, &serde_json::json!({
        "cases": reports,
        "unsupported_workers": unsupported_workers,
    }))?;
    if !passed {
        return Err("release traffic soak failed, see the fixture gate results".into());
    }
    Ok(())
}

fn perf(mut flags: Args) -> Result<(), String> {
    use crate::perf_scale::{PerfCase, PerfLayout};
    let layout_name = text(&mut flags, "--layout")?;
    let layout = match layout_name.as_str() {
        "compact" => PerfLayout::Compact,
        "growing" => PerfLayout::Growing,
        _ => return Err("--layout requires compact or growing".into()),
    };
    let case = PerfCase {
        agents: number(&mut flags, "--agents")?,
        warmup_ticks: number(&mut flags, "--warmup")?,
        measured_ticks: number(&mut flags, "--ticks")?,
        workers: 1,
    };
    let pairs = number(&mut flags, "--pairs")?;
    let mode_list = text(&mut flags, "--modes")?;
    let modes: std::collections::BTreeSet<_> = mode_list.split(',').collect();
    if !modes.contains("default") || modes.iter().any(|mode| {
        !matches!(*mode, "parent" | "default" | "observer" | "pilot")
    }) {
        return Err("--modes requires default and accepts parent, observer and pilot".into());
    }
    let unsupported_workers = serial_workers(&mut flags)?;
    let manifest = flags.remove("--manifest").flatten().map(absolute).transpose()?;
    let json = flags.remove("--json").flatten().map(absolute).transpose()?;
    reject_unused(&flags)?;
    initialize()?;
    let report = super::perf::run(manifest.as_deref(), case, layout, &layout_name, pairs, modes.contains("parent"))?;
    let unsupported_modes: Vec<_> = modes.into_iter().filter(|mode| matches!(*mode, "observer" | "pilot")).collect();
    let passed = report["paired_gate_passed"].as_bool();
    write_json(json, &serde_json::json!({
        "report": report,
        "unsupported_workers": unsupported_workers,
        "unsupported_modes": unsupported_modes,
    }))?;
    if passed == Some(false) {
        return Err("serial paired performance gate failed".into());
    }
    Ok(())
}

fn number<T: std::str::FromStr>(flags: &mut Args, flag: &str) -> Result<T, String> {
    text(flags, flag)?.parse().map_err(|_| format!("invalid {flag}"))
}

fn serial_workers(flags: &mut Args) -> Result<Vec<usize>, String> {
    let value = if flags.contains_key("--workers") {
        text(flags, "--workers")?
    } else {
        "1".into()
    };
    let workers: std::collections::BTreeSet<usize> = value.split(',').map(str::parse)
        .collect::<Result<_, _>>().map_err(|_| "invalid worker list")?;
    if !workers.contains(&1) || workers.contains(&0) {
        return Err("traffic repair requires including the supported worker count of 1".into());
    }
    Ok(workers.into_iter().filter(|&workers| workers != 1).collect())
}

fn initialize() -> Result<(), String> {
    crate::ensure_repo_root_cwd();
    verify_runtime_data(&std::env::current_dir().map_err(|error| error.to_string())?)?;
    ScenarioCtx::initialize();
    Ok(())
}

fn write_json(json: Option<PathBuf>, value: &impl Serialize) -> Result<(), String> {
    let output: Box<dyn Write> = match json {
        Some(path) => Box::new(std::fs::File::create(path).map_err(|err| err.to_string())?),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut writer = BufWriter::new(output);
    serde_json::to_writer_pretty(&mut writer, value).map_err(|err| err.to_string())?;
    writeln!(writer)
        .and_then(|_| writer.flush())
        .map_err(|err| err.to_string())
}

fn stage(mut flags: Args) -> Result<(), String> {
    let manifest = path(&mut flags, "--manifest")?;
    let output = path(&mut flags, "--out")?;
    let requested = match text(&mut flags, "--class")?.as_str() {
        "release" => FixtureClass::Release,
        "diagnostic" => FixtureClass::Diagnostic,
        _ => return Err("--class requires release or diagnostic".into()),
    };
    let bundle_root = flags
        .remove("--bundle-root")
        .flatten()
        .map(absolute)
        .transpose()?;
    let run_source_commit = if flags.contains_key("--run-source") {
        Some(text(&mut flags, "--run-source")?)
    } else {
        None
    };
    reject_unused(&flags)?;
    initialize()?;
    let staged = stage_published_slots(
        &manifest,
        requested,
        &output,
        StageOptions {
            bundle_root: bundle_root.as_deref(),
            run_source_commit: run_source_commit.as_deref(),
        },
    )?;
    for id in staged {
        println!("Staged {id} at {}", output.join(id.as_str()).display());
    }
    Ok(())
}

fn reject_unused(flags: &Args) -> Result<(), String> {
    match flags.keys().next() {
        Some(flag) => Err(format!(
            "unexpected flag {flag} for this traffic-repair operation"
        )),
        None => Ok(()),
    }
}

fn text(flags: &mut Args, flag: &str) -> Result<String, String> {
    flags
        .remove(flag)
        .flatten()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| format!("{flag} requires a text value"))
}

fn path(flags: &mut Args, flag: &str) -> Result<PathBuf, String> {
    absolute(
        flags
            .remove(flag)
            .flatten()
            .ok_or_else(|| format!("{flag} requires a path"))?,
    )
}

fn absolute(path: OsString) -> Result<PathBuf, String> {
    std::path::absolute(Path::new(&path)).map_err(|err| err.to_string())
}

#[cfg(test)]
#[test]
fn p7_comparison_verifies_all_requested_modes_independently() {
    use Verification::*;
    let mut runs = Vec::new();
    let verified = verify_staged("fixture", &42, [true; 3], |mode| {
        runs.push(mode);
        Ok(42)
    })
    .unwrap();
    assert_eq!(runs, [IndependentRepeat, CheckpointReload, CommandReplay]);
    assert_eq!(verified, runs);

    let refused = verify_staged("fixture", &42, [true; 3], |mode| {
        Ok(if mode == CheckpointReload { 21 } else { 42 })
    });
    assert!(refused.unwrap_err().contains("CheckpointReload"));
}
