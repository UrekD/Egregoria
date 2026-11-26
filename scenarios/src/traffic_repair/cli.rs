use super::comparison::compare_builtin_repair_resumed;
use super::{
    compare_builtin_repair, stage_published_slots, FixtureArm, FixtureClass, StageOptions,
};
use crate::ScenarioCtx;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

type Args = BTreeMap<String, Option<OsString>>;

pub fn cli(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let operation = args
        .next()
        .ok_or("traffic-repair requires compare or stage")?;
    let mut flags = Args::new();
    while let Some(arg) = args.next() {
        let flag = arg.to_str().ok_or("invalid traffic-repair flag")?;
        let value = match flag {
            "--twice" | "--resume" => None,
            "--fixtures" | "--workers" | "--json" | "--manifest" | "--class" | "--out"
            | "--bundle-root" | "--run-source" => Some(
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
        _ => Err(format!("unknown traffic-repair operation {operation:?}")),
    }
}

fn compare(mut flags: Args) -> Result<(), String> {
    let fixtures = text(&mut flags, "--fixtures")?;
    let workers = flags
        .remove("--workers")
        .flatten()
        .unwrap_or_else(|| OsString::from("1"));
    if workers != "1" {
        return Err("traffic-repair comparison supports one simulation worker per arm".into());
    }
    let json = flags.remove("--json").flatten().map(absolute).transpose()?;
    let twice = flags.remove("--twice").is_some();
    let resume = flags.remove("--resume").is_some();
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

    ScenarioCtx::initialize();
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
        reports.insert(id, report);
    }

    let output: Box<dyn Write> = match json {
        Some(path) => Box::new(std::fs::File::create(path).map_err(|err| err.to_string())?),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut writer = BufWriter::new(output);
    serde_json::to_writer_pretty(&mut writer, &reports).map_err(|err| err.to_string())?;
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
    ScenarioCtx::initialize();
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
