//! Matched serial benchmark runs against a predeclared parent executable.
use super::staging::{digest, read_rooted};
use crate::perf_scale::{benchmark_layout_with_scale, PerfCase, PerfLayout};
use crate::runner::{MEDIAN_TICKS_PER_SEC, P99_MS_PER_TICK};
use serde::Deserialize;
use serde_json::{json, Value};
use simulation::PopulationScale;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParentPin {
    version: u32,
    parent_binary: PathBuf,
    parent_sha256: String,
}

pub(super) fn run(
    manifest: Option<&Path>,
    case: PerfCase,
    layout: PerfLayout,
    layout_name: &str,
    pairs: usize,
    run_parent: bool,
) -> Result<Value, String> {
    if !(1..=40_000).contains(&case.agents)
        || !(1..=16).contains(&pairs)
        || (run_parent && pairs < 3)
        || !(1..=10_000).contains(&case.measured_ticks)
        || case.warmup_ticks > 10_000
    {
        return Err("unsupported performance matrix size or pair count".into());
    }
    let run_default = |case| {
        let report = benchmark_layout_with_scale(case, layout, PopulationScale::default())?;
        serde_json::to_value(report).map_err(|error| error.to_string())
    };
    if !run_parent {
        let mut runs = Vec::new();
        for _ in 0..pairs {
            runs.push(run_default(case)?);
        }
        return Ok(json!({"default": runs, "paired_gate": "not_requested"}));
    }
    let manifest = manifest.ok_or("parent performance mode requires a pinned --manifest")?;
    let pin: ParentPin =
        serde_json::from_slice(&super::staging::read_member(manifest, 1024 * 1024)?)
            .map_err(|error| format!("invalid performance manifest: {error}"))?;
    let root = manifest
        .parent()
        .ok_or("performance manifest has no directory")?;
    let path = root.join(&pin.parent_binary);
    let check_parent = || {
        let bytes = read_rooted(root, &pin.parent_binary, 64 * 1024 * 1024)?;
        if pin.version != 1 || digest(&bytes)? != pin.parent_sha256 {
            return Err("parent benchmark executable does not match its pin".to_owned());
        }
        Ok(())
    };
    let run_baseline = |case: PerfCase| {
        check_parent()?;
        let output = Command::new(&path)
            .args(["perf-scale", "--layout", layout_name, "--sim-threads", "1"])
            .arg("--agents")
            .arg(case.agents.to_string())
            .arg("--warmup")
            .arg(case.warmup_ticks.to_string())
            .arg("--ticks")
            .arg(case.measured_ticks.to_string())
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        serde_json::from_slice::<Value>(&output.stdout).map_err(|error| error.to_string())
    };
    for _ in 0..2 {
        let warmup = PerfCase {
            measured_ticks: 1,
            ..case
        };
        run_baseline(warmup)?;
        run_default(warmup)?;
    }
    let mut reports = Vec::new();
    let mut throughput = Vec::new();
    let mut tail = Vec::new();
    for index in 0..pairs {
        let (parent, default) = if index % 2 == 0 {
            (run_baseline(case)?, run_default(case)?)
        } else {
            let default = run_default(case)?;
            (run_baseline(case)?, default)
        };
        for field in [
            "agents",
            "initial_agents",
            "initial_population",
            "persons_per_agent",
            "warmup_ticks",
            "measured_ticks",
            "completed_steps",
            "workers",
            "simulation_options",
            "lot_layout",
            "layout",
        ] {
            if parent.get(field) != default.get(field) {
                return Err(format!(
                    "parent and candidate benchmark workloads differ in {field}"
                ));
            }
        }
        let measure = |report: &Value, field: &str| {
            report[field]
                .as_f64()
                .or_else(|| report["values"][field].as_f64())
                .or_else(|| report["extra"][field].as_f64())
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| format!("benchmark report lacks {field}"))
        };
        throughput.push(
            measure(&default, MEDIAN_TICKS_PER_SEC)? / measure(&parent, MEDIAN_TICKS_PER_SEC)?,
        );
        tail.push(measure(&default, P99_MS_PER_TICK)? / measure(&parent, P99_MS_PER_TICK)?);
        reports.push(json!({"parent_first": index % 2 == 0, "parent": parent, "default": default}));
    }
    throughput.sort_by(f64::total_cmp);
    tail.sort_by(f64::total_cmp);
    let median = |values: &[f64]| (values[(values.len() - 1) / 2] + values[values.len() / 2]) * 0.5;
    let throughput = median(&throughput);
    let tail = median(&tail);
    Ok(json!({
        "parent_sha256": pin.parent_sha256,
        "pairs": reports,
        "median_throughput_ratio": throughput,
        "median_tick_p99_ratio": tail,
        "paired_gate_passed": throughput >= 0.95 && tail <= 1.05,
    }))
}
