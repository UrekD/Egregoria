use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use common::saveload::{self, Encoder, JSONPretty, JSON};
use engine::{Context, Key, MouseButton};
use geom::{vec2, vec3, Radians, Vec2, OBB};
use prototypes::{BuildingGen, GoodsCompanyID};
use scenarios::builders::{self, CommandSink};
use serde::{Deserialize, Serialize};
use simulation::economy::{Budget, CityStats, Government, ZoneDemand};
use simulation::map::{BuildingKind, Map, ProjectFilter, ProjectKind, Zone, ZoneGrid, ZoneKind};
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand, WorldCommands};
use simulation::Simulation;

use crate::gui::{
    overlays::OverlayMode, toolbox::building::BuildingTab, top_demand_driver, zoning::ZONES,
    GuiState, InspectedBuilding, Tool,
};
use crate::uiworld::{ReceivedCommands, UiWorld};

const WATCHDOG: Duration = Duration::from_secs(15 * 60);
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
    Camera {
        x: f32,
        y: f32,
        z: f32,
        yaw: f32,
        pitch: f32,
        dist: f32,
    },
    Builder(Builder),
    SelectTool(Tool),
    SetOverlay(OverlayMode),
    OpenWindow(String),
    Assert(Assertion),
    Screenshot(String),
    DumpState(String),
    Exit,
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
    BuildingCount { prototype: String, count: usize },
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
}

#[derive(Deserialize)]
enum ScriptKey {
    Escape,
}

enum PointerEvent {
    Move(Vec2),
    Button { down: bool },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Builder {
    GridCity,
    FillHouses,
    PlaceCompany,
}

#[derive(Clone, Copy)]
enum Wait {
    Map { tick: u64, settle_frames: u64 },
    Frames { start: u64, count: u64 },
    Screenshot,
}

pub(crate) struct Automation {
    steps: VecDeque<Step>,
    pointer_events: VecDeque<PointerEvent>,
    step_number: usize,
    dir: PathBuf,
    exit_after_script: bool,
    wait: Option<Wait>,
    first_rejection: Option<(&'static str, Rejection)>,
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
        step_number: 0,
        dir,
        exit_after_script: args.exit_after_script,
        wait: None,
        first_rejection: None,
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
        sim: &RwLock<Simulation>,
        schedule: &mut SeqSchedule,
        rebuilt_version: Option<u64>,
    ) -> Result<bool, String> {
        let result = self.advance(ctx, uiw, sim, schedule, rebuilt_version);
        result.map_err(|e| format!("script step {}: {e}", self.step_number))
    }

    fn advance(
        &mut self,
        ctx: &mut Context,
        uiw: &UiWorld,
        sim: &RwLock<Simulation>,
        schedule: &mut SeqSchedule,
        rebuilt_version: Option<u64>,
    ) -> Result<bool, String> {
        *uiw.write::<ReceivedCommands>() = ReceivedCommands::default();
        match self.wait {
            Some(Wait::Map { tick, .. }) if rebuilt_version != Some(tick) => {
                return Ok(true);
            }
            Some(Wait::Map { settle_frames, .. }) if settle_frames > 0 => {
                self.wait = Some(Wait::Frames {
                    start: ctx.gfx.tick,
                    count: settle_frames,
                });
                return Ok(true);
            }
            Some(Wait::Frames { start, count }) if ctx.gfx.tick - start < count => {
                return Ok(true);
            }
            Some(Wait::Screenshot) => match ctx.take_screenshot_result() {
                Some(result) => result?,
                None => return Ok(true),
            },
            _ => {}
        }
        self.wait = None;

        let pending = std::mem::take(&mut *uiw.commands());
        if !pending.is_empty() {
            self.apply_commands(uiw, &mut sim.write().unwrap(), schedule, pending);
            return Ok(true);
        }

        // Each event gets its own update and layout pass before the next event.
        if let Some(event) = self.pointer_events.pop_front() {
            event.send(ctx);
            return Ok(true);
        }

        let Some(step) = self.steps.pop_front() else {
            if self.exit_after_script {
                ctx.request_exit();
            }
            return Ok(false);
        };
        self.step_number += 1;
        match step {
            Step::WaitFrames(count) => {
                self.wait = Some(Wait::Frames {
                    start: ctx.gfx.tick,
                    count,
                });
            }
            Step::RunTicks(count) if count > 0 => {
                let mut sim = sim.write().unwrap();
                for _ in 0..count {
                    sim.tick(schedule, []);
                }
                self.wait = Some(Wait::Map {
                    tick: sim.get_tick(),
                    settle_frames: 2,
                });
            }
            Step::RunTicks(_) => {}
            Step::Commands(commands) if !commands.is_empty() => {
                let mut batch = WorldCommands::default();
                batch.extend(commands);
                self.apply_commands(uiw, &mut sim.write().unwrap(), schedule, batch);
            }
            Step::Commands(_) => {}
            Step::BuildSpecialBuildings(placements) => {
                let mut sim = sim.write().unwrap();
                let mut batch = WorldCommands::default();
                {
                    let map = sim.map();
                    for placement in placements {
                        batch.push(placement.command(&map)?);
                    }
                }
                if !batch.is_empty() {
                    self.apply_commands(uiw, &mut sim, schedule, batch);
                }
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
                PointerEvent::Move(start).send(ctx);
            }
            Step::Key { key, down } => key.send(ctx, down),
            Step::EmptyTreasury => {
                // Set up the rejection fixture before submitting the real construction command.
                sim.write().unwrap().write::<Government>().money = prototypes::Money::ZERO;
            }
            Step::Builder(builder) => {
                let mut sim = sim.write().unwrap();
                let start_tick = sim.get_tick();
                let mut sink = LiveCommandSink {
                    sim: &mut sim,
                    schedule,
                    queued: Vec::new(),
                    first_rejection: &mut self.first_rejection,
                };
                builder.build(&mut sink)?;
                let tick = sim.get_tick();
                if start_tick == tick {
                    return Err("builder submitted no commands".into());
                }
                self.wait = Some(Wait::Map {
                    tick,
                    settle_frames: 2,
                });
            }
            Step::Camera {
                x,
                y,
                z,
                yaw,
                pitch,
                dist,
            } => {
                let pos = vec3(x, y, z);
                if !pos.is_finite()
                    || !yaw.is_finite()
                    || !pitch.is_finite()
                    || !dist.is_finite()
                    || dist <= 0.0
                {
                    return Err("invalid camera position, angles, or distance".into());
                }
                let mut orbit = uiw.camera_mut();
                orbit.targetpos = pos;
                orbit.targetyaw = Radians(yaw);
                orbit.targetpitch = Radians(pitch);
                orbit.targetdist = dist;
                orbit.camera.pos = pos;
                orbit.camera.yaw = orbit.targetyaw;
                orbit.camera.pitch = orbit.targetpitch;
                orbit.camera.dist = dist;
            }
            Step::SelectTool(tool) => *uiw.write::<Tool>() = tool,
            Step::SetOverlay(mode) => *uiw.write::<OverlayMode>() = mode,
            Step::OpenWindow(name) => {
                let mut gui = uiw.write::<GuiState>();
                let (_, open) = gui
                    .windows
                    .window_states()
                    .find(|(candidate, _)| *candidate == name)
                    .ok_or_else(|| format!("unknown window {name:?}"))?;
                *open = true;
            }
            Step::Assert(assertion) => {
                assertion.check(uiw, &sim.read().unwrap(), self.first_rejection)?
            }
            Step::Screenshot(name) => {
                ctx.request_screenshot(self.output_path(&name, "png")?)?;
                self.wait = Some(Wait::Screenshot);
            }
            Step::DumpState(name) => {
                self.dump_state(&self.output_path(&name, "json")?, uiw, &sim.read().unwrap())?;
            }
            Step::Exit => {
                ctx.request_exit();
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn apply_commands(
        &mut self,
        uiw: &UiWorld,
        sim: &mut Simulation,
        schedule: &mut SeqSchedule,
        commands: WorldCommands,
    ) {
        if commands.iter().all(WorldCommand::is_instant) {
            sim.apply_commands(commands.as_ref());
        } else {
            sim.tick(schedule, commands.as_ref());
        }
        remember_rejection(&mut self.first_rejection, sim);
        *uiw.write::<ReceivedCommands>() = ReceivedCommands::new(commands);
        // The next step can run as soon as this batch has been rendered.
        self.wait = Some(Wait::Map {
            tick: sim.get_tick(),
            settle_frames: 0,
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

    fn dump_state(&self, path: &Path, uiw: &UiWorld, sim: &Simulation) -> Result<(), String> {
        #[derive(Serialize)]
        struct Snapshot<'a> {
            tick: u64,
            active_tool: Tool,
            open_windows: Vec<&'static str>,
            city_stats: &'a CityStats,
            zone_cells: usize,
            zone_demand: &'a ZoneDemand,
            demand_drivers: BTreeMap<ZoneKind, (&'static str, f32)>,
            tax_rates: [u16; 4],
            rejections: Vec<(&'static str, String)>,
        }

        let open_windows = uiw
            .write::<GuiState>()
            .windows
            .window_states()
            .filter_map(|(name, open)| (*open).then_some(name))
            .collect();
        let demand = sim.read::<ZoneDemand>();
        let budget = sim.read::<Budget>();
        let state = Snapshot {
            tick: sim.get_tick(),
            active_tool: *uiw.read::<Tool>(),
            open_windows,
            city_stats: &sim.read::<CityStats>(),
            zone_cells: sim.read::<ZoneGrid>().cells.len(),
            zone_demand: &demand,
            demand_drivers: demand
                .values
                .keys()
                .filter_map(|&zone| top_demand_driver(sim, zone).map(|driver| (zone, driver)))
                .collect(),
            tax_rates: ZONES.map(|zone| budget.tax_rate(zone)),
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
            let z = map.environment.height(xy).unwrap_or_default();
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
    fn check(
        self,
        uiw: &UiWorld,
        sim: &Simulation,
        first_rejection: Option<(&'static str, Rejection)>,
    ) -> Result<(), String> {
        match self {
            Self::NoRejections => {
                if let Some((command, reason)) = first_rejection {
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
        }
        Ok(())
    }
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
        };
        if screen.is_finite() {
            Ok(screen)
        } else {
            Err("pointer position must project to finite screen coordinates".into())
        }
    }
}

impl PointerEvent {
    fn send(self, ctx: &mut Context) {
        use yakui::event::Event;

        match self {
            Self::Move(pos) => {
                ctx.input.mouse.screen_delta += pos - ctx.input.mouse.screen;
                ctx.input.mouse.screen = pos;
                ctx.yakui
                    .yakui
                    .handle_event(Event::CursorMoved(Some(yakui::Vec2::new(pos.x, pos.y))));
            }
            Self::Button { down } => {
                let captured = ctx.yakui.yakui.handle_event(Event::MouseButtonChanged {
                    button: yakui::input::MouseButton::One,
                    down,
                });
                if down && !captured {
                    ctx.input.mouse.pressed.insert(MouseButton::Left);
                } else if !down {
                    ctx.input.mouse.pressed.remove(&MouseButton::Left);
                }
            }
        }
    }
}

impl ScriptKey {
    fn send(self, ctx: &mut Context, down: bool) {
        let (key, code) = match self {
            Self::Escape => (Key::Escape, yakui::input::KeyCode::Escape),
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
    fn build(self, sink: &mut dyn CommandSink) -> Result<(), String> {
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

struct LiveCommandSink<'a> {
    sim: &'a mut Simulation,
    schedule: &'a mut SeqSchedule,
    queued: Vec<WorldCommand>,
    first_rejection: &'a mut Option<(&'static str, Rejection)>,
}

impl CommandSink for LiveCommandSink<'_> {
    fn sim(&self) -> &Simulation {
        self.sim
    }

    fn submit(&mut self, commands: &[WorldCommand]) {
        self.queued.extend_from_slice(commands);
    }

    fn has_pending_commands(&self) -> bool {
        !self.queued.is_empty()
    }

    fn flush(&mut self) {
        self.sim.tick(self.schedule, &self.queued);
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
                .any(|s| matches!(s, Step::DumpState(label) if label == name)));
            if name == "grown-city" {
                assert!(steps.iter().any(|s| matches!(s, Step::RunTicks(300_000))));
            }
        }
    }
}
