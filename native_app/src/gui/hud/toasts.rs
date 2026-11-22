use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use goryak::{
    blur_bg, error, mincolumn, on_secondary_container, padxy, secondary_container, textc,
};
use simulation::map::ElectricityNetworkID;
use simulation::map_dynamic::{LineID, Transit, UtilityFlow};
use simulation::souls::events::{FireEventKind, Fires};
use simulation::world_command::{CommandOutcomes, Rejection};
use simulation::Simulation;
use yakui::{reflow, Alignment, Dim2, Pivot};

use crate::uiworld::UiWorld;

const LIFETIME: Duration = Duration::from_secs(5);

type Outcome = (&'static str, Rejection);

#[derive(Default)]
pub(crate) struct ToastState {
    visible: bool,
    sequence: u64,
    started: Option<Instant>,
    city: CityEvents,
    notices: Vec<Notice>,
}

#[derive(Default)]
struct CityEvents {
    tick: Option<u64>,
    map: usize,
    fire_cursor: u64,
    no_water: BTreeSet<ElectricityNetworkID>,
    suspended_lines: BTreeSet<LineID>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Event {
    Fire,
    Collapse,
    NoWater,
    TransitSuspended,
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
            (Self::TransitSuspended, 1) => "A transit line was suspended".into(),
            (Self::TransitSuspended, _) => format!("{count} transit lines were suspended"),
        }
    }
}

impl CityEvents {
    fn update(&mut self, sim: &Simulation) -> Option<[usize; 4]> {
        let tick = sim.get_tick();
        let map = sim.map();
        let map_id = std::ptr::from_ref(&map.subscribers) as usize;
        let same_city = self.map == map_id && self.tick.is_some();
        let utility_refresh = !same_city || self.tick.unwrap_or_default() / 3000 != tick / 3000;
        self.map = map_id;
        self.tick = Some(tick);
        if !same_city {
            self.no_water.clear();
        }
        let shortages = if utility_refresh {
            let no_water: BTreeSet<_> = sim
                .read::<UtilityFlow>()
                .networks()
                .filter(|(_, totals)| totals.water_demand > 0.0 && !totals.has_water())
                .map(|(network, _)| network)
                .collect();
            let started = no_water.difference(&self.no_water).count();
            self.no_water = no_water;
            started
        } else {
            0
        };

        let fires = sim.read::<Fires>();
        let latest = fires.events.back().map_or(0, |event| event.seq);
        let mut started = 0;
        let mut collapsed = 0;
        if same_city && latest > self.fire_cursor {
            for event in fires.events_since(self.fire_cursor) {
                match event.kind {
                    FireEventKind::Ignited => started += 1,
                    FireEventKind::Collapsed => collapsed += 1,
                    FireEventKind::Extinguished => {}
                }
            }
        }
        self.fire_cursor = latest;

        let suspended_lines = sim
            .read::<Transit>()
            .lines
            .iter()
            .filter_map(|(id, line)| line.suspended.then_some(id))
            .collect::<BTreeSet<_>>();
        let suspended = suspended_lines.difference(&self.suspended_lines).count();
        self.suspended_lines = suspended_lines;
        same_city.then_some([started, collapsed, shortages, suspended])
    }
}

impl ToastState {
    pub(super) fn observe(&mut self, sim: &Simulation, visible: bool, now: Instant) {
        let outcomes = sim.read::<CommandOutcomes>();
        let sequence = outcomes.sequence;
        let counts = self.city.update(sim);
        if !visible || !self.visible || counts.is_none() {
            self.visible = visible;
            self.sequence = sequence;
            self.started = None;
            self.notices.clear();
            return;
        }

        if self
            .started
            .is_some_and(|at| now.duration_since(at) >= LIFETIME)
        {
            self.started = None;
        }
        if self.sequence != sequence {
            self.sequence = sequence;
            self.started = if outcomes.last.is_empty() {
                None
            } else {
                self.started.or(Some(now))
            };
        }
        self.notices
            .retain(|notice| now.duration_since(notice.at) < LIFETIME);

        let [fires, collapses, new_shortages, suspended] = counts.unwrap_or_default();
        for (event, count) in [
            (Event::Fire, fires),
            (Event::Collapse, collapses),
            (Event::TransitSuspended, suspended),
        ] {
            if count == 0 {
                continue;
            }
            if let Some(notice) = self.notices.iter_mut().find(|notice| notice.event == event) {
                notice.count += count;
            } else {
                self.notices.push(Notice {
                    event,
                    count,
                    at: now,
                });
            }
        }

        let count = self.city.no_water.len();
        if count == 0 {
            self.notices.retain(|notice| notice.event != Event::NoWater);
        } else if let Some(notice) = self
            .notices
            .iter_mut()
            .find(|notice| notice.event == Event::NoWater)
        {
            notice.count = count;
        } else if new_shortages > 0 {
            self.notices.push(Notice {
                event: Event::NoWater,
                count,
                at: now,
            });
        }
    }
}

pub(super) fn render(sim: &Simulation, uiworld: &UiWorld) {
    let state = uiworld.read::<ToastState>();
    let outcomes = sim.read::<CommandOutcomes>();
    let rejected = state.started.is_some() && !outcomes.last.is_empty();
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
            format!("{command}: {}", command_message(command, *reason)),
        );
    }
    if unique.len() > 3 {
        textc(
            on_secondary_container(),
            format!("{} more reasons", unique.len() - 3),
        );
    }
}

fn command_message(command: &str, reason: Rejection) -> &'static str {
    match (command, reason) {
        ("TransitAddStop", Rejection::MissingReference | Rejection::InvalidConnection) => {
            "Choose a driving lane beside a sidewalk and away from junctions"
        }
        ("TransitCreateLine" | "TransitEditLine", Rejection::InvalidConnection) => {
            "The stops must form a drivable loop"
        }
        _ => message(reason),
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
        Rejection::TransitNotImplemented => "Transit editing is unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec2;
    use prototypes::{Tick, TICKS_PER_HOUR};
    use scenarios::{
        builders::{fill_houses, grid_city},
        ScenarioCtx,
    };
    use simulation::souls::events::FireEvent;
    use simulation::world_command::WorldCommand;
    use simulation::SimulationOptions;

    #[test]
    fn p2_ui_wire_demolition_after_fire_is_not_a_collapse() {
        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        let mut events = CityEvents::default();
        test.sim.write::<Fires>().by_building.insert(house, Tick(0));
        events.update(&test.sim);
        test.sim.write::<Fires>().by_building.remove(&house);
        test.sim.write::<prototypes::GameTime>().tick = Tick(3 * TICKS_PER_HOUR + 1);
        events.update(&test.sim);
        test.submit(&[WorldCommand::MapRemoveBuilding(house)]);
        test.run_ticks(1);
        assert!(test.sim.map().get(house).is_none());
        assert_eq!(events.update(&test.sim).unwrap()[1], 0);
    }

    #[test]
    fn p2_ui_wire_consumes_fire_events_once_even_in_the_same_tick() {
        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        let mut events = CityEvents::default();
        let record = |seq, kind| {
            test.sim.write::<Fires>().events.push_back(FireEvent {
                seq,
                tick: Tick(0),
                building: house,
                kind,
            });
        };
        record(1, FireEventKind::Ignited);
        assert!(events.update(&test.sim).is_none());
        record(2, FireEventKind::Extinguished);
        record(3, FireEventKind::Ignited);
        record(4, FireEventKind::Collapsed);
        assert_eq!(events.update(&test.sim), Some([1, 1, 0, 0]));
        assert_eq!(events.update(&test.sim), Some([0, 0, 0, 0]));
    }
}
