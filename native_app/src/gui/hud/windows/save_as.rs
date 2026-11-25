use goryak::{
    button_primary, button_secondary, error, minrow, on_secondary_container, text_edit, textc,
    Window,
};
use yakui::widgets::Pad;

use super::{self as session, SessionState, SessionUi};
use crate::uiworld::{SaveLoadState, UiWorld};
use common::saveload::SlotId;

#[derive(Default)]
pub(super) struct SaveAsForm {
    name: String,
    overwrite: Option<(SlotId, String)>,
    empty: bool,
}

pub(crate) fn open_save_as(uiw: &UiWorld) {
    if !session::can_save(uiw) {
        return;
    }
    let io = uiw.read::<SaveLoadState>();
    if io.saving() {
        return;
    }
    uiw.write::<SessionUi>().save_as = Some(SaveAsForm {
        name: "New Save".into(),
        ..SaveAsForm::default()
    });
    uiw.write::<SessionState>().refresh_slots = true;
    session::reset_interaction(uiw);
}

pub(super) fn render(uiw: &UiWorld) {
    let mut ui = uiw.write::<SessionUi>();
    let Some(form) = ui.save_as.as_mut() else {
        return;
    };
    let mut opened = true;
    let mut cancel = false;
    let mut target = None;
    let mut new_name = None;

    Window {
        title: if form.overwrite.is_some() {
            "Overwrite save?"
        } else {
            "Save As"
        }
        .into(),
        pad: Pad::all(16.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| {
        if let Some((slot, name)) = form.overwrite.clone() {
            textc(
                on_secondary_container(),
                format!("Replace the save in {slot}?"),
            );
            minrow(8.0, || {
                if button_primary("Overwrite").show().clicked {
                    target = Some((slot, name));
                }
                if button_secondary("Back").show().clicked {
                    form.overwrite = None;
                }
            });
            return;
        }

        textc(on_secondary_container(), "Save name");
        text_edit(280.0, &mut form.name, "Save name");
        if form.empty {
            textc(error(), "Enter a save name.");
        }
        minrow(8.0, || {
            if button_primary("Save").show().clicked {
                let name = form.name.trim().to_owned();
                form.empty = name.is_empty();
                if form.empty {
                    return;
                }
                let stem = session::city_slot(&name, &[]);
                let state = uiw.read::<SessionState>();
                let existing = state.slots.iter().find(|(entry, _)| {
                    !entry.id.as_str().starts_with("autosave_")
                        && (entry.id == stem
                            || session::display_name(entry).eq_ignore_ascii_case(&name))
                });
                if let Some((entry, _)) = existing {
                    form.overwrite = Some((entry.id.clone(), name));
                } else {
                    new_name = Some(name);
                }
            }
            cancel = button_secondary("Cancel").show().clicked;
        });
    });
    if !opened || cancel || target.is_some() || new_name.is_some() {
        ui.save_as = None;
        drop(ui);
        session::reset_interaction(uiw);
        if let Some(target) = target {
            session::request_save(uiw, Some(target));
        } else if let Some(name) = new_name {
            session::request_named_save(uiw, name);
        }
    }
}
