use goryak::{
    blur_bg, button_primary, button_secondary, mincolumn, minrow, on_secondary_container, padxy,
    secondary_container, textc, tooltip,
};
use simulation::Simulation;
use yakui::{opaque, reflow, Alignment, Dim2, Pivot};

use crate::gui::{GuiState, Tool};
use crate::uiworld::UiWorld;

#[path = "../../tools/transit_line.rs"]
mod transit_line;
#[path = "../../tools/transit_stop.rs"]
mod transit_stop;

#[derive(Default)]
pub(crate) struct TransitTools {
    line: transit_line::LineDraft,
    remove_stops: bool,
}

pub(super) fn render(uiworld: &UiWorld, sim: &Simulation) {
    let mut state = uiworld.write::<TransitTools>();

    if let Some(line) = uiworld
        .read::<GuiState>()
        .windows
        .update_transit_lines(uiworld, sim)
    {
        state.line.begin_edit(line);
        *uiworld.write::<Tool>() = Tool::TransitLine;
    }

    let mut tool = *uiworld.read::<Tool>();
    if tool != Tool::TransitLine {
        state.line.cancel();
    }
    if tool != Tool::TransitStop {
        state.remove_stops = false;
    }

    reflow(
        Alignment::BOTTOM_RIGHT,
        Pivot::BOTTOM_RIGHT,
        Dim2::pixels(-12.0, -100.0),
        || {
            opaque(|| {
                blur_bg(secondary_container().with_alpha(0.92), 8.0, || {
                    padxy(8.0, 6.0, || {
                        mincolumn(4.0, || {
                            if tool == Tool::TransitStop {
                                let hint = if state.remove_stops {
                                    "Click a stop to remove it."
                                } else {
                                    "Click a lane beside a sidewalk, away from junctions."
                                };
                                textc(on_secondary_container(), hint);
                            }
                            if tool == Tool::TransitLine {
                                let action = if state.line.editing() {
                                    "Edit loop"
                                } else {
                                    "New loop"
                                };
                                textc(
                                    on_secondary_container(),
                                    format!("{action} · {} of 30 stops", state.line.len()),
                                );
                                textc(
                                    on_secondary_container(),
                                    "Click stops in order. Click the first to close.",
                                );
                            }

                            minrow(6.0, || {
                                if tool_button(tool, Tool::TransitStop) {
                                    tool = Tool::TransitStop;
                                    state.remove_stops = false;
                                    state.line.cancel();
                                }
                                if tool_button(tool, Tool::TransitLine) {
                                    tool = Tool::TransitLine;
                                    state.line.cancel();
                                }
                                if tool == Tool::TransitStop
                                    && tooltip("Switch between adding and removing stops", || {
                                        button_secondary(if state.remove_stops {
                                            "Add stops"
                                        } else {
                                            "Remove stops"
                                        })
                                        .show()
                                    })
                                    .clicked
                                {
                                    state.remove_stops = !state.remove_stops;
                                }
                                if matches!(tool, Tool::TransitStop | Tool::TransitLine)
                                    && tooltip("Leave the transit tool", || {
                                        button_secondary("Cancel").show()
                                    })
                                    .clicked
                                {
                                    tool = Tool::Hand;
                                    state.line.cancel();
                                }
                            });
                        });
                    });
                });
            });
        },
    );

    *uiworld.write::<Tool>() = tool;
    match tool {
        Tool::TransitStop => transit_stop::update(sim, uiworld, state.remove_stops),
        Tool::TransitLine => transit_line::update(sim, uiworld, &mut state.line),
        _ => {}
    }
}

fn tool_button(tool: Tool, choice: Tool) -> bool {
    let label = super::tool_label(choice);
    tooltip(label, || {
        if tool == choice {
            button_primary(label).show()
        } else {
            button_secondary(label).show()
        }
    })
    .clicked
}

#[cfg(test)]
impl TransitTools {
    pub(crate) fn seed_draft(
        &mut self,
        line: simulation::map_dynamic::LineID,
        stops: &[simulation::map_dynamic::StopID],
    ) {
        self.line.begin_edit(line);
        for &stop in stops {
            assert!(self.line.select_stop(stop).is_none());
        }
        self.remove_stops = true;
    }

    pub(crate) fn draft_is_clear(&self) -> bool {
        !self.line.editing() && self.line.len() == 0 && !self.remove_stops
    }
}
