use std::collections::{BTreeMap, VecDeque};

use prototypes::{Money, Tick};
use simulation::diagnostics::elapsed_tick;
use simulation::economy::{
    balance::{DEMAND_INTERVAL_TICKS, MIN_DEMAND_POPULATION},
    Budget, BudgetCategory, Government, ZoneDemand,
};
use simulation::map::{BuildingID, ElectricityNetworkID, ZoneKind};
use simulation::map_dynamic::{ElectricityFlow, LineID, Transit, UtilityFlow, UtilityStatus};
use simulation::souls::events::{FireEventKind, Fires};
use simulation::souls::residence::Residences;
use simulation::world_command::{CommandOutcomes, Rejection};
use simulation::Simulation;
use slotmapd::Key;

const CAPACITY: usize = 200;
const SAMPLE_INTERVAL: u64 = DEMAND_INTERVAL_TICKS;
const UNEMPLOYMENT_PERCENT: usize = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NotificationKind {
    CommandRejected,
    NegativeBudget,
    BudgetDeficit,
    Blackout,
    NoWater,
    NoSewage,
    Fire,
    FireCollapse,
    SuspendedLine,
    HighUnemployment,
    StalledDemand,
}

impl NotificationKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::CommandRejected => "Command rejected",
            Self::NegativeBudget => "Negative treasury",
            Self::BudgetDeficit => "Operating deficit",
            Self::Blackout => "Blackout",
            Self::NoWater => "Water shortage",
            Self::NoSewage => "Sewage shortage",
            Self::Fire => "Fire",
            Self::FireCollapse => "Fire damage",
            Self::SuspendedLine => "Suspended line",
            Self::HighUnemployment => "High unemployment",
            Self::StalledDemand => "Stalled demand",
        }
    }

    pub(crate) fn severity(self) -> Severity {
        match self {
            Self::StalledDemand => Severity::Info,
            Self::BudgetDeficit | Self::SuspendedLine | Self::HighUnemployment => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AlertTarget {
    Building(BuildingID),
    Line(LineID),
    CityWide,
}

#[derive(Clone, Debug)]
pub(crate) struct Notification {
    pub kind: NotificationKind,
    pub severity: Severity,
    pub tick: Tick,
    pub target: AlertTarget,
    pub text: String,
}

#[derive(Default)]
pub(crate) struct NotificationCenter {
    items: VecDeque<Notification>,
}

impl NotificationCenter {
    pub(crate) fn push(&mut self, notification: Notification) {
        push_bounded(&mut self.items, notification);
    }

    pub(crate) fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = &Notification> + ExactSizeIterator {
        self.items.iter()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }
}

fn push_bounded(items: &mut VecDeque<Notification>, notification: Notification) {
    if items.len() == CAPACITY {
        items.pop_front();
    }
    items.push_back(notification);
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AlertSource {
    Network(ElectricityNetworkID),
    Line(LineID),
    CityWide,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AlertKey {
    kind: NotificationKind,
    source: AlertSource,
}

/// The HUD observes even while hidden. The next visible frame publishes the
/// bounded backlog to history and uses those notifications for the toast UI.
#[derive(Default)]
pub(super) struct NotificationSampler {
    pending: VecDeque<Notification>,
    last_batch_seq: Option<u64>,
    fire_cursor: Option<u64>,
    last_sampled_tick: Option<Tick>,
    active: BTreeMap<AlertKey, Tick>,
    operating_balance: Option<i128>,
    operating_losses: u8,
    stalled_samples: u8,
}

impl NotificationSampler {
    pub(super) fn observe(&mut self, sim: &Simulation) {
        let tick = elapsed_tick(sim);
        self.observe_rejections(sim, tick);
        self.observe_fires(sim, tick);
        if self
            .last_sampled_tick
            .is_none_or(|previous| tick.0.saturating_sub(previous.0) >= SAMPLE_INTERVAL)
        {
            self.last_sampled_tick = Some(tick);
            self.sample(sim, tick);
            // The sampling interval gives recovered conditions a cooldown before
            // they can recur. Resolved or deleted sources keep no state.
            self.active.retain(|_, seen| *seen == tick);
        }
    }

    pub(super) fn drain(&mut self) -> impl ExactSizeIterator<Item = Notification> + '_ {
        self.pending.drain(..)
    }

    #[cfg(test)]
    pub(super) fn is_clear(&self) -> bool {
        self.pending.is_empty()
            && self.last_batch_seq.is_none()
            && self.fire_cursor.is_none()
            && self.last_sampled_tick.is_none()
            && self.active.is_empty()
            && self.operating_balance.is_none()
            && self.operating_losses == 0
            && self.stalled_samples == 0
    }

    fn emit(&mut self, kind: NotificationKind, tick: Tick, target: AlertTarget, text: String) {
        push_bounded(
            &mut self.pending,
            Notification {
                kind,
                severity: kind.severity(),
                tick,
                target,
                text,
            },
        );
    }

    fn observe_rejections(&mut self, sim: &Simulation, tick: Tick) {
        let outcomes = sim.read::<CommandOutcomes>();
        let sequence = outcomes.batch_seq();
        let previous = self.last_batch_seq.replace(sequence);
        // A world first observed after replay can carry a playback batch.
        if previous.is_none_or(|previous| previous == sequence) {
            return;
        }
        let mut reasons = Vec::new();
        for &(command, reason) in &outcomes.last {
            if !reasons.contains(&(command, reason)) {
                reasons.push((command, reason));
                self.emit(
                    NotificationKind::CommandRejected,
                    tick,
                    AlertTarget::CityWide,
                    format!("{command}: {}", command_message(command, reason)),
                );
            }
        }
    }

    fn observe_fires(&mut self, sim: &Simulation, tick: Tick) {
        let fires = sim.read::<Fires>();
        let latest = fires.events.back().map_or(0, |event| event.seq);
        if let Some(previous) = self.fire_cursor.replace(latest) {
            if latest <= previous {
                return;
            }
            for event in fires.events_since(previous) {
                let (kind, text) = match event.kind {
                    FireEventKind::Ignited => (NotificationKind::Fire, "A building caught fire"),
                    FireEventKind::Collapsed => (
                        NotificationKind::FireCollapse,
                        "A building collapsed after a fire",
                    ),
                    FireEventKind::Extinguished => continue,
                };
                self.emit(
                    kind,
                    event.tick,
                    AlertTarget::Building(event.building),
                    text.into(),
                );
            }
        } else {
            for &building in fires.by_building.keys() {
                self.emit(
                    NotificationKind::Fire,
                    tick,
                    AlertTarget::Building(building),
                    "A building is on fire".into(),
                );
            }
        }
    }

    fn condition(
        &mut self,
        kind: NotificationKind,
        source: AlertSource,
        tick: Tick,
        target: AlertTarget,
        text: impl FnOnce() -> String,
    ) {
        let key = AlertKey { kind, source };
        if self.active.insert(key, tick).is_none() {
            self.emit(kind, tick, target, text());
        }
    }

    fn sample(&mut self, sim: &Simulation, tick: Tick) {
        use AlertSource::CityWide;
        use NotificationKind::*;

        let treasury = sim.read::<Government>().money;
        if treasury < Money::ZERO {
            self.condition(
                NegativeBudget,
                CityWide,
                tick,
                AlertTarget::CityWide,
                || format!("The treasury balance fell to {treasury}"),
            );
        }
        let operating_balance = sim
            .read::<Budget>()
            .days()
            .values()
            .flat_map(|day| day.iter())
            .filter(|(category, _)| **category != BudgetCategory::Construction)
            .map(|(_, delta)| *delta)
            .sum::<i128>();
        if let Some(previous) = self.operating_balance.replace(operating_balance) {
            if operating_balance < previous {
                self.operating_losses = self.operating_losses.saturating_add(1);
            } else if operating_balance > previous {
                self.operating_losses = 0;
            }
        }
        if self.operating_losses >= 2 {
            self.condition(BudgetDeficit, CityWide, tick, AlertTarget::CityWide, || {
                "Operating expenses have repeatedly exceeded income".into()
            });
        }

        let humans = &sim.world().humans;
        let population = humans.len();
        let unemployed = humans.values().filter(|human| human.work.is_none()).count();
        if population >= MIN_DEMAND_POPULATION
            && unemployed * 100 >= population * UNEMPLOYMENT_PERCENT
        {
            self.condition(
                HighUnemployment,
                CityWide,
                tick,
                AlertTarget::CityWide,
                || format!("{unemployed} of {population} citizens are unemployed"),
            );
        }
        let demand = sim.read::<ZoneDemand>();
        let stalled = population >= MIN_DEMAND_POPULATION
            && [
                ZoneKind::Residential,
                ZoneKind::Commercial,
                ZoneKind::Industrial,
                ZoneKind::Office,
            ]
            .into_iter()
            .all(|zone| demand.values.get(&zone).is_some_and(|&value| value <= 0.0));
        self.stalled_samples = if stalled {
            self.stalled_samples.saturating_add(1)
        } else {
            0
        };
        if self.stalled_samples >= 2 {
            self.condition(StalledDemand, CityWide, tick, AlertTarget::CityWide, || {
                "Demand has stalled across all zone types".into()
            });
        }

        for line in sim
            .read::<Transit>()
            .lines
            .values()
            .filter(|line| line.suspended)
        {
            self.condition(
                SuspendedLine,
                AlertSource::Line(line.id),
                tick,
                AlertTarget::Line(line.id),
                || {
                    format!(
                        "Transit line {} suspended service",
                        line.id.data().as_ffi() as u32
                    )
                },
            );
        }
        self.sample_networks(sim, tick);
    }

    fn sample_networks(&mut self, sim: &Simulation, tick: Tick) {
        use NotificationKind::*;

        let map = sim.map();
        let power = sim.read::<ElectricityFlow>();
        let utilities = sim.read::<UtilityFlow>();
        let residences = sim.read::<Residences>();
        let totals = utilities.networks().collect::<BTreeMap<_, _>>();
        for network in map.electricity.networks() {
            let buildings = || {
                network
                    .buildings
                    .iter()
                    .copied()
                    .filter(|&id| map.get(id).is_some_and(|building| !building.is_orphaned()))
            };
            let source = AlertSource::Network(network.id);
            if power.blackout(network.id) {
                if let Some(building) = buildings().next() {
                    self.condition(
                        Blackout,
                        source,
                        tick,
                        AlertTarget::Building(building),
                        || "An electrical network is in blackout".into(),
                    );
                }
            }
            let Some(totals) = totals.get(&network.id) else {
                continue;
            };
            for (kind, shortage, text) in [
                (
                    NoWater,
                    totals.water_demand > 0.0 && !totals.has_water(),
                    "A network is short of water",
                ),
                (
                    NoSewage,
                    totals.sewage_demand > 0.0 && !totals.has_sewage(),
                    "A network lacks sewage capacity",
                ),
            ] {
                if shortage {
                    if let Some(building) = buildings().find(|&id| {
                        utilities
                            .status(&map, &residences, id)
                            .is_some_and(|status| {
                                let service = if kind == NoWater {
                                    status.water
                                } else {
                                    status.sewage
                                };
                                service == UtilityStatus::Shortage
                            })
                    }) {
                        self.condition(kind, source, tick, AlertTarget::Building(building), || {
                            text.into()
                        });
                    }
                }
            }
        }
    }
}

fn command_message(command: &str, reason: Rejection) -> &'static str {
    use Rejection::*;
    match (command, reason) {
        ("TransitAddStop", MissingReference | InvalidConnection) => {
            "Choose a driving lane beside a sidewalk and away from junctions"
        }
        ("TransitCreateLine" | "TransitEditLine", InvalidConnection) => {
            "The stops must form a drivable loop"
        }
        (_, MissingReference) => "Object no longer exists",
        (_, InvalidConnection) => "Invalid connection",
        (_, InvalidBatchIndex) => "Invalid connection group",
        (_, InvalidBuilding) => "Building cannot be placed here",
        (_, InvalidZone) => "Zoning area is outside the map or too large",
        (_, InvalidTaxRate) => "Tax rate is outside the allowed range",
        (_, InvalidTerraform) => "Terrain edit is outside the allowed range",
        (_, TooManyRandomCars) => "A command can spawn at most 1,000 random cars",
        (_, InvalidTrain) => "Train cannot be placed here",
        (_, CostOverflow) => "Construction cost is too large",
        (_, InsufficientFunds) => "Not enough money",
        (_, MutationFailed) => "The map change could not be applied",
        (_, TransitNotImplemented) => "Transit editing is unavailable",
        (_, UnexpectedInit) => "Init is available only when creating a world",
        (_, InvalidInitOptions) => "Invalid world options",
    }
}

#[cfg(test)]
#[test]
fn p4_notification_history_evicts_oldest_and_clears() {
    let mut center = NotificationCenter::default();
    for tick in 0..=CAPACITY as u64 {
        center.push(Notification {
            kind: NotificationKind::Blackout,
            severity: Severity::Warning,
            tick: Tick(tick),
            target: AlertTarget::CityWide,
            text: "Blackout".into(),
        });
    }
    let ticks: Vec<_> = center.iter().map(|item| item.tick.0).collect();
    assert_eq!(ticks, (1..=CAPACITY as u64).collect::<Vec<_>>());
    center.clear();
    assert_eq!(center.iter().len(), 0);
}

#[cfg(test)]
mod tests {
    use geom::vec2;
    use prototypes::GameTime;
    use scenarios::{
        builders::{fill_houses, grid_city},
        ScenarioCtx,
    };
    use simulation::map_dynamic::ElectricityFlow;
    use simulation::world_command::WorldCommand;
    use simulation::SimulationOptions;

    use super::*;

    fn city() -> (ScenarioCtx, BuildingID) {
        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        test.run_ticks(1);
        (test, house)
    }

    #[test]
    fn p4g_equal_rejected_batches_alert_once_each() {
        let (mut test, _) = city();
        let mut sampler = NotificationSampler::default();
        let mut center = NotificationCenter::default();
        let rejected = [WorldCommand::MapRemoveBuilding(BuildingID::default())];
        sampler.observe(&test.sim);
        for _ in 0..2 {
            test.sim.apply_commands(&rejected);
            sampler.observe(&test.sim);
            sampler.observe(&test.sim);
        }
        for notice in sampler.drain() {
            center.push(notice);
        }
        let rejections = center
            .iter()
            .filter(|item| item.kind == NotificationKind::CommandRejected)
            .collect::<Vec<_>>();
        assert_eq!(rejections.len(), 2);
        assert_eq!(rejections[0].text, rejections[1].text);
    }

    #[test]
    fn p4g_sustained_blackout_alerts_once_and_recovery_rearms_it() {
        let (test, house) = city();
        let mut sampler = NotificationSampler::default();
        let blackout = {
            let mut flow = test.sim.write::<ElectricityFlow>();
            std::mem::take(&mut *flow)
        };
        let network = test.sim.map().electricity.net_id(house).unwrap();
        assert!(blackout.blackout(network));
        let count = |sampler: &mut NotificationSampler| {
            sampler
                .drain()
                .filter(|item| item.kind == NotificationKind::Blackout)
                .count()
        };
        let elapsed = test.sim.get_tick();
        let sample_at = |sampler: &mut NotificationSampler, offset| {
            *test.sim.write::<GameTime>() = GameTime::new(Tick(elapsed + offset));
            sampler.observe(&test.sim);
        };
        sample_at(&mut sampler, 0);
        assert_eq!(count(&mut sampler), 0);
        *test.sim.write::<ElectricityFlow>() = blackout;
        sample_at(&mut sampler, SAMPLE_INTERVAL - 1);
        assert_eq!(count(&mut sampler), 0);
        sample_at(&mut sampler, SAMPLE_INTERVAL + 7);
        assert_eq!(count(&mut sampler), 1);
        sample_at(&mut sampler, 2 * SAMPLE_INTERVAL + 7);
        assert_eq!(count(&mut sampler), 0);
        let blackout = std::mem::take(&mut *test.sim.write::<ElectricityFlow>());
        sample_at(&mut sampler, 3 * SAMPLE_INTERVAL + 7);
        assert_eq!(count(&mut sampler), 0);
        assert!(!sampler
            .active
            .keys()
            .any(|key| key.kind == NotificationKind::Blackout));
        *test.sim.write::<ElectricityFlow>() = blackout;
        sample_at(&mut sampler, 4 * SAMPLE_INTERVAL + 7);
        assert_eq!(count(&mut sampler), 1);
    }

    #[test]
    fn p4g_world_swap_clears_published_and_pending_notifications() {
        use common::saveload::SlotId;
        use geom::{Camera, Vec3};

        use crate::gui::hud::ToastState;
        use crate::rendering::OrbitCamera;
        use crate::session::{apply_world_replacement, WorldReplacement};
        use crate::uiworld::UiWorld;

        crate::init::init();
        let (mut test, house) = city();
        let (replacement, new_house) = city();
        assert_eq!(house, new_house);
        let mut uiw = UiWorld::with_defaults();
        let mut camera = Camera::new(Vec3::ZERO, 1280.0, 720.0);
        camera.update();
        uiw.insert(camera);
        uiw.insert(OrbitCamera::from_camera(camera));
        uiw.write::<ToastState>().track_test_world(&test.sim);
        uiw.write::<NotificationCenter>().push(Notification {
            kind: NotificationKind::Blackout,
            severity: Severity::Error,
            tick: Tick(1),
            target: AlertTarget::Building(house),
            text: "An electrical network is in blackout".into(),
        });
        assert!(!uiw.read::<ToastState>().world_state_is_clear());
        apply_world_replacement(
            &uiw,
            &mut test.sim,
            &mut Simulation::schedule(),
            WorldReplacement::Snapshot(replacement.sim, SlotId::legacy()),
            |_| {},
        );
        assert_eq!(uiw.read::<NotificationCenter>().iter().len(), 0);
        assert!(uiw.read::<ToastState>().world_state_is_clear());
    }
}
