use std::collections::VecDeque;
use std::io::{self, Write};
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::saveload::{Bincode, Encoder, SnapshotRevision};
use common::timestep::{debug_up_dt, MAX_WARP};
use simulation::map::{LandTileId, TilePurchased, WaterEditError, WaterEditOutcome};
use simulation::transportation::RoadThroughput;
use simulation::utils::replay::{ReplayAdvance, ReplayBudget, ReplayNext, ReplayOperation};
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::{
    CommandOutcomes, LinkOutcome, Rejection, WorldCommand, WorldCommands,
};
use simulation::{Simulation, SimulationReplayLoader, SlotCapture};

use crate::automation::{FixtureOp, FixtureOutcome};
use crate::gui::traffic_repair::TrafficEditReceipt;
use crate::rendering::world_snapshot::{CaptureWork, SnapshotSource, ViewRequest, WorldUpdate};
use crate::session::{save_coordinator::SavePermit, CityProgress};
use engine::frame_profile::SimWorkTotals;

pub(crate) const BATCH_BYTES: usize = 256 * 1024;
const BATCH_COMMANDS: usize = 256;
const BATCH_WORK: usize = 2_048;
// The replay envelope preserves historical groups and import commands as a
// single operation. It is independent of the live editor and reply budgets.
const REPLAY_COMMANDS: usize = 16_384;
const REPLAY_BYTES: usize = 4 * 1024 * 1024;
const REPLAY_WORK: usize = 32_768;
const BATCH_CREDITS: usize = 8;
// One extra credit is always available to fence or stop a full batch queue.
const CONTROL_CREDITS: usize = 1;
const CREDITS: usize = BATCH_CREDITS + CONTROL_CREDITS;
const EXTERNAL_LOCK_RETRY: Duration = Duration::from_micros(250);
const REDRAW_TURN: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BatchId {
    pub generation: u64,
    pub sequence: u64,
}

impl BatchId {
    pub(crate) fn covers(self, older: Self) -> bool {
        self.generation == older.generation && self.sequence >= older.sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Boundary {
    pub generation: u64,
    pub completed_step: u64,
    pub last_batch: Option<BatchId>,
    pub tick: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TilePurchaseReceipt {
    pub generation: u64,
    pub frame: u64,
    pub batch_sequence: u64,
    pub command_index: usize,
    pub tile: LandTileId,
    pub result: Result<TilePurchased, Rejection>,
}

impl TilePurchaseReceipt {
    pub(crate) fn from_outcomes<'a>(
        generation: u64,
        frame: u64,
        outcomes: &'a CommandOutcomes,
        commands: impl IntoIterator<Item = (usize, &'a WorldCommand)> + 'a,
    ) -> impl Iterator<Item = Self> + 'a {
        commands
            .into_iter()
            .filter_map(move |(command_index, command)| {
                let WorldCommand::BuyTile { tile } = *command else {
                    return None;
                };
                Some(Self {
                    generation,
                    frame,
                    command_index,
                    tile,
                    batch_sequence: outcomes.batch_seq(),
                    result: outcomes.tile_purchase(command_index)?,
                })
            })
    }
}

pub(crate) struct AppliedBatch {
    pub id: BatchId,
    pub boundary: Boundary,
    pub advanced: bool,
    pub outcome_sequence: u64,
    pub accepted_indices: Vec<usize>,
    pub rejections: Vec<(usize, &'static str, Rejection)>,
    pub link_outcomes: Vec<(usize, LinkOutcome)>,
    pub water_edits: Vec<(usize, Result<WaterEditOutcome, WaterEditError>)>,
    pub tile_purchases: Vec<TilePurchaseReceipt>,
    pub traffic_edits: Vec<TrafficEditReceipt>,
    pub commands: WorldCommands,
}

impl AppliedBatch {
    pub(crate) fn accepted_commands(&self) -> WorldCommands {
        let mut accepted = WorldCommands::default();
        let mut links = self.link_outcomes.iter().peekable();
        for (index, command) in self.commands.as_ref().iter().enumerate() {
            if let WorldCommand::MapMakeMultipleConnections(projects, connections) = command {
                while links.peek().is_some_and(|(command, _)| *command == index) {
                    let (_, (link, result)) = links.next().unwrap();
                    if result.is_ok() {
                        let (from, to, inter, pattern) = &connections[*link];
                        accepted.push(WorldCommand::MapMakeConnection {
                            from: projects[*from],
                            to: projects[*to],
                            inter: *inter,
                            pat: pattern.clone(),
                        });
                    }
                }
            } else if self.accepted_indices.contains(&index) {
                accepted.push(command.clone());
            }
        }
        accepted
    }
}

pub(crate) enum Request {
    Batch(WorldCommands),
    SetWarp(u32),
    PauseFence {
        through_batch: Option<BatchId>,
    },
    Resume,
    Step {
        count: usize,
    },
    PausedFixtureOp(FixtureOp),
    Capture {
        boundary: Option<Boundary>,
        revision: SnapshotRevision,
        permit: Option<SavePermit>,
    },
    CaptureComparison {
        revision: SnapshotRevision,
        limit: usize,
    },
    ReplayControl(ReplayControl),
    ReleaseOwner,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayControl {
    Speed(usize),
    Step(usize),
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerMode {
    Live,
    Paused,
    Replay,
    Transferring,
    Stopped,
}

#[derive(Clone)]
pub(crate) struct SimStatus {
    pub boundary: Boundary,
    pub owner_mode: OwnerMode,
    pub achieved_warp: Option<(u32, f64)>,
    pub last_tick_duration: Duration,
    pub schedule_samples: Arc<[(String, f32)]>,
    pub replay_progress: Option<ReplayProgress>,
}

#[derive(Clone, Copy)]
pub(crate) struct ReplayProgress {
    pub steps: Option<(u64, u64)>,
    pub tick: u64,
    pub last_tick: u64,
}

pub(crate) struct Reply {
    pub id: BatchId,
    pub kind: ReplyKind,
}

pub(crate) enum ReplyKind {
    Applied(AppliedBatch),
    WarpSet,
    Paused(Boundary),
    Resumed,
    Stepped(Boundary),
    FixtureDone(Result<FixtureOutcome, String>, Boundary),
    Captured(Result<(SlotCapture, CityProgress), String>),
    ReplayChanged,
    ReplayStepped(Boundary),
    Interrupted(Boundary),
    Releasing(Boundary),
    Stopping,
    Rejected(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubmitError {
    Full,
    Closed,
    Oversized,
    Stale,
}

pub(crate) struct SubmitErrorWithRequest(pub SubmitError, pub Request);

struct Envelope {
    id: BatchId,
    request: Request,
}

struct Credit {
    id: BatchId,
    batch: bool,
}

// Reader and writer reservations share one atomic state. The deadline for an
// unused redraw turn belongs to the worker and is never accessed by the frame.
#[derive(Default)]
struct Admission {
    state: AtomicU8,
    redraw_until: Mutex<Option<Instant>>,
    redraw_waker: OnceLock<Box<dyn Fn() + Send + Sync>>,
    worker: OnceLock<thread::Thread>,
}

const READER: u8 = 1 << 0;
const WRITER: u8 = 1 << 1;
const INTENT: u8 = 1 << 2;
const WAITING: u8 = 1 << 3;
const REDRAW: u8 = 1 << 4;

struct Turn<'a> {
    admission: &'a Admission,
    reader: bool,
}

#[derive(Debug)]
enum WorkerWait {
    Continue,
    Yield,
    Timeout(Duration),
    Redraw(Duration),
    Reader,
}

impl Admission {
    fn set_redraw_waker<F: Fn() + Send + Sync + 'static>(&self, waker: impl FnOnce() -> F) {
        self.redraw_waker.get_or_init(|| Box::new(waker()));
    }

    fn can_read(state: u8) -> bool {
        state & (WRITER | READER) == 0 && (state & INTENT == 0 || state & REDRAW != 0)
    }

    #[cfg(test)]
    fn read(&self) -> Option<Turn<'_>> {
        self.read_for_frame(true)
    }

    fn read_for_frame(&self, reserve: bool) -> Option<Turn<'_>> {
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if Self::can_read(state) {
                    Some((state | READER) & !(WAITING | REDRAW))
                } else {
                    reserve.then_some(state | WAITING)
                }
            })
            .ok()
            .filter(|&state| Self::can_read(state))
            .map(|_| Turn {
                admission: self,
                reader: true,
            })
    }

    fn cancel_write(&self) {
        self.release(INTENT);
    }

    fn release(&self, flags: u8) -> u8 {
        let redraw_ready = |state| state & WAITING != 0 && Self::can_read(state & !flags);
        let state = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let next = state & !flags;
                Some(if redraw_ready(state) {
                    (next & !WAITING) | REDRAW
                } else {
                    next
                })
            })
            .unwrap();
        if redraw_ready(state) {
            if let Some(wake) = self.redraw_waker.get() {
                wake();
            }
        }
        state
    }

    fn write(&self) -> Result<Turn<'_>, WorkerWait> {
        let mut redraw_until = self.redraw_until.lock().unwrap();
        let state = self.state.fetch_or(INTENT, Ordering::AcqRel);
        let remaining = if state & REDRAW != 0 {
            let now = Instant::now();
            redraw_until
                .get_or_insert(now + REDRAW_TURN)
                .saturating_duration_since(now)
        } else {
            *redraw_until = None;
            Duration::ZERO
        };
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let reserved = state & (WRITER | READER) != 0;
                let redraw = state & REDRAW != 0 && !remaining.is_zero();
                (!reserved && !redraw).then_some((state | WRITER) & !REDRAW)
            })
            .map_err(|state| {
                if state & READER != 0 {
                    WorkerWait::Reader
                } else if !remaining.is_zero() {
                    // A redraw may be dropped while the window is minimized.
                    WorkerWait::Redraw(remaining)
                } else {
                    WorkerWait::Yield
                }
            })?;
        *redraw_until = None;
        Ok(Turn {
            admission: self,
            reader: false,
        })
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let released = if self.reader { READER } else { WRITER | INTENT };
        let state = self.admission.release(released);
        if self.reader && state & INTENT != 0 {
            if let Some(worker) = self.admission.worker.get() {
                worker.unpark();
            }
        }
    }
}

pub(crate) struct WorldView {
    pub world: Arc<RwLock<Simulation>>,
    admission: Admission,
    replaced: AtomicBool,
    render_replaced: AtomicBool,
    completed_step: AtomicU64,
    snapshots: Mutex<RenderMailbox>,
    sample_requested: AtomicBool,
    profile_waits: AtomicBool,
    reader_wait_ns: AtomicU64,
    redraw_wait_ns: AtomicU64,
    max_idle_ns: AtomicU64,
    worker_snapshot_ns: AtomicU64,
    client_snapshot_ns: AtomicU64,
    actor_scans: AtomicU64,
    actor_buffer_grows: AtomicU64,
    actor_pool_bytes: AtomicU64,
}

#[derive(Default)]
struct RenderMailbox {
    source: SnapshotSource,
    request: Option<ViewRequest>,
}

pub(crate) struct ReadLease<'a> {
    guard: RwLockReadGuard<'a, Simulation>,
    _turn: Turn<'a>,
}

struct WriteLease<'a> {
    guard: RwLockWriteGuard<'a, Simulation>,
    _turn: Turn<'a>,
}

impl Deref for ReadLease<'_> {
    type Target = Simulation;

    fn deref(&self) -> &Simulation {
        &self.guard
    }
}

impl WorldView {
    fn new(world: Arc<RwLock<Simulation>>) -> Self {
        Self {
            world,
            admission: Admission::default(),
            replaced: AtomicBool::new(false),
            render_replaced: AtomicBool::new(false),
            completed_step: AtomicU64::new(0),
            snapshots: Mutex::default(),
            sample_requested: AtomicBool::new(false),
            profile_waits: AtomicBool::new(false),
            reader_wait_ns: AtomicU64::new(0),
            redraw_wait_ns: AtomicU64::new(0),
            max_idle_ns: AtomicU64::new(0),
            worker_snapshot_ns: AtomicU64::new(0),
            client_snapshot_ns: AtomicU64::new(0),
            actor_scans: AtomicU64::new(0),
            actor_buffer_grows: AtomicU64::new(0),
            actor_pool_bytes: AtomicU64::new(0),
        }
    }

    // The worker updates this within its write turn, alongside the sampled world.
    pub(crate) fn completed_step(&self) -> u64 {
        self.completed_step.load(Ordering::Relaxed)
    }

    pub(crate) fn enable_wait_profile(&self) {
        self.profile_waits.store(true, Ordering::Relaxed);
    }

    pub(crate) fn work_totals(&self) -> SimWorkTotals {
        SimWorkTotals {
            read_lease_ns: self.reader_wait_ns.load(Ordering::Relaxed),
            redraw_turn_ns: self.redraw_wait_ns.load(Ordering::Relaxed),
            max_idle_ns: self.max_idle_ns.load(Ordering::Relaxed),
            worker_snapshot_ns: self.worker_snapshot_ns.load(Ordering::Relaxed),
            client_snapshot_ns: self.client_snapshot_ns.load(Ordering::Relaxed),
            actor_scans: self.actor_scans.load(Ordering::Relaxed),
            actor_buffer_grows: self.actor_buffer_grows.load(Ordering::Relaxed),
            actor_pool_bytes: self.actor_pool_bytes.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn poll_render(&self, request: Option<ViewRequest>) -> Option<WorldUpdate> {
        let mut mailbox = self.snapshots.try_lock().ok()?;
        if let Some(request) = request {
            mailbox.request = Some(request);
            self.sample_requested.store(true, Ordering::Relaxed);
        }
        mailbox.source.take()
    }

    fn snapshot_timer(&self) -> Option<Instant> {
        self.profile_waits
            .load(Ordering::Relaxed)
            .then(Instant::now)
    }

    fn record_capture(&self, start: Option<Instant>, counter: &AtomicU64, work: CaptureWork) {
        if let Some(start) = start {
            counter.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if work.actor_scan {
                self.actor_scans.fetch_add(1, Ordering::Relaxed);
                self.actor_buffer_grows
                    .fetch_add(work.buffer_grows, Ordering::Relaxed);
                self.actor_pool_bytes
                    .store(work.actor_pool_bytes as u64, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn capture_map(&self, sim: &Simulation, generation: u64) -> u64 {
        let replaced = self.render_replaced.swap(false, Ordering::Relaxed);
        let mut mailbox = self.snapshots.lock().unwrap();
        let start = self.snapshot_timer();
        let epoch = mailbox.source.capture_map(sim, generation, replaced);
        self.record_capture(start, &self.client_snapshot_ns, CaptureWork::default());
        epoch
    }

    pub(crate) fn capture_render(
        &self,
        sim: &Simulation,
        boundary: Boundary,
        request: ViewRequest,
        paused: bool,
        force: bool,
    ) -> Option<WorldUpdate> {
        let mut mailbox = self.snapshots.lock().unwrap();
        mailbox.request = None;
        self.sample_requested.store(false, Ordering::Relaxed);
        let start = self.snapshot_timer();
        let work = mailbox
            .source
            .capture_view(sim, boundary, request, paused, force);
        self.record_capture(start, &self.client_snapshot_ns, work);
        mailbox.source.take()
    }

    fn sample_render(&self, sim: &Simulation, boundary: Boundary) {
        if !self.sample_requested.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut mailbox) = self.snapshots.try_lock() else {
            return;
        };
        let Some(request) = mailbox.request.take() else {
            return;
        };
        self.sample_requested.store(false, Ordering::Relaxed);
        let replaced = self.render_replaced.swap(false, Ordering::Relaxed);
        let start = self.snapshot_timer();
        mailbox
            .source
            .capture_map(sim, boundary.generation, replaced);
        let work = mailbox
            .source
            .capture_view(sim, boundary, request, false, false);
        self.record_capture(start, &self.worker_snapshot_ns, work);
    }

    /// Call with the frame's read lease to observe a replacement before drawing.
    pub(crate) fn take_replaced(&self) -> bool {
        self.replaced.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn set_redraw_waker<F: Fn() + Send + Sync + 'static>(
        &self,
        waker: impl FnOnce() -> F,
    ) {
        self.admission.set_redraw_waker(waker);
    }

    pub(crate) fn try_read(&self) -> Option<ReadLease<'_>> {
        self.try_read_for_frame(true)
    }

    pub(crate) fn try_read_for_frame(&self, reserve: bool) -> Option<ReadLease<'_>> {
        let turn = self.admission.read_for_frame(reserve)?;
        let guard = self.world.try_read().ok()?;
        Some(ReadLease { guard, _turn: turn })
    }

    fn try_write(&self) -> Result<WriteLease<'_>, WorkerWait> {
        let turn = self.admission.write()?;
        let guard = match self.world.try_write() {
            Ok(guard) => guard,
            // A detached import can still be releasing its shared capture
            // guard after a replay ends or an owner transfer is acknowledged.
            Err(TryLockError::WouldBlock) => return Err(WorkerWait::Timeout(EXTERNAL_LOCK_RETRY)),
            Err(TryLockError::Poisoned(_)) => panic!("simulation world poisoned"),
        };
        Ok(WriteLease { guard, _turn: turn })
    }
}

pub(crate) struct ReleasedOwner {
    pub world: Arc<RwLock<Simulation>>,
    pub schedule: SeqSchedule,
    pub boundary: Boundary,
}

pub(crate) struct SimThread {
    generation: u64,
    sequence: u64,
    credits: std::collections::VecDeque<Credit>,
    batch_credits: usize,
    requests: SyncSender<Envelope>,
    replies: Receiver<Reply>,
    status: Arc<Mutex<SimStatus>>,
    view: Arc<WorldView>,
    handle: Option<JoinHandle<(SeqSchedule, Boundary)>>,
    #[cfg(target_arch = "wasm32")]
    inline: Option<InlineWorker>,
}

#[cfg(target_arch = "wasm32")]
struct InlineWorker {
    worker: Worker,
    requests: Receiver<Envelope>,
    replies: SyncSender<Reply>,
}

impl SimThread {
    pub(crate) fn start(
        sim: Simulation,
        schedule: SeqSchedule,
        generation: u64,
        replay: Option<SimulationReplayLoader>,
    ) -> Self {
        let view = Arc::new(WorldView::new(Arc::new(RwLock::new(sim))));
        Self::from_view(view, schedule, generation, replay)
    }

    pub(crate) fn from_world(
        world: Arc<RwLock<Simulation>>,
        schedule: SeqSchedule,
        generation: u64,
    ) -> Self {
        Self::from_world_with_replay(world, schedule, generation, None)
    }

    pub(crate) fn from_world_with_replay(
        world: Arc<RwLock<Simulation>>,
        schedule: SeqSchedule,
        generation: u64,
        replay: Option<SimulationReplayLoader>,
    ) -> Self {
        Self::from_view(
            Arc::new(WorldView::new(world)),
            schedule,
            generation,
            replay,
        )
    }

    fn from_view(
        view: Arc<WorldView>,
        schedule: SeqSchedule,
        generation: u64,
        replay: Option<SimulationReplayLoader>,
    ) -> Self {
        let initial_tick = view.world.try_read().expect("unowned world").get_tick();
        let worker = Worker::new(schedule, generation, initial_tick, replay);
        Self::start_worker(view, worker, 0)
    }

    #[cfg(feature = "multiplayer")]
    pub(crate) fn restore_paused_owner(&mut self) -> Option<Result<(Self, Boundary), ()>> {
        let owner = self.take_owner()?;
        Some(owner.map(|owner| {
            let boundary = owner.boundary;
            let mut worker = Worker::new(owner.schedule, boundary.generation, boundary.tick, None);
            worker.status.boundary = boundary;
            worker.status.owner_mode = OwnerMode::Paused;
            worker.paused = true;
            let view = Arc::new(WorldView::new(owner.world));
            (Self::start_worker(view, worker, self.sequence), boundary)
        }))
    }

    fn start_worker(view: Arc<WorldView>, worker: Worker, sequence: u64) -> Self {
        let generation = worker.status.boundary.generation;
        view.completed_step
            .store(worker.status.boundary.completed_step, Ordering::Relaxed);
        let status = Arc::new(Mutex::new(worker.status.clone()));
        let (requests, input) = mpsc::sync_channel(CREDITS);
        let (output, replies) = mpsc::sync_channel(CREDITS);
        #[cfg(not(target_arch = "wasm32"))]
        let handle = {
            let view = Arc::clone(&view);
            let status = Arc::clone(&status);
            Some(
                thread::Builder::new()
                    .name("simulation".into())
                    .spawn(move || worker.run(&view, &status, input, output))
                    .expect("start simulation worker"),
            )
        };
        #[cfg(target_arch = "wasm32")]
        let handle = None;
        Self {
            generation,
            sequence,
            credits: Default::default(),
            batch_credits: BATCH_CREDITS,
            requests,
            replies,
            status,
            view,
            handle,
            #[cfg(target_arch = "wasm32")]
            inline: Some(InlineWorker {
                worker,
                requests: input,
                replies: output,
            }),
        }
    }

    pub(crate) fn view(&self) -> Arc<WorldView> {
        Arc::clone(&self.view)
    }

    pub(crate) fn status(&self) -> Option<SimStatus> {
        self.status.try_lock().ok().map(|status| status.clone())
    }

    pub(crate) fn submit(&mut self, request: Request) -> Result<BatchId, SubmitErrorWithRequest> {
        let id = BatchId {
            generation: self.generation,
            sequence: self.sequence + 1,
        };
        self.try_submit(id, request)?;
        Ok(id)
    }

    pub(crate) fn try_submit(
        &mut self,
        id: BatchId,
        request: Request,
    ) -> Result<(), SubmitErrorWithRequest> {
        if id.generation != self.generation || id.sequence != self.sequence + 1 {
            return Err(SubmitErrorWithRequest(SubmitError::Stale, request));
        }
        if self.is_finished() {
            return Err(SubmitErrorWithRequest(SubmitError::Closed, request));
        }
        let batch = matches!(&request, Request::Batch(_));
        let reserved = matches!(
            &request,
            Request::PauseFence { .. }
                | Request::Stop
                | Request::ReleaseOwner
                | Request::ReplayControl(ReplayControl::End)
        );
        let available = self.batch_credits + if reserved { CONTROL_CREDITS } else { 0 };
        if self.credits.len() >= available
            || (batch
                && self.credits.iter().filter(|credit| credit.batch).count() >= self.batch_credits)
        {
            return Err(SubmitErrorWithRequest(SubmitError::Full, request));
        }
        if let Request::Batch(commands) = &request {
            if !can_submit_batch(commands) {
                return Err(SubmitErrorWithRequest(SubmitError::Oversized, request));
            }
        }
        match self.requests.try_send(Envelope { id, request }) {
            Ok(()) => {
                self.sequence = id.sequence;
                self.credits.push_back(Credit { id, batch });
                if let Some(handle) = &self.handle {
                    handle.thread().unpark();
                }
                Ok(())
            }
            Err(TrySendError::Full(envelope)) => {
                Err(SubmitErrorWithRequest(SubmitError::Full, envelope.request))
            }
            Err(TrySendError::Disconnected(envelope)) => Err(SubmitErrorWithRequest(
                SubmitError::Closed,
                envelope.request,
            )),
        }
    }

    pub(crate) fn poll_reply(&mut self) -> Option<Reply> {
        let reply = self.replies.try_recv().ok()?;
        let index = self
            .credits
            .iter()
            .position(|credit| credit.id == reply.id)
            .expect("reply without credit");
        self.credits.remove(index);
        Some(reply)
    }

    pub(crate) fn is_finished(&self) -> bool {
        #[cfg(target_arch = "wasm32")]
        return self.inline.as_ref().is_none_or(|inline| inline.worker.exit);
        #[cfg(not(target_arch = "wasm32"))]
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn take_owner(&mut self) -> Option<Result<ReleasedOwner, ()>> {
        if !self.is_finished() {
            return None;
        }
        #[cfg(target_arch = "wasm32")]
        let owner = self
            .inline
            .take()
            .map(|inline| Ok((inline.worker.schedule, inline.worker.status.boundary)));
        #[cfg(not(target_arch = "wasm32"))]
        let owner = self
            .handle
            .take()
            .map(|handle| handle.join().map_err(|_| ()));
        owner.map(|owner| {
            owner.map(|(schedule, boundary)| ReleasedOwner {
                world: self.view.world.clone(),
                schedule,
                boundary,
            })
        })
    }

    pub(crate) fn pump_inline(&mut self) {
        #[cfg(target_arch = "wasm32")]
        if let Some(inline) = self.inline.as_mut() {
            let worker = &mut inline.worker;
            if !worker.exit {
                worker.run_one(&self.view, &self.status, &inline.requests, &inline.replies);
            }
        }
    }
}

struct LimitedWriter(usize);

impl Write for LimitedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() > self.0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.0 -= data.len();
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn fits(commands: &WorldCommands) -> bool {
    commands.as_ref().len() <= BATCH_COMMANDS
        && work_fits(commands.iter(), BATCH_WORK, false)
        && Bincode::encode_writer(commands, LimitedWriter(BATCH_BYTES)).is_ok()
}

pub(crate) fn can_submit_batch(commands: &WorldCommands) -> bool {
    fits(commands) || is_background_import(commands)
}

fn is_background_import(commands: &WorldCommands) -> bool {
    if cfg!(target_arch = "wasm32") {
        return false;
    }
    match commands.as_ref() {
        [WorldCommand::MapLoadParis] => true,
        [WorldCommand::MapLoadTestField { size, .. }] => *size <= 100 && !fits(commands),
        _ => false,
    }
}

fn is_background_replay_import(commands: &[(prototypes::Tick, WorldCommand)]) -> bool {
    !cfg!(target_arch = "wasm32")
        && commands.iter().any(|(_, command)| {
            matches!(
                command,
                WorldCommand::MapLoadParis | WorldCommand::MapLoadTestField { .. }
            )
        })
}

pub(crate) fn fits_replay_batch(commands: &[(prototypes::Tick, WorldCommand)]) -> bool {
    commands.len() <= REPLAY_COMMANDS
        && work_fits(
            commands.iter().map(|(_, command)| command),
            REPLAY_WORK,
            true,
        )
        && Bincode::encode_writer(&commands, LimitedWriter(REPLAY_BYTES)).is_ok()
}

// Include generated objects and indexed outcomes as well as the command itself.
fn work_fits<'a>(
    commands: impl Iterator<Item = &'a WorldCommand>,
    limit: usize,
    replay: bool,
) -> bool {
    let mut remaining = limit;
    for command in commands {
        use WorldCommand::*;
        let expanded = match command {
            // An Init in a playback batch is rejected without bootstrapping.
            Init(_) if !replay => return false,
            SpawnRandomCars { n_cars } if replay || *n_cars <= 512 => *n_cars,
            AddTrain { n_wagons, .. } if replay || *n_wagons <= 128 => *n_wagons as usize,
            SpawnTrain { wagons, .. } if replay || wagons.len() <= 128 => wagons.len(),
            SpawnRandomCars { .. } | AddTrain { .. } | SpawnTrain { .. } => return false,
            MapLoadParis => 11_348 + 17_958,
            MapLoadTestField { size, .. } => {
                let size = *size as usize;
                size.saturating_mul(size.saturating_add(size.saturating_sub(1).saturating_mul(2)))
            }
            MapMakeConnection { pat, .. } => pat.lanes().count(),
            MapMakeMultipleConnections(projects, links) => {
                if projects.len().saturating_add(links.len()) > remaining {
                    return false;
                }
                links.iter().fold(projects.len(), |work, (_, _, _, pat)| {
                    work.saturating_add(2).saturating_add(pat.lanes().count())
                })
            }
            Terraform { radius, .. } => ((radius / 100.0).powi(2) as usize).min(400),
            ZonePaint { area, .. } => {
                // Cells are 8 m square. Charge for the footprint and cap the
                // cost of shapes that the map will reject before applying.
                ((area.axis()[0].mag() * area.axis()[1].mag() / 256.0) as usize).min(625)
            }
            UpdateZone { zone, .. }
            | MapBuildSpecialBuilding {
                zone: Some(zone), ..
            } => zone.poly.0.len(),
            TransitCreateLine { stops } | TransitEditLine { stops, .. } => stops.len(),
            // These can scan or rebuild a significant part of the existing map.
            SetLotLayout(_) | EditWater(_) => BATCH_WORK / 2,
            _ => 0,
        };
        let Some(left) = remaining.checked_sub(expanded.saturating_add(1)) else {
            return false;
        };
        remaining = left;
    }
    true
}

enum ImportKind {
    Live {
        id: BatchId,
        commands: WorldCommands,
    },
    Replay(ReplayOperation),
}

struct PendingImport {
    kind: ImportKind,
    result: Receiver<Result<(Simulation, Duration), String>>,
    prepared: Option<(Simulation, Duration)>,
    cancelled: Arc<AtomicBool>,
}

impl PendingImport {
    fn live_id(&self) -> Option<BatchId> {
        match self.kind {
            ImportKind::Live { id, .. } => Some(id),
            ImportKind::Replay(_) => None,
        }
    }
}

impl Drop for PendingImport {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct Worker {
    schedule: SeqSchedule,
    status: SimStatus,
    clock: Clock,
    paused: bool,
    replay: Option<SimulationReplayLoader>,
    replay_budget: Option<ReplayBudget>,
    replay_speed: usize,
    replay_step: Option<BatchId>,
    replay_cancelled: Option<BatchId>,
    replay_due: Instant,
    explicit_steps: Option<(BatchId, usize)>,
    fixture: Option<(BatchId, FixtureOp)>,
    import: Option<PendingImport>,
    pending: Option<Envelope>,
    waiting: VecDeque<Envelope>,
    exit: bool,
    next_sample: Instant,
}

impl Worker {
    fn new(
        schedule: SeqSchedule,
        generation: u64,
        tick: u64,
        replay: Option<SimulationReplayLoader>,
    ) -> Self {
        let now = Instant::now();
        let replay_progress = replay.as_ref().map(Self::replay_progress);
        Self {
            schedule,
            status: SimStatus {
                boundary: Boundary {
                    generation,
                    completed_step: 0,
                    last_batch: None,
                    tick,
                },
                owner_mode: if replay.is_some() {
                    OwnerMode::Replay
                } else {
                    OwnerMode::Live
                },
                achieved_warp: None,
                last_tick_duration: Duration::ZERO,
                schedule_samples: Arc::from([]),
                replay_progress,
            },
            clock: Clock::new(now),
            paused: false,
            replay,
            replay_budget: None,
            replay_speed: 0,
            replay_step: None,
            replay_cancelled: None,
            replay_due: now,
            explicit_steps: None,
            fixture: None,
            import: None,
            pending: None,
            waiting: VecDeque::new(),
            exit: false,
            next_sample: now,
        }
    }

    fn replay_progress(replay: &SimulationReplayLoader) -> ReplayProgress {
        ReplayProgress {
            steps: replay.step_progress(),
            tick: replay.pastt.0,
            last_tick: replay.replay.last_tick_recorded.0,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run(
        mut self,
        view: &WorldView,
        status: &Mutex<SimStatus>,
        input: Receiver<Envelope>,
        output: SyncSender<Reply>,
    ) -> (SeqSchedule, Boundary) {
        let _ = view.admission.worker.set(thread::current());
        while !self.exit {
            let wait = self.run_one(view, status, &input, &output);
            if matches!(wait, WorkerWait::Continue) {
                continue;
            }
            let max = self.live_max();
            let counter = match wait {
                WorkerWait::Reader => Some(&view.reader_wait_ns),
                WorkerWait::Redraw(_) => Some(&view.redraw_wait_ns),
                _ if max => Some(&view.max_idle_ns),
                _ => None,
            };
            let started = counter
                .filter(|_| view.profile_waits.load(Ordering::Relaxed))
                .map(|counter| (counter, Instant::now()));
            match wait {
                WorkerWait::Yield => thread::yield_now(),
                WorkerWait::Timeout(delay) | WorkerWait::Redraw(delay) => {
                    thread::park_timeout(delay)
                }
                WorkerWait::Reader => thread::park(),
                WorkerWait::Continue => unreachable!(),
            }
            if let Some((counter, started)) = started {
                let nanos = started.elapsed().as_nanos() as u64;
                counter.fetch_add(nanos, Ordering::Relaxed);
                if max && matches!(wait, WorkerWait::Reader | WorkerWait::Redraw(_)) {
                    view.max_idle_ns.fetch_add(nanos, Ordering::Relaxed);
                }
            }
        }
        (self.schedule, self.status.boundary)
    }

    fn live_max(&self) -> bool {
        !self.paused && self.replay.is_none() && self.clock.warp == MAX_WARP
    }

    fn run_one(
        &mut self,
        view: &WorldView,
        status: &Mutex<SimStatus>,
        input: &Receiver<Envelope>,
        output: &SyncSender<Reply>,
    ) -> WorkerWait {
        let now = Instant::now();
        self.clock.update(now);
        let live_import = self.import.as_ref().and_then(PendingImport::live_id);
        if self.explicit_steps.is_some() || self.fixture.is_some() || live_import.is_some() {
            // Reply credits also bound this buffer. Keep earlier requests in
            // FIFO order while finding a terminal request behind the long run.
            self.waiting.extend(input.try_iter());
            if self
                .waiting
                .iter()
                .any(|envelope| matches!(envelope.request, Request::Stop | Request::ReleaseOwner))
            {
                let id = self
                    .explicit_steps
                    .take()
                    .map(|(id, _)| id)
                    .or_else(|| self.fixture.take().map(|(id, _)| id))
                    .or_else(|| {
                        self.import = None;
                        live_import
                    })
                    .unwrap();
                view.admission.cancel_write();
                self.send_reply(
                    output,
                    Reply {
                        id,
                        kind: ReplyKind::Interrupted(self.status.boundary),
                    },
                );
                return WorkerWait::Yield;
            }
            if live_import.is_some() {
                return self.poll_import(view, status, output);
            }
        } else if self.pending.is_none() {
            self.pending = self.waiting.pop_front();
            if self.pending.is_none() {
                match input.try_recv() {
                    Ok(envelope) => self.pending = Some(envelope),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.exit = true;
                        return WorkerWait::Yield;
                    }
                }
            }
        }
        let replay_ready = self.replay.is_some()
            && !self.paused
            && (self.replay_budget.is_some() || (self.replay_speed > 0 && now >= self.replay_due));
        if self.pending.is_none()
            && self.explicit_steps.is_none()
            && self.fixture.is_none()
            && !replay_ready
            && (self.paused || self.replay.is_some() || !self.clock.due())
        {
            view.admission.cancel_write();
            if self.replay.is_some() && !self.paused && self.replay_speed > 0 {
                return WorkerWait::Timeout(self.replay_due.saturating_duration_since(now));
            }
            let delay = if self.paused || self.replay.is_some() {
                Duration::from_millis(20)
            } else {
                self.clock.wait()
            };
            return WorkerWait::Timeout(delay);
        }
        if self.replay.is_none()
            && self.pending.as_ref().is_some_and(|pending| {
                matches!(&pending.request, Request::Batch(commands) if is_background_import(commands))
            })
        {
            view.admission.cancel_write();
            let Envelope { id, request } = self.pending.take().unwrap();
            let Request::Batch(commands) = request else { unreachable!() };
            self.import = Some(Self::start_import(
                view,
                commands.clone(),
                ImportKind::Live { id, commands },
            ));
            return WorkerWait::Yield;
        }
        if self.pending.is_none() && replay_ready {
            if let Some(wait) = self.advance_replay_import(view, status, output) {
                return wait;
            }
        }

        // Timing controls and stop/fence do not need the world. In particular,
        // stopping never depends on a redraw consuming a reserved reader turn.
        let world_required = self.pending.as_ref().is_none_or(|envelope| {
            matches!(
                envelope.request,
                Request::Batch(_) | Request::Capture { .. } | Request::CaptureComparison { .. }
            )
        });
        let reply = if world_required {
            let mut lease = match view.try_write() {
                Ok(lease) => lease,
                Err(wait) => return wait,
            };
            let reply = if let Some(envelope) = self.pending.take() {
                self.handle(envelope, Some(&mut lease.guard))
            } else {
                self.advance(&mut lease.guard, now, input)
            };
            view.completed_step
                .store(self.status.boundary.completed_step, Ordering::Relaxed);
            if self.live_max() {
                view.sample_render(&lease.guard, self.status.boundary);
            }
            reply
        } else {
            view.admission.cancel_write();
            let envelope = self.pending.take().unwrap();
            self.handle(envelope, None)
        };
        self.status.achieved_warp = self.clock.achieved.map(|rate| (self.clock.warp, rate));
        if now >= self.next_sample {
            self.status.schedule_samples = self.schedule.times().into();
            self.next_sample = now + Duration::from_millis(250);
        }
        *status.lock().unwrap() = self.status.clone();
        let cancelled = self.replay_cancelled.take().map(|id| Reply {
            id,
            kind: ReplyKind::Rejected("The replay step was cancelled."),
        });
        for reply in cancelled.into_iter().chain(reply) {
            self.send_reply(output, reply);
        }
        if cfg!(not(target_arch = "wasm32")) && self.live_max() && !self.exit {
            WorkerWait::Continue
        } else {
            WorkerWait::Yield
        }
    }

    fn terminal_waiting(waiting: &mut VecDeque<Envelope>, input: &Receiver<Envelope>) -> bool {
        waiting.extend(input.try_iter());
        waiting
            .iter()
            .any(|envelope| matches!(envelope.request, Request::Stop | Request::ReleaseOwner))
    }

    fn advance_replay_import(
        &mut self,
        view: &WorldView,
        status: &Mutex<SimStatus>,
        output: &SyncSender<Reply>,
    ) -> Option<WorkerWait> {
        let replay = self.replay.as_ref().unwrap();
        let budget = self
            .replay_budget
            .get_or_insert_with(|| ReplayBudget::new(self.replay_speed));
        let ReplayNext::Run(operation) =
            replay.next_operation(prototypes::Tick(self.status.boundary.tick), budget)
        else {
            return None;
        };
        if let Some(pending) = &self.import {
            // Pause and other controls can change the budget while the job runs.
            // Only publish when the current budget still wants this operation.
            return matches!(pending.kind, ImportKind::Replay(staged) if staged == operation)
                .then(|| self.poll_import(view, status, output));
        }
        let commands = replay.commands_for(operation);
        if !is_background_replay_import(commands) {
            return None;
        }
        view.admission.cancel_write();
        let commands: Vec<_> = commands
            .iter()
            .map(|(_, command)| command.clone())
            .collect();
        self.import = Some(Self::start_import(
            view,
            WorldCommands::from(commands),
            ImportKind::Replay(operation),
        ));
        Some(WorkerWait::Yield)
    }

    fn start_import(view: &WorldView, commands: WorldCommands, kind: ImportKind) -> PendingImport {
        let advanced = match kind {
            ImportKind::Live { .. } => true,
            ImportKind::Replay(operation) => operation.advances(),
        };
        let source = view.world.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let is_cancelled = cancelled.clone();
        let (done, result) = mpsc::sync_channel(1);
        let job = thread::Builder::new()
            .name("map import".into())
            .spawn(move || {
                let prepare = || -> Result<_, String> {
                    // This is an immutable capture concurrent with frame reads.
                    // Release the source before constructing or importing the copy.
                    let (snapshot, restore_traffic) = {
                        let sim = source.read().map_err(|err| err.to_string())?;
                        if is_cancelled.load(Ordering::Acquire) {
                            return Err("Map import cancelled.".into());
                        }
                        let snapshot = Bincode::encode(&*sim).map_err(|err| err.to_string())?;
                        let restore_traffic = sim.read::<RoadThroughput>().for_map_copy(&sim.map());
                        (snapshot, restore_traffic)
                    };
                    drop(source);
                    if is_cancelled.load(Ordering::Acquire) {
                        return Err("Map import cancelled.".into());
                    }
                    let mut sim: Simulation =
                        Bincode::decode(&snapshot).map_err(|err| err.to_string())?;
                    drop(snapshot);
                    if is_cancelled.load(Ordering::Acquire) {
                        return Err("Map import cancelled.".into());
                    }
                    let traffic = restore_traffic(&sim.map());
                    sim.insert(traffic);
                    let elapsed = if advanced {
                        sim.tick(&mut Simulation::schedule(), commands.as_ref())
                    } else {
                        sim.apply_commands(commands.as_ref());
                        Duration::ZERO
                    };
                    Ok((sim, elapsed))
                };
                let _ = done.send(prepare());
            });
        if let Err(error) = job {
            log::error!("Could not start map import: {error}");
        }
        PendingImport {
            kind,
            result,
            prepared: None,
            cancelled,
        }
    }

    fn poll_import(
        &mut self,
        view: &WorldView,
        status: &Mutex<SimStatus>,
        output: &SyncSender<Reply>,
    ) -> WorkerWait {
        let pending = self.import.as_mut().unwrap();
        if pending.prepared.is_none() {
            let result = match pending.result.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => {
                    return WorkerWait::Timeout(Duration::from_millis(2));
                }
                Err(mpsc::TryRecvError::Disconnected) => Err("Map import worker stopped.".into()),
            };
            match result {
                Ok(prepared) => pending.prepared = Some(prepared),
                Err(error) => {
                    log::error!("Map import preparation failed: {error}");
                    let import = self.import.take().unwrap();
                    let reply_id = match import.kind {
                        ImportKind::Live { id, .. } => Some(id),
                        ImportKind::Replay(_) => {
                            let step = self.replay_step.take();
                            self.end_replay();
                            *status.lock().unwrap() = self.status.clone();
                            step
                        }
                    };
                    if let Some(id) = reply_id {
                        self.send_reply(
                            output,
                            Reply {
                                id,
                                kind: ReplyKind::Rejected("Map import preparation failed."),
                            },
                        );
                    }
                    return WorkerWait::Yield;
                }
            }
        }
        let mut published = match view.try_write() {
            Ok(lease) => lease,
            Err(wait) => return wait,
        };
        let mut pending = self.import.take().unwrap();
        let (sim, elapsed) = pending.prepared.take().unwrap();
        let reply = match &mut pending.kind {
            ImportKind::Live { id, commands } => {
                self.clock.clear();
                self.status.boundary.completed_step += 1;
                self.status.boundary.tick = sim.get_tick();
                self.status.last_tick_duration = elapsed;
                Some(Reply {
                    id: *id,
                    kind: self.applied_batch(&sim, *id, std::mem::take(commands), true),
                })
            }
            ImportKind::Replay(operation) => {
                let tick = prototypes::Tick(sim.get_tick());
                self.replay.as_mut().unwrap().commit_operation(
                    *operation,
                    tick,
                    self.replay_budget.as_mut().unwrap(),
                );
                self.record_replay_operation(tick.0, operation.advances(), elapsed);
                None
            }
        };
        let old = std::mem::replace(&mut *published.guard, sim);
        view.completed_step
            .store(self.status.boundary.completed_step, Ordering::Relaxed);
        view.replaced.store(true, Ordering::Release);
        view.render_replaced.store(true, Ordering::Relaxed);
        drop(published);
        *status.lock().unwrap() = self.status.clone();
        if let Some(reply) = reply {
            self.send_reply(output, reply);
        }
        drop(old);
        WorkerWait::Yield
    }

    fn send_reply(&mut self, output: &SyncSender<Reply>, reply: Reply) {
        // A request occupies a reply credit until the receiver polls it.
        match output.try_send(reply) {
            Ok(()) => {}
            Err(TrySendError::Disconnected(_)) => self.exit = true,
            Err(TrySendError::Full(_)) => panic!("simulation reply credit exhausted"),
        }
    }

    fn handle(&mut self, envelope: Envelope, sim: Option<&mut Simulation>) -> Option<Reply> {
        let Envelope { id, request } = envelope;
        let kind = match request {
            Request::Batch(commands) if !commands.is_empty() && self.replay.is_none() => {
                let sim = sim.unwrap();
                let advanced = !commands.iter().all(WorldCommand::is_instant);
                if advanced {
                    self.tick(sim, &commands);
                } else {
                    sim.apply_commands(commands.as_ref());
                    self.status.boundary.tick = sim.get_tick();
                }
                self.applied_batch(sim, id, commands, advanced)
            }
            Request::Batch(_) => ReplyKind::Rejected("The world cannot apply that batch."),
            Request::SetWarp(warp) => {
                self.clock.set_warp(warp.min(MAX_WARP));
                ReplyKind::WarpSet
            }
            Request::PauseFence { through_batch }
                if through_batch == self.status.boundary.last_batch =>
            {
                self.paused = true;
                self.clock.clear();
                self.status.owner_mode = OwnerMode::Paused;
                ReplyKind::Paused(self.status.boundary)
            }
            Request::PauseFence { .. } => {
                ReplyKind::Rejected("The pause fence did not match the last applied batch.")
            }
            Request::Resume => {
                self.paused = false;
                self.clock.clear();
                self.status.owner_mode = if self.replay.is_some() {
                    OwnerMode::Replay
                } else {
                    OwnerMode::Live
                };
                ReplyKind::Resumed
            }
            Request::Step { count: 0 } => ReplyKind::Stepped(self.status.boundary),
            Request::Step { count } if self.replay.is_none() && self.clock.warp == 0 => {
                self.explicit_steps = Some((id, count));
                return None;
            }
            Request::Step { .. } => {
                ReplyKind::Rejected("The simulation must be paused before stepping.")
            }
            Request::PausedFixtureOp(op) if self.replay.is_none() && self.clock.warp == 0 => {
                self.fixture = Some((id, op));
                return None;
            }
            Request::PausedFixtureOp(_) => {
                ReplyKind::Rejected("The simulation must be paused for a fixture.")
            }
            Request::Capture {
                boundary,
                revision,
                permit,
            } => {
                let sim = sim.unwrap();
                ReplyKind::Captured(if permit.as_ref().is_some_and(SavePermit::cancelled) {
                    Err("The save was cancelled.".into())
                } else if boundary
                    .is_some_and(|expected| expected != self.status.boundary || !self.paused)
                {
                    Err("The world moved past the requested save boundary.".into())
                } else {
                    sim.capture_slot(revision)
                        .map(|snapshot| (snapshot, CityProgress::capture(sim)))
                        .map_err(|err| err.to_string())
                })
            }
            Request::CaptureComparison { revision, limit } => {
                let sim = sim.unwrap();
                ReplyKind::Captured(
                    sim.capture_slot_with_limit(revision, limit)
                        .map(|snapshot| (snapshot, CityProgress::capture(sim)))
                        .map_err(|err| err.to_string()),
                )
            }
            Request::ReplayControl(control) => {
                if self.replay.is_none() {
                    ReplyKind::Rejected("There is no active replay.")
                } else {
                    self.replay_budget = None;
                    self.replay_cancelled = self.replay_step.take();
                    self.replay_speed = 0;
                    self.replay_due = Instant::now();
                    match control {
                        ReplayControl::Speed(speed) => self.replay_speed = speed,
                        ReplayControl::Step(count) => {
                            self.replay_budget = Some(ReplayBudget::new(count));
                            self.replay_step = Some(id);
                            return None;
                        }
                        ReplayControl::End => self.end_replay(),
                    }
                    ReplyKind::ReplayChanged
                }
            }
            Request::ReleaseOwner => {
                self.import = None;
                self.replay_cancelled = self.replay_step.take();
                self.exit = true;
                self.status.owner_mode = OwnerMode::Transferring;
                ReplyKind::Releasing(self.status.boundary)
            }
            Request::Stop => {
                self.import = None;
                self.replay_cancelled = self.replay_step.take();
                self.exit = true;
                self.status.owner_mode = OwnerMode::Stopped;
                ReplyKind::Stopping
            }
        };
        Some(Reply { id, kind })
    }

    fn applied_batch(
        &mut self,
        sim: &Simulation,
        id: BatchId,
        commands: WorldCommands,
        advanced: bool,
    ) -> ReplyKind {
        self.status.boundary.last_batch = Some(id);
        let outcomes = sim.read::<CommandOutcomes>();
        ReplyKind::Applied(AppliedBatch {
            id,
            boundary: self.status.boundary,
            advanced,
            outcome_sequence: outcomes.batch_seq(),
            accepted_indices: outcomes.accepted.clone(),
            rejections: (0..commands.as_ref().len())
                .flat_map(|i| {
                    outcomes
                        .rejections_for(i)
                        .iter()
                        .map(move |&(label, reason)| (i, label, reason))
                })
                .collect(),
            link_outcomes: (0..commands.as_ref().len())
                .flat_map(|i| outcomes.link_outcomes(i).iter().map(move |&link| (i, link)))
                .collect(),
            water_edits: (0..commands.as_ref().len())
                .filter_map(|i| outcomes.water_edit(i).cloned().map(|result| (i, result)))
                .collect(),
            tile_purchases: TilePurchaseReceipt::from_outcomes(
                id.generation,
                self.status.boundary.tick,
                &outcomes,
                commands.iter().enumerate(),
            )
            .collect(),
            traffic_edits: commands.iter().enumerate().filter_map(|(index, command)| {
                TrafficEditReceipt::from_outcome(id.generation, self.status.boundary.tick, None,
                    index, true, index, command, &outcomes)
            }).collect(),
            commands,
        })
    }

    fn advance(
        &mut self,
        sim: &mut Simulation,
        now: Instant,
        input: &Receiver<Envelope>,
    ) -> Option<Reply> {
        if let Some((id, mut op)) = self.fixture.take() {
            let mut steps = 0;
            let mut interrupted = false;
            let waiting = &mut self.waiting;
            let result = op.run(sim, &mut self.schedule, &mut steps, &mut || {
                interrupted = Self::terminal_waiting(waiting, input);
                interrupted
            });
            self.status.boundary.completed_step += steps;
            self.status.boundary.tick = sim.get_tick();
            if interrupted {
                return Some(Reply {
                    id,
                    kind: ReplyKind::Interrupted(self.status.boundary),
                });
            }
            if let Some(result) = result {
                return Some(Reply {
                    id,
                    kind: ReplyKind::FixtureDone(result, self.status.boundary),
                });
            }
            self.tick(sim, &WorldCommands::default());
            self.fixture = Some((id, op));
            return None;
        }
        if let Some((id, count)) = self.explicit_steps {
            self.tick(sim, &WorldCommands::default());
            self.explicit_steps = (count > 1).then(|| (id, count - 1));
            return (count == 1).then(|| Reply {
                id,
                kind: ReplyKind::Stepped(self.status.boundary),
            });
        }
        if let Some(replay) = &mut self.replay {
            let start = Instant::now();
            let budget = self
                .replay_budget
                .get_or_insert_with(|| ReplayBudget::new(self.replay_speed));
            let advance = replay.advance_one(sim, &mut self.schedule, budget);
            match advance {
                ReplayAdvance::Operation { tick, advanced, .. } => {
                    self.record_replay_operation(tick.0, advanced, start.elapsed());
                }
                ReplayAdvance::Settled { complete } => {
                    self.replay_budget = None;
                    self.replay_due = now
                        + if self.replay_speed >= MAX_WARP as usize {
                            Duration::ZERO
                        } else {
                            debug_up_dt()
                        };
                    let step = self.replay_step.take();
                    if complete {
                        self.end_replay();
                    }
                    return step.map(|id| Reply {
                        id,
                        kind: ReplyKind::ReplayStepped(self.status.boundary),
                    });
                }
            }
        } else {
            self.tick(sim, &WorldCommands::default());
        }
        None
    }

    fn record_replay_operation(&mut self, tick: u64, advanced: bool, elapsed: Duration) {
        self.status.replay_progress = self.replay.as_ref().map(Self::replay_progress);
        self.status.boundary.tick = tick;
        if advanced {
            self.status.boundary.completed_step += 1;
            self.status.last_tick_duration = elapsed;
        }
    }

    fn end_replay(&mut self) {
        self.import = None;
        self.replay = None;
        self.replay_budget = None;
        self.status.replay_progress = None;
        self.status.owner_mode = if self.paused {
            OwnerMode::Paused
        } else {
            OwnerMode::Live
        };
        self.clock.clear();
    }

    fn tick(&mut self, sim: &mut Simulation, commands: &WorldCommands) {
        self.clock.consume_due();
        self.status.last_tick_duration = sim.tick(&mut self.schedule, commands.as_ref());
        self.status.boundary.completed_step += 1;
        self.status.boundary.tick = sim.get_tick();
        self.clock.completed += 1;
    }
}

struct Clock {
    last: Instant,
    warp: u32,
    debt: Duration,
    sample_start: Instant,
    completed: u64,
    achieved: Option<f64>,
}

impl Clock {
    fn new(now: Instant) -> Self {
        Self {
            last: now,
            warp: 0,
            debt: Duration::ZERO,
            sample_start: now,
            completed: 0,
            achieved: None,
        }
    }

    fn set_warp(&mut self, warp: u32) {
        if self.warp != warp {
            self.warp = warp;
            self.clear();
        }
    }

    fn clear(&mut self) {
        self.last = Instant::now();
        self.sample_start = self.last;
        self.debt = Duration::ZERO;
        self.completed = 0;
        self.achieved = None;
    }

    fn period(&self) -> Duration {
        debug_up_dt() / self.warp.max(1)
    }

    fn update(&mut self, now: Instant) {
        if self.warp != 0 {
            self.debt = (self.debt + now.saturating_duration_since(self.last))
                .min(Duration::from_millis(250));
            let sample = now.saturating_duration_since(self.sample_start);
            if sample >= Duration::from_secs(1) {
                self.achieved = Some(
                    self.completed as f64 * debug_up_dt().as_secs_f64() / sample.as_secs_f64(),
                );
                self.completed = 0;
                self.sample_start = now;
            }
        }
        self.last = now;
    }

    fn due(&self) -> bool {
        self.warp == MAX_WARP || self.warp != 0 && self.debt >= self.period()
    }

    fn wait(&self) -> Duration {
        if self.warp == 0 {
            Duration::from_millis(20)
        } else {
            self.period().saturating_sub(self.debt)
        }
    }

    fn consume_due(&mut self) {
        if self.due() {
            self.debt = self.debt.saturating_sub(self.period());
        }
    }
}

#[cfg(test)]
mod tests;
