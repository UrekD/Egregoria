use yakui::{constrained, reflow, Alignment, Color, Constraints, Dim2, Pivot, Vec2};

use goryak::{
    blur_bg, fixed_spacer, mincolumn, padxy, secondary_container, text_edit, textc, VertScroll,
    VertScrollSize,
};
use prototypes::GameTime;
use simulation::multiplayer::chat::{Message, MessageKind};
use simulation::world_command::WorldCommand;

use crate::inputmap::{InputAction, InputMap};
use crate::uiworld::UiWorld;

#[derive(Default)]
pub struct GUIChatState {
    cur_msg: String,
    chat_bar_showed: bool,
    pending: Option<String>,
}

pub(crate) fn close(uiw: &UiWorld) -> bool {
    let mut state = uiw.write::<GUIChatState>();
    if !std::mem::take(&mut state.chat_bar_showed) {
        return false;
    }
    state.cur_msg.clear();
    true
}

pub(crate) fn open_from_input(uiw: &UiWorld) {
    if crate::session::world_interaction(uiw)
        && !uiw.read::<crate::gui::GuiState>().hidden
        && uiw
            .read::<InputMap>()
            .just_act
            .contains(&InputAction::OpenChat)
    {
        uiw.write::<GUIChatState>().chat_bar_showed = true;
    }
}

pub(crate) fn pending(uiw: &UiWorld) -> bool {
    uiw.read::<GUIChatState>().pending.is_some()
}

pub(crate) fn submit_pending(uiw: &UiWorld, time: GameTime) {
    if !uiw.can_author() || !crate::session::world_interaction(uiw) {
        return;
    }
    if let Some(text) = uiw.write::<GUIChatState>().pending.take() {
        uiw.commands().push(WorldCommand::SendMessage {
            message: Message {
                name: "player".to_string(),
                text,
                sent_at: time.instant(),
                color: geom::Color::WHITE,
                kind: MessageKind::PlayerChat,
            },
        });
    }
}

pub(super) fn render(uiw: &UiWorld, msgs: &[Message]) {
    let mut state = uiw.write::<GUIChatState>();
    if !state.chat_bar_showed && msgs.is_empty() {
        return;
    }

    reflow(
        Alignment::BOTTOM_LEFT,
        Pivot::BOTTOM_LEFT,
        Dim2::pixels(0.0, -192.0),
        || {
            let alpha = if state.chat_bar_showed { 0.7 } else { 0.2 };
            blur_bg(secondary_container().with_alpha(alpha), 0.0, || {
                mincolumn(0.0, || {
                    VertScroll {
                        size: VertScrollSize::Exact(300.0),
                        align_bot: true,
                    }
                    .show(|| {
                        constrained(
                            Constraints {
                                min: Vec2::new(250.0, 0.0),
                                max: Vec2::new(250.0, f32::INFINITY),
                            },
                            || {
                                padxy(8.0, 8.0, || {
                                    mincolumn(8.0, || {
                                        for message in msgs.iter().rev() {
                                            let color = message.color;

                                            let text = message.text.clone();

                                            textc(
                                                Color::rgb(
                                                    (color.r * 255.0) as u8,
                                                    (color.g * 255.0) as u8,
                                                    (color.b * 255.0) as u8,
                                                ),
                                                text,
                                            );
                                        }
                                    });
                                });
                            },
                        );
                    });
                    if state.chat_bar_showed {
                        if text_edit(250.0, &mut state.cur_msg, "")
                            && !state.cur_msg.is_empty()
                            && state.pending.is_none()
                        {
                            state.pending = Some(std::mem::take(&mut state.cur_msg));
                            state.chat_bar_showed = false;
                        }
                    } else {
                        fixed_spacer((0.0, 30.0));
                    }
                });
            });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use yakui::event::Event;
    use yakui::input::{KeyCode, MouseButton};
    use yakui::{Rect, WidgetId, Yakui};

    fn textbox(ui: &Yakui, id: WidgetId) -> Option<WidgetId> {
        let node = ui.dom().get(id)?;
        if node.widget.is::<yakui::widgets::TextBoxWidget>() {
            return Some(id);
        }
        node.children.iter().find_map(|&id| textbox(ui, id))
    }

    #[test]
    fn client_max_chat_composer_keeps_focus_through_owned_redraw() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let uiw = UiWorld::with_defaults();
        *uiw.write::<crate::session::SessionScreen>() = crate::session::SessionScreen::Playing;
        uiw.write::<InputMap>()
            .just_act
            .insert(InputAction::OpenChat);
        open_from_input(&uiw);
        uiw.write::<InputMap>().just_act.clear();
        let mut yak = Yakui::new();
        let viewport = Vec2::new(800.0, 700.0);
        yak.set_surface_size(viewport);
        yak.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));
        let frame = |yak: &mut Yakui| {
            yak.start();
            render(&uiw, &[]);
            yak.finish();
        };
        frame(&mut yak);
        let id = textbox(&yak, yak.dom().root()).unwrap();
        let bounds = yak.layout_dom().get(id).unwrap().rect;
        yak.handle_event(Event::CursorMoved(Some(bounds.pos() + bounds.size() * 0.5)));
        frame(&mut yak);
        yak.handle_event(Event::MouseButtonChanged {
            button: MouseButton::One,
            down: true,
        });
        yak.handle_event(Event::MouseButtonChanged {
            button: MouseButton::One,
            down: false,
        });
        frame(&mut yak);
        yak.handle_event(Event::TextInput('a'));
        frame(&mut yak);
        assert_eq!(textbox(&yak, yak.dom().root()), Some(id));
        assert_eq!(uiw.read::<GUIChatState>().cur_msg, "a");
        // Yakui applies the updated controlled text as widget props on the next draw.
        frame(&mut yak);
        yak.handle_event(Event::TextInput('b'));
        frame(&mut yak);
        assert_eq!(uiw.read::<GUIChatState>().cur_msg, "ab");
        yak.handle_event(Event::KeyChanged {
            key: KeyCode::Enter,
            down: true,
        });
        frame(&mut yak);
        assert!(uiw.read::<GUIChatState>().cur_msg.is_empty());
        assert!(uiw.commands().is_empty());
        let time = GameTime::new(prototypes::Tick(123));
        uiw.write::<crate::uiworld::SaveLoadState>()
            .authoring_blocked = true;
        submit_pending(&uiw, time);
        assert!(pending(&uiw));
        assert!(uiw.commands().is_empty());

        uiw.write::<crate::uiworld::SaveLoadState>()
            .authoring_blocked = false;
        submit_pending(&uiw, time);
        submit_pending(&uiw, time);
        assert!(
            matches!(uiw.commands().as_ref(), [WorldCommand::SendMessage { message }]
            if message.text == "ab" && message.sent_at == time.instant())
        );
        assert!(!pending(&uiw));
    }
}
