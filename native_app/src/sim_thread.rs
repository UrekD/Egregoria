use std::io::{self, Write};
use std::ops::Deref;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::saveload::{Bincode, Encoder, SnapshotRevision};
use common::timestep::{debug_up_dt, MAX_WARP};
use simulation::map::{WaterEditError, WaterEditOutcome};
use simulation::utils::replay::{ReplayAdvance, ReplayBudget};
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::{
    CommandOutcomes, LinkOutcome, Rejection, WorldCommand, WorldCommands,
};
use simulation::{EncodedSlot, Simulation, SimulationReplayLoader};

use crate::automation::{FixtureOp, FixtureOutcome};
use crate::session::{save_coordinator::SavePermit, CityProgress};

pub(crate) const BATCH_BYTES: usize = 256 * 1024;
const BATCH_COMMANDS: usize = 256;
const BATCH_CREDITS: usize = 8;
// One extra credit is always available to fence or stop a full batch queue.
const CONTROL_CREDITS: usize = 1;
const CREDITS: usize = BATCH_CREDITS + CONTROL_CREDITS;
const CONTENDED_RETRY: Duration = Duration::from_micros(250);
const REDRAW_TURN: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BatchId {
    pub generation: u64,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Boundary {
    pub generation: u64,
    pub completed_step: u64,
    pub last_batch: Option<BatchId>,
    pub tick: u64,
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
    Captured(Result<(EncodedSlot, CityProgress), String>),
    ReplayChanged,
    ReplayStepped(Boundary),
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

impl Admission {
    fn can_read(state: u8) -> bool {
        state & (WRITER | READER) == 0 && (state & INTENT == 0 || state & REDRAW != 0)
    }

    fn signal_redraw(&self) -> bool {
        let blocked = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (!Self::can_read(state)).then_some(state | WAITING)
            });
        blocked.is_err()
    }

    fn read(&self) -> Option<Turn<'_>> {
        let state = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some(if Self::can_read(state) {
                    (state | READER) & !(WAITING | REDRAW)
                } else {
                    state | WAITING
                })
            })
            .unwrap();
        Self::can_read(state).then(|| Turn {
            admission: self,
            reader: true,
        })
    }

    fn cancel_write(&self) {
        self.state.fetch_and(!INTENT, Ordering::Release);
    }

    fn write(&self) -> Option<Turn<'_>> {
        let mut redraw_until = self.redraw_until.lock().unwrap();
        let state = self.state.fetch_or(INTENT, Ordering::AcqRel);
        let allow_redraw = if state & REDRAW != 0 {
            Instant::now() < *redraw_until.get_or_insert_with(|| Instant::now() + REDRAW_TURN)
        } else {
            *redraw_until = None;
            false
        };
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let reserved = state & (WRITER | READER) != 0;
                let redraw = state & REDRAW != 0 && allow_redraw;
                (!reserved && !redraw).then_some((state | WRITER) & !REDRAW)
            })
            .ok()?;
        *redraw_until = None;
        Some(Turn {
            admission: self,
            reader: false,
        })
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if self.reader {
            self.admission.state.fetch_and(!READER, Ordering::Release);
        } else {
            self.admission
                .state
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                    let redraw = if state & WAITING != 0 { REDRAW } else { 0 };
                    Some((state & !(WRITER | INTENT | WAITING)) | redraw)
                })
                .unwrap();
        }
    }
}

pub(crate) struct WorldView {
    pub world: Arc<RwLock<Simulation>>,
    admission: Arc<Admission>,
}

pub(crate) struct ReadLease<'a> {
    guard: RwLockReadGuard<'a, Simulation>,
    _turn: Turn<'a>,
}

impl Deref for ReadLease<'_> {
    type Target = Simulation;

    fn deref(&self) -> &Simulation {
        &self.guard
    }
}

impl WorldView {
    pub(crate) fn signal_redraw(&self) -> bool {
        self.admission.signal_redraw()
    }

    pub(crate) fn try_read(&self) -> Option<ReadLease<'_>> {
        let turn = self.admission.read()?;
        let guard = self.world.try_read().ok()?;
        Some(ReadLease { guard, _turn: turn })
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
        let view = Arc::new(WorldView {
            world: Arc::new(RwLock::new(sim)),
            admission: Arc::default(),
        });
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
            Arc::new(WorldView {
                world,
                admission: Arc::default(),
            }),
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
            sequence: 0,
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
            if !fits(commands) {
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
        && commands.iter().all(command_fits)
        && Bincode::encode_writer(commands, LimitedWriter(BATCH_BYTES)).is_ok()
}

pub(crate) fn fits_replay_batch(commands: &[(prototypes::Tick, WorldCommand)]) -> bool {
    if commands.len() > BATCH_COMMANDS || !commands.iter().all(|(_, command)| command_fits(command))
    {
        return false;
    }
    let commands: Vec<_> = commands.iter().map(|(_, command)| command).collect();
    Bincode::encode_writer(&commands, LimitedWriter(BATCH_BYTES)).is_ok()
}

fn command_fits(command: &WorldCommand) -> bool {
    match command {
        WorldCommand::Init(_) => false,
        WorldCommand::SpawnRandomCars { n_cars } => *n_cars <= 512,
        WorldCommand::AddTrain { n_wagons, .. } => *n_wagons <= 128,
        WorldCommand::SpawnTrain { wagons, .. } => wagons.len() <= 128,
        WorldCommand::MapLoadTestField { size, .. } => *size <= 100,
        WorldCommand::MapMakeMultipleConnections(projects, links) => {
            projects.len() <= 512 && links.len() <= 256
        }
        _ => true,
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
    pending: Option<Envelope>,
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
            pending: None,
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
        while !self.exit {
            let delay = self.run_one(view, status, &input, &output);
            if delay.is_zero() {
                thread::yield_now();
            } else {
                thread::park_timeout(delay);
            }
        }
        (self.schedule, self.status.boundary)
    }

    fn run_one(
        &mut self,
        view: &WorldView,
        status: &Mutex<SimStatus>,
        input: &Receiver<Envelope>,
        output: &SyncSender<Reply>,
    ) -> Duration {
        let now = Instant::now();
        self.clock.update(now);
        if self.pending.is_none() && self.explicit_steps.is_none() && self.fixture.is_none() {
            match input.try_recv() {
                Ok(envelope) => self.pending = Some(envelope),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.exit = true;
                    return Duration::ZERO;
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
                return self.replay_due.saturating_duration_since(now);
            }
            return if self.paused || self.replay.is_some() {
                Duration::from_millis(20)
            } else {
                self.clock.wait()
            };
        }
        // Timing controls and stop/fence do not need the world. In particular,
        // stopping never depends on a redraw consuming a reserved reader turn.
        let world_required = self.pending.as_ref().is_none_or(|envelope| {
            matches!(
                envelope.request,
                Request::Batch(_) | Request::Capture { .. }
            )
        });
        let reply = if world_required {
            let Some(_turn) = view.admission.write() else {
                return CONTENDED_RETRY;
            };
            let mut sim = view
                .world
                .try_write()
                .expect("world accessed outside admission");
            if let Some(envelope) = self.pending.take() {
                self.handle(envelope, Some(&mut sim))
            } else {
                self.advance(&mut sim, now)
            }
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
            // A request occupies a reply credit until the receiver polls it.
            match output.try_send(reply) {
                Ok(()) => {}
                Err(TrySendError::Disconnected(_)) => self.exit = true,
                Err(TrySendError::Full(_)) => panic!("simulation reply credit exhausted"),
            }
        }
        Duration::ZERO
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
                    commands,
                })
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
                    sim.encode_slot(revision)
                        .map(|snapshot| (snapshot, CityProgress::capture(sim)))
                        .map_err(|err| err.to_string())
                })
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
                self.exit = true;
                self.status.owner_mode = OwnerMode::Transferring;
                ReplyKind::Releasing(self.status.boundary)
            }
            Request::Stop => {
                self.exit = true;
                self.status.owner_mode = OwnerMode::Stopped;
                ReplyKind::Stopping
            }
        };
        Some(Reply { id, kind })
    }

    fn advance(&mut self, sim: &mut Simulation, now: Instant) -> Option<Reply> {
        if let Some((id, mut op)) = self.fixture.take() {
            let mut steps = 0;
            let result = op.run(sim, &mut self.schedule, &mut steps);
            self.status.boundary.completed_step += steps;
            self.status.boundary.tick = sim.get_tick();
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
            let budget = self.replay_budget.get_or_insert_with(|| {
                // Play advances one step per 20 ms. Fast and MAX are work conserving.
                ReplayBudget::new(if self.replay_speed == 1 {
                    1
                } else {
                    self.replay_speed
                })
            });
            let advance = replay.advance_one(sim, &mut self.schedule, budget);
            self.status.replay_progress = Some(Self::replay_progress(replay));
            self.status.boundary.tick = sim.get_tick();
            match advance {
                ReplayAdvance::Operation { advanced: true, .. } => {
                    self.status.boundary.completed_step += 1;
                    self.status.last_tick_duration = start.elapsed();
                }
                ReplayAdvance::Operation { .. } => {}
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

    fn end_replay(&mut self) {
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
