use std::collections::{BTreeMap, VecDeque};

use prototypes::{DayTime, GameTime, ItemID, Money, Tick};
use simulation::diagnostics::elapsed_tick;
use simulation::economy::progression::{milestone_population, milestone_reward, ProgressionState};
use simulation::economy::{
    balance::{DEMAND_INTERVAL_TICKS, MIN_DEMAND_POPULATION},
    Budget, BudgetCategory, Government, ZoneDemand,
};
use simulation::map::{BuildingID, ZoneKind};
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
    MilestoneReached,
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
            Self::MilestoneReached => "Milestone reached",
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
            Self::MilestoneReached | Self::StalledDemand => Severity::Info,
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
    tick: Tick,
    pub daytime: DayTime,
    source: Option<AlertSource>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum AlertSource {
    Line(LineID),
    CityWide,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AlertKey {
    kind: NotificationKind,
    source: AlertSource,
}

#[derive(Clone, Copy)]
struct SampleTime {
    elapsed: Tick,
    calendar: GameTime,
}

impl SampleTime {
    fn daytime_at(self, tick: Tick) -> DayTime {
        // Fire events use elapsed ticks, which are independent of clock changes.
        let age = self.elapsed.0.saturating_sub(tick.0);
        if age == 0 {
            self.calendar.daytime
        } else {
            GameTime::new(Tick(self.calendar.tick.0.saturating_sub(age))).daytime
        }
    }
}

struct ActiveAlert {
    started: Tick,
    last_seen: Tick,
    target: AlertTarget,
    text: String,
}

#[derive(Default)]
struct NetworkAlert {
    first: Option<BuildingID>,
    count: usize,
    awaiting_refresh: bool,
}

impl NetworkAlert {
    fn add_affected(&mut self, id: BuildingID) {
        self.first.get_or_insert(id);
        self.count += 1;
    }

    fn record_status(&mut self, id: BuildingID, status: UtilityStatus) {
        match status {
            UtilityStatus::Shortage => self.add_affected(id),
            UtilityStatus::Unknown => self.awaiting_refresh = true,
            UtilityStatus::Served | UtilityStatus::Disconnected => {}
        }
    }
}

/// The HUD observes even while hidden. The next visible frame publishes the
/// bounded backlog to history and uses those notifications for the toast UI.
#[derive(Default)]
pub(super) struct NotificationSampler {
    pending: VecDeque<Notification>,
    last_batch_seq: Option<u64>,
    fire_cursor: Option<u64>,
    last_milestone: Option<u8>,
    last_sampled_tick: Option<Tick>,
    active: BTreeMap<AlertKey, ActiveAlert>,
    operating_balance: Option<i128>,
    operating_losses: u8,
    stalled_samples: u8,
}

impl NotificationSampler {
    pub(super) fn observe(&mut self, sim: &Simulation) {
        let tick = elapsed_tick(sim);
        let now = SampleTime {
            elapsed: tick,
            calendar: *sim.read::<GameTime>(),
        };
        self.observe_rejections(sim, now);
        self.observe_fires(sim, now);
        let milestone = sim.read::<ProgressionState>().highest_milestone;
        if let Some(previous) = self.last_milestone.replace(milestone) {
            for reached in previous.saturating_add(1)..=milestone {
                if let Some(grant) = milestone_reward(reached) {
                    self.emit(
                        NotificationKind::MilestoneReached,
                        now.elapsed,
                        now.calendar.daytime,
                        None,
                        AlertTarget::CityWide,
                        format!("Milestone {reached} reached · {grant} awarded"),
                    );
                }
            }
        }
        if self
            .last_sampled_tick
            .is_none_or(|previous| tick.0.saturating_sub(previous.0) >= SAMPLE_INTERVAL)
        {
            self.last_sampled_tick = Some(tick);
            self.sample(sim, now);
            // The sampling interval gives recovered conditions a cooldown before
            // they can recur. Resolved or deleted sources keep no state.
            self.active.retain(|_, active| active.last_seen == tick);
        }
    }

    pub(super) fn drain(&mut self) -> impl ExactSizeIterator<Item = Notification> + '_ {
        self.pending.drain(..)
    }

    pub(super) fn details<'a>(&'a self, notice: &'a Notification) -> (&'a str, AlertTarget) {
        if let Some(source) = notice.source {
            let key = AlertKey {
                kind: notice.kind,
                source,
            };
            if let Some(active) = self
                .active
                .get(&key)
                .filter(|active| active.started == notice.tick)
            {
                return (&active.text, active.target);
            }
        }
        (&notice.text, notice.target)
    }

    #[cfg(test)]
    pub(super) fn is_clear(&self) -> bool {
        self.pending.is_empty()
            && self.last_batch_seq.is_none()
            && self.fire_cursor.is_none()
            && self.last_milestone.is_none()
            && self.last_sampled_tick.is_none()
            && self.active.is_empty()
            && self.operating_balance.is_none()
            && self.operating_losses == 0
            && self.stalled_samples == 0
    }

    fn emit(
        &mut self,
        kind: NotificationKind,
        tick: Tick,
        daytime: DayTime,
        source: Option<AlertSource>,
        target: AlertTarget,
        text: String,
    ) {
        push_bounded(
            &mut self.pending,
            Notification {
                kind,
                severity: kind.severity(),
                tick,
                daytime,
                source,
                target,
                text,
            },
        );
    }

    fn observe_rejections(&mut self, sim: &Simulation, now: SampleTime) {
        let outcomes = sim.read::<CommandOutcomes>();
        let sequence = outcomes.batch_seq();
        let previous = self.last_batch_seq.replace(sequence);
        if previous == Some(sequence) {
            return;
        }
        let mut reasons = Vec::new();
        for &(command, reason) in &outcomes.last {
            if !reasons.contains(&(command, reason)) {
                reasons.push((command, reason));
                self.emit(
                    NotificationKind::CommandRejected,
                    now.elapsed,
                    now.calendar.daytime,
                    None,
                    AlertTarget::CityWide,
                    format!(
                        "{}: {}",
                        command_label(command),
                        command_message(command, reason)
                    ),
                );
            }
        }
    }

    fn observe_fires(&mut self, sim: &Simulation, now: SampleTime) {
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
                    now.daytime_at(event.tick),
                    None,
                    AlertTarget::Building(event.building),
                    text.into(),
                );
            }
        } else if let Some((&building, _)) = fires.by_building.first_key_value() {
            let count = fires.by_building.len();
            let (target, text) = if count == 1 {
                (
                    AlertTarget::Building(building),
                    "A building was on fire".into(),
                )
            } else {
                (
                    AlertTarget::CityWide,
                    format!("{count} buildings were on fire"),
                )
            };
            self.emit(
                NotificationKind::Fire,
                now.elapsed,
                now.calendar.daytime,
                None,
                target,
                text,
            );
        }
    }

    fn condition(
        &mut self,
        kind: NotificationKind,
        source: AlertSource,
        now: SampleTime,
        target: AlertTarget,
        live_text: String,
        history_text: impl FnOnce() -> String,
    ) {
        let key = AlertKey { kind, source };
        if let Some(active) = self.active.get_mut(&key) {
            active.last_seen = now.elapsed;
            active.target = target;
            active.text = live_text;
            return;
        }
        self.active.insert(
            key,
            ActiveAlert {
                started: now.elapsed,
                last_seen: now.elapsed,
                target,
                text: live_text,
            },
        );
        self.emit(
            kind,
            now.elapsed,
            now.calendar.daytime,
            Some(source),
            target,
            history_text(),
        );
    }

    fn sample(&mut self, sim: &Simulation, now: SampleTime) {
        use AlertSource::CityWide;
        use NotificationKind::*;

        let treasury = sim.read::<Government>().money;
        if treasury < Money::ZERO {
            self.condition(
                NegativeBudget,
                CityWide,
                now,
                AlertTarget::CityWide,
                format!("The treasury balance is {treasury}"),
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
            self.condition(
                BudgetDeficit,
                CityWide,
                now,
                AlertTarget::CityWide,
                "Operating expenses have repeatedly exceeded income".into(),
                || "Operating expenses repeatedly exceeded income".into(),
            );
        }

        let humans = &sim.world().humans;
        let population = humans.len();
        let jobs = ItemID::new("job-opening");
        let unemployed = humans
            .values()
            .filter(|human| {
                human.work.is_none() && human.bought.0.get(&jobs).is_none_or(Vec::is_empty)
            })
            .count();
        if population >= MIN_DEMAND_POPULATION
            && unemployed * 100 >= population * UNEMPLOYMENT_PERCENT
        {
            self.condition(
                HighUnemployment,
                CityWide,
                now,
                AlertTarget::CityWide,
                format!("{unemployed} of {population} citizens are unemployed"),
                || format!("{unemployed} of {population} citizens were unemployed"),
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
            self.condition(
                StalledDemand,
                CityWide,
                now,
                AlertTarget::CityWide,
                "Demand has stalled across all zone types".into(),
                || "Demand stalled across all zone types".into(),
            );
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
                now,
                AlertTarget::Line(line.id),
                format!(
                    "Transit line {} is suspended",
                    line.id.data().as_ffi() as u32
                ),
                || {
                    format!(
                        "Transit line {} suspended service",
                        line.id.data().as_ffi() as u32
                    )
                },
            );
        }
        self.sample_networks(sim, now);
    }

    fn sample_networks(&mut self, sim: &Simulation, now: SampleTime) {
        use NotificationKind::*;

        let map = sim.map();
        let power = sim.read::<ElectricityFlow>();
        let utilities = sim.read::<UtilityFlow>();
        let residences = sim.read::<Residences>();
        let mut blackout = NetworkAlert::default();
        let mut water = NetworkAlert::default();
        let mut sewage = NetworkAlert::default();
        for network in map.electricity.networks() {
            let in_blackout = power.blackout(network.id);
            for &id in &network.buildings {
                if map.get(id).is_none_or(|building| building.is_orphaned()) {
                    continue;
                }
                if in_blackout {
                    blackout.add_affected(id);
                }
                if let Some(status) = utilities.status(&map, &residences, id) {
                    water.record_status(id, status.water);
                    sewage.record_status(id, status.sewage);
                }
            }
        }
        for (kind, alert, issue) in [
            (Blackout, blackout, "power"),
            (NoWater, water, "water"),
            (NoSewage, sewage, "sewage service"),
        ] {
            // The graph ID is the lowest member and can change while an outage persists.
            let source = AlertSource::CityWide;
            if let Some(building) = alert.first {
                let count = alert.count;
                let (buildings, verb) = if count == 1 {
                    ("building", "lacks")
                } else {
                    ("buildings", "lack")
                };
                self.condition(
                    kind,
                    source,
                    now,
                    AlertTarget::Building(building),
                    format!("{count} {buildings} {verb} {issue}"),
                    || format!("{count} {buildings} lacked {issue}"),
                );
            } else if alert.awaiting_refresh {
                // Utility totals can lag a topology edit until the next utility pass.
                if let Some(active) = self.active.get_mut(&AlertKey { kind, source }) {
                    active.last_seen = now.elapsed;
                }
            }
        }
    }
}

fn command_label(command: &str) -> &'static str {
    match command {
        "Init" => "Create city",
        "MapRemoveIntersection" => "Demolish intersection",
        "MapRemoveRoad" => "Demolish road",
        "MapRemoveBuilding" => "Demolish building",
        "MapBuildHouse" => "Build house",
        "Terraform" => "Edit terrain",
        "SendMessage" => "Send message",
        "SpawnRandomCars" => "Spawn cars",
        "AddTrain" | "SpawnTrain" => "Place train",
        "MapMakeConnection" | "MapMakeMultipleConnections" => "Build road or track",
        "MapUpdateIntersectionPolicy" => "Edit intersection",
        "MapBuildSpecialBuilding" => "Place building",
        "MapLoadParis" | "MapLoadTestField" => "Load map",
        "UpdateZone" => "Resize building zone",
        "SetGameTime" => "Set game clock",
        "ZonePaint" => "Paint zoning",
        "SetTaxRate" => "Set tax rate",
        "TransitAddStop" => "Add bus stop",
        "TransitRemoveStop" => "Remove bus stop",
        "TransitCreateLine" => "Create bus line",
        "TransitEditLine" => "Edit bus line",
        "TransitRemoveLine" => "Remove bus line",
        "TransitSetVehicles" => "Set bus fleet size",
        "TransitSetFare" => "Set bus fare",
        _ => "City action",
    }
}

fn command_message(command: &str, reason: Rejection) -> std::borrow::Cow<'static, str> {
    use Rejection::*;
    if let Locked { milestone } = reason {
        if let Some(population) = milestone_population(milestone) {
            return format!("Unlocks at {population} citizens").into();
        }
    }
    let message = match (command, reason) {
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
        (_, Locked { .. }) => "Locked by city progression",
        (_, InvalidWarehouse) => "Choose an active warehouse",
    };
    message.into()
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
            daytime: GameTime::new(Tick(tick)).daytime,
            source: None,
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
    use simulation::map_dynamic::{electricity_flow_system, utility_flow_update};
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
    fn p4g_first_observation_reports_the_first_rejected_batch() {
        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        test.sim
            .apply_commands(&[WorldCommand::MapRemoveBuilding(BuildingID::default())]);
        let mut sampler = NotificationSampler::default();
        sampler.observe(&test.sim);
        sampler.observe(&test.sim);
        let notices = sampler.drain().collect::<Vec<_>>();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].kind, NotificationKind::CommandRejected);
        assert_eq!(
            notices[0].text,
            "Demolish building: Object no longer exists"
        );
    }

    #[test]
    fn af1_sustained_water_alert_refreshes_its_count_without_a_second_toast() {
        let (mut test, _) = city();
        let mut sampler = NotificationSampler::default();
        let mut center = NotificationCenter::default();
        let mut sample = |test: &mut ScenarioCtx, tick| {
            *test.sim.write::<GameTime>() = GameTime::new(Tick(tick));
            let (world, resources) = test.sim.world_res();
            electricity_flow_system(world, resources);
            utility_flow_update(&mut test.sim);
            sampler.observe(&test.sim);
            let notices = sampler.drain().collect::<Vec<_>>();
            let toasts = notices
                .iter()
                .filter(|n| n.kind == NotificationKind::NoWater)
                .count();
            for notice in notices {
                center.push(notice);
            }
            let notice = center
                .iter()
                .find(|n| n.kind == NotificationKind::NoWater)
                .unwrap();
            let (text, target) = sampler.details(notice);
            assert!(matches!(target, AlertTarget::Building(_)));
            assert_eq!(
                notice.daytime,
                GameTime::new(Tick(SAMPLE_INTERVAL + 1)).daytime
            );
            (toasts, text.to_owned())
        };
        assert_eq!(
            sample(&mut test, SAMPLE_INTERVAL + 1),
            (1, "1 building lacks water".into())
        );
        fill_houses(&mut test, 1);
        assert_eq!(
            sample(&mut test, 2 * SAMPLE_INTERVAL + 1),
            (0, "2 buildings lack water".into())
        );
    }

    #[test]
    fn p4g_removing_building_3v1_preserves_sustained_network_alerts() {
        let (mut test, first_house) = city();
        let other_house = fill_houses(&mut test, 1)[0];
        assert_eq!(first_house.data().as_ffi(), (1 << 32) | 3);
        let network = test.sim.map().electricity.net_id(first_house).unwrap();
        assert_eq!(
            test.sim.map().electricity.net_id(other_house),
            Some(network)
        );

        let sample = |test: &mut ScenarioCtx, sampler: &mut NotificationSampler, tick| {
            *test.sim.write::<GameTime>() = GameTime::new(Tick(tick));
            let (world, resources) = test.sim.world_res();
            electricity_flow_system(world, resources);
            utility_flow_update(&mut test.sim);
            let status = test
                .sim
                .read::<UtilityFlow>()
                .status(&test.sim.map(), &test.sim.read::<Residences>(), other_house)
                .unwrap();
            assert_eq!(status.water, UtilityStatus::Shortage);
            assert_eq!(status.sewage, UtilityStatus::Shortage);
            assert_eq!(status.totals.unwrap().sewage_demand, 0.0);
            sampler.observe(&test.sim);
            sampler
                .drain()
                .filter_map(|notice| match notice.kind {
                    NotificationKind::Blackout
                    | NotificationKind::NoWater
                    | NotificationKind::NoSewage => Some(notice.kind),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let mut sampler = NotificationSampler::default();
        let first = sample(&mut test, &mut sampler, SAMPLE_INTERVAL + 1);
        test.sim
            .apply_commands(&[WorldCommand::MapRemoveBuilding(first_house)]);
        assert_ne!(
            test.sim.map().electricity.net_id(other_house),
            Some(network)
        );
        assert!(sample(&mut test, &mut sampler, 2 * SAMPLE_INTERVAL + 1).is_empty());
        assert_eq!(
            first,
            [
                NotificationKind::Blackout,
                NotificationKind::NoWater,
                NotificationKind::NoSewage,
            ]
        );
    }

    #[test]
    fn p4g_initial_fires_are_coalesced() {
        let (mut test, house) = city();
        let other_house = fill_houses(&mut test, 1)[0];
        test.sim
            .write::<Fires>()
            .by_building
            .extend([(house, Tick(1)), (other_house, Tick(1))]);
        let mut sampler = NotificationSampler::default();
        sampler.observe(&test.sim);
        sampler.observe(&test.sim);
        let fires = sampler
            .drain()
            .filter(|notice| notice.kind == NotificationKind::Fire)
            .collect::<Vec<_>>();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].target, AlertTarget::CityWide);
    }

    #[test]
    fn p4g_pending_hires_are_excluded_from_unemployment_alerts() {
        use simulation::economy::{Trade, TradeTarget};
        use simulation::souls::human::spawn_human;
        use simulation::SoulID;

        let (mut test, _) = city();
        let houses = fill_houses(&mut test, MIN_DEMAND_POPULATION);
        assert_eq!(houses.len(), MIN_DEMAND_POPULATION);
        let jobs = ItemID::new("job-opening");
        for house in houses {
            let id = spawn_human(&mut test.sim, house).unwrap();
            let target = TradeTarget(SoulID::Human(id));
            let human = test.sim.world_mut_unchecked().humans.get_mut(id).unwrap();
            human.bought.0.insert(
                jobs,
                vec![Trade {
                    buyer: target,
                    seller: target,
                    qty: 1,
                    kind: jobs,
                    money_delta: Money::ZERO,
                }],
            );
        }
        let mut sampler = NotificationSampler::default();
        sampler.observe(&test.sim);
        assert!(!sampler
            .drain()
            .any(|notice| notice.kind == NotificationKind::HighUnemployment));
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
            daytime: GameTime::new(Tick(1)).daytime,
            source: None,
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
