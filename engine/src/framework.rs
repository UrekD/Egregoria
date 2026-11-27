use rayon::ThreadPoolBuilder;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use winit::dpi::PhysicalSize;
use winit::window::Window;
use winit::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
};

use crate::egui::EguiWrapper;
use crate::frame_profile::CpuPhase;
use crate::{get_cursor_icon, AudioContext, FrameContext, GfxContext, InputContext};

#[allow(unused_variables)]
pub trait State: 'static {
    fn new(ctx: &mut Context) -> Self;

    /// Called every frame to update the game state.
    fn update(&mut self, ctx: &mut Context) {}

    /// Called every frame to prepare the game rendering.
    fn render(&mut self, fc: &mut FrameContext) {}

    /// Pump state once, then optionally present. A custom frame can borrow one
    /// view across scene preparation and both GUI passes.
    fn frame(&mut self, ctx: &mut Context)
    where
        Self: Sized,
    {
        self.update(ctx);
        ctx.present(self, Self::render, Self::render_gui, Self::render_yakui);
    }

    /// Called when the window is resized.
    fn resized(&mut self, ctx: &mut Context, size: (u32, u32, f64)) {}

    /// Called when the window asks to exit (e.g ALT+F4) to be able to control the flow, for example to ask "save before exit?".
    /// Return true to exit, false to cancel.
    fn exit(&mut self) -> bool {
        true
    }

    /// Called every frame to prepare the gui rendering.
    fn render_gui(&mut self, ui: &egui::Context) {}

    /// Called every frame to prepare the gui rendering.
    fn render_yakui(&mut self) {}
}

async fn run<S: State>(el: EventLoop<()>, window: Arc<Window>) {
    let mut ctx = Context::new(window, &el).await;
    let mut state = S::new(&mut ctx);
    ctx.gfx.defines_changed = false;

    let mut scale_factor = ctx.gfx.window.scale_factor();
    log::info!("initial scale factor: {:?}", scale_factor);
    let mut last_update = Instant::now();
    let report_frames = std::env::var_os("EGREGORIA_FRAME_TIMINGS").is_some();
    let mut last_present = None::<Instant>;
    let mut retry_redraw = None;

    el.run(move |event, target| {
        target.set_control_flow(retry_redraw.map_or(ControlFlow::Poll, ControlFlow::WaitUntil));

        if let Event::WindowEvent { event, .. } = &event {
            let egui_events = ctx.egui.platform.egui_input().events.len();
            ctx.egui.handle_event(&ctx.gfx.window, event);
            let raw = ctx.egui.platform.egui_input_mut();
            if raw.events.len() == egui_events {
                if let WindowEvent::MouseInput {
                    state: winit::event::ElementState::Released,
                    button,
                    ..
                } = event
                {
                    // Egui's adapter omits button events without a cursor position.
                    if push_egui_release(raw, (*button).into()) {
                        raw.events.push(egui::Event::PointerGone);
                    }
                }
            }
            if matches!(event, WindowEvent::Focused(false)) {
                for &button in ctx.input.physical_mouse_buttons() {
                    push_egui_release(raw, button);
                }
                raw.events.push(egui::Event::PointerGone);
                #[cfg(feature = "yakui")]
                ctx.yakui
                    .yakui
                    .handle_event(yakui::event::Event::CursorMoved(None));
            }
            ctx.input.observe_window_event(event);
        }

        #[cfg(feature = "yakui")]
        if ctx.yakui.handle_event(&event) && !ctx.keybind_mode {
            return;
        }

        match event {
            Event::AboutToWait if retry_redraw.is_some_and(|retry| Instant::now() >= retry) => {
                retry_redraw = None;
                target.set_control_flow(ControlFlow::Poll);
                ctx.gfx.window.request_redraw();
            }
            Event::DeviceEvent { event, .. } => {
                ctx.input.handle_device(&event);
            }
            Event::WindowEvent { event, .. } => {
                ctx.input.handle(&event);

                if ctx.gfx.update_sc {
                    ctx.gfx.update_sc = false;
                    let size = (ctx.gfx.size.0, ctx.gfx.size.1, scale_factor);
                    ctx.next_surface = None;
                    ctx.gfx.resize(size);
                    state.resized(&mut ctx, size);
                }

                match event {
                    WindowEvent::Resized(physical_size) => {
                        log::info!("resized: {:?}", physical_size);
                        let size = (physical_size.width, physical_size.height, scale_factor);
                        ctx.next_surface = None;
                        ctx.gfx.resize(size);
                        state.resized(&mut ctx, size);
                    }
                    WindowEvent::ScaleFactorChanged {
                        scale_factor: sf, ..
                    } => {
                        log::info!("scale_factor: {:?}", scale_factor);
                        scale_factor = sf;
                        let size = (ctx.gfx.size.0, ctx.gfx.size.1, scale_factor);
                        ctx.next_surface = None;
                        ctx.gfx.resize(size);
                        state.resized(&mut ctx, size);
                    }
                    WindowEvent::CloseRequested => {
                        if state.exit() {
                            target.exit();
                        }
                    }
                    WindowEvent::RedrawRequested => {
                        retry_redraw = None;
                        profiling::finish_frame!();
                        profiling::scope!("frame");
                        let now = Instant::now();
                        ctx.delta = now.duration_since(last_update).as_secs_f32();
                        last_update = now;
                        ctx.input.publish_pointer_frame(&ctx.egui);
                        let before = ctx.gfx.tick;
                        if let Some(profile) = &mut ctx.gfx.frame_profile {
                            profile.begin_redraw();
                        }
                        state.frame(&mut ctx);
                        ctx.finish_present(now);
                        if ctx.gfx.tick == before {
                            if let Some(profile) = &mut ctx.gfx.frame_profile {
                                profile.skip_redraw();
                            }
                        }
                        if ctx.exit_requested {
                            target.exit();
                            return;
                        }
                        let (icon, changed) = get_cursor_icon();
                        if changed {
                            ctx.gfx.window.set_cursor_icon(icon);
                        }
                        ctx.input.end_frame();
                        ctx.times.total_cpu_time = last_update.elapsed().as_secs_f32();
                        if report_frames {
                            if ctx.gfx.tick != before {
                                let presented = Instant::now();
                                let interval =
                                    last_present.replace(presented).map_or(0.0, |last| {
                                        presented.duration_since(last).as_secs_f64() * 1000.0
                                    });
                                eprintln!(
                                    "frame_ms {:.3} {:.3}",
                                    ctx.times.total_cpu_time * 1000.0,
                                    interval
                                );
                            } else {
                                eprintln!("frame_skipped");
                            }
                        }
                        if ctx.gfx.tick == before {
                            let retry = Instant::now() + std::time::Duration::from_millis(1);
                            retry_redraw = Some(retry);
                            target.set_control_flow(ControlFlow::WaitUntil(retry));
                        } else {
                            target.set_control_flow(ControlFlow::Poll);
                            ctx.gfx.window.request_redraw();
                        }
                    }
                    _ => (),
                }
            }
            _ => (),
        }
    })
    .expect("Failed to run event loop");
}

fn push_egui_release(raw: &mut egui::RawInput, button: crate::MouseButton) -> bool {
    use crate::MouseButton;
    let button = match button {
        MouseButton::Left => egui::PointerButton::Primary,
        MouseButton::Right => egui::PointerButton::Secondary,
        MouseButton::Middle => egui::PointerButton::Middle,
        MouseButton::Other(4) => egui::PointerButton::Extra1,
        MouseButton::Other(5) => egui::PointerButton::Extra2,
        MouseButton::Other(_) => return false,
    };
    raw.events.push(egui::Event::PointerButton {
        pos: egui::pos2(-1.0, -1.0),
        button,
        pressed: false,
        modifiers: raw.modifiers,
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p4d_egui_release_clears_button_after_pointer_gone() {
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.events.push(egui::Event::PointerButton {
            pos: egui::pos2(15.0, 15.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        raw.events.push(egui::Event::PointerGone);
        let _ = ctx.run(raw, |_| {});
        assert!(ctx.input(|input| input.pointer.primary_down()));

        let mut raw = egui::RawInput::default();
        assert!(!push_egui_release(&mut raw, crate::MouseButton::Other(21)));
        assert!(raw.events.is_empty());
        assert!(push_egui_release(&mut raw, crate::MouseButton::Left));
        raw.events.push(egui::Event::PointerGone);
        let _ = ctx.run(raw, |_| {});
        assert!(!ctx.input(|input| input.pointer.primary_down()));
        assert!(ctx.input(|input| input.pointer.hover_pos().is_none()));
    }
}

pub fn init() {
    #[cfg(target_arch = "wasm32")]
    {
        std::panic::set_hook(Box::new(console_error_panic_hook::hook));
        console_log::init().expect("Failed to initialize logger");
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        common::logger::MyLog::init();
    }
}

pub fn start<S: State>() {
    let _ = ThreadPoolBuilder::new().num_threads(8).build_global();
    let el = EventLoop::new().expect("Failed to create event loop");

    #[cfg(target_arch = "wasm32")]
    {
        let window = WindowBuilder::new()
            .with_transparent(true)
            .with_title("Egregoria")
            .with_inner_size(winit::dpi::PhysicalSize {
                width: 1422,
                height: 700,
            })
            .build(&el)
            .unwrap();

        use winit::platform::web::WindowExtWebSys;
        // On wasm, append the canvas to the document body
        web_sys::window()
            .and_then(|win| win.document())
            .and_then(|doc| doc.body())
            .and_then(|body| {
                body.append_child(&web_sys::Element::from(window.canvas()))
                    .ok()
            })
            .expect("Failed to append canvas to body");
        wasm_bindgen_futures::spawn_local(run(el, Arc::new(window)));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let size = match el.primary_monitor() {
            Some(monitor) => monitor.size(),
            None => el.available_monitors().next().unwrap().size(),
        };

        let wb = winit::window::WindowBuilder::new();

        let window;
        #[cfg(target_os = "windows")]
        {
            // Disable drag and drop on windows to allow cpal to init on the main thread
            // https://github.com/rust-windowing/winit/issues/1185
            use winit::platform::windows::WindowBuilderExtWindows;
            window = wb.with_drag_and_drop(false);
        }
        #[cfg(not(target_os = "windows"))]
        {
            window = wb;
        }
        let window = window
            .with_inner_size(PhysicalSize::new(
                size.width as f32 * 0.8,
                size.height as f32 * 0.8,
            ))
            .with_title(format!("Egregoria {}", include_str!("../../VERSION")))
            .build(&el)
            .expect("Failed to create window");
        let window = Arc::new(window);
        beul::execute(run::<S>(el, window))
    }
}

#[derive(Default)]
pub struct EngineTimes {
    /// Time taken by the engine to process the render commands
    pub render_time: f32,
    /// Time taken to update/render the gui
    pub gui_time: f32,
    /// Total time taken to do CPU work: update/render prepare/render/gui
    pub total_cpu_time: f32,
}

pub struct PreparedGui {
    egui: egui::FullOutput,
    build_time: f32,
}

struct PreparedScene {
    surface: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
    encoders: crate::gfx::Encoders,
    drawables: Vec<Box<dyn crate::Drawable>>,
    defer_main: bool,
}

struct PreparedFrame {
    scene: PreparedScene,
    gui: PreparedGui,
}

/// Context is the main struct that contains all the context of the game.
/// It holds the necessary state for graphics, input, audio, and the window.
pub struct Context {
    // Drop acquired textures before the graphics context that owns the surface.
    next_surface: Option<wgpu::SurfaceTexture>,
    prepared_frame: Option<PreparedFrame>,
    pub gfx: GfxContext,
    pub input: InputContext,
    pub audio: AudioContext,
    pub delta: f32,
    /// Makes sure all events go to InputContext even if catched by yakui
    pub keybind_mode: bool,
    pub times: EngineTimes,
    pub egui: EguiWrapper,
    screenshot_request: Option<PathBuf>,
    screenshot_result: Option<Result<(), String>>,
    exit_requested: bool,
    #[cfg(feature = "yakui")]
    pub yakui: crate::yakui::YakuiWrapper,
}

impl Context {
    pub async fn new(window: Arc<Window>, el: &EventLoop<()>) -> Self {
        let gfx = GfxContext::new(window).await;
        let input = InputContext::default();
        let audio = AudioContext::new();
        let egui = EguiWrapper::new(&gfx, el);

        Self {
            input,
            audio,
            delta: 0.0,
            keybind_mode: false,
            times: EngineTimes::default(),
            egui,
            next_surface: None,
            prepared_frame: None,
            screenshot_request: None,
            screenshot_result: None,
            exit_requested: false,
            #[cfg(feature = "yakui")]
            yakui: crate::yakui::YakuiWrapper::new(&gfx, &gfx.window),
            gfx,
        }
    }

    /// Acquire the surface after pumping input. A scoped world frame can call
    /// this before taking its read lease so graphics backpressure does not hold it.
    pub fn acquire_surface(&mut self) -> bool {
        if self.exit_requested {
            return false;
        }
        if self.next_surface.is_some() {
            return true;
        }
        let start = self.gfx.profile_timer();
        let surface = self.gfx.surface.get_current_texture();
        self.gfx.profile_elapsed(CpuPhase::SurfaceAcquire, start);
        match surface {
            Ok(surface) => {
                self.next_surface = Some(surface);
                true
            }
            Err(wgpu::SurfaceError::Timeout) => false,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.gfx.update_sc = true;
                false
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                log::error!("Out of memory acquiring the surface");
                self.request_exit();
                false
            }
        }
    }

    /// Build the UI while the caller has access to its current data.
    pub fn prepare_gui<S>(
        &mut self,
        state: &mut S,
        render_gui: impl FnOnce(&mut S, &egui::Context, &GfxContext),
        render_yakui: impl FnOnce(&mut S),
    ) -> PreparedGui {
        let start = Instant::now();
        #[cfg(feature = "yakui")]
        self.yakui.prepare(|| render_yakui(state));
        let egui = self
            .egui
            .prepare(&self.gfx.window, |ui| render_gui(state, ui, &self.gfx));
        PreparedGui {
            egui,
            build_time: start.elapsed().as_secs_f32(),
        }
    }

    fn finish_gui(&mut self, gui: PreparedGui) -> PreparedGui {
        #[cfg(feature = "yakui")]
        {
            let start = Instant::now();
            self.yakui.finish();
            PreparedGui {
                build_time: gui.build_time + start.elapsed().as_secs_f32(),
                ..gui
            }
        }
        #[cfg(not(feature = "yakui"))]
        gui
    }

    fn prepare_scene(
        &mut self,
        render: impl FnOnce(&mut FrameContext<'_>),
    ) -> Option<PreparedScene> {
        if !self.acquire_surface() {
            return None;
        }
        let surface = self.next_surface.take().unwrap();
        let start = self.gfx.profile_timer();
        let (encoders, view) = self.gfx.start_frame(&surface);
        self.gfx.profile_elapsed(CpuPhase::Encode, start);
        self.gfx.perf.clear();
        let mut drawables = Vec::new();
        let mut frame = FrameContext::new(&mut self.gfx, &mut drawables);
        render(&mut frame);
        let defer_main = frame.defer_main;
        Some(PreparedScene {
            surface,
            view,
            encoders,
            drawables,
            defer_main,
        })
    }

    /// Complete UI layout and prepare the draw list. The engine encodes,
    /// submits and presents after State::frame returns.
    pub fn present_prepared(
        &mut self,
        gui: PreparedGui,
        render: impl FnOnce(&mut FrameContext<'_>),
    ) {
        let gui = self.finish_gui(gui);
        if let Some(scene) = self.prepare_scene(render) {
            self.prepared_frame = Some(PreparedFrame { scene, gui });
        }
    }

    pub fn present<S>(
        &mut self,
        state: &mut S,
        render: impl FnOnce(&mut S, &mut FrameContext<'_>),
        render_gui: impl FnOnce(&mut S, &egui::Context),
        render_yakui: impl FnOnce(&mut S),
    ) {
        if let Some(scene) = self.prepare_scene(|frame| render(state, frame)) {
            let gui = self.prepare_gui(state, |state, ui, _| render_gui(state, ui), render_yakui);
            let gui = self.finish_gui(gui);
            self.prepared_frame = Some(PreparedFrame { scene, gui });
        }
    }

    fn finish_present(&mut self, frame_start: Instant) {
        let Some(PreparedFrame {
            scene:
                PreparedScene {
                    surface,
                    view,
                    mut encoders,
                    drawables,
                    defer_main,
                },
            gui,
        }) = self.prepared_frame.take()
        else {
            return;
        };
        let (render_time, paint_time) =
            self.gfx
                .encode_frame(&mut encoders, &view, &drawables, defer_main, |mut ctx| {
                    #[cfg(feature = "yakui")]
                    self.yakui.paint(&mut ctx);
                    self.egui.paint(ctx, gui.egui);
                });
        if let Some(profile) = &mut self.gfx.frame_profile {
            profile.record_cpu(
                CpuPhase::Ui,
                std::time::Duration::from_secs_f32(gui.build_time),
            );
        }
        self.times.render_time = render_time + gui.build_time;
        self.times.gui_time = paint_time + gui.build_time;
        self.gfx.finish_frame(encoders);
        let screenshot = self
            .screenshot_request
            .take()
            .map(|path| self.gfx.capture_screenshot(&surface.texture, &path));
        let start = self.gfx.profile_timer();
        surface.present();
        self.gfx.profile_elapsed(CpuPhase::Present, start);
        self.gfx.profile_presented(frame_start);
        if screenshot.is_some() {
            self.screenshot_result = screenshot;
        }
    }

    /// Capture the next presented frame, including its UI.
    pub fn request_screenshot(&mut self, path: PathBuf) -> Result<(), String> {
        if self.screenshot_request.is_some() || self.screenshot_result.is_some() {
            return Err("a screenshot is already pending".into());
        }
        self.screenshot_request = Some(path);
        Ok(())
    }

    /// Take the result once readback and PNG writing have finished.
    pub fn take_screenshot_result(&mut self) -> Option<Result<(), String>> {
        self.screenshot_result.take()
    }

    pub fn wants_exit(&self) -> bool {
        self.exit_requested
    }

    pub fn defer_exit(&mut self) {
        self.exit_requested = false;
    }

    pub fn request_exit(&mut self) {
        self.exit_requested = true;
    }
}
