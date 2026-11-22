use goryak::{
    button_primary, button_secondary, minrow, on_secondary_container, padxy, textc, tooltip,
};
use simulation::economy::progression::{can_build, BuildCapability};
use simulation::Simulation;

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

pub(super) fn update_lines(uiworld: &UiWorld, sim: &Simulation) {
    if let Some(line) = uiworld
        .read::<GuiState>()
        .windows
        .update_transit_lines(uiworld, sim)
    {
        uiworld.write::<TransitTools>().line.begin_edit(line);
        *uiworld.write::<Tool>() = Tool::TransitLine;
    }
}

pub(super) fn buttons(uiworld: &UiWorld, sim: &Simulation) {
    minrow(6.0, || {
        let current = *uiworld.read::<Tool>();
        for choice in [Tool::TransitStop, Tool::TransitLine] {
            if choice == Tool::TransitLine {
                if let Err(lock) = can_build(sim, BuildCapability::BusService) {
                    tooltip(format!("Unlocks at {} citizens", lock.population), || {
                        let mut button = button_secondary("Bus line");
                        button.style.fill = button.style.fill.with_alpha(0.45);
                        button.style.text.color = button.style.text.color.with_alpha(0.55);
                        button.hover_style = button.style.clone();
                        button.down_style = button.style.clone();
                        button.show();
                    });
                    continue;
                }
            }
            if tool_button(current, choice) {
                let mut state = uiworld.write::<TransitTools>();
                state.line.cancel();
                state.remove_stops = false;
                *uiworld.write::<Tool>() = choice;
            }
        }
    });
}

pub(super) fn properties(uiworld: &UiWorld, tool: Tool) {
    let mut state = uiworld.write::<TransitTools>();
    padxy(12.0, 8.0, || {
        minrow(10.0, || {
            if tool == Tool::TransitStop {
                let hint = if state.remove_stops {
                    "Click a stop to remove it."
                } else {
                    "Click a lane beside a sidewalk, away from junctions."
                };
                textc(on_secondary_container(), hint);
                if tooltip("Switch between adding and removing stops", || {
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
            } else {
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
            if tooltip("Leave the transit tool", || {
                button_secondary("Cancel").show()
            })
            .clicked
            {
                *uiworld.write::<Tool>() = Tool::Hand;
            }
        });
    });
}

pub(super) fn update_tool(uiworld: &UiWorld, sim: &Simulation) {
    let mut state = uiworld.write::<TransitTools>();
    let tool = *uiworld.read::<Tool>();
    if tool != Tool::TransitLine {
        state.line.cancel();
    }
    if tool != Tool::TransitStop {
        state.remove_stops = false;
    }
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
