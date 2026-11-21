use crate::uiworld::UiWorld;
use goryak::{
    blur_bg, error, mincolumn, on_secondary_container, padxy, secondary_container, textc,
};
use simulation::world_command::{CommandOutcomes, Rejection};
use simulation::Simulation;
use std::time::{Duration, Instant};
use yakui::{reflow, use_state, Alignment, Dim2, Pivot};

const LIFETIME: Duration = Duration::from_secs(5);

type Outcome = (&'static str, Rejection);

#[derive(Default)]
struct ToastState {
    last: Vec<Outcome>,
    started: Option<Instant>,
}

pub fn render(uiworld: &UiWorld, sim: &Simulation) {
    let state = use_state(ToastState::default);
    let outcomes = sim.read::<CommandOutcomes>();
    let mut state = state.borrow_mut();
    let received = uiworld.received_commands();
    if state.last != outcomes.last || received.iter().next().is_some() {
        state.last.clone_from(&outcomes.last);
        state.started = (!state.last.is_empty()).then(Instant::now);
    }

    if !state.started.is_some_and(|at| at.elapsed() < LIFETIME) {
        return;
    }

    reflow(
        Alignment::TOP_RIGHT,
        Pivot::TOP_RIGHT,
        Dim2::pixels(-14.0, 108.0),
        || {
            blur_bg(secondary_container().with_alpha(0.96), 8.0, || {
                padxy(12.0, 8.0, || {
                    mincolumn(4.0, || {
                        textc(error(), "Command rejected");
                        let mut unique = Vec::new();
                        for &outcome in &state.last {
                            if !unique.contains(&outcome) {
                                unique.push(outcome);
                            }
                        }
                        for (command, reason) in unique.iter().take(3) {
                            textc(
                                on_secondary_container(),
                                format!("{command}: {}", message(*reason)),
                            );
                        }
                        if unique.len() > 3 {
                            textc(
                                on_secondary_container(),
                                format!("{} more reasons", unique.len() - 3),
                            );
                        }
                    });
                });
            });
        },
    );
}

fn message(reason: Rejection) -> &'static str {
    match reason {
        Rejection::MissingReference => "Object no longer exists",
        Rejection::InvalidConnection => "Invalid connection",
        Rejection::InvalidBatchIndex => "Invalid connection group",
        Rejection::InvalidBuilding => "Building cannot be placed here",
        Rejection::InvalidZone => "Zoning area is outside the map or too large",
        Rejection::InvalidTaxRate => "Tax rate is outside the allowed range",
        Rejection::InvalidTrain => "Train cannot be placed here",
        Rejection::CostOverflow => "Construction cost is too large",
        Rejection::InsufficientFunds => "Not enough money",
        Rejection::MutationFailed => "The map change could not be applied",
    }
}
