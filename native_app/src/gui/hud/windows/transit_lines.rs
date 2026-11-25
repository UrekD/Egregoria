use std::collections::BTreeMap;

use goryak::{
    button_primary, button_secondary, error, fixed_spacer, mincolumn, minrow,
    on_secondary_container, outline, textc, titlec, VertScrollSize, Window,
};
use prototypes::Money;
use simulation::economy::TransitStats;
use simulation::map_dynamic::{LineID, Transit, TransitLine};
use simulation::world_command::WorldCommand;
use simulation::Simulation;
use slotmapd::Key;
use yakui::widgets::{Pad, Slider};
use yakui::{colored_box_container, divider, Color, Vec4};

use crate::uiworld::UiWorld;

#[derive(Default)]
pub(crate) struct PanelState {
    lines: Vec<LineDetails>,
    commands: Vec<WorldCommand>,
    edit: Option<LineID>,
}

struct LineDetails {
    line: TransitLine,
    running: usize,
    boardings: u64,
}

impl super::GUIWindows {
    // The toolbox has the world context. The window uses its current snapshot
    // and sends player actions back through the same frame's HUD state.
    pub(crate) fn update_transit_lines(
        &self,
        uiworld: &UiWorld,
        sim: &Simulation,
    ) -> Option<LineID> {
        if !crate::session::world_interaction(uiworld) {
            return None;
        }
        let mut state = uiworld.write::<PanelState>();
        if uiworld.can_author() {
            uiworld.commands().extend(state.commands.drain(..));
        }
        let edit = state.edit.take();
        state.lines.clear();
        if self.transit_lines_open {
            let transit = sim.read::<Transit>();
            let stats = sim.read::<TransitStats>();
            let mut running = BTreeMap::<_, usize>::new();
            for (&bus, run) in &transit.runs {
                if sim.get(bus).is_some() {
                    *running.entry(run.line).or_default() += 1;
                }
            }
            state
                .lines
                .extend(transit.lines.values().map(|line| LineDetails {
                    line: line.clone(),
                    running: running.get(&line.id).copied().unwrap_or_default(),
                    boardings: stats.boardings.get(&line.id).copied().unwrap_or_default(),
                }));
        }
        edit
    }
}

pub(super) fn transit_lines(uiworld: &UiWorld, opened: &mut bool) {
    let mut state = uiworld.write::<PanelState>();
    let PanelState {
        lines,
        commands,
        edit,
        ..
    } = &mut *state;
    let mut start_edit = false;
    let can_author = uiworld.can_author();
    Window {
        title: "Transit lines".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 8.0,
    }
    .show(|| {
        if lines.is_empty() {
            textc(on_secondary_container(), "No transit lines");
            return;
        }
        VertScrollSize::Fixed(440.0).show(|| {
            mincolumn(10.0, || {
                for (index, details) in lines.iter().enumerate() {
                    if index != 0 {
                        divider(outline().with_alpha(0.4), 2.0, 1.0);
                    }
                    if render_line(details, commands, can_author) {
                        *edit = Some(details.line.id);
                        start_edit = true;
                    }
                }
            });
        });
    });
    if can_author {
        uiworld.commands().extend(commands.drain(..));
    }
    if start_edit {
        *opened = false;
    }
}

fn render_line(details: &LineDetails, commands: &mut Vec<WorldCommand>, can_author: bool) -> bool {
    let line = &details.line;
    let color = on_secondary_container();
    minrow(8.0, || {
        let [r, g, b, a] = <[f32; 4]>::from(geom::LinearColor::from(line.color));
        colored_box_container(Color::from_linear(Vec4::new(r, g, b, a)), || {
            fixed_spacer((13.0, 13.0));
        });
        titlec(color, format!("Line {}", line.id.data().as_ffi() as u32));
        if line.suspended {
            textc(error(), "Suspended");
        }
    });
    textc(
        color,
        format!(
            "{} stops · {} running · {} boardings",
            line.stops.len(),
            details.running,
            details.boardings
        ),
    );
    for row in line.stops.chunks(8) {
        let stops = row
            .iter()
            .map(|stop| (stop.data().as_ffi() as u32).to_string())
            .collect::<Vec<_>>()
            .join(" > ");
        textc(color, stops);
    }

    minrow(18.0, || {
        mincolumn(0.0, || {
            textc(color, format!("Target buses  {}", line.target_vehicles));
            let mut slider = Slider::new(f64::from(line.target_vehicles), 0.0, 10.0);
            slider.step = Some(1.0);
            if let Some(value) = slider.show().value.filter(|_| can_author) {
                commands.push(WorldCommand::TransitSetVehicles {
                    line: line.id,
                    n: value as u8,
                });
            }
        });
        mincolumn(0.0, || {
            textc(color, format!("Fare  {}", line.fare));
            let slider = Slider::new(
                line.fare.cents() as f64,
                0.0,
                Money::new_bucks(10).cents() as f64,
            );
            if let Some(value) = slider.show().value.filter(|_| can_author) {
                let fare = Money::new_cents(value.round() as i64);
                if fare != line.fare {
                    commands.push(WorldCommand::TransitSetFare {
                        line: line.id,
                        fare,
                    });
                }
            }
        });
    });

    let mut edit = false;
    minrow(8.0, || {
        edit = button_primary("Edit route").show().clicked && can_author;
        if button_secondary("Remove line").show().clicked && can_author {
            commands.push(WorldCommand::TransitRemoveLine { line: line.id });
        }
    });
    edit
}

#[cfg(test)]
impl PanelState {
    pub(crate) fn seed_edit(&mut self, line: &TransitLine) {
        self.lines.push(LineDetails {
            line: line.clone(),
            running: 0,
            boardings: 0,
        });
        self.edit = Some(line.id);
        self.commands.push(WorldCommand::TransitRemoveLine { line: line.id });
    }

    pub(crate) fn is_clear(&self) -> bool {
        self.lines.is_empty() && self.commands.is_empty() && self.edit.is_none()
    }
}
