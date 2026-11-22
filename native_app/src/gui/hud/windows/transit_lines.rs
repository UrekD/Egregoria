use goryak::{on_secondary_container, textc, Window};
use yakui::widgets::Pad;

pub(super) fn transit_lines(opened: &mut bool) {
    Window {
        title: "Transit lines".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 8.0,
    }
    .show(|| {
        textc(on_secondary_container(), "No transit lines");
    });
}
