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
    sequence: u64,
    started: Option<Instant>,
}

pub fn render(sim: &Simulation) {
    let state = use_state(ToastState::default);
    let outcomes = sim.read::<CommandOutcomes>();
    let mut state = state.borrow_mut();
    if state.sequence != outcomes.sequence {
        state.sequence = outcomes.sequence;
        state.started = (!outcomes.last.is_empty()).then(Instant::now);
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
                        let mut unique = Vec::<Outcome>::new();
                        for &outcome in &outcomes.last {
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
        Rejection::InvalidTerraform => "Terrain edit is outside the allowed range",
        Rejection::TooManyRandomCars => "A command can spawn at most 1,000 random cars",
        Rejection::InvalidTrain => "Train cannot be placed here",
        Rejection::CostOverflow => "Construction cost is too large",
        Rejection::InsufficientFunds => "Not enough money",
        Rejection::MutationFailed => "The map change could not be applied",
    }
}
