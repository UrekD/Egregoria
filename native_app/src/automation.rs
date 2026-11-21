use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use common::saveload::{self, Encoder, JSONPretty, JSON};
use engine::Context;
use geom::{vec2, vec3, Radians};
use scenarios::builders::{self, CommandSink};
use serde::{Deserialize, Serialize};
use simulation::economy::CityStats;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::WorldCommand;
use simulation::Simulation;

use crate::gui::{GuiState, Tool};
use crate::uiworld::UiWorld;

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
    OpenWindow(String),
    Screenshot(String),
    DumpState(String),
    Exit,
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
    Map { submitted_at: u64 },
    Frames { start: u64, count: u64 },
    Screenshot,
}

pub(crate) struct Automation {
    steps: VecDeque<Step>,
    step_number: usize,
    dir: PathBuf,
    exit_after_script: bool,
    wait: Option<Wait>,
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
        step_number: 0,
        dir,
        exit_after_script: args.exit_after_script,
        wait: None,
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
        match self.wait {
            Some(Wait::Map { submitted_at }) => {
                let tick = sim.read().unwrap().get_tick();
                if tick > submitted_at && rebuilt_version == Some(tick) {
                    self.wait = Some(Wait::Frames {
                        start: ctx.gfx.tick,
                        count: 2,
                    });
                }
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
                let submitted_at = sim.get_tick();
                for _ in 0..count {
                    sim.tick(schedule, []);
                }
                self.wait = Some(Wait::Map { submitted_at });
            }
            Step::RunTicks(_) => {}
            Step::Builder(builder) => {
                let mut sim = sim.write().unwrap();
                let mut sink = LiveCommandSink {
                    sim: &mut sim,
                    schedule,
                    queued: Vec::new(),
                    submitted_at: None,
                };
                builder.build(&mut sink)?;
                let submitted_at = sink.submitted_at.ok_or("builder submitted no commands")?;
                self.wait = Some(Wait::Map { submitted_at });
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
            Step::OpenWindow(name) => {
                let mut gui = uiw.write::<GuiState>();
                let (_, open) = gui
                    .windows
                    .window_states()
                    .find(|(candidate, _)| *candidate == name)
                    .ok_or_else(|| format!("unknown window {name:?}"))?;
                *open = true;
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
        }

        let open_windows = uiw
            .write::<GuiState>()
            .windows
            .window_states()
            .filter_map(|(name, open)| (*open).then_some(name))
            .collect();
        let state = Snapshot {
            tick: sim.get_tick(),
            active_tool: *uiw.read::<Tool>(),
            open_windows,
            city_stats: &sim.read::<CityStats>(),
        };
        let write = || -> io::Result<()> {
            let mut file = BufWriter::new(File::create(path)?);
            JSONPretty::encode_writer(&state, &mut file)?;
            file.flush()
        };
        write().map_err(|e| format!("writing state {}: {e}", path.display()))
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
    submitted_at: Option<u64>,
}

impl CommandSink for LiveCommandSink<'_> {
    fn sim(&self) -> &Simulation {
        self.sim
    }

    fn submit(&mut self, commands: &[WorldCommand]) {
        if !commands.is_empty() {
            self.submitted_at = Some(self.sim.get_tick());
            self.queued.extend_from_slice(commands);
        }
    }

    fn has_pending_commands(&self) -> bool {
        !self.queued.is_empty()
    }

    fn flush(&mut self) {
        self.sim.tick(self.schedule, &self.queued);
        self.queued.clear();
    }
}
