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
use crate::{get_cursor_icon, AudioContext, FrameContext, GfxContext, InputContext};

#[allow(unused_variables)]
pub trait State: 'static {
    fn new(ctx: &mut Context) -> Self;

    /// Called every frame to update the game state.
    fn update(&mut self, ctx: &mut Context);

    /// Called every frame to prepare the game rendering.
    fn render(&mut self, fc: &mut FrameContext);

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
    #[cfg(feature = "yakui")]
    fn render_yakui(&mut self) {}
}

async fn run<S: State>(el: EventLoop<()>, window: Arc<Window>) {
    let mut ctx = Context::new(window, &el).await;
    let mut state = S::new(&mut ctx);
    ctx.gfx.defines_changed = false;

    let mut scale_factor = ctx.gfx.window.scale_factor();
    log::info!("initial scale factor: {:?}", scale_factor);
    let mut last_update = Instant::now();

    el.run(move |event, target| {
        target.set_control_flow(ControlFlow::Poll);

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
            Event::DeviceEvent { event, .. } => {
                ctx.input.handle_device(&event);
            }
            Event::WindowEvent { event, .. } => {
                ctx.input.handle(&event);

                if ctx.gfx.update_sc {
                    ctx.gfx.update_sc = false;
                    let size = (ctx.gfx.size.0, ctx.gfx.size.1, scale_factor);
                    ctx.gfx.resize(size);
                    state.resized(&mut ctx, size);
                }

                match event {
                    WindowEvent::Resized(physical_size) => {
                        log::info!("resized: {:?}", physical_size);
                        let size = (physical_size.width, physical_size.height, scale_factor);
                        ctx.gfx.resize(size);
                        state.resized(&mut ctx, size);
                    }
                    WindowEvent::ScaleFactorChanged {
                        scale_factor: sf, ..
                    } => {
                        log::info!("scale_factor: {:?}", scale_factor);
                        scale_factor = sf;
                        let size = (ctx.gfx.size.0, ctx.gfx.size.1, scale_factor);
                        ctx.gfx.resize(size);
                        state.resized(&mut ctx, size);
                    }
                    WindowEvent::CloseRequested => {
                        if state.exit() {
                            target.exit();
                        }
                    }
                    WindowEvent::RedrawRequested => {
                        let sco = match ctx.gfx.surface.get_current_texture() {
                            Ok(swapchainframe) => swapchainframe,
                            Err(wgpu::SurfaceError::Timeout) => ctx
                                .gfx
                                .surface
                                .get_current_texture()
                                .expect("Failed to acquire next swap chain texture after timeout"),
                            Err(wgpu::SurfaceError::Outdated)
                            | Err(wgpu::SurfaceError::Lost)
                            | Err(wgpu::SurfaceError::OutOfMemory) => {
                                let size = ctx.gfx.size;
                                ctx.gfx.resize(size);
                                state.resized(&mut ctx, size);
                                ctx.gfx
                                    .surface
                                    .get_current_texture()
                                    .expect("Failed to acquire next swap chain texture after losing surface")
                            }
                        };

                        profiling::finish_frame!();
                        profiling::scope!("frame");
                        let d = last_update.elapsed();
                        last_update = Instant::now();
                        ctx.delta = d.as_secs_f32();
                        ctx.input.publish_pointer_frame(&ctx.egui);
                        state.update(&mut ctx);
                        if ctx.exit_requested {
                            target.exit();
                            return;
                        }

                        let (mut enc, view) = ctx.gfx.start_frame(&sco);
                        (ctx.times.render_time, ctx.times.gui_time) = ctx
                            .gfx
                            .render(&mut enc, &view, &mut state, |state, mut gctx| {
                                #[cfg(feature = "yakui")]
                                ctx.yakui.render(&mut gctx, || {
                                    state.render_yakui();
                                });
                                ctx.egui.render(gctx, |ui| {
                                    state.render_gui(ui);
                                });
                            });

                        ctx.gfx.finish_frame(enc);
                        if let Some(path) = ctx.screenshot_request.take() {
                            ctx.screenshot_result =
                                Some(ctx.gfx.capture_screenshot(&sco.texture, &path));
                        }
                        let (icon, changed) = get_cursor_icon();
                        if changed {
                            ctx.gfx.window.set_cursor_icon(icon);
                        }
                        ctx.input.end_frame();
                        ctx.times.total_cpu_time = last_update.elapsed().as_secs_f32();

                        sco.present();
                        ctx.gfx.window.request_redraw();
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

/// Context is the main struct that contains all the context of the game.
/// It holds the necessary state for graphics, input, audio, and the window.
pub struct Context {
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
            screenshot_request: None,
            screenshot_result: None,
            exit_requested: false,
            #[cfg(feature = "yakui")]
            yakui: crate::yakui::YakuiWrapper::new(&gfx, &gfx.window),
            gfx,
        }
    }

    /// Capture the next rendered frame after its UI pass.
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

    pub fn request_exit(&mut self) {
        self.exit_requested = true;
    }
}
