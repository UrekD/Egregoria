use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use goryak::{
    blur_bg, error, mincolumn, on_secondary_container, padxy, secondary_container, textc,
};
use prototypes::{Tick, TICKS_PER_HOUR};
use simulation::map::{BuildingID, ElectricityNetworkID};
use simulation::map_dynamic::UtilityFlow;
use simulation::souls::events::Fires;
use simulation::world_command::{CommandOutcomes, Rejection};
use simulation::Simulation;
use yakui::{reflow, use_state, Alignment, Dim2, Pivot};

const LIFETIME: Duration = Duration::from_secs(5);

type Outcome = (&'static str, Rejection);

#[derive(Default)]
struct ToastState {
    sequence: u64,
    started: Option<Instant>,
    city: CityEvents,
    notices: Vec<Notice>,
}

#[derive(Default)]
struct CityEvents {
    tick: Option<u64>,
    map: usize,
    fires: BTreeMap<BuildingID, Tick>,
    no_water: BTreeSet<ElectricityNetworkID>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Event {
    Fire,
    Collapse,
    NoWater,
}

struct Notice {
    event: Event,
    count: usize,
    at: Instant,
}

impl Event {
    fn describe(self, count: usize) -> String {
        match (self, count) {
            (Self::Fire, 1) => "A building caught fire".into(),
            (Self::Fire, _) => format!("{count} buildings caught fire"),
            (Self::Collapse, 1) => "A building collapsed after a fire".into(),
            (Self::Collapse, _) => format!("{count} buildings collapsed after fires"),
            (Self::NoWater, 1) => "A network is short of water".into(),
            (Self::NoWater, _) => format!("{count} networks are short of water"),
        }
    }
}

impl CityEvents {
    fn update(&mut self, sim: &Simulation) -> Option<[usize; 3]> {
        let tick = sim.get_tick();
        let map = sim.map();
        let map_id = std::ptr::from_ref(&map.subscribers) as usize;
        if self.tick == Some(tick) && self.map == map_id {
            return Some([0; 3]);
        }
        let same_city = self.map == map_id && self.tick.is_some_and(|last| tick >= last);
        self.map = map_id;
        self.tick = Some(tick);

        let fires = sim.read::<Fires>();
        let no_water: BTreeSet<_> = sim
            .read::<UtilityFlow>()
            .networks()
            .filter(|(_, totals)| totals.water_demand > totals.water_production)
            .map(|(network, _)| network)
            .collect();

        let started = fires
            .by_building
            .iter()
            .filter(|&(id, start)| self.fires.get(id) != Some(start) && map.get(*id).is_some())
            .count();

        // The fire snapshot publishes ignition ticks. An extinguished building
        // remains on the map, and early demolitions cannot hit the fire deadline.
        let collapsed = self
            .fires
            .iter()
            .filter(|&(&id, start)| {
                !fires.by_building.contains_key(&id)
                    && map.get(id).is_none()
                    && tick.saturating_sub(start.0) >= 3 * TICKS_PER_HOUR
            })
            .count();
        let shortages = no_water.difference(&self.no_water).count();

        self.fires.clone_from(&fires.by_building);
        self.fires.retain(|&id, _| map.get(id).is_some());
        self.no_water = no_water;
        same_city.then_some([started, collapsed, shortages])
    }
}

pub fn render(sim: &Simulation) {
    let state = use_state(ToastState::default);
    let outcomes = sim.read::<CommandOutcomes>();
    let mut state = state.borrow_mut();
    let now = Instant::now();
    if state.sequence != outcomes.sequence {
        state.sequence = outcomes.sequence;
        state.started = (!outcomes.last.is_empty()).then_some(now);
    }

    state
        .notices
        .retain(|notice| now.duration_since(notice.at) < LIFETIME);
    let counts = state.city.update(sim).unwrap_or_else(|| {
        state.notices.clear();
        [0; 3]
    });
    for (event, count) in [Event::Fire, Event::Collapse, Event::NoWater]
        .into_iter()
        .zip(counts)
    {
        if count == 0 {
            continue;
        }
        if let Some(notice) = state
            .notices
            .iter_mut()
            .find(|notice| notice.event == event)
        {
            notice.count += count;
            notice.at = now;
        } else {
            state.notices.push(Notice {
                event,
                count,
                at: now,
            });
        }
    }

    let rejected = state
        .started
        .is_some_and(|at| now.duration_since(at) < LIFETIME);
    if !rejected && state.notices.is_empty() {
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
                        if rejected {
                            render_rejections(&outcomes.last);
                        }
                        for notice in &state.notices {
                            textc(error(), notice.event.describe(notice.count));
                        }
                    });
                });
            });
        },
    );
}

fn render_rejections(outcomes: &[Outcome]) {
    textc(error(), "Command rejected");
    let mut unique = Vec::<Outcome>::new();
    for &outcome in outcomes {
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
