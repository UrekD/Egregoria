use std::ops::Range;
use std::path::Path;

use common::saveload::{self, Encoder, JSON};
use engine::frame_profile::{CpuPhase, FrameSample, GpuStatus, GPU_PASS_LABELS};
use engine::GfxContext;
use serde::Serialize;

use crate::game_loop::VERSION;
use crate::session::SessionState;
use crate::uiworld::UiWorld;

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    game_version: &'a str,
    os: &'static str,
    architecture: &'static str,
    adapter: String,
    backend: String,
    device_type: String,
    driver: String,
    driver_info: String,
    slot: Option<&'a str>,
    realtime_steps_per_second: u64,
    cpu_phases: &'static [&'static str],
    gpu_passes: &'static [&'static str],
    frames: Vec<&'a FrameSample>,
}

pub(crate) fn write_report(
    gfx: &GfxContext,
    uiw: &UiWorld,
    path: &Path,
    range: Option<Range<u64>>,
) -> Result<(), String> {
    let profile = gfx
        .frame_profile
        .as_ref()
        .ok_or("frame profiling is disabled")?;
    let frames: Vec<_> = profile
        .samples()
        .iter()
        .filter(|sample| {
            range
                .as_ref()
                .is_none_or(|range| range.contains(&sample.frame))
        })
        .collect();
    let expected = range.map(|range| range.end - range.start);
    if frames.is_empty() || expected.is_some_and(|expected| frames.len() as u64 != expected) {
        return Err("the requested frame samples are unavailable".into());
    }

    let adapter = gfx.adapter_info();
    let session = uiw.read::<SessionState>();
    let report = Report {
        schema_version: 2,
        game_version: VERSION.trim(),
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        adapter: adapter.name,
        backend: format!("{:?}", adapter.backend),
        device_type: format!("{:?}", adapter.device_type),
        driver: adapter.driver,
        driver_info: adapter.driver_info,
        slot: session.active_slot.as_ref().map(|slot| slot.as_str()),
        realtime_steps_per_second: prototypes::TICKS_PER_REALTIME_SECOND,
        cpu_phases: &CpuPhase::LABELS,
        gpu_passes: &GPU_PASS_LABELS,
        frames,
    };
    saveload::write_atomic_with(path, |writer| JSON::encode_writer(&report, writer))
        .map_err(|error| error.to_string())
}

#[derive(Default)]
pub(crate) struct Panel {
    export_requested: bool,
    status: Option<String>,
}

impl Panel {
    pub(crate) fn export_requested(&mut self, gfx: &GfxContext, uiw: &UiWorld) {
        if !std::mem::take(&mut self.export_requested) {
            return;
        }
        let path = saveload::save_dir().join("frame-profile.json");
        self.status = Some(match write_report(gfx, uiw, &path, None) {
            Ok(()) => {
                let path = std::path::absolute(&path).unwrap_or(path);
                format!("Saved {}", path.display())
            }
            Err(error) => format!("Could not save report. {error}"),
        });
    }

    pub(crate) fn draw(&mut self, ui: &egui::Context, gfx: &GfxContext) {
        let Some(profile) = &gfx.frame_profile else {
            return;
        };
        egui::Window::new("Frame Profile")
            .anchor(egui::Align2::RIGHT_TOP, [-12.0, 56.0])
            .resizable(false)
            .vscroll(true)
            .show(ui, |ui| {
                let samples = profile.samples();
                let Some(latest) = samples.back() else {
                    ui.label("Waiting for a presented frame");
                    return;
                };
                self.export_requested |= ui.button("Save JSON report").clicked();
                if let Some(status) = &self.status {
                    ui.label(status);
                }
                ui.label(format!(
                    "Frame {} · CPU {:.2} ms",
                    latest.frame, latest.cpu_frame_ms
                ));
                if let Some(interval) = latest.present_interval_ms {
                    ui.label(format!("Presented interval {interval:.2} ms"));
                }
                if let Some(rate) = latest.sim_steps_per_second {
                    let warp = rate / prototypes::TICKS_PER_REALTIME_SECOND as f64;
                    ui.label(format!("Simulation {rate:.1} steps/s ({warp:.2}×)"));
                }
                if let Some(chunks) = latest.pending_water_chunks {
                    ui.label(format!("Water chunks awaiting preparation {chunks}"));
                }
                egui::Grid::new("frame_profile_cpu")
                    .num_columns(2)
                    .show(ui, |ui| {
                        draw_timings(ui, &CpuPhase::LABELS, &latest.cpu_ms);
                    });

                let gpu = samples
                    .iter()
                    .rev()
                    .find(|sample| sample.gpu_status == GpuStatus::Measured);
                match (latest.gpu_status, gpu) {
                    (GpuStatus::Unsupported, _) => {
                        ui.label("GPU timestamps unavailable on this adapter")
                    }
                    (GpuStatus::CpuOnly, _) => ui.label("CPU-only profiling"),
                    (GpuStatus::ReadbackFailed, _) => ui.label("GPU timestamp readback failed"),
                    (GpuStatus::Busy, _) => ui.label("GPU timestamp readback slots busy"),
                    (GpuStatus::Pending | GpuStatus::Measured, Some(gpu)) => {
                        ui.label(format!("GPU frame {}", gpu.frame))
                    }
                    _ => ui.label("Waiting for GPU timestamps"),
                };
                if let Some(gpu) = gpu {
                    egui::Grid::new("frame_profile_gpu")
                        .num_columns(2)
                        .show(ui, |ui| {
                            draw_timings(ui, &GPU_PASS_LABELS, &gpu.gpu_ms);
                        });
                }
            });
    }
}

fn draw_timings(ui: &mut egui::Ui, names: &[&str], times: &[Option<f64>]) {
    for (name, time) in names.iter().zip(times) {
        if let Some(time) = time {
            ui.label(*name);
            ui.monospace(format!("{time:.2} ms"));
            ui.end_row();
        }
    }
}
