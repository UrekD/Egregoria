use std::collections::{BTreeMap, VecDeque};
use std::ptr::addr_of;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use common::history::History;
use common::saveload::{self, SlotId};
use engine::{Context, FrameContext, InputContext, MeshBuilder, Tesselator};
use geom::{vec2, vec3, Camera, LinearColor, AABB};
use simulation::economy::Government;
use simulation::Simulation;

use crate::audio::GameAudio;
use crate::automation::Automation;
use crate::debug_gui::debug_window::DebugObjs;
use crate::debug_gui::render_oldgui;
use crate::gui;
use crate::gui::follow::FollowEntity;
use crate::gui::keybinds::KeybindState;
use crate::gui::overlays::land_tiles;
use crate::gui::terraforming::TerraformingResource;
use crate::gui::toolbox::building;
use crate::gui::windows::settings::{manage_settings, Settings};
use crate::gui::UiTextures;
use crate::gui::{render_newgui, GuiState, TimeAlways, Tool};
use crate::inputmap::{Bindings, InputAction, InputMap, WorldInput};
#[cfg(feature = "multiplayer")]
use crate::network::NetworkState;
use crate::rendering::{InstancedRender, MapRenderOptions, MapRenderer, OrbitCamera};
use crate::session::save_coordinator::{SaveCoordinator, SavePermit};
use crate::session::{
    self, CityProgress, Departure, DeparturePhase, DiscoveredSlots, FailedSnapshots, PreparedWorld,
    SaveInFlight, SaveKind, SaveRequest, SessionScreen, SessionState, SlotCompatibility,
    WorldJobError, WorldJobKind, WorldJobOutcome, WorldReplacement,
};
use crate::sim_thread::{
    self, BatchId, Boundary, ReplyKind, Request, SimThread, SubmitError, SubmitErrorWithRequest,
};
use crate::uiworld::{ReceivedCommands, SaveLoadState, UiWorld};
use prototypes::GameTime;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::{WorldCommand, WorldCommands};

pub const VERSION: &str = include_str!("../../VERSION");

/// State is the main struct that contains all the state of the game and game UI.
pub struct State {
    pub sim: Arc<RwLock<Simulation>>,
    pub uiw: UiWorld,
    pub game_schedule: SeqSchedule,
    driver: Option<SimThread>,
    last_batch: Option<BatchId>,
    applied_batch: Option<BatchId>,
    rendered_batch: Option<BatchId>,
    batch_in_flight: bool,
    staged: Option<WorldCommands>,
    sent_warp: u32,
    sampled_step: Option<(u64, u64)>,
    fence: Fence,
    install: Option<Install>,
    release_sent: bool,
    capture: Option<PendingCapture>,
    retiring_save: Option<(u64, SaveInFlight)>,
    deleted_save: Option<u64>,
    save_notice_until: Option<Instant>,
    departure_target: Option<DepartureTarget>,
    lot_brush_dirty: bool,
    lot_brush_save: Option<Receiver<()>>,

    delete: Option<PendingDelete>,
    camera_bounds: AABB,
    deferred_input: VecDeque<WorldInputSample>,
    bulldozer_view: Option<DragView>,
    close_requested: bool,
    #[cfg(feature = "multiplayer")]
    network: Option<NetworkTransition>,
    #[cfg(feature = "multiplayer")]
    network_disconnecting: bool,

    instanced_renderer: InstancedRender,
    map_renderer: Option<MapRenderer>,
    immediate_renderer: MeshBuilder<true>,
    automation: Option<Automation>,
    startup: Option<Receiver<Result<StartupWorld, String>>>,
    dormant_script_state: Option<(u64, BTreeMap<String, u64>)>,

    all_audio: GameAudio,
}

#[derive(Default)]
enum Fence {
    #[default]
    Open,
    Pending(BatchId),
    Paused(Boundary),
    Resuming(BatchId),
}

enum Install {
    Startup(StartupWorld),
    Job(PreparedWorld, WorldJobKind),
    Replacement(WorldReplacement),
    #[cfg(feature = "multiplayer")]
    Network(Simulation, crate::network::Client),
    #[cfg(feature = "multiplayer")]
    RestoreNetworkSource,
}

#[cfg(feature = "multiplayer")]
enum NetworkTransition {
    Starting(Receiver<Result<NetworkReady, String>>),
    Connecting(crate::network::Client),
    Hosting(crate::network::Server),
}

#[cfg(feature = "multiplayer")]
enum NetworkReady {
    Client(crate::network::Client),
    Server(crate::network::Server),
}

enum DepartureTarget {
    Pending(Receiver<std::io::Result<SaveRequest>>),
    Ready(SaveRequest),
}

struct PendingDelete {
    slot: SlotId,
    generation: u64,
    result: Receiver<std::io::Result<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveDisposition {
    Current,
    Deleted,
    Retired { deleted: bool },
}

type SaveResult = Result<(CityProgress, simulation::SavePhase), String>;

const DEFERRED_INPUT_LIMIT: usize = 8;

struct WorldInputSample {
    generation: u64,
    input: WorldInput,
    pointer: engine::PointerFrame,
    ray: Option<geom::Ray3>,
    press_events: Vec<egui::Event>,
    tool: Tool,
    company: Option<prototypes::GoodsCompanyID>,
    drag_view: Option<DragView>,
}

#[derive(Clone, Copy, PartialEq)]
struct DragView {
    eye: geom::Vec3,
    inverse_projection: [geom::Vec4; 4],
    viewport: (f32, f32),
}

impl DragView {
    fn new(camera: &Camera) -> Self {
        let geom::Matrix4 { x, y, z, w } = camera.inv_proj_cache;
        Self {
            eye: camera.eye(),
            inverse_projection: [x, y, z, w],
            viewport: (camera.viewport_w, camera.viewport_h),
        }
    }
}

impl WorldInputSample {
    fn same_tool(&self, current: &Self) -> bool {
        self.generation == current.generation
            && self.tool == current.tool
            && self.company == current.company
    }

    fn drag_uses_changed_view(&self, current: Option<DragView>) -> bool {
        self.drag_view.is_some_and(|view| current != Some(view))
    }

    fn has_edge(&self) -> bool {
        self.input.has_edge()
            || self.pointer.interrupted
            || !self.pointer.released_buttons.is_empty()
    }
}

struct PendingCapture {
    request: BatchId,
    permit: SavePermit,
    coordinator: SaveCoordinator,
    slot: SlotId,
    name: String,
    camera: common::saveload::SlotCamera,
    sender: mpsc::Sender<SaveResult>,
}

impl engine::framework::State for State {
    fn new(ctx: &mut Context) -> Self {
        profiling::scope!("game_loop::init");
        goryak::set_blur_texture(ctx.yakui.blur_bg_texture);

        let camera = OrbitCamera::load((ctx.gfx.size.0, ctx.gfx.size.1));

        log::info!("loaded egui_render");

        let automation = Automation::take_startup();
        let game_schedule = SeqSchedule::default();
        let mut uiworld = UiWorld::init();
        let mut startup = None;
        let sim = if automation.is_some() {
            crate::init::configure_script_settings(&uiworld);
            let sim = Simulation::new_with_options(scenarios::builtin::smoke::OPTIONS);
            uiworld.write::<SessionState>().active_slot = Some(SlotId::legacy());
            uiworld.write::<SessionState>().city_name = "World (legacy)".into();
            uiworld.write::<SaveLoadState>().saved_progress = Some(CityProgress::capture(&sim));
            sim
        } else {
            *uiworld.write::<SessionScreen>() = SessionScreen::MainMenu;
            uiworld.write::<SessionState>().starting_up = true;
            let (sender, result) = mpsc::channel();
            startup = Some(result);
            std::thread::spawn(move || {
                let _ = sender.send(startup_world());
            });
            Simulation::new(false)
        };

        uiworld.write::<SaveLoadState>().threaded = true;
        let mut bindings = uiworld.write::<Bindings>();
        bindings.fill_missing_defaults();
        uiworld.write::<InputMap>().build_input_tree(&mut bindings);
        drop(bindings);

        uiworld.insert(UiTextures::new(&mut ctx.gfx, &mut ctx.yakui));

        uiworld.insert(camera.camera);
        uiworld.insert(camera);

        log::info!("version is {}", VERSION);

        {
            let s = uiworld.read::<Settings>();
            manage_settings(ctx, &s);
        }

        defer!(log::info!("finished init of game loop"));
        building::do_icons(ctx, &uiworld);

        sim.map().dispatch_all();
        let world = Arc::new(RwLock::new(sim));
        let driver = Some(SimThread::from_world(
            world.clone(),
            Simulation::schedule(),
            0,
        ));
        let me = Self {
            uiw: uiworld,
            game_schedule,
            instanced_renderer: InstancedRender::new(&mut ctx.gfx),
            map_renderer: startup
                .is_none()
                .then(|| MapRenderer::new(&mut ctx.gfx, &world.try_read().unwrap())),
            all_audio: GameAudio::new(&mut ctx.audio),
            sim: world,
            driver,
            last_batch: None,
            applied_batch: None,
            rendered_batch: None,
            batch_in_flight: false,
            staged: None,
            sent_warp: 0,
            sampled_step: None,
            fence: Fence::Open,
            install: None,
            release_sent: false,
            capture: None,
            retiring_save: None,
            deleted_save: None,
            save_notice_until: None,
            departure_target: None,
            lot_brush_dirty: false,
            lot_brush_save: None,
            delete: None,
            camera_bounds: AABB::zero(),
            deferred_input: VecDeque::new(),
            bulldozer_view: None,
            close_requested: false,
            #[cfg(feature = "multiplayer")]
            network: None,
            #[cfg(feature = "multiplayer")]
            network_disconnecting: false,
            immediate_renderer: MeshBuilder::new(ctx.gfx.tess_material),
            automation,
            startup,
            dormant_script_state: None,
        };
        me
    }

    fn frame(&mut self, ctx: &mut Context) {
        self.uiw.write::<TimeAlways>().0 += ctx.delta;
        {
            let mut timings = self.uiw.write::<Timings>();
            timings.engine_render_time.add_value(ctx.times.render_time);
            timings.gui_time.add_value(ctx.times.gui_time);
            timings.total_cpu_time.add_value(ctx.times.total_cpu_time);
            timings.all.add_value(ctx.delta);
        }

        self.uiw.write::<SessionState>().interaction_blocked = false;
        self.invalidate_placements();
        self.pump_simulation();
        if !self.step_session(ctx) {
            return;
        }
        manage_settings(ctx, &self.uiw.read::<Settings>());
        let use_world = |state: &mut Self, ctx: &mut Context, sim: &Simulation| {
            let scripted = state.automation.is_some();
            if scripted && !state.step_automation(ctx, sim) {
                return;
            }
            let view_input = state.prepare_input(ctx, scripted);
            state.update_warp();
            state.uiw.write::<SaveLoadState>().authoring_blocked = !state.can_collect_commands();
            let commands_observed = state.update_world(ctx, sim, scripted, view_input);
            let yakui_width =
                ctx.yakui.yakui.layout_dom().unscaled_viewport().size().x / ctx.yakui.zoom_factor;
            ctx.present(
                state,
                |state, frame| state.render_world(frame, sim),
                |state, ui| state.render_world_gui(ui, sim),
                |state| state.render_world_yakui(sim, yakui_width),
            );
            state.invalidate_placements();
            if state.driver.is_some() {
                state.seal_frame_commands();
                state.update_warp();
            }
            if commands_observed {
                *state.uiw.write::<ReceivedCommands>() = ReceivedCommands::default();
            }
        };
        let mut busy = true;
        if let Some(view) = self.driver.as_ref().map(SimThread::view) {
            if view.signal_redraw() && ctx.acquire_surface() {
                if let Some(sim) = view.try_read() {
                    if view.take_replaced() {
                        ctx.gfx.lamplights.reset(&ctx.gfx.device, &ctx.gfx.queue);
                        self.map_renderer = Some(MapRenderer::new(&mut ctx.gfx, &sim));
                        sim.map().dispatch_all();
                    }
                    use_world(self, ctx, &sim);
                    busy = false;
                }
            }
        } else if ctx.acquire_surface() {
            let world = self.sim.clone();
            if let Ok(sim) = world.try_read() {
                use_world(self, ctx, &sim);
                busy = false;
            };
        }
        if busy {
            let scripted = self.automation.is_some();
            if !scripted && self.prepare_input(ctx, false) {
                if session::world_interaction(&self.uiw) {
                    let sample = self.sample_world_input(ctx);
                    self.defer_world_input(sample);
                }
                self.manage_io(ctx, None);
            }
            self.update_warp();
            self.uiw.camera_mut().update(ctx);
            self.manage_gfx_params(ctx);
        }
    }

    fn resized(&mut self, ctx: &mut Context, size: (u32, u32, f64)) {
        self.uiw
            .write::<OrbitCamera>()
            .resize(ctx, size.0 as f32, size.1 as f32);
    }

    fn exit(&mut self) -> bool {
        self.close_requested = true;
        false
    }
}

impl State {
    fn prepare_input(&mut self, ctx: &mut Context, scripted: bool) -> bool {
        self.uiw
            .write::<InputMap>()
            .observe_pointer_release(ctx, scripted);
        ctx.keybind_mode = self.uiw.read::<KeybindState>().enabled.is_some();
        self.uiw.write::<KeybindState>().update(
            &mut self.uiw.write::<Bindings>(),
            &mut self.uiw.write::<InputMap>(),
            &ctx.input,
        );
        let view_input = session::view_input(&self.uiw) && !session::modal(&self.uiw);
        let land_tiles_enabled = !ctx.keybind_mode && session::world_interaction(&self.uiw);
        let land_tiles_pressed = self.uiw.write::<InputMap>().land_tiles_pressed(
            &ctx.input,
            land_tiles_enabled && !ctx.egui.last_kb_captured,
            land_tiles_enabled && !ctx.egui.last_mouse_captured,
        );
        land_tiles::latch_shortcut(&self.uiw, land_tiles_enabled, land_tiles_pressed);
        if view_input {
            let replaying = self.uiw.read::<SaveLoadState>().is_replaying();
            self.uiw.write::<InputMap>().prepare_frame(
                &ctx.input,
                !ctx.egui.last_kb_captured || replaying,
                !ctx.egui.last_mouse_captured,
            );
            if ctx.keybind_mode {
                self.uiw.write::<InputMap>().suppress_shortcuts();
            } else if !replaying {
                GuiState::apply_time_shortcut(&self.uiw);
                let switches_tool = {
                    let input = self.uiw.read::<InputMap>();
                    input.just_act.contains(&InputAction::Cancel)
                        || input.just_act.contains(&InputAction::Bulldozer)
                };
                if switches_tool {
                    self.deferred_input.clear();
                    self.bulldozer_view = None;
                    gui::tool_shortcuts(&self.uiw);
                    let mut input = self.uiw.write::<InputMap>();
                    input.just_act.remove(&InputAction::Cancel);
                    input.just_act.remove(&InputAction::Bulldozer);
                }
            }
            self.uiw.write::<GuiState>().hidden ^= self
                .uiw
                .read::<InputMap>()
                .just_act
                .contains(&InputAction::HideInterface);
        }

        view_input
    }

    fn update_world(
        &mut self,
        ctx: &mut Context,
        sim: &Simulation,
        scripted: bool,
        view_input: bool,
    ) -> bool {
        profiling::scope!("game_loop::update_world");
        let mut commands_observed = false;
        let rejections = std::mem::take(&mut self.uiw.write::<ReceivedCommands>().rejections);
        for (sequence, rejected) in rejections {
            self.uiw
                .write::<crate::gui::ToastState>()
                .observe_applied(sim, sequence, &rejected);
        }
        self.camera_bounds = sim.map().environment().bounds();
        let interaction = view_input && session::world_interaction(&self.uiw);
        if interaction && land_tiles::shortcut(&self.uiw, sim) {
            self.deferred_input.clear();
            self.bulldozer_view = None;
        }
        if interaction {
            let mut sample = self.sample_world_input(ctx);
            if !self.uiw.can_author() {
                self.defer_world_input(sample);
            } else {
                let deferred = self.deferred_input.pop_front();
                if self
                    .bulldozer_view
                    .is_some_and(|view| sample.drag_view != Some(view))
                    || deferred
                        .as_ref()
                        .is_some_and(|deferred| deferred.drag_uses_changed_view(sample.drag_view))
                {
                    self.interrupt_bulldozer(&mut sample);
                    self.apply_world_input(sim, sample);
                } else if let Some(deferred) =
                    deferred.filter(|deferred| deferred.same_tool(&sample))
                {
                    let input = sample.input.clone();
                    let pointer = sample.pointer.clone();
                    self.apply_world_input(sim, deferred);
                    self.defer_world_input(sample);
                    self.uiw.write::<InputMap>().set_world_input(input);
                    pointer.publish();
                } else {
                    self.apply_world_input(sim, sample);
                }
                commands_observed = true;
            }
        }

        if !scripted && view_input {
            self.manage_io(ctx, Some(sim));
        }
        if let Some(renderer) = &mut self.map_renderer {
            renderer.update(sim, ctx);
        }
        ctx.gfx.set_time(sim.read::<GameTime>().timestamp as f32);
        for (sound, kind) in self.uiw.write::<ImmediateSound>().orders.drain(..) {
            ctx.audio.play(sound, kind);
        }
        if self.startup.is_none() && !self.uiw.read::<SessionState>().interaction_blocked {
            self.all_audio.update(sim, &self.uiw, &mut ctx.audio);
        }
        if !scripted && interaction {
            FollowEntity::update_camera(&self.uiw, sim);
        }
        self.uiw.camera_mut().update(ctx);
        if scripted {
            *self.uiw.write::<Camera>() = self.uiw.read::<OrbitCamera>().camera;
        }
        self.manage_gfx_params(ctx);
        commands_observed
    }

    fn can_collect_commands(&self) -> bool {
        self.driver.is_none()
            || (!self.batch_in_flight
                && self.install.is_none()
                && matches!(self.fence, Fence::Open)
                && self.staged.is_none()
                && self.uiw.commands().is_empty())
    }

    fn invalidate_placements(&mut self) {
        let tool = *self.uiw.read::<Tool>();
        let company = self.placement_company(tool);
        self.deferred_input
            .retain(|sample| sample.tool == tool && sample.company == company);
        if tool != Tool::Bulldozer {
            self.bulldozer_view = None;
        }
    }

    fn placement_company(&self, tool: Tool) -> Option<prototypes::GoodsCompanyID> {
        if tool != Tool::SpecialBuilding {
            return None;
        }
        self.uiw
            .read::<gui::specialbuilding::SpecialBuildingResource>()
            .opt
            .as_ref()
            .and_then(|kind| kind.company)
    }

    fn interrupt_bulldozer(&mut self, current: &mut WorldInputSample) {
        current.pointer.interrupted = true;
        self.deferred_input
            .retain(|sample| sample.drag_view.is_none());
        self.bulldozer_view = None;
        self.uiw.write::<InputMap>().require_fresh_pointer_press();
    }

    fn sample_world_input(&self, ctx: &mut Context) -> WorldInputSample {
        let pointer = engine::pointer_frame();
        let camera = self.uiw.camera().camera;
        let ray = if pointer.blocked || ctx.egui.last_mouse_captured {
            None
        } else {
            camera.unproj_ray(ctx.input.mouse.screen)
        };
        let tool = *self.uiw.read::<Tool>();
        let press_events = if tool == Tool::Bulldozer {
            ctx.egui
                .platform
                .egui_input()
                .events
                .iter()
                .filter(|event| matches!(event, egui::Event::PointerButton { .. }))
                .take(16)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        WorldInputSample {
            generation: self.uiw.read::<SessionState>().generation,
            input: self.uiw.read::<InputMap>().world_input(),
            pointer,
            ray,
            press_events,
            tool,
            company: self.placement_company(tool),
            drag_view: (tool == Tool::Bulldozer && self.uiw.read::<Settings>().bulldozer_drag)
                .then(|| DragView::new(&camera)),
        }
    }

    fn defer_world_input(&mut self, sample: WorldInputSample) {
        if !sample.has_edge() {
            return;
        }
        if self.deferred_input.len() < DEFERRED_INPUT_LIMIT {
            self.deferred_input.push_back(sample);
        } else {
            session::notify(
                &self.uiw,
                "Simulation busy. Wait for the pending edits before placing more.",
            );
        }
    }

    fn apply_world_input(&mut self, sim: &Simulation, sample: WorldInputSample) {
        self.bulldozer_view = if sample.input.select_held() && !sample.pointer.interrupted {
            sample.drag_view
        } else {
            None
        };
        let tool = *self.uiw.read::<Tool>();
        let mut input = self.uiw.write::<InputMap>();
        input.set_world_input(sample.input);
        input.set_world_ray(sample.ray, &sim.map(), tool.is_roadbuild());
        drop(input);
        sample.pointer.publish();
        if tool == Tool::Bulldozer {
            self.uiw
                .write::<gui::bulldozer::BulldozerState>()
                .record_press_order(&sample.press_events);
        }
        gui::run_ui_systems(sim, &self.uiw);
    }

    fn render_world(&mut self, ctx: &mut FrameContext<'_>, sim: &Simulation) {
        profiling::scope!("game_loop::render");
        let Some(map_renderer) = &mut self.map_renderer else {
            return;
        };
        let start = Instant::now();
        let camera = self.uiw.camera().camera;
        let time = render_time(&sim);

        if self.automation.is_some() {
            // Scripted map changes are applied through ticks, so the tick identifies the snapshot.
            map_renderer.meshb.set_map_version(sim.get_tick());
            self.rendered_batch = self.applied_batch;
        }
        let generation = self.uiw.read::<SessionState>().generation;
        gui::overlays::prepare_ground(&self.uiw, &sim, map_renderer, &mut ctx.gfx, generation);
        let tool = *self.uiw.read::<Tool>();
        map_renderer.render(
            &sim.map(),
            time.seconds,
            &camera,
            MapRenderOptions {
                show_arrows: tool.show_arrows(),
                show_lots: tool.show_lots(),
            },
            &mut self.uiw.write::<ImmediateDraw>(),
            ctx,
        );

        self.instanced_renderer.render(&sim, ctx);
        if !self.uiw.read::<SessionState>().interaction_blocked {
            gui::overlays::draw(&sim, &self.uiw);
        }

        {
            profiling::scope!("immediate_draw");
            self.immediate_renderer.clear();
            let mut tess = self.immediate_renderer.mk_tess();
            self.uiw.camera().cull_tess(&mut tess);
            Self::prepare_debug_draw(&self.uiw, &sim, &mut tess);

            {
                let mut immediate = self.uiw.write::<ImmediateDraw>();
                immediate.apply(&mut tess, ctx);
                immediate.orders.clear();
            }
            if let Some(mut mesh) = self.immediate_renderer.build(ctx.gfx) {
                mesh.skip_depth = true;
                ctx.draw(mesh);
            }
        }

        self.uiw
            .write::<Timings>()
            .render
            .add_value(start.elapsed().as_secs_f32());
    }

    fn render_world_gui(&mut self, ui: &egui::Context, sim: &Simulation) {
        if session::playing(&self.uiw)
            && !self.uiw.read::<SessionState>().interaction_blocked
            && !session::modal(&self.uiw)
            && self.uiw.can_author()
        {
            render_oldgui(ui, &self.uiw, sim);
        }
    }

    fn render_world_yakui(&mut self, sim: &Simulation, yakui_width: f32) {
        if !self.uiw.read::<SessionState>().interaction_blocked {
            if !session::playing(&self.uiw) {
                session::render_session(&self.uiw, sim);
            } else if session::modal(&self.uiw) {
                session::render_modal(&self.uiw, sim);
            } else {
                render_newgui(&self.uiw, sim, yakui_width);
            }
            yakui::widgets::List::row().show(|| session::render_status(&self.uiw));
        }
        goryak::render_tooltip();
    }

    fn step_session(&mut self, ctx: &mut Context) -> bool {
        self.delete_slot();
        self.poll_save();
        self.poll_lot_brush_save();

        #[cfg(feature = "multiplayer")]
        if self.close_requested {
            self.cancel_network_for_close();
        }

        if self.install.is_none() && std::mem::take(&mut self.close_requested) {
            if self.automation.is_some() {
                crate::automation::fail("window closed before the script finished");
            }
            if !self.uiw.read::<SaveLoadState>().exit_requested
                && self.uiw.read::<SessionState>().departure.is_none()
            {
                self.request_departure(Departure::Exit);
            }
        }
        if self.uiw.read::<SaveLoadState>().exit_requested {
            if self.driver.is_some() && self.staged.is_none() {
                self.seal_frame_commands();
            }
            self.submit_frame_commands();
            self.release_worker(false);
            if self.driver.as_ref().is_none_or(SimThread::is_finished) {
                if let Some(mut driver) = self.driver.take() {
                    let _ = driver.take_owner();
                }
                if ready_to_exit(&self.uiw)
                    && self.retiring_save.is_none()
                    && self.delete.is_none()
                    && self.lot_brush_save.is_none()
                    && self.uiw.camera_mut().poll_save_on_exit(Instant::now())
                {
                    ctx.request_exit();
                }
            }
            self.uiw.write::<SessionState>().interaction_blocked = true;
            return false;
        }

        if self.install.is_some() {
            self.poll_install(ctx);
            return true;
        }
        if self.poll_startup() {
            return true;
        }

        let replacement = self.uiw.write::<SaveLoadState>().pending.take();
        if let Some(replacement) = replacement {
            match replacement.validate_native_size() {
                Ok(()) => self.install = Some(Install::Replacement(replacement)),
                Err(error) => session::notify(&self.uiw, error),
            }
            return true;
        }

        self.uiw.write::<SessionState>().poll_slot_scan();
        if self.automation.is_none() {
            self.route_close_input(ctx);
        }
        if std::mem::take(&mut self.uiw.write::<SessionState>().replay_requested) {
            self.request_departure(Departure::ReplayDebug);
        }
        self.drive_worker();
        #[cfg(feature = "multiplayer")]
        self.poll_network();
        if !self.poll_world_job() || self.uiw.read::<SessionState>().interaction_blocked {
            return true;
        }

        self.start_save();
        #[cfg(feature = "multiplayer")]
        if self.driver.is_none()
            && session::playing(&self.uiw)
            && !self.network_disconnecting
            && crate::network::sim_update(self)
        {
            self.network_disconnecting = true;
        }
        true
    }

    fn step_automation(&mut self, ctx: &mut Context, sim: &Simulation) -> bool {
        if self.startup.is_some()
            || self.install.is_some()
            || session::world_job_active(&self.uiw)
            || self.batch_in_flight
            || self.staged.is_some()
            || !self.uiw.commands().is_empty()
            || self.uiw.read::<SaveLoadState>().saving()
        {
            return true;
        }
        self.check_dormant_script_state(sim);
        let Some(driver) = self.driver.as_mut() else {
            return true;
        };
        let Some(automation) = self.automation.as_mut() else {
            return true;
        };
        match automation.step(
            ctx,
            &self.uiw,
            sim,
            driver,
            self.map_renderer
                .as_ref()
                .and_then(|renderer| renderer.meshb.rebuilt_version()),
            self.rendered_batch,
        ) {
            Ok(true) => {}
            Ok(false) => {
                self.assert_dormant_script_state(sim);
                self.automation = None;
                if ctx.wants_exit() {
                    self.uiw.write::<SaveLoadState>().exit_requested = true;
                    ctx.defer_exit();
                }
                return false;
            }
            Err(error) => crate::automation::fail(&error),
        }
        self.route_close_input(ctx);
        let open_save_as = self
            .uiw
            .write::<GuiState>()
            .windows
            .window_states()
            .find(|(name, _)| *name == "Save As")
            .is_some_and(|(_, open)| std::mem::take(open));
        if open_save_as {
            session::open_save_as(&self.uiw);
        }
        true
    }

    fn request_departure(&self, destination: Departure) {
        if session::playing(&self.uiw) {
            if self.driver.is_some() {
                self.uiw.write::<SaveLoadState>().departure_intent = Some(destination);
            } else if let Ok(sim) = self.sim.try_read() {
                session::request_departure(&self.uiw, &sim, destination);
            }
        } else if matches!(destination, Departure::Exit) {
            session::finish_departure(&self.uiw, destination);
        }
    }

    fn pump_simulation(&mut self) {
        let Some(driver) = self.driver.as_mut() else {
            self.uiw.write::<Timings>().per_game_system = self.game_schedule.times();
            return;
        };
        driver.pump_inline();
        while let Some(reply) = driver.poll_reply() {
            match reply.kind {
                ReplyKind::Applied(batch) => {
                    if let Some(automation) = &mut self.automation {
                        automation.applied(&batch);
                    }
                    self.batch_in_flight = false;
                    self.last_batch = Some(batch.id);
                    self.applied_batch = Some(batch.id);
                    land_tiles::receive_worker_batch(&self.uiw, &batch);
                    gui::traffic_repair::receive_worker_batch(&self.uiw, &batch);
                    self.uiw.write::<ReceivedCommands>().receive(&batch);
                    for (index, result) in batch.water_edits {
                        if let WorldCommand::EditWater(edit) = &batch.commands.as_ref()[index] {
                            self.uiw
                                .write::<TerraformingResource>()
                                .water
                                .receive_outcome(edit, &result);
                        }
                    }
                }
                ReplyKind::Paused(boundary) => {
                    if matches!(self.fence, Fence::Pending(id) if id == reply.id) {
                        self.fence = Fence::Paused(boundary);
                    }
                }
                ReplyKind::Resumed if matches!(self.fence, Fence::Resuming(id) if id == reply.id) =>
                {
                    self.fence = Fence::Open;
                }
                ReplyKind::Captured(result) => {
                    if self
                        .capture
                        .as_ref()
                        .is_some_and(|capture| capture.request == reply.id)
                    {
                        let capture = self.capture.take().unwrap();
                        publish_capture(capture, result);
                    }
                }
                ReplyKind::Rejected(error) => {
                    gui::traffic_repair::reject_worker_batch(&self.uiw, reply.id, error);
                    if Some(reply.id) == self.last_batch {
                        self.batch_in_flight = false;
                        self.last_batch = self.applied_batch;
                        if self.automation.is_some() {
                            crate::automation::fail(error);
                        }
                    }
                    if let Some(automation) = &mut self.automation {
                        if let Err(error) = automation.worker_reply(crate::sim_thread::Reply {
                            id: reply.id,
                            kind: ReplyKind::Rejected(error),
                        }) {
                            crate::automation::fail(&error);
                        }
                    }
                    if matches!(self.fence, Fence::Pending(id) | Fence::Resuming(id) if id == reply.id)
                    {
                        self.fence = Fence::Open;
                    }
                    session::notify(&self.uiw, error);
                }
                ReplyKind::Interrupted(_) if Some(reply.id) == self.last_batch => {
                    self.batch_in_flight = false;
                    self.last_batch = self.applied_batch;
                }
                _ => {
                    if let Some(automation) = &mut self.automation {
                        if let Err(error) = automation.worker_reply(reply) {
                            crate::automation::fail(&error);
                        }
                    }
                }
            }
        }
        if let Some(status) = driver.status() {
            let mut io = self.uiw.write::<SaveLoadState>();
            io.achieved_warp = status.achieved_warp;
            if let Some(replay) = &mut io.replay_ui {
                if let Some(progress) = status.replay_progress {
                    replay.progress = progress;
                } else {
                    drop(io);
                    session::end_replay(&self.uiw);
                }
            }
            let mut timings = self.uiw.write::<Timings>();
            timings.per_game_system = status.schedule_samples.to_vec();
            let step = (status.boundary.generation, status.boundary.completed_step);
            if self.sampled_step.replace(step) != Some(step) {
                timings
                    .world_update
                    .add_value(status.last_tick_duration.as_secs_f32());
            }
        }
        if driver.is_finished() && !self.release_sent {
            self.capture = None;
            self.uiw.write::<SessionState>().interaction_blocked = true;
            let mut io = self.uiw.write::<SaveLoadState>();
            io.departure_intent = None;
            io.exit_requested = true;
            log::error!("Simulation worker stopped unexpectedly");
        }
    }

    fn seal_frame_commands(&mut self) {
        if self.staged.is_some() {
            return;
        }
        let commands = std::mem::take(&mut *self.uiw.commands());
        if commands.is_empty() {
            return;
        }
        if sim_thread::can_submit_batch(&commands) {
            self.staged = Some(commands);
        } else {
            gui::specialbuilding::reject_local_batch(&self.uiw, &commands);
            land_tiles::reject_local_batch(&self.uiw, &commands);
            gui::traffic_repair::reject_local_batch(&self.uiw, &commands);
            for command in commands.iter() {
                if let WorldCommand::EditWater(edit) = command {
                    self.uiw
                        .write::<TerraformingResource>()
                        .water
                        .receive_outcome(edit, &Err(simulation::map::WaterEditError::Limit));
                }
            }
            session::notify(&self.uiw, "This edit exceeds the simulation's batch limit.");
        }
    }

    fn submit_frame_commands(&mut self) {
        if self.batch_in_flight {
            return;
        }
        let Some(driver) = &mut self.driver else {
            return;
        };
        let Some(commands) = self.staged.take() else {
            return;
        };
        let land_purchase = land_tiles::queued_in(&self.uiw, &commands);
        let traffic_edit = gui::traffic_repair::worker_batch_queued(&self.uiw, &commands);
        match driver.submit(Request::Batch(commands)) {
            Ok(id) => {
                if land_purchase {
                    land_tiles::bind_worker_batch(&self.uiw, id);
                }
                if traffic_edit {
                    gui::traffic_repair::bind_worker_batch(&self.uiw, id);
                }
                self.last_batch = Some(id);
                self.batch_in_flight = true;
            }
            Err(SubmitErrorWithRequest(SubmitError::Full, Request::Batch(batch))) => {
                self.staged = Some(batch);
            }
            Err(SubmitErrorWithRequest(SubmitError::Oversized, _)) => {
                unreachable!("a sealed frame already passed admission")
            }
            Err(_) => {}
        }
    }

    fn drive_worker(&mut self) {
        if self.driver.is_some() {
            self.seal_frame_commands();
        }
        self.submit_frame_commands();
        let Some(driver) = &mut self.driver else {
            return;
        };
        let pause = !session::playing(&self.uiw) || session::modal(&self.uiw);
        if pause
            && matches!(self.fence, Fence::Open)
            && self.staged.is_none()
            && self.uiw.commands().is_empty()
        {
            if let Ok(id) = driver.submit(Request::PauseFence {
                through_batch: self.last_batch,
            }) {
                self.fence = Fence::Pending(id);
            }
        } else if !pause && matches!(self.fence, Fence::Paused(_)) {
            if let Ok(id) = driver.submit(Request::Resume) {
                self.fence = Fence::Resuming(id);
            }
        }
        if let Fence::Paused(_) = self.fence {
            // The fence has drained the accepted tail and stopped timed steps.
            let destination = self.uiw.write::<SaveLoadState>().departure_intent.take();
            if let Some(destination) = destination {
                let view = driver.view();
                if let Some(sim) = view.try_read() {
                    session::decide_departure(&self.uiw, CityProgress::capture(&sim), destination);
                } else {
                    self.uiw.write::<SaveLoadState>().departure_intent = Some(destination);
                };
            }
        }
        {
            let mut io = self.uiw.write::<SaveLoadState>();
            if let Some(replay) = &mut io.replay_ui {
                if let Some(action) = replay.control {
                    if driver.submit(Request::ReplayControl(action)).is_ok() {
                        replay.control = None;
                    }
                }
            }
        }
        self.update_warp();
    }

    fn update_warp(&mut self) {
        let Some(driver) = &mut self.driver else {
            return;
        };
        if self.staged.is_some() {
            return;
        }
        let warp = if !session::playing(&self.uiw)
            || session::modal(&self.uiw)
            || self.uiw.read::<SaveLoadState>().is_replaying()
        {
            0
        } else {
            self.uiw.read::<Settings>().time_warp
        };
        if warp != self.sent_warp && driver.submit(Request::SetWarp(warp)).is_ok() {
            self.sent_warp = warp;
        }
    }

    #[cfg(feature = "multiplayer")]
    fn cancel_network_for_close(&mut self) {
        // Once the remote world is installed, close that world normally. Until
        // then, the local world owns the close decision even if bootstrap won
        // the race and the old worker is already releasing its owner.
        let ready = matches!(self.install, Some(Install::Network(..)));
        let waiting = self.network.take().is_some();
        let mut io = self.uiw.write::<SaveLoadState>();
        let queued = io.network_target.take().is_some();
        if ready || waiting || queued {
            io.network_busy = false;
            io.network_cancel = false;
            if self.release_sent {
                self.install = Some(Install::RestoreNetworkSource);
            } else if ready {
                self.install = None;
            }
        }
    }

    #[cfg(feature = "multiplayer")]
    fn poll_network(&mut self) {
        use crate::gui::windows::network::NetworkConnectionInfo;
        use crate::network::{self, Bootstrap, NetworkTarget};
        use common::saveload::Encoder;

        let (cancel, target) = {
            let mut io = self.uiw.write::<SaveLoadState>();
            (std::mem::take(&mut io.network_cancel), io.network_target)
        };
        if cancel && !self.release_sent {
            if self.driver.is_some() {
                self.network = None;
                let mut io = self.uiw.write::<SaveLoadState>();
                io.network_busy = false;
                io.network_target = None;
                return;
            }
            self.network_disconnecting = true;
        }

        if self.network_disconnecting {
            let generation = self.uiw.read::<SessionState>().generation + 1;
            if self.uiw.read::<SaveLoadState>().save_result.is_some()
                || self.delete.is_some()
                || !self.uiw.read::<SaveCoordinator>().try_swap(generation)
            {
                return;
            }
            *self.uiw.write::<NetworkState>() = NetworkState::default();
            self.uiw.write::<SessionState>().generation = generation;
            land_tiles::reset(&self.uiw, generation);
            gui::traffic_repair::network_reset(&self.uiw, generation);
            let mut io = self.uiw.write::<SaveLoadState>();
            io.threaded = true;
            io.save_result = None;
            io.achieved_warp = None;
            let _ = std::mem::take(&mut *self.uiw.commands());
            *self.uiw.write::<ReceivedCommands>() = Default::default();
            self.driver = Some(SimThread::from_world(
                self.sim.clone(),
                std::mem::take(&mut self.game_schedule),
                generation,
            ));
            self.last_batch = None;
            self.applied_batch = None;
            self.rendered_batch = None;
            self.batch_in_flight = false;
            self.release_sent = false;
            self.sent_warp = 0;
            self.fence = Fence::Open;
            self.network_disconnecting = false;
            drop(io);
            session::notify(
                &self.uiw,
                "Disconnected. Pending multiplayer input was discarded.",
            );
            return;
        }

        if let (Some(target), Fence::Paused(boundary)) = (target, &self.fence) {
            let tick = boundary.tick;
            self.uiw.write::<SaveLoadState>().network_target = None;
            let (sender, receiver) = mpsc::channel();
            let mut info = (*self.uiw.read::<NetworkConnectionInfo>()).clone();
            std::thread::spawn(move || {
                common::saveload::JSONPretty::save_silent(&info, "netinfo");
                let network = match target {
                    NetworkTarget::Host => {
                        network::start_server(&mut info, tick).map(NetworkReady::Server)
                    }
                    NetworkTarget::Connect => {
                        network::start_client(&mut info).map(NetworkReady::Client)
                    }
                };
                let _ = sender.send(network.ok_or(info.error));
            });
            self.network = Some(NetworkTransition::Starting(receiver));
        }

        let Some(transition) = self.network.take() else {
            return;
        };
        let fail = |uiw: &UiWorld, message: String| {
            uiw.write::<NetworkConnectionInfo>().error = message;
            uiw.write::<SaveLoadState>().network_busy = false;
        };
        self.network = match transition {
            NetworkTransition::Starting(receiver) => match receiver.try_recv() {
                Ok(Ok(NetworkReady::Server(server))) => Some(NetworkTransition::Hosting(server)),
                Ok(Ok(NetworkReady::Client(client))) => Some(NetworkTransition::Connecting(client)),
                Ok(Err(message)) => {
                    fail(&self.uiw, message);
                    None
                }
                Err(TryRecvError::Empty) => Some(NetworkTransition::Starting(receiver)),
                Err(TryRecvError::Disconnected) => {
                    fail(&self.uiw, "Network startup stopped unexpectedly.".into());
                    None
                }
            },
            NetworkTransition::Connecting(mut client) => match network::poll_bootstrap(&mut client)
            {
                Bootstrap::Waiting => Some(NetworkTransition::Connecting(client)),
                Bootstrap::Ready(sim) => {
                    self.install = Some(Install::Network(sim, client));
                    None
                }
                Bootstrap::Failed(message) => {
                    fail(&self.uiw, message);
                    None
                }
            },
            NetworkTransition::Hosting(server) => {
                self.release_worker(true);
                let generation = self.uiw.read::<SessionState>().generation + 1;
                if self
                    .driver
                    .as_ref()
                    .is_some_and(|driver| !driver.is_finished())
                    || self.uiw.read::<SaveLoadState>().save_result.is_some()
                    || self.delete.is_some()
                    || !self.uiw.read::<SaveCoordinator>().try_swap(generation)
                {
                    Some(NetworkTransition::Hosting(server))
                } else if let Some(Ok(owner)) = self.driver.as_mut().and_then(SimThread::take_owner)
                {
                    debug_assert_eq!(owner.boundary.generation + 1, generation);
                    self.driver = None;
                    self.sim = owner.world;
                    self.game_schedule = owner.schedule;
                    self.uiw.write::<SessionState>().generation = generation;
                    gui::traffic_repair::network_reset(&self.uiw, generation);
                    let mut io = self.uiw.write::<SaveLoadState>();
                    io.threaded = false;
                    io.network_busy = false;
                    io.save_result = None;
                    io.achieved_warp = None;
                    *self.uiw.write::<NetworkState>() = NetworkState::Server(server);
                    self.release_sent = false;
                    self.fence = Fence::Open;
                    None
                } else {
                    fail(
                        &self.uiw,
                        "The simulation worker failed during host transfer.".into(),
                    );
                    None
                }
            }
        };
    }

    fn release_worker(&mut self, transfer: bool) {
        if self.release_sent || self.staged.is_some() || !self.uiw.commands().is_empty() {
            return;
        }
        if let Some(driver) = &mut self.driver {
            if driver.is_finished()
                || driver
                    .submit(if transfer {
                        Request::ReleaseOwner
                    } else {
                        Request::Stop
                    })
                    .is_ok()
            {
                self.release_sent = true;
            }
        }
    }

    fn route_close_input(&self, ctx: &Context) {
        route_close_input(
            &self.uiw,
            &ctx.input,
            ctx.egui.last_kb_captured,
            ctx.egui.last_mouse_captured,
        );
    }

    /// All scripts using session screens verify that the dormant city is immutable.
    fn check_dormant_script_state(&mut self, sim: &Simulation) {
        if self.automation.is_none() {
            return;
        }
        let dormant = !session::playing(&self.uiw) || session::modal(&self.uiw);
        if dormant && self.dormant_script_state.is_none() {
            self.dormant_script_state = Some((sim.get_tick(), sim.hashes()));
        } else if !dormant {
            self.assert_dormant_script_state(sim);
        }
    }

    fn assert_dormant_script_state(&mut self, sim: &Simulation) {
        if let Some((tick, hashes)) = self.dormant_script_state.take() {
            if sim.get_tick() != tick || sim.hashes() != hashes {
                crate::automation::fail("session menu changed the dormant city's tick or hashes");
            }
            log::info!("Verified dormant city tick {tick} and hashes");
        }
    }

    /// Returns true while startup owns the dormant world.
    fn poll_startup(&mut self) -> bool {
        let Some(startup) = &self.startup else {
            return false;
        };
        let result = match startup.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => {
                Err("The startup worker stopped unexpectedly.".into())
            }
        };
        self.startup = None;
        match result {
            Ok(startup) => self.install = Some(Install::Startup(startup)),
            Err(error) => {
                log::error!("{error}");
                std::process::exit(1);
            }
        }
        true
    }

    fn install_startup(&mut self, ctx: &mut Context, startup: StartupWorld, generation: u64) {
        let StartupWorld {
            world,
            slots,
            failed_snapshots,
            loaded_entry,
            message,
        } = startup;
        self.replace_world(ctx, world.replacement, generation);
        session::set_screen(&self.uiw, SessionScreen::MainMenu);
        let mut state = self.uiw.write::<SessionState>();
        state.starting_up = false;
        state.refresh_slots = false;
        state.city_name = world.name;
        if let Some(entry) = loaded_entry {
            state.remember_loaded_snapshot(entry);
        }
        state.failed_snapshots = failed_snapshots;
        state.update_slot_catalog(slots);
        let mut io = self.uiw.write::<SaveLoadState>();
        io.autosave_index = (0..3)
            .min_by_key(|&index| {
                let id = SlotId::autosave(index);
                state.slots.iter().find_map(|(entry, _)| {
                    (entry.id == id).then(|| {
                        entry
                            .meta
                            .as_ref()
                            .map_or(entry.modified_unix, |meta| meta.saved_at_unix)
                    })
                })
            })
            .unwrap_or_default();
        io.startup_message = message;
    }

    /// Returns false while a world job owns the session transition.
    fn poll_world_job(&mut self) -> bool {
        if self.uiw.read::<SessionState>().departure.is_some() {
            return true;
        }
        let Some((job, result)) = take_world_job_result(&self.uiw, &engine::pointer_frame()) else {
            return !session::world_job_active(&self.uiw);
        };
        let mut io = self.uiw.write::<SaveLoadState>();
        match result {
            Ok(WorldJobOutcome::Replace(world)) if crate::network::is_singleplayer(&self.uiw) => {
                match world.replacement.validate_native_size() {
                    Ok(()) => self.install = Some(Install::Job(world, job.kind)),
                    Err(error) if matches!(job.kind, WorldJobKind::Replay) => {
                        drop(io);
                        session::notify(&self.uiw, format!("Could not load replay. {error}"));
                    }
                    Err(error) => io.world_error = Some(error),
                }
            }
            Ok(WorldJobOutcome::ResumeDormant) if crate::network::is_singleplayer(&self.uiw) => {
                drop(io);
                if let WorldJobKind::Snapshot(entry) = job.kind {
                    if let Some(view) = self.driver.as_ref().map(SimThread::view) {
                        if let Some(sim) = view.try_read() {
                            session::restore_dormant_camera(&self.uiw, &sim, &entry);
                        }
                    } else if let Ok(sim) = self.sim.try_read() {
                        session::restore_dormant_camera(&self.uiw, &sim, &entry);
                    }
                }
                session::set_screen(&self.uiw, SessionScreen::Playing);
            }
            Ok(_) => {
                io.world_error = Some("New Game and Load are unavailable in multiplayer.".into())
            }
            Err(WorldJobError::SnapshotChanged) => {
                self.uiw.write::<SessionState>().refresh_slots = true;
                io.world_error = Some("The save changed. Choose it again after scanning.".into());
            }
            Err(WorldJobError::Failed(error)) => match job.kind {
                WorldJobKind::Snapshot(entry) => {
                    self.uiw.write::<SessionState>().mark_unreadable(&entry);
                    io.world_error = Some(error);
                }
                WorldJobKind::Replay => {
                    drop(io);
                    session::notify(&self.uiw, format!("Could not load replay. {error}"));
                }
                WorldJobKind::NewGame => io.world_error = Some(error),
            },
        }
        false
    }

    fn poll_install(&mut self, ctx: &mut Context) {
        self.uiw.write::<SessionState>().interaction_blocked = true;
        #[cfg(feature = "multiplayer")]
        if matches!(self.install, Some(Install::RestoreNetworkSource)) {
            match self
                .driver
                .as_mut()
                .and_then(SimThread::restore_paused_owner)
            {
                Some(Ok((driver, boundary))) => {
                    self.driver = Some(driver);
                    self.fence = Fence::Paused(boundary);
                    self.sent_warp = 0;
                    self.release_sent = false;
                    self.install = None;
                }
                Some(Err(())) => {
                    self.install = None;
                    self.uiw.write::<SaveLoadState>().exit_requested = true;
                }
                None => {}
            }
            return;
        }
        self.submit_frame_commands();
        self.release_worker(true);
        if self
            .driver
            .as_ref()
            .is_some_and(|driver| !driver.is_finished())
        {
            return;
        }
        let generation = self.uiw.read::<SessionState>().generation + 1;
        if !self.uiw.read::<SaveCoordinator>().try_swap(generation) {
            return;
        }
        if let Some(mut driver) = self.driver.take() {
            if !matches!(driver.take_owner(), Some(Ok(_))) {
                self.install = None;
                self.uiw.write::<SaveLoadState>().exit_requested = true;
                return;
            }
        }
        match self.install.take().unwrap() {
            Install::Startup(startup) => self.install_startup(ctx, startup, generation),
            Install::Replacement(replacement) => self.replace_world(ctx, replacement, generation),
            #[cfg(feature = "multiplayer")]
            Install::Network(sim, client) => {
                *self.uiw.write::<NetworkState>() = NetworkState::Client(client);
                self.replace_world(ctx, WorldReplacement::Network(sim), generation);
            }
            #[cfg(feature = "multiplayer")]
            Install::RestoreNetworkSource => unreachable!("source restore polled before install"),
            Install::Job(world, kind) => {
                self.replace_world(ctx, world.replacement, generation);
                self.uiw.write::<SessionState>().city_name = world.name;
                match kind {
                    WorldJobKind::Snapshot(entry) => self
                        .uiw
                        .write::<SessionState>()
                        .remember_loaded_snapshot(entry),
                    WorldJobKind::NewGame => {
                        // The new driver starts at zero warp.
                        let treasury = self.sim.try_read().unwrap().read::<Government>().money;
                        session::notify(
                            &self.uiw,
                            format!("Opening balance after starter infrastructure is {treasury}."),
                        );
                    }
                    WorldJobKind::Replay => {}
                }
            }
        }
    }

    fn replace_world(&mut self, ctx: &mut Context, replacement: WorldReplacement, generation: u64) {
        if self.dormant_script_state.is_some() {
            let world = self.sim.clone();
            let old = world
                .try_read()
                .expect("the old world was released before replacement");
            self.assert_dormant_script_state(&old);
        }
        // Move the old receiver out before the UI is reset. Publication that
        // won the generation gate can still have a result in flight.
        let old_generation = self.uiw.read::<SessionState>().generation;
        let save_notice = {
            let mut io = self.uiw.write::<SaveLoadState>();
            if let Some(save) = io.save_result.take() {
                debug_assert!(self.retiring_save.is_none());
                self.retiring_save = Some((old_generation, save));
            }
            io.notice
                .take_if(|(until, _)| Some(*until) == self.save_notice_until)
        };
        if let Some(capture) = self.capture.take() {
            let _ = capture.sender.send(Err("The save was cancelled.".into()));
        }
        let sim = session::install_world(&self.uiw, replacement, generation);
        self.all_audio.reset_world();
        self.immediate_renderer.clear();
        ctx.gfx.lamplights.reset(&ctx.gfx.device, &ctx.gfx.queue);
        self.map_renderer = Some(MapRenderer::new(&mut ctx.gfx, &sim));
        sim.map().dispatch_all();
        ctx.gfx.update_simplelit_bg();
        let schedule = Simulation::schedule();
        self.sim = Arc::new(RwLock::new(sim));
        let threaded = crate::network::is_singleplayer(&self.uiw);
        let mut io = self.uiw.write::<SaveLoadState>();
        io.notice = save_notice;
        io.threaded = threaded;
        if threaded {
            let replay = io.replay.take();
            self.driver = Some(SimThread::from_world_with_replay(
                self.sim.clone(),
                schedule,
                generation,
                replay,
            ));
            if let Some(progress) = self
                .driver
                .as_ref()
                .and_then(SimThread::status)
                .and_then(|status| status.replay_progress)
            {
                io.replay_ui = Some(crate::uiworld::ReplayUi {
                    progress,
                    speed: 1,
                    control: Some(sim_thread::ReplayControl::Speed(1)),
                });
            }
        } else {
            self.game_schedule = schedule;
        }
        self.release_sent = false;
        self.fence = Fence::Open;
        self.sent_warp = 0;
        self.last_batch = None;
        self.applied_batch = None;
        self.rendered_batch = None;
        self.batch_in_flight = false;
        self.staged = None;
        self.departure_target = None;
        self.deferred_input.clear();
        self.bulldozer_view = None;
    }

    fn start_save(&mut self) {
        session::poll_named_save(&self.uiw);
        if !session::can_save(&self.uiw) || self.retiring_save.is_some() || self.delete.is_some() {
            return;
        }
        let departure_save = self
            .uiw
            .read::<SessionState>()
            .departure
            .is_some_and(|departure| departure.phase == DeparturePhase::SaveRequested);
        let autosave_allowed = session::world_interaction(&self.uiw);
        let mut io = self.uiw.write::<SaveLoadState>();
        if io.save_result.is_some()
            || io.named_save.is_some()
            || !(departure_save
                || io.pending_save.is_some()
                || (autosave_allowed && io.please_save))
        {
            return;
        }
        let request = if departure_save {
            let request = match &self.departure_target {
                None => {
                    let plan = session::departure_save_plan(&self.uiw);
                    let (sender, receiver) = mpsc::channel();
                    self.departure_target = Some(DepartureTarget::Pending(receiver));
                    std::thread::spawn(move || {
                        let _ = sender.send(plan(saveload::save_dir()));
                    });
                    return;
                }
                Some(DepartureTarget::Pending(receiver)) => match receiver.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Empty) => return,
                    Err(TryRecvError::Disconnected) => Err(std::io::Error::other(
                        "The save request stopped unexpectedly.",
                    )),
                },
                Some(DepartureTarget::Ready(request)) => Ok(request.clone()),
            };
            match request {
                Ok(request) => {
                    self.departure_target = Some(DepartureTarget::Ready(request.clone()));
                    request
                }
                Err(error) => {
                    self.departure_target = None;
                    drop(io);
                    self.uiw
                        .write::<SessionState>()
                        .departure
                        .as_mut()
                        .unwrap()
                        .phase = DeparturePhase::SaveFailed;
                    session::notify(&self.uiw, format!("Could not choose a save slot. {error}"));
                    return;
                }
            }
        } else if let Some(request) = &io.pending_save {
            request.clone()
        } else {
            let slot = SlotId::autosave(io.autosave_index);
            let state = self.uiw.read::<SessionState>();
            SaveRequest {
                slot,
                name: format!(
                    "{} (autosave)",
                    state.city_name.trim_end_matches(" (autosave)")
                ),
                kind: SaveKind::Autosave,
            }
        };

        let mut session = self.uiw.write::<SessionState>();
        let coordinator = self.uiw.read::<SaveCoordinator>().clone();
        let Some(permit) = coordinator.request(session.generation, &request.slot) else {
            return;
        };
        let (sender, result) = mpsc::channel();
        let id = permit.id;
        let revision = session::snapshot_revision();
        let camera = self.uiw.camera().slot_camera();
        let slot = request.slot.clone();
        let name = request.name.clone();
        if let Some(driver) = &mut self.driver {
            let boundary = if departure_save {
                let Fence::Paused(boundary) = self.fence else {
                    return;
                };
                Some(boundary)
            } else {
                None
            };
            let Ok(request) = driver.submit(Request::Capture {
                boundary,
                revision,
                permit: Some(permit.clone()),
            }) else {
                return;
            };
            self.capture = Some(PendingCapture {
                request,
                permit,
                coordinator,
                slot,
                name,
                camera,
                sender,
            });
        } else {
            let sim = self.sim.clone();
            std::thread::spawn(move || {
                let captured = (|| {
                    let sim = sim
                        .read()
                        .map_err(|_| "The simulation world failed.".to_owned())?;
                    if permit.cancelled() {
                        return Err("The save was cancelled.".into());
                    }
                    sim.capture_slot(revision)
                        .map(|snapshot| (snapshot, CityProgress::capture(&sim)))
                        .map_err(|error| error.to_string())
                })();
                let saved =
                    encode_and_publish_slot(&coordinator, &permit, &slot, name, camera, captured);
                let _ = sender.send(saved);
            });
        }
        match request.kind {
            SaveKind::Departure => {
                self.departure_target = None;
                session.departure.as_mut().unwrap().phase = DeparturePhase::Saving(id);
            }
            SaveKind::Autosave => {
                io.please_save = false;
                io.autosave_index = (io.autosave_index + 1) % 3;
            }
            _ => {
                io.pending_save = None;
                io.please_save = false;
            }
        }
        io.save_result = Some(SaveInFlight {
            id,
            request,
            revision,
            result,
        });
    }

    fn waiting_for_delete(&self, generation: u64, save: &SaveInFlight) -> bool {
        self.delete.as_ref().is_some_and(|delete| {
            delete.generation == generation && delete.slot == save.request.slot
        })
    }

    fn poll_save(&mut self) {
        if let Some((generation, save)) = &self.retiring_save {
            if !self.waiting_for_delete(*generation, save) {
                if let Some(result) = try_save_result(save) {
                    let (_, save) = self.retiring_save.take().unwrap();
                    let deleted = self.deleted_save.take() == Some(save.id);
                    self.report_save(save, result, SaveDisposition::Retired { deleted });
                }
            }
        }
        let generation = self.uiw.read::<SessionState>().generation;
        let completion = {
            let mut io = self.uiw.write::<SaveLoadState>();
            if io
                .save_result
                .as_ref()
                .is_some_and(|save| self.waiting_for_delete(generation, save))
            {
                return;
            }
            take_save_result(&mut io.save_result)
        };
        if let Some((save, result)) = completion {
            let disposition = if self.deleted_save.take() == Some(save.id) {
                SaveDisposition::Deleted
            } else {
                SaveDisposition::Current
            };
            self.report_save(save, result, disposition);
            self.lot_brush_dirty = true;
        }
    }

    fn report_save(
        &mut self,
        save: SaveInFlight,
        result: SaveResult,
        disposition: SaveDisposition,
    ) {
        if apply_save_result(&self.uiw, save, result, disposition) {
            self.save_notice_until = self
                .uiw
                .read::<SaveLoadState>()
                .notice
                .as_ref()
                .map(|(until, _)| *until);
        }
    }

    fn poll_lot_brush_save(&mut self) {
        if let Some(save) = &self.lot_brush_save {
            if matches!(save.try_recv(), Err(TryRecvError::Empty)) {
                return;
            }
            self.lot_brush_save = None;
        }
        if std::mem::take(&mut self.lot_brush_dirty) {
            self.lot_brush_save = Some(self.uiw.save_lot_brush_async());
        }
    }

    fn delete_slot(&mut self) {
        if let Some(delete) = &self.delete {
            let result = match delete.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    Err(std::io::Error::other("Save deletion stopped unexpectedly"))
                }
            };
            if let Err(error) = result {
                session::notify(
                    &self.uiw,
                    format!("Could not delete {}. {error}", delete.slot),
                );
            } else if let Some(id) = apply_delete_success(
                &self.uiw,
                delete.generation,
                &delete.slot,
                self.retiring_save.as_ref(),
            ) {
                self.deleted_save = Some(id);
            }
            self.uiw.write::<SessionState>().refresh_slots = true;
            self.delete = None;
        }
        if let Some(slot) = self.uiw.write::<SaveLoadState>().delete_slot.take() {
            let (sender, receiver) = mpsc::channel();
            let coordinator = self.uiw.read::<SaveCoordinator>().clone();
            let target = slot.clone();
            let generation = self.uiw.read::<SessionState>().generation;
            self.delete = Some(PendingDelete {
                slot,
                generation,
                result: receiver,
            });
            std::thread::spawn(move || {
                let _ = sender.send(coordinator.delete(generation, saveload::save_dir(), &target));
            });
        }
    }

    fn manage_gfx_params(&mut self, ctx: &mut Context) {
        let t = std::f32::consts::TAU
            * (ctx.gfx.render_params.value().time - 8.0 * GameTime::HOUR as f32)
            / GameTime::DAY as f32;

        let sun = vec3(t.cos(), t.sin() * 0.5, t.sin() + 0.5).normalize();

        self.uiw.insert(ctx.gfx.perf.as_static());

        let params = ctx.gfx.render_params.value_mut();
        params.time_always = self.uiw.time_always();
        params.sun_col = 4.0
            * sun.z.max(0.0).sqrt().sqrt()
            * LinearColor::new(1.0, 0.95 + sun.z * 0.05, 0.95 + sun.z * 0.05, 1.0);
        let camera = self.uiw.read::<OrbitCamera>();
        params.sun = sun;
        params.viewport = vec2(ctx.gfx.size.0 as f32, ctx.gfx.size.1 as f32);
        params.sun_shadow_proj = camera
            .camera
            .build_sun_shadowmap_matrix(
                sun,
                params.shadow_mapping_resolution as f32,
                &ctx.gfx.frustrum,
            )
            .try_into()
            .unwrap();
        params.unproj_pos = self
            .uiw
            .read::<InputMap>()
            .unprojected
            .unwrap_or_default()
            .xy();
        params.terraforming_mode_radius = matches!(*self.uiw.read::<Tool>(), Tool::Terraforming)
            .then(|| self.uiw.read::<TerraformingResource>().radius)
            .unwrap_or_default();
        drop(camera);
        let c = simulation::colors();
        params.sand_col = c.sand_col.into();
        params.sea_col = c.sea_col.into();
    }

    fn manage_io(&mut self, ctx: &mut Context, sim: Option<&Simulation>) {
        let map = sim.map(Simulation::map);
        self.uiw.camera_mut().camera_movement(
            ctx,
            ctx.delta,
            &self.uiw.read::<InputMap>(),
            &self.uiw.read::<Settings>(),
            self.camera_bounds,
            |p| map.as_ref().and_then(|map| map.environment().height(p)),
        );
        *self.uiw.write::<Camera>() = self.uiw.read::<OrbitCamera>().camera;
    }

    fn prepare_debug_draw(uiw: &UiWorld, sim: &Simulation, tess: &mut Tesselator<'_>) {
        if session::world_interaction(uiw) {
            profiling::scope!("debug_objs");
            let objs = uiw.read::<DebugObjs>();
            for (val, _, obj) in &objs.0 {
                if *val {
                    obj(tess, sim, uiw);
                }
            }
        }

        let mut immediate = uiw.write::<ImmediateDraw>();
        let map = sim.map();
        let mut col = LinearColor::WHITE;
        col.a = 0.2;
        unsafe {
            for v in &*addr_of!(geom::DEBUG_POS) {
                immediate.circle(*v, 1.0).color(LinearColor::RED);
            }
            for v in &*addr_of!(geom::DEBUG_OBBS) {
                immediate
                    .obb(
                        *v,
                        map.environment().height(v.center()).unwrap_or(0.0) + 8.0,
                    )
                    .color(col);
            }
            for v in &*addr_of!(geom::DEBUG_SPLINES) {
                immediate
                    .polyline(
                        v.smart_points(1.0, 0.0, 1.0)
                            .map(|x| x.z(10.0))
                            .collect::<Vec<_>>(),
                        5.0,
                        false,
                    )
                    .color(col);
            }
            geom::DEBUG_OBBS.clear();
            geom::DEBUG_SPLINES.clear();
            geom::DEBUG_POS.clear();
        }
    }
}

fn try_save_result(save: &SaveInFlight) -> Option<SaveResult> {
    match save.result.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            Some(Err("The save worker stopped unexpectedly.".into()))
        }
    }
}

fn take_save_result(in_flight: &mut Option<SaveInFlight>) -> Option<(SaveInFlight, SaveResult)> {
    let result = try_save_result(in_flight.as_ref()?)?;
    Some((in_flight.take().unwrap(), result))
}

fn apply_delete_success(
    uiw: &UiWorld,
    generation: u64,
    slot: &SlotId,
    retiring: Option<&(u64, SaveInFlight)>,
) -> Option<u64> {
    let retired = retiring
        .filter(|(old_generation, save)| {
            *old_generation == generation && save.request.slot == *slot
        })
        .map(|(_, save)| save.id);
    let mut session = uiw.write::<SessionState>();
    if session.generation != generation {
        return retired;
    }
    let mut io = uiw.write::<SaveLoadState>();
    if session.snapshot_slot() == Some(slot) {
        session.set_snapshot_slot(Some(slot.clone()));
        io.saved_progress = None;
    }
    io.save_result
        .as_ref()
        .filter(|save| save.request.slot == *slot)
        .map(|save| save.id)
        .or(retired)
}

fn apply_save_result(
    uiw: &UiWorld,
    save: SaveInFlight,
    result: SaveResult,
    disposition: SaveDisposition,
) -> bool {
    let mut session = uiw.write::<SessionState>();
    let mut io = uiw.write::<SaveLoadState>();
    // Refresh even on failure if the snapshot changed before a durability error.
    session.refresh_slots = true;
    let current_generation = !matches!(disposition, SaveDisposition::Retired { .. });
    let deleted = matches!(
        disposition,
        SaveDisposition::Deleted | SaveDisposition::Retired { deleted: true }
    );
    let departure = session.departure.filter(|departure| {
        current_generation && departure.phase == DeparturePhase::Saving(save.id)
    });
    let mut destination = None;
    let notice = match result {
        Ok(_) if deleted => departure.map(|_| "The save was deleted.".into()),
        Ok((progress, phase)) => {
            if disposition == SaveDisposition::Current {
                if matches!(save.request.kind, SaveKind::SaveAs | SaveKind::Departure) {
                    session.set_snapshot_slot(Some(save.request.slot.clone()));
                    session.city_name = save.request.name;
                }
                if !matches!(save.request.kind, SaveKind::Autosave) {
                    io.saved_progress = Some(progress);
                    session.remember_published_snapshot(save.revision);
                }
            }
            destination = departure.map(|departure| departure.destination);
            if let simulation::SavePhase::SidecarsFailed { replay, meta } = phase {
                log::warn!("Saved snapshot; sidecar errors: replay={replay:?}, metadata={meta:?}");
                let sidecars = match (replay.is_some(), meta.is_some()) {
                    (true, true) => "replay and metadata",
                    (true, false) => "replay",
                    _ => "metadata",
                };
                Some(format!("City saved. Could not save {sidecars}."))
            } else if matches!(save.request.kind, SaveKind::Manual | SaveKind::SaveAs) {
                Some("City saved.".into())
            } else {
                None
            }
        }
        Err(error) => {
            log::error!("Could not save {}: {error}", save.request.slot);
            Some(format!("Could not save {}. {error}", save.request.slot))
        }
    };
    if departure.is_some() && destination.is_none() {
        session.departure.as_mut().unwrap().phase = DeparturePhase::SaveFailed;
    }
    drop(io);
    drop(session);

    if current_generation {
        uiw.write::<GuiState>().last_save = Instant::now();
    }
    let notified = notice.is_some();
    if let Some(notice) = notice {
        session::notify(uiw, notice);
    }
    if let Some(destination) = destination {
        session::finish_departure(uiw, destination);
    }
    notified
}

fn publish_capture(
    capture: PendingCapture,
    captured: Result<(simulation::SlotCapture, CityProgress), String>,
) {
    std::thread::spawn(move || {
        let saved = encode_and_publish_slot(
            &capture.coordinator,
            &capture.permit,
            &capture.slot,
            capture.name,
            capture.camera,
            captured,
        );
        let _ = capture.sender.send(saved);
    });
}

fn encode_and_publish_slot(
    coordinator: &SaveCoordinator,
    permit: &SavePermit,
    slot: &SlotId,
    name: String,
    camera: common::saveload::SlotCamera,
    captured: Result<(simulation::SlotCapture, CityProgress), String>,
) -> SaveResult {
    let (capture, progress) = captured?;
    if permit.cancelled() {
        return Err("The save was cancelled.".into());
    }
    let snapshot = capture.encode().map_err(|error| error.to_string())?;
    snapshot
        .with_display_name(name)
        .with_camera(camera)
        .publish_with_gate(saveload::save_dir(), slot, || {
            coordinator.begin_publish(permit)
        })
        .ok_or_else(|| "The save was cancelled.".to_owned())?
        .map(|phase| (progress, phase))
        .map_err(|error| error.to_string())
}

fn render_time(sim: &Simulation) -> GameTime {
    *sim.read::<GameTime>()
}

fn take_world_job_result(
    uiw: &UiWorld,
    pointer: &engine::PointerFrame,
) -> Option<(session::WorldJob, Result<WorldJobOutcome, WorldJobError>)> {
    let job_id = uiw.read::<SessionState>().world_job_id;
    let mut io = uiw.write::<SaveLoadState>();
    let job = io.world_job.as_ref()?;
    if job.id != job_id {
        io.world_job = None;
        return None;
    }
    // Yakui registers a click on the primary release and the loading
    // dialog reads it during render, after this update.
    if !job.presented
        || matches!(job.kind, WorldJobKind::NewGame)
            && (pointer.held_buttons.contains(&engine::MouseButton::Left)
                || pointer
                    .released_buttons
                    .contains(&engine::MouseButton::Left))
    {
        return None;
    }
    let result = match job.result.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Err(WorldJobError::Failed(
            "The city worker stopped unexpectedly.".into(),
        )),
    };
    let job = io.world_job.take().unwrap();
    Some((job, result))
}

fn route_close_input(
    uiw: &UiWorld,
    input: &InputContext,
    keyboard_captured: bool,
    mouse_captured: bool,
) {
    let keyboard = !keyboard_captured || session::world_job_active(uiw);
    let pressed = uiw
        .write::<InputMap>()
        .close_pressed(input, keyboard, !mouse_captured);
    if pressed && uiw.read::<KeybindState>().enabled.is_none() {
        session::close_modal(uiw);
    }
}

fn ready_to_exit(uiw: &UiWorld) -> bool {
    let io = uiw.read::<SaveLoadState>();
    io.exit_requested && io.save_result.is_none()
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn scale_t0_render_read_with_writer_pending() {
    use std::sync::TryLockError;
    use std::thread;
    use std::time::Duration;

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let sim = Simulation::new(false);
    let expected_tick = sim.get_tick();
    let sim = Arc::new(RwLock::new(sim));
    let (guard_held, guard_ready) = mpsc::channel();
    let (prepare, prepare_frame) = mpsc::channel();
    let (frame_prepared, frame) = mpsc::channel();
    let reader = thread::spawn({
        let sim = Arc::clone(&sim);
        move || {
            let guard = sim.read().unwrap();
            guard_held.send(()).unwrap();
            prepare_frame.recv().unwrap();
            let time = render_time(&guard);
            drop(guard);
            frame_prepared.send(time.tick.0).unwrap();
        }
    });
    let timeout = Duration::from_secs(5);
    guard_ready.recv_timeout(timeout).unwrap();

    let writer = thread::spawn({
        let sim = Arc::clone(&sim);
        move || drop(sim.write().unwrap())
    });
    // On Linux, a queued writer prevents further readers while the first reader holds the lock.
    let deadline = Instant::now() + timeout;
    loop {
        match sim.try_read() {
            Err(TryLockError::WouldBlock) => break,
            Err(error) => panic!("could not probe the simulation lock: {error}"),
            Ok(_) => {}
        }
        assert!(Instant::now() < deadline, "the writer did not queue");
        thread::sleep(Duration::from_millis(1));
    }

    prepare.send(()).unwrap();
    assert_eq!(
        frame
            .recv_timeout(timeout)
            .expect("render read blocked behind the writer"),
        expected_tick,
    );
    reader.join().unwrap();
    writer.join().unwrap();
}

#[cfg(test)]
mod p4_fix_esc_tests {
    use super::*;
    use crate::inputmap::{InputCombination, UnitInput};
    use engine::input::Key;

    fn playing_ui() -> UiWorld {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        UiWorld::with_defaults()
    }

    #[test]
    fn scale_t1_fix_deferred_placement_is_bound_to_the_selected_company() {
        let sample = |company| WorldInputSample {
            generation: 4,
            input: InputMap::default().world_input(),
            pointer: Default::default(),
            ray: None,
            press_events: Vec::new(),
            tool: Tool::SpecialBuilding,
            company,
            drag_view: None,
        };

        let school = Some(prototypes::GoodsCompanyID::new("school"));
        let bakery = Some(prototypes::GoodsCompanyID::new("bakery"));
        let old = sample(school);
        assert!(old.same_tool(&sample(school)));
        assert!(!old.same_tool(&sample(bakery)));
        assert!(!old.same_tool(&sample(None)));
    }

    #[test]
    fn scale_t1_fix_deferred_bulldozer_drag_requires_the_sampled_view() {
        let mut camera = Camera::new(geom::Vec3::ZERO, 1280.0, 720.0);
        let sample = WorldInputSample {
            generation: 3,
            input: InputMap::default().world_input(),
            pointer: Default::default(),
            ray: None,
            press_events: Vec::new(),
            tool: Tool::Bulldozer,
            company: None,
            drag_view: Some(DragView::new(&camera)),
        };
        assert!(!sample.drag_uses_changed_view(Some(DragView::new(&camera))));
        camera.pos.x += 100.0;
        assert!(sample.drag_uses_changed_view(Some(DragView::new(&camera))));
        assert!(sample.drag_uses_changed_view(None));
    }

    #[test]
    fn p4_fix_esc_rebound_close_and_held_press_across_pause() {
        let uiw = playing_ui();
        let key = Key::c("Q");
        let mut binding = InputCombination::default();
        binding.push_unique(UnitInput::Key(key.clone()));
        let mut bindings = Bindings::default();
        bindings.0.get_mut(&InputAction::Close).unwrap().0 = vec![binding];
        uiw.write::<InputMap>().build_input_tree(&mut bindings);

        let mut input = InputContext::default();
        input.keyboard.pressed.insert(key.clone());
        route_close_input(&uiw, &input, true, false);
        assert!(!uiw.read::<SessionState>().paused);
        route_close_input(&uiw, &input, false, false);
        assert!(!uiw.read::<SessionState>().paused);
        input.keyboard.pressed.clear();
        route_close_input(&uiw, &input, false, false);
        input.keyboard.pressed.insert(Key::Escape);
        route_close_input(&uiw, &input, false, false);
        assert!(!uiw.read::<SessionState>().paused);
        input.keyboard.pressed.clear();
        input.keyboard.pressed.insert(key.clone());
        route_close_input(&uiw, &input, false, false);
        assert!(uiw.read::<SessionState>().paused);
        route_close_input(&uiw, &input, false, false);
        assert!(uiw.read::<SessionState>().paused);
        input.keyboard.pressed.clear();
        route_close_input(&uiw, &input, false, false);
        input.keyboard.pressed.insert(key);
        route_close_input(&uiw, &input, false, false);
        assert!(!uiw.read::<SessionState>().paused);
    }

    fn save_ack_fixture() -> (UiWorld, SaveInFlight, CityProgress) {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let progress = CityProgress::capture(&Simulation::new(false));
        let uiw = playing_ui();
        let (_, result) = mpsc::channel();
        let slot = SlotId::new("test-save").unwrap();
        uiw.write::<SessionState>()
            .set_snapshot_slot(Some(slot.clone()));
        let save = SaveInFlight {
            id: 7,
            revision: session::snapshot_revision(),
            request: SaveRequest {
                slot,
                name: "Saved city".into(),
                kind: SaveKind::Manual,
            },
            result,
        };
        (uiw, save, progress)
    }

    #[test]
    fn detached_save_delete_wins_in_either_reply_order() {
        for delete_first in [true, false] {
            let (uiw, save, progress) = save_ack_fixture();
            let slot = save.request.slot.clone();
            let generation = uiw.read::<SessionState>().generation;
            let mut deleted = None;
            uiw.write::<SaveLoadState>().saved_progress = Some(progress);
            uiw.write::<SaveLoadState>().save_result = Some(save);
            if delete_first {
                deleted = apply_delete_success(&uiw, generation, &slot, None);
            }
            let save = uiw.write::<SaveLoadState>().save_result.take().unwrap();
            let disposition = if deleted == Some(save.id) {
                SaveDisposition::Deleted
            } else {
                SaveDisposition::Current
            };
            apply_save_result(
                &uiw,
                save,
                Ok((progress, simulation::SavePhase::Complete)),
                disposition,
            );
            if !delete_first {
                apply_delete_success(&uiw, generation, &slot, None);
            } else {
                assert!(uiw.read::<SaveLoadState>().notice.is_none());
            }
            assert!(uiw.read::<SaveLoadState>().saved_progress.is_none());
        }
    }

    #[test]
    fn detached_save_old_generation_delete_cannot_clear_new_city_progress() {
        let (uiw, save, progress) = save_ack_fixture();
        uiw.write::<SessionState>().generation = 1;
        uiw.write::<SaveLoadState>().saved_progress = Some(progress);
        assert_eq!(
            apply_delete_success(&uiw, 0, &save.request.slot, None),
            None
        );
        assert_eq!(uiw.read::<SaveLoadState>().saved_progress, Some(progress));
    }

    #[test]
    fn detached_save_success_after_delete_and_replacement_has_no_toast() {
        let (uiw, save, progress) = save_ack_fixture();
        uiw.write::<SessionState>().generation = 1;
        let retired = (0, save);
        let deleted = apply_delete_success(&uiw, 0, &retired.1.request.slot, Some(&retired));
        assert_eq!(deleted, Some(retired.1.id));
        apply_save_result(
            &uiw,
            retired.1,
            Ok((progress, simulation::SavePhase::Complete)),
            SaveDisposition::Retired {
                deleted: deleted.is_some(),
            },
        );
        assert!(uiw.read::<SaveLoadState>().notice.is_none());
        assert!(uiw.read::<SaveLoadState>().saved_progress.is_none());
    }

    #[test]
    fn detached_save_retiring_generation_reports_late_manual_results() {
        let (uiw, mut save, _) = save_ack_fixture();
        let coordinator = SaveCoordinator::default();
        let permit = coordinator.request(0, &save.request.slot).unwrap();
        let (sender, receiver) = mpsc::channel();
        save.result = receiver;
        uiw.write::<SaveLoadState>().save_result = Some(save);
        assert!(coordinator.try_swap(1)); // Replacement wins while encoding.
        assert!(coordinator.publish(&permit, || ()).is_none());
        let mut retiring = uiw.write::<SaveLoadState>().save_result.take();
        uiw.write::<SessionState>().generation = 1;
        assert!(take_save_result(&mut retiring).is_none());
        sender.send(Err("The save was cancelled.".into())).unwrap();
        let (save, result) = take_save_result(&mut retiring).unwrap();
        apply_save_result(
            &uiw,
            save,
            result,
            SaveDisposition::Retired { deleted: false },
        );
        assert!(uiw
            .read::<SaveLoadState>()
            .notice
            .as_ref()
            .unwrap()
            .1
            .contains("cancelled"));

        let (uiw, mut save, progress) = save_ack_fixture();
        let coordinator = SaveCoordinator::default();
        let permit = coordinator.request(0, &save.request.slot).unwrap();
        let (sender, receiver) = mpsc::channel();
        save.result = receiver;
        uiw.write::<SaveLoadState>().save_result = Some(save);
        assert!(coordinator.publish(&permit, || ()).is_some()); // Publication wins.
        assert!(coordinator.try_swap(1));
        let mut retiring = uiw.write::<SaveLoadState>().save_result.take();
        let new_slot = SlotId::new("new-city").unwrap();
        uiw.write::<SessionState>().generation = 1;
        uiw.write::<SessionState>()
            .set_snapshot_slot(Some(new_slot.clone()));
        let mut new_city = Simulation::new(false);
        new_city.tick(&mut Simulation::schedule(), std::iter::empty());
        let new_progress = CityProgress::capture(&new_city);
        assert_ne!(progress, new_progress);
        uiw.write::<SaveLoadState>().saved_progress = Some(new_progress);
        sender
            .send(Ok((
                progress,
                simulation::SavePhase::SidecarsFailed {
                    replay: Some(common::saveload::AtomicWriteError::PreCommit(
                        std::io::Error::other("fixture"),
                    )),
                    meta: None,
                },
            )))
            .unwrap();
        let (save, result) = take_save_result(&mut retiring).unwrap();
        apply_save_result(
            &uiw,
            save,
            result,
            SaveDisposition::Retired { deleted: false },
        );
        assert_eq!(
            uiw.read::<SaveLoadState>().saved_progress,
            Some(new_progress)
        );
        assert_eq!(uiw.read::<SessionState>().snapshot_slot(), Some(&new_slot));
        assert_eq!(
            uiw.read::<SaveLoadState>().notice.as_ref().unwrap().1,
            "City saved. Could not save replay."
        );
    }

    #[test]
    fn p4_fix_esc_quit_waits_for_in_flight_autosave() {
        let uiw = playing_ui();
        let (sender, result) = mpsc::channel();
        session::finish_departure(&uiw, Departure::Exit);
        uiw.write::<SaveLoadState>().save_result = Some(SaveInFlight {
            id: 1,
            revision: session::snapshot_revision(),
            request: SaveRequest {
                slot: SlotId::autosave(0),
                name: "City".into(),
                kind: SaveKind::Autosave,
            },
            result,
        });
        assert!(!ready_to_exit(&uiw));
        sender.send(Err("test save result".into())).unwrap();
        uiw.write::<SaveLoadState>()
            .save_result
            .take()
            .unwrap()
            .result
            .try_recv()
            .unwrap()
            .unwrap_err();
        assert!(ready_to_exit(&uiw));
    }

    #[test]
    fn p4_fix_final_cancel_click_gets_the_frame_before_a_ready_generation() {
        let uiw = playing_ui();
        let (sender, result) = mpsc::channel();
        uiw.write::<SaveLoadState>().world_job = Some(session::WorldJob {
            id: 0,
            kind: WorldJobKind::NewGame,
            presented: true,
            result,
        });
        sender
            .send(Ok(WorldJobOutcome::Replace(PreparedWorld {
                replacement: WorldReplacement::NewGame(Simulation::new(false), SlotId::legacy()),
                name: "Ready city".into(),
            })))
            .unwrap();
        let mut input = InputContext::default();
        input.observe_mouse_button(engine::MouseButton::Left, true);
        input.observe_mouse_button(engine::MouseButton::Left, false);
        assert!(take_world_job_result(&uiw, &input.pointer_frame(false)).is_none());
        assert!(session::cancel_generation(&uiw));
        input.end_frame();
        assert!(take_world_job_result(&uiw, &input.pointer_frame(false)).is_none());
    }
}

struct StartupWorld {
    world: PreparedWorld,
    slots: std::io::Result<DiscoveredSlots>,
    failed_snapshots: FailedSnapshots,
    loaded_entry: Option<common::saveload::SlotEntry>,
    message: Option<String>,
}

fn startup_world() -> Result<StartupWorld, String> {
    let root = saveload::save_dir();
    let mut slots = session::discover_slots(root);
    let mut failed_snapshots = FailedSnapshots::default();
    let mut message = None;
    for attempt in 0..2 {
        let Ok(candidates) = &slots else { break };
        let mut listing_changed = false;
        for (entry, status) in candidates {
            if *status != SlotCompatibility::Compatible {
                continue;
            }
            match Simulation::load_listed_snapshot(root, entry) {
                Ok(sim) => {
                    if let Err(error) = session::validate_native_world(&sim) {
                        log::warn!("Could not load {} at startup: {error}", entry.id);
                        message.get_or_insert(error);
                        continue;
                    }
                    let world = PreparedWorld {
                        replacement: WorldReplacement::from_slot(sim, entry),
                        name: session::display_name(entry).to_owned(),
                    };
                    let loaded_entry = Some(entry.clone());
                    return Ok(StartupWorld {
                        world,
                        slots,
                        failed_snapshots,
                        loaded_entry,
                        message,
                    });
                }
                Err(simulation::SaveLoadError::SnapshotChanged) if attempt == 0 => {
                    listing_changed = true;
                    break;
                }
                Err(simulation::SaveLoadError::SnapshotChanged) => continue,
                Err(error) => {
                    log::warn!("Could not load {} at startup: {error}", entry.id);
                    message.get_or_insert_with(|| error.to_string());
                    failed_snapshots.record(entry);
                }
            }
        }
        if !listing_changed {
            break;
        }
        slots = session::discover_slots(root);
    }

    let (mut sim, recovery) = Simulation::load_or_new("world")?;
    if let Err(error) = session::validate_native_world(&sim) {
        message.get_or_insert(error);
        sim = Simulation::new(true);
    }
    if recovery.is_some() {
        slots = session::discover_slots(root);
    }
    Ok(StartupWorld {
        world: PreparedWorld {
            replacement: WorldReplacement::NewGame(sim, SlotId::legacy()),
            name: "World (legacy)".into(),
        },
        slots,
        failed_snapshots,
        loaded_entry: None,
        message: recovery.or(message),
    })
}

#[derive(Clone, Default)]
pub struct Timings {
    pub all: History,
    pub world_update: History,
    pub render: History,
    pub engine_render_time: History,
    pub gui_time: History,
    pub total_cpu_time: History,
    pub per_game_system: Vec<(String, f32)>,
}
