use std::collections::BTreeMap;
use std::ptr::addr_of;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use common::history::History;
use common::saveload::{self, SlotId};
use engine::{Context, FrameContext, InputContext, MeshBuilder, Tesselator};
use geom::{vec2, vec3, Camera, LinearColor};
use simulation::economy::Government;
use simulation::Simulation;

use crate::audio::GameAudio;
use crate::automation::Automation;
use crate::debug_gui::debug_window::DebugObjs;
use crate::debug_gui::render_oldgui;
use crate::gui;
use crate::gui::follow::FollowEntity;
use crate::gui::keybinds::KeybindState;
use crate::gui::terraforming::TerraformingResource;
use crate::gui::toolbox::building;
use crate::gui::windows::settings::{manage_settings, Settings};
use crate::gui::UiTextures;
use crate::gui::{render_newgui, GuiState, TimeAlways, Tool};
use crate::inputmap::{Bindings, InputAction, InputMap};
use crate::network::NetworkState;
use crate::rendering::{InstancedRender, MapRenderOptions, MapRenderer, OrbitCamera};
use crate::session::save_coordinator::SaveCoordinator;
use crate::session::{
    self, CityProgress, Departure, DeparturePhase, DiscoveredSlots, FailedSnapshots, PreparedWorld,
    SaveInFlight, SaveKind, SaveRequest, SessionScreen, SessionState, SlotCompatibility,
    WorldJobError, WorldJobKind, WorldJobOutcome, WorldReplacement,
};
use crate::uiworld::{ReceivedCommands, SaveLoadState, UiWorld};
use prototypes::GameTime;
use simulation::utils::scheduler::SeqSchedule;

pub const VERSION: &str = include_str!("../../VERSION");

/// State is the main struct that contains all the state of the game and game UI.
pub struct State {
    pub sim: Arc<RwLock<Simulation>>,
    pub uiw: UiWorld,
    pub game_schedule: SeqSchedule,

    instanced_renderer: InstancedRender,
    map_renderer: Option<MapRenderer>,
    immediate_renderer: MeshBuilder<true>,
    automation: Option<Automation>,
    startup: Option<Receiver<Result<StartupWorld, String>>>,
    dormant_script_state: Option<(u64, BTreeMap<String, u64>)>,

    all_audio: GameAudio,
}

impl engine::framework::State for State {
    fn new(ctx: &mut Context) -> Self {
        profiling::scope!("game_loop::init");
        goryak::set_blur_texture(ctx.yakui.blur_bg_texture);

        let camera = OrbitCamera::load((ctx.gfx.size.0, ctx.gfx.size.1));

        log::info!("loaded egui_render");

        let automation = Automation::take_startup();
        let game_schedule = Simulation::schedule();
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

        let me = Self {
            uiw: uiworld,
            game_schedule,
            instanced_renderer: InstancedRender::new(&mut ctx.gfx),
            map_renderer: startup
                .is_none()
                .then(|| MapRenderer::new(&mut ctx.gfx, &sim)),
            all_audio: GameAudio::new(&mut ctx.audio),
            sim: Arc::new(RwLock::new(sim)),
            immediate_renderer: MeshBuilder::new(ctx.gfx.tess_material),
            automation,
            startup,
            dormant_script_state: None,
        };
        me.sim.write().unwrap().map().dispatch_all();
        me
    }

    fn update(&mut self, ctx: &mut Context) {
        profiling::scope!("game_loop::update");
        self.uiw.write::<TimeAlways>().0 += ctx.delta;

        {
            let mut timings = self.uiw.write::<Timings>();
            timings.engine_render_time.add_value(ctx.times.render_time);
            timings.gui_time.add_value(ctx.times.gui_time);
            timings.total_cpu_time.add_value(ctx.times.total_cpu_time);
        }

        self.uiw.write::<SessionState>().interaction_blocked = false;
        let scripted = self.automation.is_some();
        if !self.step_session(ctx) {
            return;
        }

        self.uiw
            .write::<InputMap>()
            .observe_pointer_release(ctx, scripted);
        let world_interaction = session::world_interaction(&self.uiw);
        if world_interaction && !ctx.egui.last_mouse_captured {
            let sim = self.sim.read().unwrap();
            let map = sim.map();
            let ray = self
                .uiw
                .read::<OrbitCamera>()
                .camera
                .unproj_ray(ctx.input.mouse.screen);
            self.uiw.write::<InputMap>().ray = ray;

            if let Some(ray) = ray {
                let cast = map.environment().raycast(ray);

                let inp = &mut self.uiw.write::<InputMap>();

                inp.unprojected = cast.map(|(mut proj, _)| {
                    proj.z = proj.z.max(0.0);
                    proj
                });
                inp.unprojected_normal = cast.map(|x| x.1);
            }
        }

        ctx.keybind_mode = self.uiw.read::<KeybindState>().enabled.is_some();

        self.uiw.write::<KeybindState>().update(
            &mut self.uiw.write::<Bindings>(),
            &mut self.uiw.write::<InputMap>(),
            &ctx.input,
        );
        let view_input = session::view_input(&self.uiw) && !session::modal(&self.uiw);
        if view_input {
            // Replay controls can keep keyboard focus, but have no text inputs.
            let replaying = self.uiw.read::<SaveLoadState>().replay.is_some();
            self.uiw.write::<InputMap>().prepare_frame(
                &ctx.input,
                !ctx.egui.last_kb_captured || replaying,
                !ctx.egui.last_mouse_captured,
            );
            if ctx.keybind_mode {
                self.uiw.write::<InputMap>().suppress_shortcuts();
            }
            if world_interaction {
                if !ctx.keybind_mode {
                    GuiState::apply_time_shortcut(&self.uiw);
                }
                if *self.uiw.read::<Tool>() == Tool::Bulldozer {
                    self.uiw
                        .write::<gui::bulldozer::BulldozerState>()
                        .record_press_order(&ctx.egui.platform.egui_input().events);
                }
                gui::run_ui_systems(&self.sim.read().unwrap(), &self.uiw);
            }
        }

        self.uiw.write::<Timings>().all.add_value(ctx.delta);
        self.uiw.write::<Timings>().per_game_system = self.game_schedule.times();

        self.uiw.write::<GuiState>().hidden ^= self
            .uiw
            .read::<InputMap>()
            .just_act
            .contains(&InputAction::HideInterface);

        manage_settings(ctx, &self.uiw.read::<Settings>());
        if !scripted && view_input {
            self.manage_io(ctx);
        }

        if let Some(renderer) = &mut self.map_renderer {
            renderer.update(&self.sim.read().unwrap(), ctx);
        }

        ctx.gfx
            .set_time(self.sim.read().unwrap().read::<GameTime>().timestamp as f32);

        for (sound, kind) in self.uiw.write::<ImmediateSound>().orders.drain(..) {
            ctx.audio.play(sound, kind);
        }
        if self.startup.is_none() && !self.uiw.read::<SessionState>().interaction_blocked {
            self.all_audio
                .update(&self.sim.read().unwrap(), &self.uiw, &mut ctx.audio);
        }

        if !scripted && world_interaction {
            FollowEntity::update_camera(self);
        }
        self.uiw.camera_mut().update(ctx);
        if scripted {
            *self.uiw.write::<Camera>() = self.uiw.read::<OrbitCamera>().camera;
        }
        self.manage_gfx_params(ctx);
    }

    fn render(&mut self, ctx: &mut FrameContext<'_>) {
        profiling::scope!("game_loop::render");
        let Some(map_renderer) = &mut self.map_renderer else {
            return;
        };
        let start = Instant::now();
        let sim = self.sim.read().unwrap();

        let camera = self.uiw.camera().camera;
        let time = render_time(&sim);

        if self.automation.is_some() {
            // Scripted map changes are applied through ticks, so the tick identifies the snapshot.
            map_renderer.meshb.set_map_version(sim.get_tick());
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
            drop(sim);

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

    fn resized(&mut self, ctx: &mut Context, size: (u32, u32, f64)) {
        self.uiw
            .write::<OrbitCamera>()
            .resize(ctx, size.0 as f32, size.1 as f32);
    }

    fn exit(&mut self) -> bool {
        if self.automation.is_some() {
            crate::automation::fail("window closed before the script finished");
        }
        if !self.uiw.read::<SaveLoadState>().exit_requested {
            if session::playing(&self.uiw) {
                let sim = self.sim.read().unwrap();
                session::request_departure(&self.uiw, &sim, Departure::Exit);
            } else {
                session::finish_departure(&self.uiw, Departure::Exit);
            }
        }
        ready_to_exit(&self.uiw) && self.uiw.camera_mut().poll_save_on_exit(Instant::now())
    }

    fn render_gui(&mut self, ui: &egui::Context) {
        if session::playing(&self.uiw)
            && !self.uiw.read::<SessionState>().interaction_blocked
            && !session::modal(&self.uiw)
        {
            let sim = self.sim.read().unwrap();
            render_oldgui(ui, &self.uiw, &sim);
        }
    }

    fn render_yakui(&mut self) {
        let sim = self.sim.read().unwrap();
        if !self.uiw.read::<SessionState>().interaction_blocked {
            if !session::playing(&self.uiw) {
                session::render_session(&self.uiw, &sim);
            } else if session::modal(&self.uiw) {
                session::render_modal(&self.uiw, &sim);
            } else {
                render_newgui(&self.uiw, &sim);
            }
            // The root does not position relative children. A full-width row anchors the notice.
            yakui::widgets::List::row().show(|| session::render_status(&self.uiw));
        }
        goryak::render_tooltip();
    }
}

impl State {
    fn step_session(&mut self, ctx: &mut Context) -> bool {
        self.check_dormant_script_state();
        if self.uiw.read::<SaveLoadState>().exit_requested {
            // The worker reports completion after it has published or cleaned up staging files.
            self.poll_save();
            if ready_to_exit(&self.uiw) && self.uiw.camera_mut().poll_save_on_exit(Instant::now()) {
                ctx.request_exit();
            }
            self.uiw.write::<SessionState>().interaction_blocked = true;
            return false;
        }
        if self.poll_startup(ctx) {
            return true;
        }

        let replacement = self.uiw.write::<SaveLoadState>().pending.take();
        if let Some(replacement) = replacement {
            self.replace_world(ctx, replacement);
            return true;
        }

        self.poll_save();
        self.delete_slot();
        self.uiw.write::<SessionState>().poll_slot_scan();
        if self.automation.is_none() {
            self.route_close_input(ctx);
        }
        if std::mem::take(&mut self.uiw.write::<SessionState>().replay_requested) {
            let sim = self.sim.read().unwrap();
            session::request_departure(&self.uiw, &sim, Departure::ReplayDebug);
        }
        if !self.poll_world_job(ctx) || self.uiw.read::<SessionState>().interaction_blocked {
            return true;
        }

        self.start_save();
        if self.advance_replay() {
            return true;
        }
        if self.automation.is_some() && self.uiw.read::<SaveLoadState>().save_result.is_some() {
            return true;
        }

        if let Some(automation) = &mut self.automation {
            match automation.step(
                ctx,
                &self.uiw,
                &self.sim,
                &mut self.game_schedule,
                self.map_renderer.as_ref().unwrap().meshb.rebuilt_version(),
            ) {
                Ok(true) => {}
                Ok(false) => {
                    self.assert_dormant_script_state();
                    self.automation = None;
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
        } else if session::playing(&self.uiw)
            && (!session::modal(&self.uiw) || !crate::network::is_singleplayer(&self.uiw))
        {
            if let Some(replacement) = crate::network::sim_update(self) {
                self.replace_world(ctx, replacement);
            }
        } else {
            // Time spent in menus or a modal is not simulation catch-up time.
            #[allow(irrefutable_let_patterns)]
            if let NetworkState::Singleplayer(step) = &mut *self.uiw.write::<NetworkState>() {
                step.prepare_frame(0);
            }
        }
        true
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
    fn check_dormant_script_state(&mut self) {
        if self.automation.is_none() {
            return;
        }
        let dormant = !session::playing(&self.uiw) || session::modal(&self.uiw);
        if dormant && self.dormant_script_state.is_none() {
            let sim = self.sim.read().unwrap();
            self.dormant_script_state = Some((sim.get_tick(), sim.hashes()));
        } else if !dormant {
            self.assert_dormant_script_state();
        }
    }

    fn assert_dormant_script_state(&mut self) {
        if let Some((tick, hashes)) = self.dormant_script_state.take() {
            let sim = self.sim.read().unwrap();
            if sim.get_tick() != tick || sim.hashes() != hashes {
                crate::automation::fail("session menu changed the dormant city's tick or hashes");
            }
            log::info!("Verified dormant city tick {tick} and hashes");
        }
    }

    /// Returns true while startup owns the dormant world.
    fn poll_startup(&mut self, ctx: &mut Context) -> bool {
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
        let StartupWorld {
            world,
            slots,
            failed_snapshots,
            loaded_entry,
            message,
        } = match result {
            Ok(startup) => startup,
            Err(error) => {
                log::error!("{error}");
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        self.replace_world(ctx, world.replacement);
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
        true
    }

    /// Returns false while a world job owns the session transition.
    fn poll_world_job(&mut self, ctx: &mut Context) -> bool {
        if self.uiw.read::<SessionState>().departure.is_some() {
            return true;
        }
        let Some((job, result)) = take_world_job_result(&self.uiw, &engine::pointer_frame()) else {
            return !session::world_job_active(&self.uiw);
        };
        let mut io = self.uiw.write::<SaveLoadState>();
        match result {
            Ok(WorldJobOutcome::Replace(world)) if crate::network::is_singleplayer(&self.uiw) => {
                drop(io);
                self.replace_world(ctx, world.replacement);
                self.uiw.write::<SessionState>().city_name = world.name;
                match job.kind {
                    WorldJobKind::Snapshot(entry) => self
                        .uiw
                        .write::<SessionState>()
                        .remember_loaded_snapshot(entry),
                    WorldJobKind::NewGame => {
                        let treasury = self.sim.read().unwrap().read::<Government>().money;
                        session::notify(
                            &self.uiw,
                            format!("Opening balance after starter infrastructure is {treasury}."),
                        );
                    }
                    WorldJobKind::Replay => {}
                }
            }
            Ok(WorldJobOutcome::ResumeDormant) if crate::network::is_singleplayer(&self.uiw) => {
                drop(io);
                if let WorldJobKind::Snapshot(entry) = job.kind {
                    session::restore_dormant_camera(&self.uiw, &self.sim.read().unwrap(), &entry);
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

    fn replace_world(&mut self, ctx: &mut Context, replacement: WorldReplacement) {
        self.assert_dormant_script_state();
        let mut sim = self.sim.write().unwrap();
        session::apply_world_replacement(
            &self.uiw,
            &mut sim,
            &mut self.game_schedule,
            replacement,
            |sim| {
                self.all_audio.reset_world();
                self.immediate_renderer.clear();
                ctx.gfx.lamplights.reset(&ctx.gfx.device, &ctx.gfx.queue);
                self.map_renderer = Some(MapRenderer::new(&mut ctx.gfx, sim));
                sim.map().dispatch_all();
                ctx.gfx.update_simplelit_bg();
            },
        );
    }

    fn start_save(&mut self) {
        if !session::can_save(&self.uiw) {
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
            || !(departure_save
                || io.pending_save.is_some()
                || (autosave_allowed && io.please_save))
        {
            return;
        }
        let request = if departure_save {
            match session::departure_save_request(&self.uiw, saveload::save_dir()) {
                Ok(request) => request,
                Err(error) => {
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
            io.autosave_index = (io.autosave_index + 1) % 3;
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
        match request.kind {
            SaveKind::Departure => {
                session.departure.as_mut().unwrap().phase = DeparturePhase::Saving(permit.id)
            }
            SaveKind::Autosave => io.please_save = false,
            _ => {
                io.pending_save = None;
                io.please_save = false;
            }
        };

        let (sender, result) = mpsc::channel();
        let id = permit.id;
        let slot = request.slot.clone();
        let name = request.name.clone();
        let camera = self.uiw.camera().slot_camera();
        let revision = session::snapshot_revision();
        io.save_result = Some(SaveInFlight {
            id,
            request,
            revision,
            result,
        });
        let sim = self.sim.clone();
        std::thread::spawn(move || {
            profiling::scope!("game_loop::update::save");
            let encode = || {
                let sim = sim.read().unwrap();
                // The generation advances only while the simulation write lock is held.
                if !coordinator.is_current(&permit) {
                    return Err("The save was cancelled.".into());
                }
                let snapshot = sim
                    .encode_slot(revision)
                    .map_err(|error| error.to_string())?;
                Ok((
                    CityProgress::capture(&sim),
                    snapshot.with_display_name(name).with_camera(camera),
                ))
            };
            let saved = encode().and_then(|(progress, snapshot)| {
                coordinator
                    .publish(&permit, || snapshot.publish(saveload::save_dir(), &slot))
                    .ok_or_else(|| "The save was cancelled.".to_owned())?
                    .map(|phase| (progress, phase))
                    .map_err(|error| error.to_string())
            });
            drop(permit);
            let _ = sender.send(saved);
        });
    }

    fn poll_save(&self) {
        let mut io = self.uiw.write::<SaveLoadState>();
        let Some(in_flight) = &io.save_result else {
            return;
        };
        let saved = match in_flight.result.try_recv() {
            Ok(saved) => saved,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("The save worker stopped unexpectedly.".into()),
        };
        let SaveInFlight {
            id,
            request,
            revision,
            ..
        } = io.save_result.take().unwrap();
        let mut session = self.uiw.write::<SessionState>();
        // Refresh even after errors: the snapshot may have been renamed before fsync failed.
        session.refresh_slots = true;
        let departure = session
            .departure
            .filter(|departure| departure.phase == DeparturePhase::Saving(id));
        let mut notice = None;
        let mut destination = None;
        match saved {
            Ok((progress, phase)) => {
                if matches!(request.kind, SaveKind::SaveAs | SaveKind::Departure) {
                    session.set_snapshot_slot(Some(request.slot));
                    session.city_name = request.name;
                }
                if !matches!(request.kind, SaveKind::Autosave) {
                    io.saved_progress = Some(progress);
                    session.remember_published_snapshot(revision);
                }
                if let simulation::SavePhase::SidecarsFailed { replay, meta } = phase {
                    log::warn!(
                        "Saved snapshot; sidecar errors: replay={replay:?}, metadata={meta:?}"
                    );
                    let sidecars = match (replay.is_some(), meta.is_some()) {
                        (true, true) => "replay and metadata",
                        (true, false) => "replay",
                        _ => "metadata",
                    };
                    notice = Some(format!("City saved. Could not save {sidecars}."));
                }
                destination = departure.map(|departure| departure.destination);
            }
            Err(error) => {
                log::error!("Could not save {}: {error}", request.slot);
                notice = Some(format!("Could not save {}. {error}", request.slot));
                if departure.is_some() {
                    session.departure.as_mut().unwrap().phase = DeparturePhase::SaveFailed;
                }
            }
        }
        drop(session);
        drop(io);
        self.uiw.write::<GuiState>().last_save = Instant::now();
        self.uiw.save_to_disk();
        if let Some(notice) = notice {
            session::notify(&self.uiw, notice);
        }
        if let Some(destination) = destination {
            session::finish_departure(&self.uiw, destination);
        }
    }

    fn delete_slot(&self) {
        let slot = self.uiw.write::<SaveLoadState>().delete_slot.take();
        let Some(slot) = slot else {
            return;
        };
        let result = self
            .uiw
            .read::<SaveCoordinator>()
            .delete(saveload::save_dir(), &slot);
        if let Err(error) = result {
            session::notify(&self.uiw, format!("Could not delete {slot}. {error}"));
        }
        let mut session = self.uiw.write::<SessionState>();
        if session.snapshot_slot() == Some(&slot) {
            self.uiw.write::<SaveLoadState>().saved_progress = None;
        }
        session.refresh_slots = true;
    }

    fn advance_replay(&mut self) -> bool {
        if !session::playing(&self.uiw) {
            return false;
        }
        let mut io = self.uiw.write::<SaveLoadState>();
        let Some(replay) = io.replay.as_mut() else {
            return false;
        };
        *self.uiw.commands() = Default::default();
        *self.uiw.write::<ReceivedCommands>() = Default::default();
        let mut sim = self.sim.write().unwrap();
        if replay.advance_tick(&mut sim, &mut self.game_schedule) {
            drop(io);
            session::end_replay(&self.uiw);
            log::info!("finished loading replay");
        }
        true
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

    fn manage_io(&mut self, ctx: &mut Context) {
        let sim = self.sim.read().unwrap();
        let map = sim.map();
        //        self.camera.movespeed = settings.camera_sensibility / 100.0;
        self.uiw.camera_mut().camera_movement(
            ctx,
            ctx.delta,
            &self.uiw.read::<InputMap>(),
            &self.uiw.read::<Settings>(),
            map.environment().bounds(),
            |p| map.environment().height(p),
        );
        *self.uiw.write::<Camera>() = self.uiw.read::<OrbitCamera>().camera;

        drop(map);
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

    let (sim, recovery) = Simulation::load_or_new("world")?;
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
