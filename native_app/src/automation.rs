use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, SystemTime};

use common::saveload::{self, Bincode, Encoder, JSONPretty, JSON};
use engine::frame_profile::HISTORY_FRAMES;
use engine::{Context, GfxSettings, InputContext, Key, MouseButton, ShadowQuality};
use geom::{vec2, vec3, Radians, Vec2, Vec3, OBB};
use prototypes::{BuildingGen, GoodsCompanyID};
use scenarios::builders::{self, CommandSink};
use serde::{Deserialize, Serialize};
use simulation::economy::cargo::CargoBook;
use simulation::economy::{Budget, CityStats, Government, ZoneDemand};
use simulation::map::{
    BuildingKind, LandMode, LandTileId, Map, ProjectFilter, ProjectKind, RoadID, TraverseKind,
    WaterKind, Zone, ZoneGrid, ZoneKind,
};
use simulation::transportation::traffic_metrics::{sample_road_users, TruckCargo};
use simulation::transportation::{Location, VehicleKind};
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand, WorldCommands};
use simulation::{AnyEntity, HumanID, MapPreset, Simulation, SimulationOptions};

use crate::gui::follow::{follow_pos, FollowEntity};
use crate::gui::windows::settings::Settings;
use crate::gui::{
    overlays::{current_overlay, OverlayMode},
    toolbox,
    toolbox::building::BuildingTab,
    top_demand_driver,
    zoning::ZoningState,
    GuiState, InspectedBuilding, InspectedEntity, InspectedRoad, Tool, ToolSelection,
};
use crate::rendering::OrbitCamera;
use crate::session::{self, SessionScreen, SessionState};
use crate::sim_thread::{AppliedBatch, BatchId, Boundary, Reply, ReplyKind, Request, SimThread};
use crate::uiworld::{SaveLoadState, UiWorld};

mod land_fixture;

const WATCHDOG: Duration = Duration::from_secs(15 * 60);
const GPU_DRAIN_FRAMES: u64 = 8;
static STARTUP: Mutex<Option<Automation>> = Mutex::new(None);

#[derive(Default)]
struct Arguments {
    script: Option<PathBuf>,
    exit_after_script: bool,
    save_dir: Option<PathBuf>,
}

#[derive(Deserialize)]
enum Step {
    WaitFrames(u64),
    RunTicks(u64),
    SetWarp(u32),
    Commands(Vec<WorldCommand>),
    BuildSpecialBuildings(Vec<SpecialBuildingPlacement>),
    Pointer {
        action: PointerAction,
        #[serde(flatten)]
        position: PointerPosition,
    },
    Key {
        key: ScriptKey,
        down: bool,
    },
    EmptyTreasury,
    ConfirmLandPurchase {
        #[serde(default)]
        empty_treasury: bool,
        replace_with: Option<simulation::economy::progression::ProgressionMode>,
    },
    Camera(CameraPose),
    FrameProfileGraphics(ProfileGraphics),
    CaptureFrameProfile {
        name: String,
        frames: u64,
        pan: Option<CameraPose>,
    },
    Builder(Builder),
    /// Tick until a human matches `pick`, then inspect and follow them.
    /// The game loop does not move a scripted camera, so the follow steps do.
    FollowHuman {
        pick: HumanPick,
        max_ticks: u64,
    },
    /// Tick until the followed human reaches `until` (or for `ticks`), then move the camera to them.
    RunFollowing {
        ticks: u64,
        until: Option<FollowedLocation>,
    },
    /// Tick until a loaded truck is on a road, then inspect that road with the camera on the truck.
    InspectCargoRoad {
        max_ticks: u64,
    },
    /// `"hand"`, a toolbar category (`"zoning"`) or a mode (`"roads/curved"`).
    SelectTool(ToolSelection),
    SelectZone(ZoneKind),
    SetOverlay(OverlayMode),
    SetSession(SessionScreen),
    OpenWindow(String),
    NewTiledGame {
        side: u16,
        progression: simulation::economy::progression::ProgressionMode,
        #[serde(default)]
        round_trip: bool,
    },
    NewWetTiledFixture,
    PreviewTrafficRoad {
        anchor: [f32; 2],
        #[serde(default)]
        denied: bool,
    },
    SelectTrafficJunction,
    ConfirmTrafficRepair,
    CompareTrafficCorridor,
    SaveTrafficRepair,
    ClearBuildingInspector,
    LoadSlot(String),
    Assert(Assertion),
    Screenshot(String),
    DumpState(StateDump),
    Exit,
}

#[derive(Deserialize)]
enum ProfileGraphics {
    Default,
    Weak,
}

#[derive(Clone, Copy, Deserialize)]
struct CameraPose {
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
    pitch: f32,
    dist: f32,
}

impl CameraPose {
    fn current(orbit: &OrbitCamera) -> Self {
        let camera = &orbit.camera;
        Self {
            x: camera.pos.x,
            y: camera.pos.y,
            z: camera.pos.z,
            yaw: camera.yaw.0,
            pitch: camera.pitch.0,
            dist: camera.dist,
        }
    }

    fn validate(self) -> Result<(), String> {
        if !vec3(self.x, self.y, self.z).is_finite()
            || !self.yaw.is_finite()
            || !self.pitch.is_finite()
            || !self.dist.is_finite()
            || self.dist <= 0.0
        {
            return Err("invalid camera position, angles, or distance".into());
        }
        Ok(())
    }

    fn interpolate(self, end: Self, fraction: f32) -> Self {
        let lerp = |from: f32, to: f32| from + (to - from) * fraction;
        Self {
            x: lerp(self.x, end.x),
            y: lerp(self.y, end.y),
            z: lerp(self.z, end.z),
            yaw: lerp(self.yaw, end.yaw),
            pitch: lerp(self.pitch, end.pitch),
            dist: lerp(self.dist, end.dist),
        }
    }

    fn apply(self, orbit: &mut OrbitCamera) {
        orbit.targetpos = vec3(self.x, self.y, self.z);
        orbit.targetyaw = Radians(self.yaw);
        orbit.targetpitch = Radians(self.pitch);
        orbit.targetdist = self.dist;
        orbit.camera.pos = orbit.targetpos;
        orbit.camera.yaw = orbit.targetyaw;
        orbit.camera.pitch = orbit.targetpitch;
        orbit.camera.dist = orbit.targetdist;
    }
}

struct ProfileCapture {
    path: PathBuf,
    frames: Range<u64>,
    pan: Option<(CameraPose, CameraPose)>,
}

impl ProfileCapture {
    fn position_camera(&self, uiw: &UiWorld, frame: u64) {
        if let Some((start, end)) = self.pan {
            let t = (frame - self.frames.start) as f32
                / (self.frames.end - self.frames.start - 1) as f32;
            start.interpolate(end, t).apply(&mut uiw.camera_mut());
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StateDump {
    Name(String),
    WithOptions {
        name: String,
        #[serde(default)]
        water_samples: Vec<[f32; 2]>,
        #[serde(default)]
        include_map_hash: bool,
    },
}

#[derive(Deserialize)]
struct SpecialBuildingPlacement {
    pos: OBB,
    kind: BuildingKind,
    gen: BuildingGen,
    zone: Option<Zone>,
    road_at: [f32; 2],
}

#[derive(Deserialize)]
enum Assertion {
    NoRejections,
    ServicesTab,
    HouseInspector,
    TrafficRepair(TrafficRepairAssertion),
    LandTiles {
        tick: u64,
        owned: BTreeSet<LandTileId>,
        selected: Option<LandTileId>,
        quote: Option<String>,
        confirming: bool,
        pending: bool,
        receipt: Option<LandReceipt>,
        treasury: u32,
        extent: Option<[u16; 2]>,
    },
    MixedUseInspector {
        business_open: bool,
    },
    ZonePainted {
        center: [f32; 2],
        cell: Option<simulation::map::ZoneCell>,
    },
    TowerSelection {
        parcel_center: [f32; 2],
        background_tower: Option<[f32; 2]>,
    },
    PathPark {
        footpaths: usize,
        local_one_way: usize,
        lot_capacity: u16,
        vehicle_entrances: usize,
    },
    BuildingCount {
        prototype: String,
        count: usize,
    },
    RoadRemovals {
        before: String,
        after: String,
        expected: BTreeSet<RoadID>,
    },
    /// The followed human is still inspected and the camera is on whatever carries them.
    CameraOnFollowed,
}

#[derive(Deserialize)]
enum TrafficRepairAssertion {
    Quote,
    Applied,
    Denied,
    Junction,
    Compared,
}

#[derive(Deserialize)]
enum LandReceipt {
    Paid(i64),
    InsufficientFunds,
}

impl LandReceipt {
    fn outcome(self) -> Result<i64, Rejection> {
        match self {
            Self::Paid(price) => Ok(price),
            Self::InsufficientFunds => Err(Rejection::InsufficientFunds),
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
pub(crate) enum HumanPick {
    /// The human on a trip standing closest to their own parked car, within the
    /// distance the router walks to a personal car.
    BoardingCar,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Debug)]
pub(crate) enum FollowedLocation {
    Outside,
    Vehicle,
    Transit,
    Building,
}

impl FollowedLocation {
    fn of(location: &Location) -> Self {
        match location {
            Location::Outside => Self::Outside,
            Location::Vehicle(_) => Self::Vehicle,
            Location::Transit(_) => Self::Transit,
            Location::Building(_) => Self::Building,
        }
    }
}

#[derive(Deserialize)]
enum PointerAction {
    Move,
    Down,
    Up,
    Click,
    Drag { to: PointerPosition },
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum PointerPosition {
    World { world_pos: [f32; 3] },
    Screen { screen_pos: [f32; 2] },
    Centered { center_offset: [f32; 2] },
}

#[derive(Deserialize)]
enum ScriptKey {
    Shift,
    Escape,
    LandTiles,
}

enum PointerEvent {
    Move(Vec2),
    Button { down: bool },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Builder {
    GridCity,
    FillHouses,
    PlaceCompany,
}

#[derive(Clone, Copy)]
enum Wait {
    Applied(BatchId),
    Map {
        boundary: Boundary,
        settle_frames: u64,
    },
    Frames {
        start: u64,
        count: u64,
    },
    LoadSlot,
    Screenshot,
    TrafficReceipt,
    TrafficComparison,
    TrafficSave,
    Simulation(BatchId),
}

#[derive(Clone, Copy, Serialize)]
struct TrafficPresentation {
    generation: u64,
    submitted_frame: u64,
    first_revised_frame: Option<u64>,
}

pub(crate) struct Automation {
    steps: VecDeque<Step>,
    pointer_events: VecDeque<PointerEvent>,
    pointer_input: InputContext,
    step_number: usize,
    dir: PathBuf,
    exit_after_script: bool,
    wait: Option<Wait>,
    profile_capture: Option<ProfileCapture>,
    fixture: Option<FixtureOutcome>,
    load_slot: Option<String>,
    first_rejection: Option<(&'static str, Rejection)>,
    traffic_presentation: Option<TrafficPresentation>,
    traffic_batch: Option<BatchId>,
    land_terrain_revision: Cell<Option<(u64, u64, u64)>>,
    _watchdog: mpsc::Sender<()>,
}

pub(crate) fn configure() -> Result<(), String> {
    let args = Arguments::parse(std::env::args_os().skip(1))?;

    let Some(script) = args.script else {
        if let Some(dir) = args.save_dir {
            configure_save_dir(&dir, false)?;
        }
        return Ok(());
    };

    let (watchdog, deadline) = mpsc::channel();
    std::thread::spawn(move || {
        if deadline.recv_timeout(WATCHDOG) == Err(mpsc::RecvTimeoutError::Timeout) {
            fail("script exceeded the 15-minute watchdog");
        }
    });

    let steps = fs::read(&script)
        .and_then(|bytes| JSON::decode(&bytes))
        .map_err(|e| format!("reading script {}: {e}", script.display()))?;
    let dir = match args.save_dir {
        Some(dir) => dir,
        None => {
            let timestamp = SystemTime::UNIX_EPOCH
                .elapsed()
                .map_err(|e| e.to_string())?
                .as_nanos();
            std::env::temp_dir()
                .join("egr/shots")
                .join(timestamp.to_string())
        }
    };
    let dir = configure_save_dir(&dir, true)?;
    *STARTUP.lock().unwrap() = Some(Automation {
        steps,
        pointer_events: VecDeque::new(),
        pointer_input: InputContext::default(),
        step_number: 0,
        dir,
        exit_after_script: args.exit_after_script,
        wait: None,
        profile_capture: None,
        fixture: None,
        load_slot: None,
        first_rejection: None,
        traffic_presentation: None,
        traffic_batch: None,
        land_terrain_revision: Cell::new(None),
        _watchdog: watchdog,
    });
    Ok(())
}

pub(crate) fn fail(reason: &str) -> ! {
    eprintln!("automation: {reason}");
    std::process::exit(1);
}

impl Arguments {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Self, String> {
        let mut result = Self::default();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--script") if result.script.is_none() => {
                    result.script = Some(Self::path_arg(&mut args, "--script")?);
                }
                Some("--save-dir") if result.save_dir.is_none() => {
                    result.save_dir = Some(Self::path_arg(&mut args, "--save-dir")?);
                }
                Some("--exit-after-script") if !result.exit_after_script => {
                    result.exit_after_script = true;
                }
                _ => return Err(format!("unknown or repeated argument {arg:?}")),
            }
        }
        if result.exit_after_script && result.script.is_none() {
            return Err("--exit-after-script requires --script".into());
        }
        Ok(result)
    }

    fn path_arg(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf, String> {
        args.next()
            .filter(|arg| !arg.is_empty() && !arg.to_string_lossy().starts_with("--"))
            .map(PathBuf::from)
            .ok_or_else(|| format!("{flag} requires a path"))
    }
}

fn configure_save_dir(path: &Path, fresh: bool) -> Result<PathBuf, String> {
    let dir = prepare_save_dir(path, fresh)
        .map_err(|e| format!("setting save directory {}: {e}", path.display()))?;
    saveload::set_save_dir(dir.clone()).map_err(|_| "save directory already set")?;
    Ok(dir)
}

fn prepare_save_dir(path: &Path, fresh: bool) -> io::Result<PathBuf> {
    let dir = resolve_path(&std::path::absolute(path)?)?;
    if fresh {
        let default = resolve_path(&std::env::current_dir()?.join("world"))?;
        if dir.starts_with(default) {
            return Err(io::Error::other(
                "script output must be outside the default world directory",
            ));
        }
    }
    fs::create_dir_all(&dir)?;
    if fresh {
        if fs::read_dir(&dir)?.next().transpose()?.is_some() {
            return Err(io::Error::other("script output directory must be empty"));
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(".egregoria-run"))?;
    }
    Ok(dir)
}

// Canonicalize existing ancestors before creating a new directory, resolving symlinks.
fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    match path.canonicalize() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(e);
            };
            Ok(resolve_path(parent)?.join(name))
        }
        result => result,
    }
}

impl Automation {
    pub(crate) fn take_startup() -> Option<Self> {
        STARTUP.lock().unwrap().take()
    }

    /// Advance one script step after its barriers. Returns whether the script is still active.
    pub(crate) fn step(
        &mut self,
        ctx: &mut Context,
        uiw: &UiWorld,
        sim: &Simulation,
        driver: &mut SimThread,
        rebuilt_version: Option<u64>,
        rendered_batch: Option<BatchId>,
    ) -> Result<bool, String> {
        // The window can still receive real cursor and focus events during a script.
        // Restore the script's cursor before Yakui handles the next queued event.
        move_yakui_pointer(ctx, self.pointer_input.mouse.screen);
        let result = self.advance(ctx, uiw, sim, driver, rebuilt_version, rendered_batch);
        match result {
            Ok(true) => {
                ctx.input.mouse.clone_from(&self.pointer_input.mouse);
                ctx.egui.last_mouse_captured = engine::pointer_over_egui(ctx.input.mouse.screen);
                self.pointer_input
                    .pointer_frame(ctx.egui.last_mouse_captured)
                    .publish();
                self.pointer_input.end_frame();
            }
            Ok(false)
                if self
                    .pointer_input
                    .physical_mouse_buttons()
                    .contains(&MouseButton::Left) =>
            {
                PointerEvent::Button { down: false }.send(ctx, &mut self.pointer_input);
                ctx.input.mouse.pressed.remove(&MouseButton::Left);
            }
            _ => {}
        }
        result.map_err(|e| format!("script step {}: {e}", self.step_number))
    }

    /// Progress a presented-frame barrier without taking a simulation lease.
    pub(crate) fn advance_frame_wait(
        &mut self,
        ctx: &mut Context,
        uiw: &UiWorld,
    ) -> Result<bool, String> {
        if let Some(capture) = &self.profile_capture {
            let next_frame = ctx.gfx.tick + 1;
            if next_frame < capture.frames.end {
                capture.position_camera(uiw, next_frame);
                return Ok(true);
            }
            let profile = ctx
                .gfx
                .frame_profile
                .as_ref()
                .ok_or("frame profiling is disabled")?;
            if next_frame < capture.frames.end + GPU_DRAIN_FRAMES
                && profile.has_pending_gpu(capture.frames.clone())
            {
                return Ok(true);
            }
            crate::frame_profile::write_report(
                &ctx.gfx,
                uiw,
                &capture.path,
                Some(capture.frames.clone()),
            )?;
            eprintln!("automation: wrote {}", capture.path.display());
            self.profile_capture = None;
            return Ok(true);
        }

        Ok(matches!(self.wait, Some(Wait::Frames { start, count }) if ctx.gfx.tick - start < count))
    }

    fn advance(
        &mut self,
        ctx: &mut Context,
        uiw: &UiWorld,
        sim: &Simulation,
        driver: &mut SimThread,
        rebuilt_version: Option<u64>,
        rendered_batch: Option<BatchId>,
    ) -> Result<bool, String> {
        if self.advance_frame_wait(ctx, uiw)? {
            return Ok(true);
        }

        match self.wait {
            Some(Wait::Applied(id))
                if !rendered_batch.is_some_and(|rendered| rendered.covers(id)) =>
            {
                return Ok(true);
            }
            Some(Wait::Applied(id)) if self.traffic_batch == Some(id) => {
                if let Some(presentation) = &mut self.traffic_presentation {
                    presentation.first_revised_frame = Some(ctx.gfx.tick.saturating_sub(1));
                }
            }
            Some(Wait::Map { boundary, .. })
                if boundary.generation != uiw.read::<SessionState>().generation
                    || rebuilt_version != Some(boundary.tick)
                    || sim.get_tick() != boundary.tick =>
            {
                return Ok(true);
            }
            Some(Wait::Map { settle_frames, .. }) if settle_frames > 0 => {
                self.wait = Some(Wait::Frames {
                    start: ctx.gfx.tick,
                    count: settle_frames,
                });
                return Ok(true);
            }
            // The game loop resumes automation after the session's world job finishes.
            Some(Wait::LoadSlot) if !session::playing(uiw) => {
                return Err(uiw
                    .read::<SaveLoadState>()
                    .world_error
                    .clone()
                    .unwrap_or_else(|| "loading the save slot did not enter Playing".into()));
            }
            Some(Wait::TrafficReceipt) if uiw.read::<crate::gui::traffic_repair::TrafficRepairUi>()
                .script_state(sim).pending => return Ok(true),
            Some(Wait::TrafficComparison) => {
                let generation = uiw.read::<SessionState>().generation;
                if !uiw.write::<crate::gui::traffic_repair::TrafficRepairUi>()
                    .script_comparison_ready(generation)? {
                    return Ok(true);
                }
            }
            Some(Wait::TrafficSave) => {
                if uiw.read::<SaveLoadState>().saving() {
                    return Ok(true);
                }
                if !saveload::save_dir().join("p7_repaired.meta.json").is_file() {
                    return Err("Repaired slot was not saved".into());
                }
            }
            // A loaded slot can reuse the previous slot's tick while its map
            // is still staging. The game loop only reports a compatible view.
            Some(Wait::LoadSlot) if rebuilt_version.is_none() => return Ok(true),
            Some(Wait::Simulation(_)) => return Ok(true),
            Some(Wait::Screenshot) => match ctx.take_screenshot_result() {
                Some(result) => result?,
                None => return Ok(true),
            },
            _ => {}
        }
        self.wait = None;

        if let Some(id) = &self.load_slot {
            let session = uiw.read::<SessionState>();
            if session.scanning_slots() {
                return Ok(true);
            }
            if let Some(message) = &session.slot_error {
                return Err(message.clone());
            }
            let (entry, compatibility) = session
                .slots
                .iter()
                .find(|(entry, _)| entry.id.as_str() == id)
                .cloned()
                .ok_or_else(|| format!("save slot {id:?} was not found"))?;
            if let Some(reason) = compatibility.unavailable_reason() {
                return Err(format!("save slot {id:?} ({reason})"));
            }
            drop(session);
            session::load_slot(uiw, entry);
            self.load_slot = None;
            self.wait = Some(Wait::LoadSlot);
            return Ok(true);
        }

        if let Some(outcome) = self.fixture.take() {
            match outcome {
                FixtureOutcome::Follow(human) => {
                    *uiw.write::<InspectedEntity>() = InspectedEntity {
                        e: Some(human.into()),
                        ..Default::default()
                    };
                    uiw.write::<FollowEntity>().0 = Some(human.into());
                    FollowEntity::track(uiw, sim);
                }
                FixtureOutcome::Followed => FollowEntity::track(uiw, sim),
                FixtureOutcome::InspectCargo(road, pos) => {
                    *uiw.write::<InspectedEntity>() = InspectedEntity::default();
                    uiw.write::<InspectedBuilding>().e = None;
                    uiw.write::<FollowEntity>().0 = None;
                    uiw.write::<InspectedRoad>().0 = Some(road);
                    let mut orbit = uiw.camera_mut();
                    orbit.targetpos = pos;
                    orbit.camera.pos = pos;
                }
                FixtureOutcome::Builder(rejection) => {
                    if self.first_rejection.is_none() {
                        self.first_rejection = rejection;
                    }
                }
                FixtureOutcome::EmptyTreasury => {}
            }
        }

        if session::playing(uiw)
            && rebuilt_version.is_none()
            && (!self.pointer_events.is_empty()
                || matches!(
                    self.steps.front(),
                    Some(Step::Pointer { .. } | Step::Screenshot(_))
                ))
        {
            return Ok(true);
        }

        // Each event gets its own update and layout pass before the next event.
        if let Some(event) = self.pointer_events.pop_front() {
            event.send(ctx, &mut self.pointer_input);
            return Ok(true);
        }

        let Some(step) = self.steps.pop_front() else {
            if self.exit_after_script {
                ctx.request_exit();
            }
            return Ok(false);
        };
        self.step_number += 1;
        if !session::playing(uiw)
            && matches!(
                step,
                Step::RunTicks(1..)
                    | Step::Commands(_)
                    | Step::BuildSpecialBuildings(_)
                    | Step::EmptyTreasury
                    | Step::ConfirmLandPurchase { .. }
                    | Step::Builder(_)
                    | Step::FollowHuman { .. }
                    | Step::RunFollowing { .. }
                    | Step::PreviewTrafficRoad { .. }
                    | Step::SelectTrafficJunction
                    | Step::ConfirmTrafficRepair
                    | Step::CompareTrafficCorridor
                    | Step::SaveTrafficRepair
                    | Step::InspectCargoRoad { .. }
                    | Step::Camera(_)
                    | Step::CaptureFrameProfile { .. }
                    | Step::SelectTool(_)
                    | Step::SelectZone(_)
            )
        {
            return Err("world interactions require the Playing session".into());
        }
        match step {
            Step::WaitFrames(count) => {
                self.wait = Some(Wait::Frames {
                    start: ctx.gfx.tick,
                    count,
                });
            }
            Step::RunTicks(count) if count > 0 => {
                self.submit(
                    driver,
                    Request::Step {
                        count: usize::try_from(count).map_err(|_| "tick count is too large")?,
                    },
                )?;
            }
            Step::RunTicks(_) => {}
            Step::SetWarp(warp) => {
                eprintln!("automation: set warp {warp} at tick {}", sim.get_tick());
                uiw.write::<Settings>().time_warp = warp;
            }
            Step::Commands(commands) => {
                let mut batch = WorldCommands::default();
                batch.extend(commands);
                self.queue_commands(uiw, batch)?;
            }
            Step::BuildSpecialBuildings(placements) => {
                let mut batch = WorldCommands::default();
                let map = sim.map();
                for placement in placements {
                    batch.push(placement.command(&map)?);
                }
                self.queue_commands(uiw, batch)?;
            }
            Step::Pointer { action, position } => {
                let start = position.to_screen(uiw)?;
                match action {
                    PointerAction::Move => {}
                    PointerAction::Down => {
                        self.pointer_events
                            .push_back(PointerEvent::Button { down: true });
                    }
                    PointerAction::Up => {
                        self.pointer_events
                            .push_back(PointerEvent::Button { down: false });
                    }
                    PointerAction::Click => self.pointer_events.extend([
                        PointerEvent::Button { down: true },
                        PointerEvent::Button { down: false },
                    ]),
                    PointerAction::Drag { to } => self.pointer_events.extend([
                        PointerEvent::Button { down: true },
                        PointerEvent::Move(to.to_screen(uiw)?),
                        PointerEvent::Button { down: false },
                    ]),
                }
                PointerEvent::Move(start).send(ctx, &mut self.pointer_input);
            }
            Step::Key { key, down } => key.send(ctx, down),
            Step::EmptyTreasury => {
                self.submit(driver, Request::PausedFixtureOp(FixtureOp::EmptyTreasury))?
            }
            Step::ConfirmLandPurchase {
                empty_treasury,
                replace_with,
            } => {
                use crate::gui::overlays::land_tiles::LandTilesUi;
                uiw.write::<LandTilesUi>()
                    .confirm_script_purchase(uiw, sim)?;
                if empty_treasury {
                    // The fixture reaches the worker before this frame seals its queued purchase.
                    self.submit(driver, Request::PausedFixtureOp(FixtureOp::EmptyTreasury))?;
                }
                if let Some(progression) = replace_with {
                    // Leave this confirmation pending across replacement and let the regular
                    // world reset discard it. The exit path still drains accepted worker batches.
                    let _ = std::mem::take(&mut *uiw.commands());
                    replace_land_fixture(uiw, new_tiled_fixture(17, progression, false)?)?;
                }
            }
            Step::Builder(builder) => self.submit(
                driver,
                Request::PausedFixtureOp(FixtureOp::Builder(builder)),
            )?,
            Step::Camera(pose) => {
                pose.validate()?;
                pose.apply(&mut uiw.camera_mut());
            }
            Step::FrameProfileGraphics(preset) => {
                let mut graphics = GfxSettings::default();
                if matches!(preset, ProfileGraphics::Weak) {
                    graphics.shadows = ShadowQuality::NoShadows;
                    graphics.ssao = false;
                    graphics.msaa = false;
                }
                uiw.write::<Settings>().gfx = graphics;
            }
            Step::CaptureFrameProfile { name, frames, pan } => {
                if ctx.gfx.frame_profile.is_none() {
                    return Err("set EGREGORIA_FRAME_PROFILE=1 (or cpu) to capture a frame profile".into());
                }
                let max_frames = HISTORY_FRAMES as u64 - GPU_DRAIN_FRAMES;
                if !(2..=max_frames).contains(&frames) {
                    return Err(format!("frame profile capture requires 2 to {max_frames} frames"));
                }
                let pan = pan
                    .map(|end| {
                        end.validate()?;
                        Ok::<_, String>((CameraPose::current(&uiw.camera()), end))
                    })
                    .transpose()?;
                let first = ctx.gfx.tick + 1;
                let capture = ProfileCapture {
                    path: self.output_path(&name, "json")?,
                    frames: first..first + frames,
                    pan,
                };
                capture.position_camera(uiw, first);
                self.profile_capture = Some(capture);
            }
            Step::FollowHuman { pick, max_ticks } => {
                self.submit(
                    driver,
                    Request::PausedFixtureOp(FixtureOp::Follow {
                        pick,
                        remaining: max_ticks,
                    }),
                )?;
            }
            Step::RunFollowing { ticks, until } => {
                let human = followed_human(uiw)?;
                self.submit(
                    driver,
                    Request::PausedFixtureOp(FixtureOp::RunFollowing {
                        human,
                        remaining: ticks,
                        until,
                    }),
                )?;
            }
            Step::InspectCargoRoad { max_ticks } => {
                self.submit(
                    driver,
                    Request::PausedFixtureOp(FixtureOp::FindCargo {
                        remaining: max_ticks,
                    }),
                )?;
            }
            Step::SelectTool(selection) => {
                toolbox::select_tool(uiw, sim, selection);
            }
            Step::SelectZone(kind) => {
                if *uiw.read::<Tool>() != Tool::Zoning {
                    return Err("selecting a zone requires the zoning tool".into());
                }
                uiw.write::<ZoningState>().select_kind(Some(kind));
            }
            Step::SetOverlay(mode) => *uiw.write::<OverlayMode>() = mode,
            Step::SetSession(screen) => {
                if !session::set_screen(uiw, screen) {
                    return Err("session screens are unavailable during multiplayer".into());
                }
            }
            Step::NewTiledGame {
                side,
                progression,
                round_trip,
            } => {
                replace_land_fixture(uiw, new_tiled_fixture(side, progression, round_trip)?)?;
            }
            Step::NewWetTiledFixture => replace_land_fixture(uiw, land_fixture::wet_rectangle()?)?,
            Step::PreviewTrafficRoad { anchor, denied } => {
                crate::gui::traffic_repair::script_preview_road(uiw, sim, anchor, denied)?;
            }
            Step::SelectTrafficJunction => {
                crate::gui::traffic_repair::script_select_junction(uiw, sim)?;
            }
            Step::ConfirmTrafficRepair => {
                uiw.write::<crate::gui::traffic_repair::TrafficRepairUi>()
                    .confirm_script(sim, uiw)?;
                self.traffic_presentation = Some(TrafficPresentation {
                    generation: uiw.read::<SessionState>().generation,
                    submitted_frame: ctx.gfx.tick,
                    first_revised_frame: None,
                });
                self.wait = Some(Wait::TrafficReceipt);
            }
            Step::CompareTrafficCorridor => {
                let generation = uiw.read::<SessionState>().generation;
                uiw.write::<crate::gui::traffic_repair::TrafficRepairUi>()
                    .start_script_comparison(generation);
                self.wait = Some(Wait::TrafficComparison);
            }
            Step::SaveTrafficRepair => {
                let slot = common::saveload::SlotId::new("p7_repaired").unwrap();
                session::request_save(uiw, Some((slot, "P7 repaired corridor".to_owned())));
                self.wait = Some(Wait::TrafficSave);
            }
            Step::ClearBuildingInspector => {
                uiw.write::<InspectedBuilding>().e = None;
            }
            Step::OpenWindow(name) => {
                if name == "Land Tiles" && !uiw.read::<GuiState>().windows.land_tiles_open {
                    crate::gui::overlays::land_tiles::toggle(uiw);
                    return Ok(true);
                }
                let screen = match name.as_str() {
                    "Main Menu" => Some(SessionScreen::MainMenu),
                    "New Game" => Some(SessionScreen::NewGame),
                    "Load" => Some(SessionScreen::LoadGame),
                    _ => None,
                };
                if let Some(screen) = screen {
                    if !session::set_screen(uiw, screen) {
                        return Err("session screens are unavailable during multiplayer".into());
                    }
                    return Ok(true);
                }
                let mut gui = uiw.write::<GuiState>();
                let (_, open) = gui
                    .windows
                    .window_states()
                    .find(|(candidate, _)| *candidate == name)
                    .ok_or_else(|| format!("unknown window {name:?}"))?;
                *open = true;
            }
            Step::LoadSlot(id) => {
                // Scripts may enter the Load screen without a departure confirmation.
                if !session::set_screen(uiw, SessionScreen::LoadGame) {
                    return Err("loading a save slot is unavailable during multiplayer".into());
                }
                uiw.write::<SessionState>().refresh_slots = true;
                self.load_slot = Some(id);
            }
            Step::Assert(assertion) => assertion.check(uiw, sim, self)?,
            Step::Screenshot(name) => {
                ctx.request_screenshot(self.output_path(&name, "png")?)?;
                self.wait = Some(Wait::Screenshot);
            }
            Step::DumpState(dump) => {
                let (name, samples, include_map_hash) = match &dump {
                    StateDump::Name(name) => (name, &[][..], false),
                    StateDump::WithOptions {
                        name,
                        water_samples,
                        include_map_hash,
                    } => (name, water_samples.as_slice(), *include_map_hash),
                };
                Self::dump_state(
                    &self.output_path(name, "json")?,
                    uiw,
                    sim,
                    samples,
                    include_map_hash,
                    self.traffic_presentation.filter(|presentation| {
                        presentation.generation == uiw.read::<SessionState>().generation
                    }),
                )?;
            }
            Step::Exit => {
                ctx.request_exit();
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn submit(&mut self, driver: &mut SimThread, request: Request) -> Result<(), String> {
        let id = driver
            .submit(request)
            .map_err(|error| format!("could not submit simulation operation: {:?}", error.0))?;
        self.wait = Some(Wait::Simulation(id));
        Ok(())
    }

    fn queue_commands(&self, uiw: &UiWorld, batch: WorldCommands) -> Result<(), String> {
        if !crate::sim_thread::can_submit_batch(&batch) {
            return Err("script command batch exceeds the simulation limit".into());
        }
        *uiw.commands() = batch;
        Ok(())
    }

    pub(crate) fn applied(&mut self, batch: &AppliedBatch) {
        if self.first_rejection.is_none() {
            self.first_rejection = batch
                .rejections
                .first()
                .map(|&(_, label, reason)| (label, reason));
        }
        if batch.traffic_edits.iter().any(|receipt| receipt.local) {
            self.traffic_batch = Some(batch.id);
        }
        self.wait = Some(Wait::Applied(batch.id));
    }

    pub(crate) fn worker_reply(&mut self, reply: Reply) -> Result<(), String> {
        if !matches!(self.wait, Some(Wait::Simulation(id)) if id == reply.id) {
            return Ok(());
        }
        match reply.kind {
            ReplyKind::Stepped(boundary) => self.wait_for_map(boundary, 2),
            ReplyKind::FixtureDone(result, boundary) => {
                let outcome = result?;
                if matches!(outcome, FixtureOutcome::EmptyTreasury) {
                    self.wait = None;
                } else {
                    self.fixture = Some(outcome);
                    self.wait_for_map(boundary, 2);
                }
            }
            ReplyKind::Interrupted(boundary) => {
                return Err(format!(
                    "simulation operation interrupted at tick {}",
                    boundary.tick
                ));
            }
            ReplyKind::Rejected(error) => return Err(error.into()),
            _ => return Err("unexpected simulation reply".into()),
        }
        Ok(())
    }

    fn wait_for_map(&mut self, boundary: Boundary, settle_frames: u64) {
        self.wait = Some(Wait::Map {
            boundary,
            settle_frames,
        });
    }

    fn output_path(&self, name: &str, extension: &str) -> Result<PathBuf, String> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(format!("invalid output name {name:?}"));
        }
        Ok(self.dir.join(format!("{name}.{extension}")))
    }

    fn dump_state(
        path: &Path,
        uiw: &UiWorld,
        sim: &Simulation,
        water_samples: &[[f32; 2]],
        include_map_hash: bool,
        traffic_presentation: Option<TrafficPresentation>,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct Snapshot<'a> {
            tick: u64,
            active_tool: Tool,
            overlay: OverlayMode,
            preset: MapPreset,
            #[serde(skip_serializing_if = "Option::is_none")]
            map_hash: Option<u64>,
            road_ids: BTreeSet<RoadID>,
            rail_lanes: usize,
            external_markets: usize,
            freight_stations: usize,
            water_revision: u64,
            water_tiles: usize,
            water_samples: Vec<Option<WaterKind>>,
            open_windows: Vec<&'static str>,
            city_stats: &'a CityStats,
            zone_cells: usize,
            zone_coordinates: Vec<(i32, i32)>,
            zone_demand: &'a ZoneDemand,
            demand_drivers: BTreeMap<ZoneKind, (&'static str, f32)>,
            tax_rates: [u16; 4],
            rejections: Vec<(&'static str, String)>,
            traffic_repair: crate::gui::traffic_repair::TrafficRepairScriptState,
            #[serde(skip_serializing_if = "Option::is_none")]
            traffic_presentation: Option<TrafficPresentation>,
            #[serde(skip_serializing_if = "Option::is_none")]
            traffic_comparison: Option<&'a scenarios::traffic_repair::RepairComparison>,
        }

        let open_windows = uiw
            .write::<GuiState>()
            .windows
            .window_states()
            .filter_map(|(name, open)| (*open).then_some(name))
            .collect();
        let demand = sim.read::<ZoneDemand>();
        let budget = sim.read::<Budget>();
        let map = sim.map();
        let env = map.environment();
        let zones = sim.read::<ZoneGrid>();
        let map_hash = if include_map_hash {
            let bytes = Bincode::encode(&*map)
                .map_err(|error| format!("hashing map for state dump: {error}"))?;
            Some(common::hash_u64(&bytes[..]))
        } else {
            None
        };
        let buildings = map.buildings().values();
        let traffic = uiw.read::<crate::gui::traffic_repair::TrafficRepairUi>();
        let state = Snapshot {
            tick: sim.get_tick(),
            active_tool: *uiw.read::<Tool>(),
            overlay: current_overlay(*uiw.read::<OverlayMode>(), uiw),
            preset: sim.read::<SimulationOptions>().preset,
            map_hash,
            road_ids: map.roads().keys().collect(),
            rail_lanes: map
                .lanes()
                .values()
                .filter(|lane| lane.kind.is_rail())
                .count(),
            external_markets: buildings
                .clone()
                .filter(|building| building.kind == BuildingKind::ExternalTrading)
                .count(),
            freight_stations: buildings
                .filter(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))
                .count(),
            water_revision: env.water_revision(),
            water_tiles: if env.has_water_configuration() {
                env.chunks()
                    .map(|(chunk, _)| env.water_tiles(chunk).count())
                    .sum()
            } else {
                0
            },
            water_samples: water_samples
                .iter()
                .map(|&[x, y]| env.water_body_kind(vec2(x, y)))
                .collect(),
            open_windows,
            city_stats: &sim.read::<CityStats>(),
            zone_cells: zones.cells().len(),
            zone_coordinates: zones.cells().keys().copied().collect(),
            zone_demand: &demand,
            demand_drivers: demand
                .values
                .keys()
                .filter_map(|&zone| top_demand_driver(sim, zone).map(|driver| (zone, driver)))
                .collect(),
            tax_rates: ZoneKind::TAXABLE.map(|zone| budget.tax_rate(zone)),
            traffic_repair: traffic.script_state(sim),
            traffic_presentation,
            traffic_comparison: traffic.completed_comparison(),
            rejections: sim
                .read::<CommandOutcomes>()
                .last
                .iter()
                .map(|(command, reason)| (*command, format!("{reason:?}")))
                .collect(),
        };
        let write = || -> io::Result<()> {
            let mut file = BufWriter::new(File::create(path)?);
            JSONPretty::encode_writer(&state, &mut file)?;
            file.flush()
        };
        write().map_err(|e| format!("writing state {}: {e}", path.display()))
    }

    fn dump_road_ids(&self, name: &str) -> Result<BTreeSet<RoadID>, String> {
        #[derive(Deserialize)]
        struct RoadSnapshot {
            road_ids: BTreeSet<RoadID>,
        }

        let path = self.output_path(name, "json")?;
        fs::read(&path)
            .and_then(|bytes| JSON::decode::<RoadSnapshot>(&bytes))
            .map(|snapshot| snapshot.road_ids)
            .map_err(|e| format!("reading state {}: {e}", path.display()))
    }
}

fn new_tiled_fixture(
    side: u16,
    progression: simulation::economy::progression::ProgressionMode,
    round_trip: bool,
) -> Result<Simulation, String> {
    let sim = Simulation::new_game(&simulation::NewGameOptions {
        preset: simulation::MapPreset::Flat,
        land_mode: LandMode::Tiled,
        terrain_chunks: side,
        progression,
        starting_money_bucks: 150_000,
        ..simulation::NewGameOptions::default()
    })
    .map_err(|err| err.to_string())?;
    if !round_trip {
        return Ok(sim);
    }
    let snapshot = Bincode::encode(&sim).map_err(|err| err.to_string())?;
    if snapshot.len() >= 256 * 1024 * 1024 {
        return Err(format!(
            "tiled fixture snapshot exceeds its raw size cap: {}",
            snapshot.len()
        ));
    }
    Bincode::decode(&snapshot).map_err(|err| err.to_string())
}

fn replace_land_fixture(uiw: &UiWorld, sim: Simulation) -> Result<(), String> {
    session::request_world_replacement(
        uiw,
        session::WorldReplacement::NewGame(sim, saveload::SlotId::legacy()),
    )
    .then_some(())
    .ok_or_else(|| "creating a Land Tiles fixture requires singleplayer".into())
}

fn remember_rejection(first: &mut Option<(&'static str, Rejection)>, sim: &Simulation) {
    if first.is_none() {
        *first = sim.read::<CommandOutcomes>().last.first().copied();
    }
}

impl SpecialBuildingPlacement {
    fn command(self, map: &Map) -> Result<WorldCommand, String> {
        let [x, y] = self.road_at;
        let xy = vec2(x, y);
        let road = xy.is_finite().then(|| {
            let z = map.environment().height(xy).unwrap_or_default();
            map.project(xy.z(z), 0.0, ProjectFilter::ROAD).kind
        });
        let Some(ProjectKind::Road(connected_road)) = road else {
            return Err(format!("no road at {x}, {y} for {:?}", self.kind));
        };
        Ok(WorldCommand::MapBuildSpecialBuilding {
            pos: self.pos,
            kind: self.kind,
            gen: self.gen,
            zone: self.zone,
            connected_road: Some(connected_road),
        })
    }
}

impl Assertion {
    fn check(self, uiw: &UiWorld, sim: &Simulation, automation: &Automation) -> Result<(), String> {
        match self {
            Self::NoRejections => {
                if let Some((command, reason)) = automation.first_rejection {
                    return Err(format!("unexpected rejection of {command}: {reason:?}"));
                }
            }
            Self::ServicesTab => {
                if uiw.read::<GuiState>().hidden
                    || *uiw.read::<Tool>() != Tool::SpecialBuilding
                    || *uiw.read::<BuildingTab>() != BuildingTab::Services
                {
                    return Err("Services tab is not open".into());
                }
            }
            Self::TrafficRepair(expected) => {
                let state = uiw.read::<crate::gui::traffic_repair::TrafficRepairUi>().script_state(sim);
                let observed = match expected {
                    TrafficRepairAssertion::Quote => state.road.is_some() && state.quote_cost.is_some_and(|cost| cost > 0)
                        && state.lane.is_some() && state.parked.is_some(),
                    TrafficRepairAssertion::Applied => state.applied && state.debit.is_some_and(|cost| cost > 0) && !state.pending,
                    TrafficRepairAssertion::Denied => state.road.is_some() && state.error.is_some() && state.quote_cost.is_none(),
                    TrafficRepairAssertion::Junction => state.junction.is_some() && state.legal_turns > 0,
                    TrafficRepairAssertion::Compared => state.comparison_passed.is_some(),
                };
                if !observed {
                    return Err(format!("traffic repair assertion failed: {}", String::from_utf8_lossy(&JSON::encode(&state).unwrap())));
                }
            }
            Self::HouseInspector => {
                if uiw.read::<GuiState>().hidden
                    || !uiw
                        .read::<InspectedBuilding>()
                        .e
                        .and_then(|id| sim.map().get(id).map(|building| building.kind))
                        .is_some_and(|kind| kind == BuildingKind::House)
                {
                    return Err("house inspector is not open".into());
                }
            }
            Self::LandTiles {
                tick,
                owned,
                selected,
                quote,
                confirming,
                pending,
                receipt,
                treasury,
                extent,
            } => {
                use crate::gui::overlays::land_tiles::{LandTilesScriptState, LandTilesUi};
                let map = sim.map();
                let land = map.land_ownership();
                let actual_owned: BTreeSet<_> =
                    land.tiles().filter(|&tile| land.owns(tile)).collect();
                let actual_extent = land.extent().map(|extent| [extent.width, extent.height]);
                let (_, _, water, terrain, _) = map.road_preview_revision();
                drop(map);
                let generation = uiw.read::<SessionState>().generation;
                if let Some((previous_generation, previous_water, previous_terrain)) = automation
                    .land_terrain_revision
                    .replace(Some((generation, water, terrain)))
                {
                    if generation == previous_generation
                        && (water, terrain) != (previous_water, previous_terrain)
                    {
                        return Err("land purchase changed a terrain or water revision".into());
                    }
                }
                let actual_treasury = sim.read::<Government>().money.bucks();
                let actual = uiw.read::<LandTilesUi>().script_state(sim);
                let expected = LandTilesScriptState {
                    selected,
                    quote,
                    confirming,
                    pending,
                    receipt: receipt.map(LandReceipt::outcome),
                };
                if !uiw.read::<GuiState>().windows.land_tiles_open
                    || sim.get_tick() != tick
                    || actual_owned != owned
                    || extent.is_some_and(|extent| Some(extent) != actual_extent)
                    || actual != expected
                    || actual_treasury != i64::from(treasury)
                {
                    return Err(format!(
                        "land UI at tick {} with treasury {actual_treasury} and owned {actual_owned:?} on {actual_extent:?}: expected {expected:?}, got {actual:?}",
                        sim.get_tick(),
                    ));
                }
            }
            Self::MixedUseInspector { business_open } => {
                use simulation::map::GrowableBusinessState;
                let expected = if business_open { "open" } else { "shuttered" };
                let matches = uiw
                    .read::<InspectedBuilding>()
                    .e
                    .and_then(|id| sim.growable_summary(id))
                    .is_some_and(|site| {
                        site.zone.kind == ZoneKind::MixedUse
                            && site.occupants > 0
                            && if business_open {
                                site.business == Some(GrowableBusinessState::Open)
                            } else {
                                matches!(
                                    site.business,
                                    Some(GrowableBusinessState::ClosedUntil { .. })
                                )
                            }
                    });
                if !matches || uiw.read::<GuiState>().hidden {
                    return Err(format!(
                        "expected an occupied Mixed Use inspector with a {expected} business"
                    ));
                }
            }
            Self::ZonePainted { center: [x, y], cell } => {
                let sample = OBB::new(vec2(x, y), Vec2::X, 8.0, 8.0);
                let actual = sim.read::<ZoneGrid>().lot_zone(&sample);
                if actual != cell {
                    return Err(format!("expected {cell:?} at ({x}, {y}), got {actual:?}"));
                }
            }
            Self::TowerSelection {
                parcel_center,
                background_tower,
            } => {
                use crate::gui::selectable::tower_pick_geometry;
                let map = sim.map();
                let matches_tower = |building: &simulation::map::Building, [x, y]: [f32; 2]| {
                    building.obb.center().distance2(vec2(x, y)) < 0.01
                        && building.development().is_some_and(|site| site.floors >= 12)
                };
                let selected = uiw.read::<InspectedBuilding>().e;
                let building = selected
                    .and_then(|id| map.get(id))
                    .filter(|building| matches_tower(building, parcel_center))
                    .ok_or_else(|| {
                        format!("expected tower at {parcel_center:?}, selected {selected:?}")
                    })?;
                let background = background_tower
                    .map(|at| {
                        map.buildings()
                            .values()
                            .find(|building| matches_tower(building, at))
                            .ok_or_else(|| format!("missing background tower at {at:?}"))
                    })
                    .transpose()?;
                let ray = uiw
                    .read::<crate::inputmap::InputMap>()
                    .ray
                    .ok_or("missing tower pointer ray")?;
                if !tower_pick_geometry(&map, ray, building, background) {
                    return Err(format!(
                        "tower at {parcel_center:?} did not present the expected {}",
                        if background.is_some() {
                            "overlapping roof"
                        } else {
                            "sky-backed facade"
                        }
                    ));
                }
            }
            Self::PathPark {
                footpaths,
                local_one_way,
                lot_capacity,
                vehicle_entrances,
            } => {
                use simulation::map::building_parking::BuildingParkingKind;
                use simulation::map::{LaneKind, SmallRoadPreset};

                let map = sim.map();
                let actual_paths = map
                    .roads()
                    .values()
                    .filter(|road| road.is_walking_path())
                    .count();
                let local_pattern = SmallRoadPreset::LocalOneWay {
                    curb_parking: false,
                }
                .lane_pattern();
                let actual_local = map
                    .roads()
                    .values()
                    .filter(|road| {
                        road.is_one_way()
                            && road.lanes_iter().any(|(_, kind)| kind == LaneKind::Driving)
                            && road.pattern(map.lanes()) == local_pattern
                    })
                    .count();
                let lots: Vec<_> = map
                    .buildings()
                    .values()
                    .filter(|building| matches!(building.kind, BuildingKind::ParkingLot { .. }))
                    .filter_map(|building| sim.building_parking(building.id))
                    .filter_map(|summary| {
                        let BuildingParkingKind::SurfaceLot(draft) = summary.kind else {
                            return None;
                        };
                        Some((summary.capacity, draft.mouths.iter().flatten().count()))
                    })
                    .collect();
                if actual_paths != footpaths
                    || actual_local != local_one_way
                    || lots != [(lot_capacity, vehicle_entrances)]
                {
                    return Err(format!(
                        "PathPark fixture has {actual_paths} footpaths, {actual_local} local streets and parking (capacity, entrances) {lots:?}"
                    ));
                }
            }
            Self::BuildingCount { prototype, count } => {
                let kind = BuildingKind::GoodsCompany(GoodsCompanyID::new(&prototype));
                let actual = sim
                    .map()
                    .buildings()
                    .values()
                    .filter(|b| b.kind == kind)
                    .count();
                if actual != count {
                    return Err(format!(
                        "expected {count} {prototype:?} buildings, found {actual}"
                    ));
                }
            }
            Self::RoadRemovals {
                before,
                after,
                expected,
            } => {
                let before_ids = automation.dump_road_ids(&before)?;
                let after_ids = automation.dump_road_ids(&after)?;
                let removed: BTreeSet<_> = before_ids.difference(&after_ids).copied().collect();
                if removed != expected || !after_ids.is_subset(&before_ids) {
                    let added: Vec<_> = after_ids.difference(&before_ids).collect();
                    return Err(format!(
                        "{before} to {after}: expected removals {expected:?}, found {removed:?} and additions {added:?}"
                    ));
                }
            }
            Self::CameraOnFollowed => {
                let human = followed_human(uiw)?;
                if uiw.read::<InspectedEntity>().e != Some(human.into()) {
                    return Err("the followed human is no longer inspected".into());
                }
                let expected = follow_pos(sim, human.into()).ok_or("followed human despawned")?;
                let actual = uiw.camera().targetpos;
                if actual.distance(expected) > 0.01 {
                    return Err(format!(
                        "camera at {actual:?}, followed human at {expected:?}"
                    ));
                }
            }
        }
        Ok(())
    }
}

impl HumanPick {
    fn find(self, sim: &Simulation) -> Option<HumanID> {
        match self {
            Self::BoardingCar => sim
                .world()
                .humans
                .iter()
                .filter(|(_, h)| h.location == Location::Outside && h.router.target_dest.is_some())
                .filter_map(|(id, h)| {
                    let car = sim.get(h.router.personal_car?)?;
                    let dist = car.trans.pos.distance(h.trans.pos);
                    (car.vehicle.state.is_parked() && dist <= 400.0).then_some((id, dist))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, _)| id),
        }
    }
}

/// The road under the first truck (in entity order) whose manifest the road inspector lists.
fn loaded_truck_road(sim: &Simulation) -> Option<(RoadID, Vec3)> {
    let map = sim.map();
    let cargo = sim.read::<CargoBook>();
    sim.world().vehicles.iter().find_map(|(id, truck)| {
        if !matches!(truck.vehicle.kind, VehicleKind::Truck)
            || cargo
                .manifest_for(id)
                .is_none_or(|load| load.items.is_empty())
        {
            return None;
        }
        let TraverseKind::Lane(lane) = truck.it.get_travers()?.kind else {
            return None;
        };
        let road = map.get(lane)?.parent;
        sample_road_users(sim, road)
            .cargo
            .iter()
            .any(|row| row.truck == id && matches!(row.cargo, TruckCargo::Manifest(_)))
            .then_some((road, truck.trans.pos))
    })
}

fn followed_human(uiw: &UiWorld) -> Result<HumanID, String> {
    match uiw.read::<FollowEntity>().0 {
        Some(AnyEntity::HumanID(id)) => Ok(id),
        other => Err(format!("expected to follow a human, following {other:?}")),
    }
}

fn followed_location(sim: &Simulation, human: HumanID) -> Result<FollowedLocation, String> {
    sim.get(human)
        .map(|h| FollowedLocation::of(&h.location))
        .ok_or_else(|| "followed human despawned".into())
}

impl PointerPosition {
    fn to_screen(self, uiw: &UiWorld) -> Result<Vec2, String> {
        let screen = match self {
            Self::World {
                world_pos: [x, y, z],
            } => {
                let (screen, depth) = uiw.camera().project(vec3(x, y, z));
                if !depth.is_finite() || depth <= 0.0 {
                    return Err("world pointer must project in front of the camera".into());
                }
                screen
            }
            Self::Screen { screen_pos: [x, y] } => vec2(x, y),
            Self::Centered { center_offset: [x, y] } => {
                let camera = uiw.camera();
                vec2(camera.camera.viewport_w * 0.5 + x, camera.camera.viewport_h * 0.5 + y)
            }
        };
        if screen.is_finite() {
            Ok(screen)
        } else {
            Err("pointer position must project to finite screen coordinates".into())
        }
    }
}

impl PointerEvent {
    fn send(self, ctx: &mut Context, input: &mut InputContext) {
        use yakui::event::Event;

        match self {
            Self::Move(pos) => {
                input.mouse.screen_delta += pos - input.mouse.screen;
                input.mouse.screen = pos;
                move_yakui_pointer(ctx, pos);
            }
            Self::Button { down } => {
                input.observe_mouse_button(MouseButton::Left, down);
                let captured = ctx.yakui.yakui.handle_event(Event::MouseButtonChanged {
                    button: yakui::input::MouseButton::One,
                    down,
                });
                if down && !captured {
                    input.mouse.pressed.insert(MouseButton::Left);
                }
            }
        }
    }
}

fn move_yakui_pointer(ctx: &mut Context, pos: Vec2) {
    ctx.yakui
        .yakui
        .handle_event(yakui::event::Event::CursorMoved(Some(yakui::Vec2::new(
            pos.x, pos.y,
        ))));
}

impl ScriptKey {
    fn send(self, ctx: &mut Context, down: bool) {
        let (key, code) = match self {
            Self::Shift => (Key::Shift, yakui::input::KeyCode::ShiftLeft),
            Self::Escape => (Key::Escape, yakui::input::KeyCode::Escape),
            Self::LandTiles => (Key::c("L"), yakui::input::KeyCode::KeyL),
        };
        let captured = ctx
            .yakui
            .yakui
            .handle_event(yakui::event::Event::KeyChanged { key: code, down });
        if down && !captured {
            ctx.input.keyboard.pressed.insert(key);
        } else if !down {
            ctx.input.keyboard.pressed.remove(&key);
        }
    }
}

impl Builder {
    fn build(&self, sink: &mut dyn CommandSink) -> Result<(), String> {
        let origin = vec2(700.0, 700.0);
        match self {
            Self::GridCity => {
                let grid = builders::grid_city(sink, origin, 4, 4, 160.0);
                if grid.roads.len() != 24 {
                    return Err(format!("grid_city built {} of 24 roads", grid.roads.len()));
                }
            }
            Self::FillHouses => {
                if builders::fill_houses(sink, 24).is_empty() {
                    return Err("fill_houses did not build any houses".into());
                }
            }
            Self::PlaceCompany => {
                if builders::place_company(sink, "bakery", origin + vec2(80.0, -20.0)).is_none() {
                    return Err("place_company could not place bakery".into());
                }
            }
        }
        Ok(())
    }
}

pub(crate) enum FixtureOp {
    EmptyTreasury,
    Builder(Builder),
    Follow {
        pick: HumanPick,
        remaining: u64,
    },
    RunFollowing {
        human: HumanID,
        remaining: u64,
        until: Option<FollowedLocation>,
    },
    FindCargo {
        remaining: u64,
    },
}

pub(crate) enum FixtureOutcome {
    EmptyTreasury,
    Builder(Option<(&'static str, Rejection)>),
    Follow(HumanID),
    Followed,
    InspectCargo(RoadID, Vec3),
}

impl FixtureOp {
    /// Probe the current boundary. A pending probe asks the driver to run one
    /// ordinary tick before trying again.
    pub(crate) fn run(
        &mut self,
        sim: &mut Simulation,
        schedule: &mut SeqSchedule,
        steps: &mut u64,
        check_stop: &mut dyn FnMut() -> bool,
    ) -> Option<Result<FixtureOutcome, String>> {
        match self {
            Self::EmptyTreasury => {
                sim.write::<Government>().money = prototypes::Money::ZERO;
                Some(Ok(FixtureOutcome::EmptyTreasury))
            }
            Self::Builder(builder) => {
                let mut rejection = None;
                let mut sink = LiveCommandSink {
                    sim,
                    schedule,
                    queued: Vec::new(),
                    first_rejection: &mut rejection,
                    steps,
                    check_stop,
                    cancelled: false,
                };
                Some(builder.build(&mut sink).and_then(|()| {
                    if *steps == 0 {
                        Err("builder submitted no commands".into())
                    } else {
                        Ok(FixtureOutcome::Builder(rejection))
                    }
                }))
            }
            Self::Follow { pick, remaining } => {
                if let Some(human) = pick.find(sim) {
                    Some(Ok(FixtureOutcome::Follow(human)))
                } else if *remaining == 0 {
                    Some(Err(
                        "no human matched the pick in the requested ticks".into()
                    ))
                } else {
                    *remaining -= 1;
                    None
                }
            }
            Self::RunFollowing {
                human,
                remaining,
                until,
            } => match followed_location(sim, *human) {
                Ok(location)
                    if *until == Some(location) || (*remaining == 0 && until.is_none()) =>
                {
                    Some(Ok(FixtureOutcome::Followed))
                }
                Ok(location) if *remaining == 0 => {
                    Some(Err(format!("followed human still {location:?}")))
                }
                Ok(_) => {
                    *remaining -= 1;
                    None
                }
                Err(error) => Some(Err(error)),
            },
            Self::FindCargo { remaining } => {
                if let Some((road, pos)) = loaded_truck_road(sim) {
                    Some(Ok(FixtureOutcome::InspectCargo(road, pos)))
                } else if *remaining == 0 {
                    Some(Err(
                        "no loaded truck on a road in the requested ticks".into()
                    ))
                } else {
                    *remaining -= 1;
                    None
                }
            }
        }
    }
}

struct LiveCommandSink<'a> {
    sim: &'a mut Simulation,
    schedule: &'a mut SeqSchedule,
    queued: Vec<WorldCommand>,
    first_rejection: &'a mut Option<(&'static str, Rejection)>,
    steps: &'a mut u64,
    check_stop: &'a mut dyn FnMut() -> bool,
    cancelled: bool,
}

impl CommandSink for LiveCommandSink<'_> {
    fn sim(&self) -> &Simulation {
        self.sim
    }

    fn submit(&mut self, commands: &[WorldCommand]) {
        if !self.cancelled {
            self.queued.extend_from_slice(commands);
        }
    }

    fn has_pending_commands(&self) -> bool {
        !self.queued.is_empty()
    }

    fn flush(&mut self) {
        if self.cancelled || (self.check_stop)() {
            self.cancelled = true;
            self.queued.clear();
            return;
        }
        self.sim.tick(self.schedule, &self.queued);
        *self.steps += 1;
        remember_rejection(self.first_rejection, self.sim);
        self.queued.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p2_ui_wire_assertion_steps_decode() {
        for json in [
            r#"{"Assert":"NoRejections"}"#,
            r#"{"Assert":"ServicesTab"}"#,
            r#"{"Assert":"HouseInspector"}"#,
            r#"{"Assert":{"BuildingCount":{"prototype":"water-pump","count":1}}}"#,
        ] {
            assert!(JSON::decode::<Step>(json.as_bytes()).is_ok(), "{json}");
        }
    }

    #[test]
    fn p2e_set_overlay_step_decodes() {
        let step = JSON::decode::<Step>(br#"{"SetOverlay":{"Coverage":"Fire"}}"#).unwrap();
        assert!(matches!(
            step,
            Step::SetOverlay(OverlayMode::Coverage(prototypes::ServiceKind::Fire))
        ));
        assert!(JSON::decode::<Step>(br#"{"SetOverlay":{"Coverage":"WaterPump"}}"#).is_err());
        assert!(JSON::decode::<Step>(br#"{"SetOverlay":{"Coverage":"SewageOutlet"}}"#).is_err());
    }

    #[test]
    fn p4_session_and_traffic_steps_decode() {
        assert!(matches!(
            JSON::decode::<Step>(br#"{"SetSession":"MainMenu"}"#).unwrap(),
            Step::SetSession(SessionScreen::MainMenu)
        ));
        assert!(matches!(
            JSON::decode::<Step>(br#"{"SetOverlay":"Traffic"}"#).unwrap(),
            Step::SetOverlay(OverlayMode::Traffic)
        ));
        assert_eq!(SessionScreen::default(), SessionScreen::Playing);
    }

    #[test]
    fn demo_load_slot_step_decodes() {
        let json = br#"{"LoadSlot":"showcase_city"}"#;
        assert!(matches!(
            JSON::decode::<Step>(json).unwrap(),
            Step::LoadSlot(id) if id == "showcase_city"
        ));
    }

    #[test]
    fn follow1_follow_script_decodes() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/shots/follow1_showcase.json");
        let steps: Vec<Step> = JSON::decode(&fs::read(path).unwrap()).unwrap();
        assert!(steps.iter().any(|s| matches!(
            s,
            Step::FollowHuman {
                pick: HumanPick::BoardingCar,
                ..
            }
        )));
        assert!(steps.iter().any(|s| matches!(
            s,
            Step::RunFollowing {
                until: Some(FollowedLocation::Vehicle),
                ..
            }
        )));
        assert!(steps
            .iter()
            .any(|s| matches!(s, Step::Assert(Assertion::CameraOnFollowed))));
    }

    #[test]
    fn shotsprep_inspect_cargo_road_step_decodes() {
        assert!(matches!(
            JSON::decode::<Step>(br#"{"InspectCargoRoad":{"max_ticks":6000}}"#).unwrap(),
            Step::InspectCargoRoad { max_ticks: 6000 }
        ));
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/shots/p5_release.json");
        let steps: Vec<Step> = JSON::decode(&fs::read(path).unwrap()).unwrap();
        assert!(steps
            .iter()
            .any(|s| matches!(s, Step::InspectCargoRoad { .. })));
    }

    #[test]
    fn p1e_auto_steps_decode() {
        for (name, json) in [
            (
                "commands",
                r#"{"Commands":[{"SetTaxRate":{"zone":"Residential","rate_permille":150}}]}"#,
            ),
            (
                "world pointer",
                r#"{"Pointer":{"action":"Click","world_pos":[1020,900,0]}}"#,
            ),
            (
                "screen drag",
                r#"{"Pointer":{"action":{"Drag":{"to":{"screen_pos":[804,276]}}},"screen_pos":[737,276]}}"#,
            ),
            ("key", r#"{"Key":{"key":"Escape","down":true}}"#),
        ] {
            assert!(JSON::decode::<Step>(json.as_bytes()).is_ok(), "{name}");
        }
        for json in [
            r#"{"Pointer":{"action":"Move"}}"#,
            r#"{"Pointer":{"action":"Move","world_pos":[1,2,3],"screen_pos":[4,5]}}"#,
        ] {
            assert!(JSON::decode::<Step>(json.as_bytes()).is_err(), "{json}");
        }
    }

    #[test]
    fn p1e_auto_windows_are_registered() {
        let mut gui = GuiState::default();
        let names: Vec<_> = gui.windows.window_states().map(|(name, _)| name).collect();
        assert!(names.contains(&"Budget"));
        assert!(names.contains(&"City Stats"));
    }

    #[test]
    fn toolbar1_select_tool_steps_use_toolbar_names() {
        assert!(matches!(
            JSON::decode::<Step>(br#"{"SelectTool":"roads/curved"}"#).unwrap(),
            Step::SelectTool(ToolSelection::Mode(crate::gui::ToolMode::RoadCurved))
        ));
        assert!(matches!(
            JSON::decode::<Step>(br#"{"SelectTool":"transport"}"#).unwrap(),
            Step::SelectTool(ToolSelection::Category(crate::gui::ToolCategory::Transport))
        ));
        assert!(JSON::decode::<Step>(br#"{"SelectTool":"RoadbuildStraight"}"#).is_err());
    }

    #[test]
    fn toolbar1_every_shot_script_decodes() {
        let shots = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/shots");
        let mut scripts = 0;
        for entry in fs::read_dir(shots).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let bytes = fs::read(&path).unwrap();
                JSON::decode::<Vec<Step>>(&bytes)
                    .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
                scripts += 1;
            }
        }
        assert!(scripts > 10);
    }

    #[test]
    fn p1e_scripted_screenshots_and_dump_state() {
        let shots = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/shots");
        for name in [
            "zoning",
            "demand",
            "budget",
            "stats",
            "rejection",
            "grown-city",
        ] {
            let steps = fs::read(shots.join(format!("p1_{name}.json"))).unwrap();
            let steps: Vec<Step> =
                JSON::decode(&steps).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(steps
                .iter()
                .any(|s| matches!(s, Step::Screenshot(label) if label == name)));
            assert!(steps
                .iter()
                .any(|s| matches!(s, Step::DumpState(StateDump::Name(label)) if label == name)));
            if name == "grown-city" {
                assert!(steps.iter().any(|s| matches!(s, Step::RunTicks(300_000))));
            }
        }
    }

    #[test]
    fn water_gate_dump_state_hash_is_opt_in() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new_authored_flat(1, 0).unwrap();
        let uiw = UiWorld::with_defaults();
        let path = std::env::temp_dir().join(format!("egr-state-dump-{}.json", std::process::id()));
        Automation::dump_state(&path, &uiw, &sim, &[], false, None).unwrap();
        let default_state = fs::read(&path).unwrap();
        Automation::dump_state(&path, &uiw, &sim, &[], true, None).unwrap();
        let hashed_state = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        let state: BTreeMap<String, serde::de::IgnoredAny> = JSON::decode(&default_state).unwrap();
        assert!(
            !state.contains_key("map_hash"),
            "default dump included the map hash"
        );
        #[derive(Deserialize)]
        struct MapHash {
            map_hash: u64,
        }
        let state: MapHash = JSON::decode(&hashed_state).unwrap();
        let bytes = Bincode::encode(&*sim.map()).unwrap();
        assert_eq!(state.map_hash, common::hash_u64(&bytes[..]));
    }
}

#[cfg(test)]
#[test]
fn frame_profile_probe_decodes() {
    let source = include_bytes!("../../scripts/shots/frame_profile_probe.json");
    let steps: Vec<Step> = JSON::decode(source).expect("decode the native frame profile probe");
    assert_eq!(
        steps.iter().filter(|step| matches!(step, Step::LoadSlot(_))).count(),
        4
    );
    assert_eq!(
        steps.iter().filter(|step| matches!(step, Step::CaptureFrameProfile { frames: 60, pan: Some(_), .. })).count(),
        4
    );
}
