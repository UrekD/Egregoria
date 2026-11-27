use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use wgpu::{
    Backend, Buffer, BufferAsyncError, BufferDescriptor, BufferUsages, CommandEncoder, Device,
    DownlevelFlags, Features, MapMode, QuerySet, RenderPassTimestampWrites,
};

use crate::GfxSettings;

pub const HISTORY_FRAMES: usize = 600;
const READBACK_SLOTS: usize = 4;
const QUERY_STRIDE: u64 = wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT;
const READBACK_SIZE: u64 = QUERY_STRIDE * GpuPass::COUNT as u64;

#[derive(Clone, Copy)]
pub(crate) enum ProfileMode {
    Cpu,
    CpuAndGpu,
}

impl ProfileMode {
    pub(crate) fn from_env() -> Option<Self> {
        if cfg!(target_arch = "wasm32") {
            return None;
        }
        match std::env::var("EGREGORIA_FRAME_PROFILE").as_deref() {
            Ok("1") => Some(Self::CpuAndGpu),
            Ok("cpu") => Some(Self::Cpu),
            _ => None,
        }
    }
}

pub(crate) fn timestamp_features(
    mode: Option<ProfileMode>,
    supported: Features,
    backend: Backend,
    downlevel: DownlevelFlags,
) -> Features {
    if matches!(mode, Some(ProfileMode::CpuAndGpu))
        && backend != Backend::Gl
        && downlevel.contains(DownlevelFlags::NONBLOCKING_QUERY_RESOLVE)
        && supported.contains(Features::TIMESTAMP_QUERY)
    {
        supported & (Features::TIMESTAMP_QUERY | Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    } else {
        Features::empty()
    }
}

#[derive(Clone, Copy)]
pub enum CpuPhase {
    SurfaceAcquire,
    SimLease,
    WorldPrep,
    MapMesh,
    Instances,
    Water,
    Ui,
    Encode,
    Submit,
    Present,
}

impl CpuPhase {
    pub const LABELS: [&str; 10] = [
        "surface_acquire",
        "sim_lease",
        "world_prep",
        "map_mesh",
        "instances",
        "water",
        "ui_build_encode",
        "encode_jobs",
        "submit",
        "present",
    ];
}

#[derive(Clone, Copy)]
pub(crate) enum GpuPass {
    Depth,
    Environment,
    Irradiance,
    Specular,
    Shadow0,
    Shadow1,
    Shadow2,
    Shadow3,
    Ssao,
    Fog,
    Main,
    Background,
    UiBlur,
    #[cfg_attr(not(feature = "yakui"), allow(dead_code))]
    Yakui,
    Egui,
    LampUpdate,
    HeightmapResample,
    HeightmapNormals,
}

impl GpuPass {
    pub(crate) const SHADOWS: [Self; crate::N_CASCADES] =
        [Self::Shadow0, Self::Shadow1, Self::Shadow2, Self::Shadow3];
    const COUNT: usize = GPU_PASS_LABELS.len();

    fn index(self) -> usize {
        self as usize
    }
}

pub const GPU_PASS_LABELS: [&str; 18] = [
    "depth",
    "pbr_environment",
    "pbr_irradiance",
    "pbr_specular",
    "shadow_0",
    "shadow_1",
    "shadow_2",
    "shadow_3",
    "ssao",
    "fog",
    "main",
    "background",
    "ui_blur",
    "yakui",
    "egui",
    "lamp_update",
    "heightmap_resample",
    "heightmap_normals",
];

type GpuTimes = [Option<f64>; GpuPass::COUNT];
type CpuTimes = [Option<f64>; CpuPhase::LABELS.len()];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuStatus {
    CpuOnly,
    Unsupported,
    Pending,
    Measured,
    Busy,
    ReadbackFailed,
}

#[derive(Serialize)]
pub struct FrameSample {
    pub frame: u64,
    pub elapsed_ms: f64,
    pub present_interval_ms: Option<f64>,
    pub cpu_frame_ms: f64,
    pub cpu_ms: CpuTimes,
    pub gpu_status: GpuStatus,
    pub gpu_ms: GpuTimes,
    pub skipped_redraws: u64,
    pub resolution: [u32; 2],
    pub surface_present_mode: &'static str,
    pub graphics: GfxSettings,
    pub sim_step: Option<u64>,
    pub sim_sample_ms: Option<f64>,
    pub requested_warp: Option<u32>,
    pub sim_steps_per_second: Option<f64>,
    pub sim_steps: Option<u64>,
    pub sim_read_lease_ms: Option<f64>,
    pub sim_worker_read_wait_ms: Option<f64>,
    pub sim_worker_redraw_wait_ms: Option<f64>,
    pub sim_worker_max_idle_ms: Option<f64>,
    pub pending_water_chunks: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct SimWaitTotals {
    pub read_lease_ns: u64,
    pub redraw_turn_ns: u64,
    pub max_idle_ns: u64,
}

#[derive(Clone, Copy)]
struct SimObservation {
    at: Instant,
    generation: u64,
    step: u64,
    warp: u32,
    waits: Option<SimWaitTotals>,
}

#[derive(Default)]
struct SimRate {
    anchor: Option<(Instant, u64, u64)>,
    rate: Option<f64>,
}

impl SimRate {
    fn observe(&mut self, now: Instant, generation: u64, step: u64) -> Option<f64> {
        if let Some((last, old_generation, old_step)) = self.anchor {
            if generation == old_generation && step >= old_step {
                let elapsed = now.duration_since(last);
                if elapsed >= Duration::from_millis(500) {
                    self.rate = Some((step - old_step) as f64 / elapsed.as_secs_f64());
                    self.anchor = Some((now, generation, step));
                }
                return self.rate;
            }
        }
        self.anchor = Some((now, generation, step));
        self.rate = None;
        None
    }
}

pub struct FrameProfile {
    samples: VecDeque<FrameSample>,
    gpu: Option<GpuProfile>,
    unavailable: GpuStatus,
    current_gpu: GpuStatus,
    cpu: CpuTimes,
    last_present: Option<Instant>,
    first_present: Option<Instant>,
    skipped: u64,
    sim: Option<SimObservation>,
    previous_sim: Option<SimObservation>,
    read_lease: Option<Duration>,
    first_sim_sample: Option<Instant>,
    sim_rate: SimRate,
    pub pending_water_chunks: Option<usize>,
}

impl FrameProfile {
    pub(crate) fn new(mode: ProfileMode, device: &Device, queue: &wgpu::Queue) -> Self {
        let gpu = device
            .features()
            .contains(Features::TIMESTAMP_QUERY)
            .then(|| GpuProfile::new(device, queue));
        Self::with_gpu(mode, gpu)
    }

    fn with_gpu(mode: ProfileMode, gpu: Option<GpuProfile>) -> Self {
        let unavailable = match mode {
            ProfileMode::Cpu => GpuStatus::CpuOnly,
            ProfileMode::CpuAndGpu => GpuStatus::Unsupported,
        };
        Self {
            samples: VecDeque::with_capacity(HISTORY_FRAMES),
            gpu,
            unavailable,
            current_gpu: unavailable,
            cpu: [None; CpuPhase::LABELS.len()],
            last_present: None,
            first_present: None,
            skipped: 0,
            sim: None,
            previous_sim: None,
            read_lease: None,
            first_sim_sample: None,
            sim_rate: SimRate::default(),
            pending_water_chunks: None,
        }
    }

    pub fn samples(&self) -> &VecDeque<FrameSample> {
        &self.samples
    }

    pub fn has_pending_gpu(&self, frames: Range<u64>) -> bool {
        self.gpu.as_ref().is_some_and(|gpu| {
            gpu.slots.iter().any(|slot| {
                slot.pending
                    .as_ref()
                    .is_some_and(|pending| frames.contains(&pending.frame))
            })
        })
    }

    /// Keep outstanding maps alive until their callbacks finish. Their frame IDs
    /// will no longer match any sample from the new city.
    pub fn reset(&mut self) {
        self.samples.clear();
        self.last_present = None;
        self.first_present = None;
        self.skipped = 0;
        self.first_sim_sample = None;
        self.previous_sim = None;
        self.read_lease = None;
        self.sim_rate = SimRate::default();
        self.begin_redraw();
    }

    pub(crate) fn begin_redraw(&mut self) {
        self.cpu.fill(None);
        self.sim = None;
        self.pending_water_chunks = None;
        self.current_gpu = self.unavailable;
        self.start_gpu();
    }

    pub(crate) fn skip_redraw(&mut self) {
        self.skipped += 1;
    }

    pub fn record_sim(&mut self, observed_at: Instant, generation: u64, step: u64, warp: u32) {
        self.sim = Some(SimObservation {
            at: observed_at,
            generation,
            step,
            warp,
            waits: None,
        });
    }

    pub fn record_sim_waits(&mut self, waits: SimWaitTotals) {
        if let Some(sim) = &mut self.sim {
            sim.waits = Some(waits);
        }
    }

    pub fn record_sim_read_lease(&mut self, elapsed: Duration) {
        *self.read_lease.get_or_insert(Duration::ZERO) += elapsed;
    }

    pub(crate) fn record_cpu(&mut self, phase: CpuPhase, elapsed: Duration) {
        *self.cpu[phase as usize].get_or_insert(0.0) += elapsed.as_secs_f64() * 1000.0;
    }

    fn start_gpu(&mut self) {
        if let Some(gpu) = &mut self.gpu {
            self.current_gpu = if gpu.begin() {
                GpuStatus::Pending
            } else {
                GpuStatus::Busy
            };
        }
    }

    pub(crate) fn timestamp_writes(
        &self,
        pass: GpuPass,
        begin: bool,
        end: bool,
    ) -> Option<RenderPassTimestampWrites<'_>> {
        let slot = self.gpu.as_ref()?.active()?;
        if !begin && !end {
            return None;
        }
        let index = pass.index() as u32 * 2;
        let endpoints = u64::from(begin) | (u64::from(end) << 1);
        slot.written.fetch_or(endpoints << index, Ordering::Relaxed);
        Some(RenderPassTimestampWrites {
            query_set: &slot.queries,
            beginning_of_pass_write_index: begin.then_some(index),
            end_of_pass_write_index: end.then_some(index + 1),
        })
    }

    #[cfg(feature = "yakui")]
    pub(crate) fn encoder_timestamp(&self, encoder: &mut CommandEncoder, pass: GpuPass, end: bool) {
        let Some(gpu) = self.gpu.as_ref().filter(|gpu| gpu.encoder_timestamps) else {
            return;
        };
        if let Some(slot) = gpu.active() {
            let index = pass.index() as u32 * 2 + u32::from(end);
            slot.written.fetch_or(1 << index, Ordering::Relaxed);
            encoder.write_timestamp(&slot.queries, index);
        }
    }

    pub(crate) fn active_gpu(&self) -> Option<&GpuSlot> {
        self.gpu.as_ref()?.active()
    }

    pub(crate) fn map_gpu(&mut self, frame: u64) {
        if let Some(gpu) = &mut self.gpu {
            gpu.map(frame);
        }
    }

    pub(crate) fn record_present(
        &mut self,
        frame: u64,
        frame_start: Instant,
        now: Instant,
        resolution: [u32; 2],
        surface_present_mode: &'static str,
        graphics: GfxSettings,
    ) {
        let present_interval_ms = self
            .last_present
            .replace(now)
            .map(|last| (now - last).as_secs_f64() * 1000.0);
        let elapsed_ms = (now - *self.first_present.get_or_insert(now)).as_secs_f64() * 1000.0;
        let sim_sample_ms = self.sim.map(|sim| {
            let first = *self.first_sim_sample.get_or_insert(sim.at);
            (sim.at - first).as_secs_f64() * 1000.0
        });
        let sim_steps_per_second = self
            .sim
            .and_then(|sim| self.sim_rate.observe(sim.at, sim.generation, sim.step));
        let interval = self.sim.zip(self.previous_sim).filter(|(sim, previous)| {
            sim.generation == previous.generation && sim.step >= previous.step
        });
        let waits = interval.and_then(|(sim, previous)| sim.waits.zip(previous.waits));
        let wait_ms = |field: fn(SimWaitTotals) -> u64| {
            waits.map(|(current, previous)| {
                field(current).saturating_sub(field(previous)) as f64 / 1_000_000.0
            })
        };
        self.previous_sim = self.sim;
        if self.samples.len() == HISTORY_FRAMES {
            self.samples.pop_front();
        }
        self.samples.push_back(FrameSample {
            frame,
            elapsed_ms,
            present_interval_ms,
            cpu_frame_ms: (now - frame_start).as_secs_f64() * 1000.0,
            cpu_ms: self.cpu,
            gpu_status: self.current_gpu,
            gpu_ms: [None; GpuPass::COUNT],
            skipped_redraws: std::mem::take(&mut self.skipped),
            resolution,
            surface_present_mode,
            graphics,
            sim_step: self.sim.map(|sim| sim.step),
            sim_sample_ms,
            requested_warp: self.sim.map(|sim| sim.warp),
            sim_steps_per_second,
            sim_steps: interval.map(|(sim, previous)| sim.step - previous.step),
            sim_read_lease_ms: self
                .read_lease
                .take()
                .map(|held| held.as_secs_f64() * 1000.0),
            sim_worker_read_wait_ms: wait_ms(|totals| totals.read_lease_ns),
            sim_worker_redraw_wait_ms: wait_ms(|totals| totals.redraw_turn_ns),
            sim_worker_max_idle_ms: wait_ms(|totals| totals.max_idle_ns),
            pending_water_chunks: self.pending_water_chunks,
        });
    }

    /// Poll without waiting and attach completed GPU readbacks to their frame IDs.
    pub(crate) fn collect_ready(&mut self, device: &Device) {
        let Some(gpu) = &mut self.gpu else {
            return;
        };
        if gpu.slots.iter().all(|slot| slot.pending.is_none()) {
            return;
        }
        device.poll(wgpu::Maintain::Poll);
        for slot in &mut gpu.slots {
            let Some((frame, result)) = slot.take_ready(gpu.period) else {
                continue;
            };
            let sample = self
                .samples
                .front()
                .and_then(|first| frame.checked_sub(first.frame))
                .and_then(|offset| self.samples.get_mut(offset as usize));
            if let Some(sample) = sample.filter(|sample| sample.frame == frame) {
                if let Some(times) = result {
                    sample.gpu_ms = times;
                    sample.gpu_status = GpuStatus::Measured;
                } else {
                    sample.gpu_status = GpuStatus::ReadbackFailed;
                }
            }
        }
    }
}

struct GpuProfile {
    slots: [GpuSlot; READBACK_SLOTS],
    active: Option<usize>,
    period: f32,
    #[cfg(feature = "yakui")]
    encoder_timestamps: bool,
}

impl GpuProfile {
    fn new(device: &Device, queue: &wgpu::Queue) -> Self {
        Self {
            slots: std::array::from_fn(|_| GpuSlot::new(device)),
            active: None,
            period: queue.get_timestamp_period(),
            #[cfg(feature = "yakui")]
            encoder_timestamps: device
                .features()
                .contains(Features::TIMESTAMP_QUERY_INSIDE_ENCODERS),
        }
    }

    fn begin(&mut self) -> bool {
        self.active = self.slots.iter().position(|slot| slot.pending.is_none());
        if let Some(index) = self.active {
            *self.slots[index].written.get_mut() = 0;
        }
        self.active.is_some()
    }

    fn active(&self) -> Option<&GpuSlot> {
        self.active.map(|index| &self.slots[index])
    }

    fn map(&mut self, frame: u64) {
        if let Some(index) = self.active.take() {
            let slot = &mut self.slots[index];
            let (sender, ready) = mpsc::sync_channel(1);
            slot.readback
                .slice(..)
                .map_async(MapMode::Read, move |result| {
                    let _ = sender.send(result);
                });
            slot.pending = Some(PendingMap {
                frame,
                ready: Mutex::new(ready),
            });
        }
    }
}

struct PendingMap {
    frame: u64,
    ready: Mutex<Receiver<Result<(), BufferAsyncError>>>,
}

pub(crate) struct GpuSlot {
    queries: QuerySet,
    resolve: Buffer,
    readback: Buffer,
    written: AtomicU64,
    // Rayon workers share the active slot while recording disjoint passes.
    pending: Option<PendingMap>,
}

impl GpuSlot {
    fn new(device: &Device) -> Self {
        let buffer = |label, usage| {
            device.create_buffer(&BufferDescriptor {
                label: Some(label),
                size: READBACK_SIZE,
                usage,
                mapped_at_creation: false,
            })
        };
        Self {
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("frame profile timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: GpuPass::COUNT as u32 * 2,
            }),
            resolve: buffer(
                "frame profile resolve",
                BufferUsages::QUERY_RESOLVE | BufferUsages::COPY_SRC,
            ),
            readback: buffer(
                "frame profile readback",
                BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            ),
            written: AtomicU64::new(0),
            pending: None,
        }
    }

    pub(crate) fn resolve(&self, encoder: &mut CommandEncoder) {
        let written = self.written.load(Ordering::Relaxed);
        for index in 0..GpuPass::COUNT {
            if query_pair_written(written, index) {
                let start = index as u32 * 2;
                encoder.resolve_query_set(
                    &self.queries,
                    start..start + 2,
                    &self.resolve,
                    index as u64 * QUERY_STRIDE,
                );
            }
        }
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, READBACK_SIZE);
    }

    fn take_ready(&mut self, period: f32) -> Option<(u64, Option<GpuTimes>)> {
        let pending = self.pending.as_mut()?;
        let ready = match pending.ready.get_mut().unwrap().try_recv() {
            Ok(result) => result.is_ok(),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => false,
        };
        let result = if ready {
            let times = {
                let bytes = self.readback.slice(..).get_mapped_range();
                decode_gpu(&bytes, *self.written.get_mut(), period)
            };
            self.readback.unmap();
            Some(times)
        } else {
            None
        };
        let pending = self.pending.take().unwrap();
        Some((pending.frame, result))
    }
}

fn query_pair_written(written: u64, index: usize) -> bool {
    (written >> (index * 2)) & 0b11 == 0b11
}

fn decode_gpu(bytes: &[u8], written: u64, period: f32) -> GpuTimes {
    std::array::from_fn(|index| {
        if !query_pair_written(written, index) {
            return None;
        }
        let offset = index * QUERY_STRIDE as usize;
        let read = |at| u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap());
        let start = read(offset);
        let end = read(offset + 8);
        // Exclude disjoint/reset counters instead of reporting an invented duration.
        end.checked_sub(start)
            .map(|ticks| ticks as f64 * f64::from(period) / 1_000_000.0)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn frame_profile_requests_only_supported_opt_in_features() {
        let downlevel = DownlevelFlags::NONBLOCKING_QUERY_RESOLVE;
        for mode in [None, Some(ProfileMode::Cpu)] {
            assert_eq!(
                timestamp_features(mode, Features::TIMESTAMP_QUERY, Backend::Vulkan, downlevel),
                Features::empty()
            );
        }
        let mode = Some(ProfileMode::CpuAndGpu);
        assert_eq!(
            timestamp_features(
                mode,
                Features::empty(),
                Backend::Vulkan,
                downlevel,
            ),
            Features::empty()
        );
        let wanted = Features::TIMESTAMP_QUERY | Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        assert_eq!(
            timestamp_features(
                mode,
                wanted | Features::TIMESTAMP_QUERY_INSIDE_PASSES,
                Backend::Vulkan,
                downlevel,
            ),
            wanted
        );
    }

    #[test]
    fn frame_profile_rejects_blocking_query_resolve() {
        for (backend, downlevel) in [
            (Backend::Vulkan, DownlevelFlags::empty()),
            (Backend::Gl, DownlevelFlags::empty()),
            (Backend::Gl, DownlevelFlags::NONBLOCKING_QUERY_RESOLVE),
        ] {
            assert_eq!(
                timestamp_features(
                    Some(ProfileMode::CpuAndGpu),
                    Features::TIMESTAMP_QUERY,
                    backend,
                    downlevel,
                ),
                Features::empty()
            );
        }
    }

    #[test]
    fn frame_profile_excludes_incomplete_query_pairs() {
        let bytes = vec![0; READBACK_SIZE as usize];
        // Depth has only the beginning timestamp. The next pass has only its end.
        let decoded = decode_gpu(&bytes, 0b1001, 1.0);
        assert!(decoded.iter().all(Option::is_none));
    }

    #[test]
    fn frame_profile_names_encoding_job_and_ui_durations() {
        assert_eq!(CpuPhase::LABELS[CpuPhase::Encode as usize], "encode_jobs");
        assert_eq!(CpuPhase::LABELS[CpuPhase::Ui as usize], "ui_build_encode");
    }

    #[test]
    fn frame_profile_decodes_only_written_passes() {
        let mut bytes = vec![0; QUERY_STRIDE as usize * GpuPass::COUNT];
        let offset = GpuPass::Main.index() * QUERY_STRIDE as usize;
        bytes[offset..offset + 8].copy_from_slice(&100u64.to_ne_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&350u64.to_ne_bytes());
        let decoded = decode_gpu(&bytes, 0b11 << (2 * GpuPass::Main.index()), 2.0);
        assert_eq!(decoded[GpuPass::Main.index()], Some(0.0005));
        assert_eq!(decoded[GpuPass::Depth.index()], None);
    }

    #[test]
    fn frame_profile_sim_rate_uses_wall_time_and_resets_at_session_boundary() {
        let mut rate = SimRate::default();
        let now = Instant::now();
        assert_eq!(rate.observe(now, 1, 200), None);
        assert_eq!(
            rate.observe(now + Duration::from_secs(2), 1, 260),
            Some(30.0)
        );
        assert_eq!(rate.observe(now + Duration::from_secs(3), 2, 3), None);
        assert_eq!(
            rate.observe(now + Duration::from_secs(4), 2, 23),
            Some(20.0)
        );
    }

    #[test]
    fn frame_profile_records_lease_and_worker_wait_deltas() {
        let mut profile = FrameProfile::with_gpu(ProfileMode::Cpu, None);
        let now = Instant::now();
        for (frame, generation, step) in [(1, 1, 5), (2, 1, 8), (3, 2, 2)] {
            let at = now + Duration::from_millis(frame * 10);
            profile.begin_redraw();
            profile.record_sim(at, generation, step, 10_000);
            profile.record_sim_read_lease(Duration::from_micros(frame * 100));
            profile.record_sim_waits(SimWaitTotals {
                read_lease_ns: frame * 100_000,
                redraw_turn_ns: frame * 200_000,
                max_idle_ns: frame * 350_000,
            });
            profile.record_present(frame, at, at, [640, 480], "fifo", GfxSettings::default());
        }
        let sample = &profile.samples()[1];
        assert_eq!(sample.sim_steps, Some(3));
        assert_eq!(sample.sim_read_lease_ms, Some(0.2));
        assert_eq!(sample.sim_worker_read_wait_ms, Some(0.1));
        assert_eq!(sample.sim_worker_redraw_wait_ms, Some(0.2));
        assert_eq!(sample.sim_worker_max_idle_ms, Some(0.35));
        assert_eq!(profile.samples()[2].sim_steps, None);
        assert_eq!(profile.samples()[2].sim_worker_max_idle_ms, None);
    }

    #[test]
    fn frame_profile_sim_rate_uses_observation_time_with_uneven_render_delays() {
        let mut profile = FrameProfile::with_gpu(ProfileMode::Cpu, None);
        let now = Instant::now();
        for (idx, elapsed, render, step) in [(1, 0, 200, 200), (2, 1000, 800, 260)] {
            let at = now + Duration::from_millis(elapsed);
            profile.begin_redraw();
            profile.record_sim(at, 1, step, 10000);
            profile.record_present(
                idx,
                at,
                at + Duration::from_millis(render),
                [1280, 720],
                "fifo",
                GfxSettings::default(),
            );
        }
        let sample = profile.samples().back().unwrap();
        assert_eq!(sample.sim_sample_ms, Some(1000.0));
        assert_eq!(sample.sim_steps_per_second, Some(60.0));
        assert_eq!(sample.present_interval_ms, Some(1600.0));
    }
}
