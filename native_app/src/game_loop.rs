use std::collections::BTreeMap;
use std::ptr::addr_of;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use common::history::History;
use common::saveload::{self, SlotEntry, SlotId};
use engine::input::Key;
use engine::{Context, FrameContext, MeshBuilder};
use geom::{vec2, vec3, Camera, LinearColor, AABB};
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
use crate::rendering::{InstancedRender, MapRenderOptions, MapRenderer, OrbitCamera};
use crate::session::save_coordinator::SaveCoordinator;
use crate::session::{
    self, CityProgress, Departure, DeparturePhase, SaveInFlight, SaveKind, SaveRequest,
    SessionScreen, SessionState, SlotCompatibility, WorldReplacement,
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
    map_renderer: MapRenderer,
    immediate_renderer: MeshBuilder<true>,
    automation: Option<Automation>,
    escape_down: bool,
    dormant_script_state: Option<(u64, BTreeMap<String, u64>)>,

    all_audio: GameAudio,
}

impl engine::framework::State for State {
    fn new(ctx: &mut Context) -> Self {
        profiling::scope!("game_loop::init");
        goryak::set_blur_texture(ctx.yakui.blur_bg_texture);

        let mut camera = OrbitCamera::load((ctx.gfx.size.0, ctx.gfx.size.1));

        log::info!("loaded egui_render");

        let automation = Automation::take_startup();
        let game_schedule = Simulation::schedule();
        let mut uiworld = UiWorld::init();
        let sim = if automation.is_some() {
            crate::init::configure_script_settings(&uiworld);
            let sim = Simulation::new_with_options(scenarios::builtin::smoke::OPTIONS);
            uiworld.write::<SessionState>().active_slot = Some(SlotId::legacy());
            uiworld.write::<SessionState>().city_name = "World (legacy)".into();
            uiworld.write::<SaveLoadState>().saved_progress = Some(CityProgress::capture(&sim));
            sim
        } else {
            *uiworld.write::<SessionScreen>() = SessionScreen::MainMenu;
            startup_world(&uiworld)
        };
        if automation.is_none() {
            camera.reset_for_world(&sim.map());
        }

        let mut bindings = uiworld.write::<Bindings>();
        let default_bindings = Bindings::default();
        bindings
            .0
            .retain(|act, _| default_bindings.0.contains_key(act));
        for (act, comb) in default_bindings.0 {
            bindings.0.entry(act).or_insert(comb);
        }
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
            map_renderer: MapRenderer::new(&mut ctx.gfx, &sim),
            all_audio: GameAudio::new(&mut ctx.audio),
            sim: Arc::new(RwLock::new(sim)),
            immediate_renderer: MeshBuilder::new(ctx.gfx.tess_material),
            automation,
            escape_down: false,
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
                let cast = map.environment.raycast(ray);

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
            if world_interaction {
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

        self.map_renderer.update(&self.sim.read().unwrap(), ctx);

        ctx.gfx
            .set_time(self.sim.read().unwrap().read::<GameTime>().timestamp as f32);

        for (sound, kind) in self.uiw.write::<ImmediateSound>().orders.drain(..) {
            ctx.audio.play(sound, kind);
        }
        if !self.uiw.read::<SessionState>().interaction_blocked {
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
        let start = Instant::now();
        let sim = self.sim.read().unwrap();

        let camera = self.uiw.read::<OrbitCamera>();

        let time: GameTime = *self.sim.read().unwrap().read::<GameTime>();

        if self.automation.is_some() {
            // Scripted map changes are applied through ticks, so the tick identifies the snapshot.
            self.map_renderer.meshb.set_map_version(sim.get_tick());
        }
        self.map_renderer.render(
            &sim.map(),
            time.seconds,
            &camera.camera,
            MapRenderOptions {
                show_arrows: self.uiw.read::<Tool>().show_arrows(),
                show_lots: self.uiw.read::<Tool>().show_lots(),
            },
            &mut self.uiw.write::<ImmediateDraw>(),
            ctx,
        );

        self.instanced_renderer
            .render(&self.sim.read().unwrap(), ctx);
        if !self.uiw.read::<SessionState>().interaction_blocked {
            gui::overlays::draw(&sim, &self.uiw);
        }

        drop(sim);
        drop(camera);

        self.immediate_draw(ctx);

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
        if self.uiw.read::<SaveLoadState>().exit_requested || !session::playing(&self.uiw) {
            return true;
        }
        let sim = self.sim.read().unwrap();
        session::request_departure(&self.uiw, &sim, Departure::Exit);
        self.uiw.read::<SaveLoadState>().exit_requested
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
            session::render_status(&self.uiw);
        }
        goryak::render_tooltip();
    }
}

impl State {
    fn step_session(&mut self, ctx: &mut Context) -> bool {
        self.check_dormant_script_state();

        let replacement = self.uiw.write::<SaveLoadState>().pending.take();
        if let Some(replacement) = replacement {
            self.replace_world(ctx, replacement);
            return true;
        }

        self.poll_save();
        self.delete_slot();
        if std::mem::take(&mut self.uiw.write::<SessionState>().refresh_slots) {
            refresh_slots(&self.uiw);
        }
        if !self.poll_world_job(ctx) || self.uiw.read::<SessionState>().interaction_blocked {
            return true;
        }

        if self.automation.is_none() {
            self.update_pause_key(ctx);
        }
        self.start_save();
        if self.uiw.read::<SaveLoadState>().exit_requested {
            ctx.request_exit();
            return false;
        }
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
                self.map_renderer.meshb.rebuilt_version(),
            ) {
                Ok(true) => {}
                Ok(false) => {
                    self.assert_dormant_script_state();
                    self.automation = None;
                    return false;
                }
                Err(error) => crate::automation::fail(&error),
            }
            self.update_pause_key(ctx);
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
        }
        true
    }

    fn update_pause_key(&mut self, ctx: &Context) {
        let down = ctx.input.keyboard.pressed.contains(&Key::Escape);
        if down
            && !self.escape_down
            && !ctx.egui.last_kb_captured
            && self.uiw.read::<KeybindState>().enabled.is_none()
        {
            session::close_modal(&self.uiw);
        }
        self.escape_down = down;
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

    /// Returns false while a generation or snapshot worker owns the session transition.
    fn poll_world_job(&mut self, ctx: &mut Context) -> bool {
        let mut io = self.uiw.write::<SaveLoadState>();
        let Some(job) = &io.world_job else {
            return true;
        };
        if !job.presented {
            return false;
        }
        let result = match job.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("The city worker stopped unexpectedly.".into()),
        };
        io.world_job = None;
        match result {
            Ok(world) if crate::network::is_singleplayer(&self.uiw) => {
                drop(io);
                self.replace_world(ctx, world.replacement);
                self.uiw.write::<SessionState>().city_name = world.name;
            }
            Ok(_) => {
                io.world_error = Some("New Game and Load are unavailable in multiplayer.".into())
            }
            Err(error) => io.world_error = Some(error),
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
                self.map_renderer = MapRenderer::new(&mut ctx.gfx, sim);
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
        let mut session = self.uiw.write::<SessionState>();
        let active_slot = session.active_slot.as_ref().unwrap();
        let (slot, kind) = if departure_save {
            (active_slot.clone(), SaveKind::Departure)
        } else if let Some(request) = &io.pending_save {
            (request.slot.clone(), request.kind)
        } else {
            let slot = loop {
                let slot = SlotId::autosave(io.autosave_index);
                io.autosave_index = (io.autosave_index + 1) % 3;
                if &slot != active_slot {
                    break slot;
                }
            };
            (slot, SaveKind::Autosave)
        };

        let coordinator = self.uiw.read::<SaveCoordinator>().clone();
        let Some(permit) = coordinator.request(session.generation, &slot) else {
            return;
        };
        let name = match kind {
            SaveKind::SaveAs => io.pending_save.as_ref().unwrap().name.clone(),
            SaveKind::Autosave => format!(
                "{} (autosave)",
                session.city_name.trim_end_matches(" (autosave)")
            ),
            _ => session.city_name.clone(),
        };
        let request = SaveRequest { slot, name, kind };
        match kind {
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
        io.save_result = Some(SaveInFlight {
            id,
            request,
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
                    .encode_slot(session::snapshot_revision())
                    .map_err(|error| error.to_string())?;
                Ok((
                    CityProgress::capture(&sim),
                    snapshot.with_display_name(name),
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
        let SaveInFlight { id, request, .. } = io.save_result.take().unwrap();
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
                if matches!(request.kind, SaveKind::SaveAs) {
                    session.active_slot = Some(request.slot);
                    session.city_name = request.name;
                }
                if !matches!(request.kind, SaveKind::Autosave) {
                    io.saved_progress = Some(progress);
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
        if session.active_slot.as_ref() == Some(&slot) {
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
            camera_movement_bounds(map.environment.bounds()),
            |p| map.environment.height(p),
        );
        *self.uiw.write::<Camera>() = self.uiw.read::<OrbitCamera>().camera;

        drop(map);
    }

    fn immediate_draw(&mut self, ctx: &mut FrameContext) {
        profiling::scope!("immediate_draw");

        self.immediate_renderer.clear();

        let mut tess = self.immediate_renderer.mk_tess();

        self.uiw.read::<OrbitCamera>().cull_tess(&mut tess);

        if session::world_interaction(&self.uiw) {
            profiling::scope!("debug_objs");
            let sim = self.sim.read().unwrap();
            let objs = self.uiw.read::<DebugObjs>();
            for (val, _, obj) in &objs.0 {
                if *val {
                    obj(&mut tess, &sim, &self.uiw);
                }
            }
        }

        {
            let sim = self.sim.read().unwrap();
            let immediate = &mut *self.uiw.write::<ImmediateDraw>();
            let map = sim.map();
            let mut col = LinearColor::WHITE;
            col.a = 0.2;
            unsafe {
                for v in &*addr_of!(geom::DEBUG_POS) {
                    immediate.circle(*v, 1.0).color(LinearColor::RED);
                }
                for v in &*addr_of!(geom::DEBUG_OBBS) {
                    immediate
                        .obb(*v, map.environment.height(v.center()).unwrap_or(0.0) + 8.0)
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

            immediate.apply(&mut tess, ctx);
            immediate.orders.clear();
        }

        if let Some(mut x) = self.immediate_renderer.build(ctx.gfx) {
            x.skip_depth = true;
            ctx.draw(x)
        }
    }
}

fn refresh_slots(uiw: &UiWorld) {
    let result = session::discover_slots(saveload::save_dir());
    let mut session = uiw.write::<SessionState>();
    match result {
        Ok(slots) => {
            session.slots = slots;
            session.slot_error = None;
        }
        Err(error) => {
            session.slots.clear();
            session.slot_error = Some(format!("Could not list saves. {error}"));
        }
    }
}

fn startup_world(uiw: &UiWorld) -> Simulation {
    refresh_slots(uiw);
    let candidates = uiw.read::<SessionState>().slots.clone();
    // Start with the oldest rotation slot, preferring any unoccupied slot.
    uiw.write::<SaveLoadState>().autosave_index = (0..3)
        .min_by_key(|&index| {
            let id = SlotId::autosave(index);
            candidates.iter().find_map(|(entry, _)| {
                (entry.id == id).then(|| {
                    entry
                        .meta
                        .as_ref()
                        .map_or(entry.modified_unix, |meta| meta.saved_at_unix)
                })
            })
        })
        .unwrap_or_default();

    let mut startup_error = None;
    for (entry, status) in candidates {
        if status != SlotCompatibility::Compatible {
            continue;
        }
        match Simulation::load_from_slot(saveload::save_dir(), &entry.id) {
            Ok(sim) => {
                initialize_dormant_world(uiw, &sim, &entry);
                uiw.write::<SaveLoadState>().startup_message = startup_error;
                return sim;
            }
            Err(error) => {
                log::warn!("Could not load {} at startup: {error}", entry.id);
                startup_error.get_or_insert_with(|| error.to_string());
                if let Some((_, status)) = uiw
                    .write::<SessionState>()
                    .slots
                    .iter_mut()
                    .find(|(slot, _)| slot.id == entry.id)
                {
                    *status = SlotCompatibility::Unreadable;
                }
            }
        }
    }

    let (sim, recovery) = match Simulation::load_or_new("world") {
        Ok(world) => world,
        Err(error) => {
            log::error!("{error}");
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    uiw.write::<SessionState>().active_slot = Some(SlotId::legacy());
    uiw.write::<SessionState>().city_name = "World (legacy)".into();
    if recovery.is_some() {
        refresh_slots(uiw);
    }
    uiw.write::<SaveLoadState>().startup_message = recovery.or(startup_error);
    sim
}

fn initialize_dormant_world(uiw: &UiWorld, sim: &Simulation, entry: &SlotEntry) {
    let mut state = uiw.write::<SessionState>();
    state.active_slot = Some(entry.id.clone());
    state.city_name = session::display_name(entry).to_owned();
    uiw.write::<SaveLoadState>().saved_progress = Some(CityProgress::capture(sim));
}

fn camera_movement_bounds(bounds: AABB) -> AABB {
    const INSET: f32 = 3000.0;
    let inset_x = if bounds.w() >= 2.0 * INSET {
        INSET
    } else {
        0.0
    };
    let inset_y = if bounds.h() >= 2.0 * INSET {
        INSET
    } else {
        0.0
    };
    let inset = vec2(inset_x, inset_y);
    AABB::new_ll_ur(bounds.ll + inset, bounds.ur - inset)
}

#[cfg(test)]
#[test]
fn p4_camera_bounds_allow_clamping_on_small_maps() {
    for (width, height) in [
        (0.0, 0.0),
        (512.0, 512.0),
        (512.0, 8192.0),
        (8192.0, 8192.0),
    ] {
        let world = AABB::new_ll_ur(vec2(0.0, 0.0), vec2(width, height));
        let bounds = camera_movement_bounds(world);
        let clamped = vec2(
            (-100.0_f32).clamp(bounds.ll.x, bounds.ur.x),
            (height + 100.0_f32).clamp(bounds.ll.y, bounds.ur.y),
        );
        assert!(world.contains(clamped));
    }
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
