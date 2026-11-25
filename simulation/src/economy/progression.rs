use std::collections::{BTreeMap, BTreeSet};

use prototypes::{
    prototypes_iter, try_prototype, Density, GoodsCompanyID, GoodsCompanyPrototype, Money, Tick,
    TICKS_PER_HOUR,
};
use serde::{Deserialize, Serialize};

use crate::economy::{
    Budget, BudgetCategory, DailyEconomyMetrics, EconomyDayMetrics, TICKS_PER_BUDGET_DAY,
};
use crate::map_dynamic::{
    active_business, effective_load, BuildingInfos, ElectricityFlow, UtilityFlow, UtilityStatus,
};
use crate::souls::residence::Residences;
use crate::Simulation;

const MILESTONES: [(u32, Money); 3] = [
    (100, Money::new_bucks(10_000)),
    (250, Money::new_bucks(20_000)),
    (500, Money::new_bucks(30_000)),
];

const CAPABILITY_TIERS: [(BuildCapability, u8, &str); 3] = [
    (BuildCapability::BusService, 1, "Bus service"),
    (BuildCapability::ZoneDensity(Density::Medium), 1, "Medium density"),
    (BuildCapability::ZoneDensity(Density::High), 2, "High density"),
];

// Roads, low-density zoning, and the rail freight connector are ungated
// commands. Missing company names are skipped until their prototypes ship.
const PROTOTYPE_TIERS: &[(u8, &[&str])] = &[
    (
        0,
        &[
            "solar-panel",
            "coal-power-plant",
            "water-pump",
            "sewage-outlet",
            "fire-station",
            "clinic",
            "police-station",
            "school",
            "park",
            "garbage-depot",
        ],
    ),
    (2, &["small-depot"]),
    (3, &["large-warehouse"]),
];

pub const OBJECTIVE_REWARD: Money = Money::new_bucks(10_000);
const OBJECTIVES: [ObjectiveId; 2] = [ObjectiveId::PositiveBalance, ObjectiveId::StockedShops];

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressionMode {
    Guided,
    #[default]
    FreeBuild,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectiveId {
    PositiveBalance,
    StockedShops,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BuildCapability {
    Prototype(GoodsCompanyID),
    ZoneDensity(Density),
    BusService,
}

impl BuildCapability {
    /// Resolves a prototype name from a progression table against the loaded registry.
    pub fn prototype_from_name(name: &str) -> Option<Self> {
        let prototype = try_prototype(GoodsCompanyID::new(name))?;
        (prototype.name == name).then_some(Self::Prototype(prototype.id))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LockedUntil {
    pub milestone: u8,
    pub population: u32,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ProgressionState {
    pub mode: ProgressionMode,
    pub highest_milestone: u8,
    pub claimed: BTreeSet<ObjectiveId>,
    pub last_processed_day: Option<u64>,
    pub streaks: BTreeMap<ObjectiveId, u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObjectiveProgress {
    #[default]
    Unavailable,
    InProgress {
        completed: u32,
        required: u32,
    },
    Claimed,
}

#[derive(Debug)]
pub struct ProgressionStatus {
    pub mode: ProgressionMode,
    pub population: u32,
    pub highest_milestone: u8,
    pub next: Option<(u8, u32, Vec<String>)>,
    pub objectives: Vec<(ObjectiveId, ObjectiveProgress)>,
}

pub fn progression_status(sim: &Simulation) -> ProgressionStatus {
    let state = sim.read::<ProgressionState>();
    let population = population(sim);
    if state.mode == ProgressionMode::FreeBuild {
        return ProgressionStatus {
            mode: state.mode,
            population,
            highest_milestone: 0,
            next: None,
            objectives: Vec::new(),
        };
    }

    let milestone = state.highest_milestone.saturating_add(1);
    let next = milestone_population(milestone).map(|required| {
        let mut names: Vec<_> = CAPABILITY_TIERS
            .iter()
            .filter(|&&(_, tier, _)| tier == milestone)
            .map(|&(_, _, name)| name.to_owned())
            .collect();
        names.extend(
            prototypes_iter::<GoodsCompanyPrototype>()
                .filter(|proto| {
                    !proto.id.is_growable() && prototype_milestone(proto.id) == milestone
                })
                .map(|proto| proto.label.clone()),
        );
        (milestone, required, names)
    });
    let metrics = sim.read::<DailyEconomyMetrics>();
    let last_day = state
        .last_processed_day
        .and_then(|day| metrics.days.get(&day));
    let objectives = OBJECTIVES
        .map(|id| {
            let progress = if state.claimed.contains(&id) {
                ObjectiveProgress::Claimed
            } else if id == ObjectiveId::PositiveBalance
                || last_day.and_then(|day| objective_target(id, day)).is_some()
            {
                ObjectiveProgress::InProgress {
                    completed: state.streaks.get(&id).copied().unwrap_or(0),
                    required: objective_days(id),
                }
            } else {
                ObjectiveProgress::Unavailable
            };
            (id, progress)
        })
        .into();
    ProgressionStatus {
        mode: state.mode,
        population,
        highest_milestone: state.highest_milestone,
        next,
        objectives,
    }
}

fn population(sim: &Simulation) -> u32 {
    sim.world().humans.len() as u32
}

pub fn milestone_population(milestone: u8) -> Option<u32> {
    let index = usize::from(milestone.checked_sub(1)?);
    MILESTONES.get(index).map(|&(population, _)| population)
}

pub fn milestone_reward(milestone: u8) -> Option<Money> {
    let index = usize::from(milestone.checked_sub(1)?);
    MILESTONES.get(index).map(|&(_, reward)| reward)
}

fn prototype_milestone(id: GoodsCompanyID) -> u8 {
    for &(milestone, names) in PROTOTYPE_TIERS {
        if names.iter().any(|name| {
            BuildCapability::prototype_from_name(name) == Some(BuildCapability::Prototype(id))
        }) {
            return milestone;
        }
    }

    try_prototype(id).map_or(0, |proto| {
        if proto.storage.is_some() {
            3
        } else {
            proto.unlock_milestone.unwrap_or(0)
        }
    })
}

pub fn can_build(sim: &Simulation, cap: BuildCapability) -> Result<(), LockedUntil> {
    let state = sim.read::<ProgressionState>();
    if state.mode == ProgressionMode::FreeBuild {
        return Ok(());
    }
    let milestone = match cap {
        BuildCapability::Prototype(id) => prototype_milestone(id),
        _ => CAPABILITY_TIERS
            .iter()
            .find(|&&(capability, _, _)| capability == cap)
            .map_or(0, |&(_, milestone, _)| milestone),
    };
    if milestone <= state.highest_milestone {
        return Ok(());
    }

    let population_required = milestone_population(milestone).expect("valid milestone");
    if population(sim) >= population_required {
        Ok(())
    } else {
        Err(LockedUntil {
            milestone,
            population: population_required,
        })
    }
}

pub fn progression_update(sim: &mut Simulation) {
    let (mode, highest_milestone, last_processed_day) = {
        let state = sim.read::<ProgressionState>();
        (
            state.mode,
            state.highest_milestone,
            state.last_processed_day,
        )
    };
    if mode == ProgressionMode::FreeBuild {
        return;
    }

    let population = population(sim);
    for (index, &(required, reward)) in MILESTONES
        .iter()
        .enumerate()
        .skip(usize::from(highest_milestone))
    {
        if population < required {
            break;
        }
        let (_, resources) = sim.world_res();
        Budget::apply(resources, BudgetCategory::MilestoneGrant, reward);
        sim.write::<ProgressionState>().highest_milestone = index as u8 + 1;
    }

    let elapsed = sim.read::<Budget>().elapsed_ticks(Tick(sim.get_tick()));
    let closed_days = elapsed / TICKS_PER_BUDGET_DAY;
    // Each midpoint represents one elapsed budget hour. The closing tick only evaluates the day.
    if elapsed % TICKS_PER_HOUR == TICKS_PER_HOUR / 2 {
        sample_daily_utilities(sim, closed_days);
    }
    let first = last_processed_day.map_or(0, |day| day + 1);
    for day in first..closed_days {
        let claimed = evaluate_objectives(sim, day, elapsed);
        let (_, resources) = sim.world_res();
        for _ in 0..claimed {
            Budget::apply(resources, BudgetCategory::MilestoneGrant, OBJECTIVE_REWARD);
        }
    }
}

fn evaluate_objectives(sim: &Simulation, day: u64, elapsed: u64) -> usize {
    let metrics = sim.read::<DailyEconomyMetrics>();
    let closed = metrics.closed_day(day, elapsed);
    let budget = sim.read::<Budget>();
    let operating_balance: i128 = budget
        .days()
        .get(&day)
        .into_iter()
        .flatten()
        .filter(|(category, _)| {
            !matches!(
                category,
                BudgetCategory::Construction
                    | BudgetCategory::StarterInfrastructure
                    | BudgetCategory::MilestoneGrant
            )
        })
        .map(|(_, change)| *change)
        .sum();
    let mut state = sim.write::<ProgressionState>();
    let mut claimed = 0;
    for id in OBJECTIVES {
        if state.claimed.contains(&id) {
            continue;
        }
        let qualifies = closed.and_then(|day| objective_target(id, day)) == Some(true)
            && (id != ObjectiveId::PositiveBalance || operating_balance > 0);
        let streak = state.streaks.entry(id).or_default();
        *streak = if qualifies { *streak + 1 } else { 0 };
        if *streak == objective_days(id) {
            state.claimed.insert(id);
            claimed += 1;
        }
    }
    state.last_processed_day = Some(day);
    claimed
}

fn objective_days(id: ObjectiveId) -> u32 {
    match id {
        ObjectiveId::PositiveBalance => 2,
        ObjectiveId::StockedShops => 1,
    }
}

fn objective_target(id: ObjectiveId, day: &EconomyDayMetrics) -> Option<bool> {
    match id {
        ObjectiveId::PositiveBalance => {
            let served = day.utility_served_demand?;
            let demand = day.utility_total_demand?;
            (demand > 0.0).then_some(served >= 0.95 * demand)
        }
        ObjectiveId::StockedShops => {
            let due = u128::from(day.baskets_due?);
            let filled = u128::from(day.baskets_filled?);
            // The due cohort includes failed and expired baskets.
            Some(due >= 20 && filled * 10 >= due * 9)
        }
    }
}

fn sample_daily_utilities(sim: &Simulation, day: u64) {
    if let Some(fraction) = utility_served_fraction(sim) {
        let mut metrics = sim.write::<DailyEconomyMetrics>();
        let sample = metrics.days.entry(day).or_default();
        // Sum each hour's least-served fraction, with one equal weight per measured hour.
        *sample.utility_served_demand.get_or_insert(0.0) += fraction;
        *sample.utility_total_demand.get_or_insert(0.0) += 1.0;
    }
}

fn utility_served_fraction(sim: &Simulation) -> Option<f64> {
    let map = sim.map();
    let power = sim.read::<ElectricityFlow>();
    let water = sim.read::<UtilityFlow>();
    let residences = sim.read::<Residences>();
    let infos = sim.read::<BuildingInfos>();

    let mut power_demand = 0.0;
    let mut powered = 0.0;
    for network in map.electricity.networks() {
        let flow = power.network_stats(network.id);
        let demand = flow.consumed_power.watts().max(0) as f64;
        power_demand += demand;
        if !flow.blackout {
            powered += demand;
        }
    }

    let mut water_demand = 0.0;
    let mut water_served = 0.0;
    let mut sewage_served = 0.0;
    for building in map.buildings().values() {
        let business = building
            .development()
            .and_then(|_| active_business(building, &infos, sim.world()));
        let demand = effective_load(building, residences.by_building.get(&building.id), business)
            .water_demand();
        if demand == 0.0 {
            continue;
        }
        water_demand += demand;
        if let Some(status) = water.status_with_demand(&map, building.id, demand) {
            if status.water == UtilityStatus::Served {
                water_served += demand;
            }
            if status.sewage == UtilityStatus::Served {
                sewage_served += demand;
            }
        }
    }

    // Normalize each service's demand in its own units. The least-served utility
    // sets the result so a city needs adequate power, water and sewage together.
    [
        (powered, power_demand),
        (water_served, water_demand),
        (sewage_served, water_demand),
    ]
    .into_iter()
    .filter(|&(_, demand)| demand > 0.0)
    .map(|(served, demand)| served / demand)
    .reduce(f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::Government;
    use crate::map::{BuildingKind, LaneKind, ZoneKind};
    use crate::map_dynamic::Transit;
    use crate::souls::human::spawn_human;
    use crate::tests::TestCtx;
    use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
    use crate::{NewGameOptions, INITIAL_TICK};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::GameTime;

    fn guide(sim: &Simulation) {
        sim.write::<ProgressionState>().mode = ProgressionMode::Guided;
    }

    fn grants(sim: &Simulation) -> i128 {
        sim.read::<Budget>().total(BudgetCategory::MilestoneGrant)
    }

    fn bucks(amount: i64) -> i128 {
        i128::from(Money::new_bucks(amount).inner())
    }

    #[test]
    fn e3_milestone_grants_and_unlocks_survive_decline_and_reload() {
        let mut ctx = TestCtx::new();
        guide(&ctx.g);
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
        let house = ctx.build_house_near(vec2(200.0, 100.0));
        ctx.g
            .write::<Residences>()
            .by_building
            .get_mut(&house)
            .unwrap()
            .capacity = 500;
        let opening = ctx.g.read::<Government>().money;
        for _ in 0..100 {
            assert!(spawn_human(&mut ctx.g, house).is_some());
        }

        progression_update(&mut ctx.g);
        assert_eq!(grants(&ctx.g), bucks(10_000));
        assert_eq!(
            ctx.g.read::<Government>().money,
            opening + Money::new_bucks(10_000)
        );
        let mut restored: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();

        let mut removed = Vec::new();
        for id in restored.world().humans.keys().take(2).collect::<Vec<_>>() {
            removed.push(restored.world_mut_unchecked().humans.remove(id).unwrap());
        }
        progression_update(&mut restored);
        assert_eq!(restored.world().humans.len(), 98);
        assert_eq!(can_build(&restored, BuildCapability::BusService), Ok(()));
        assert_eq!(progression_status(&restored).next.unwrap().0, 2);
        for human in removed {
            restored.world_mut_unchecked().humans.insert(human);
        }
        progression_update(&mut restored);
        assert_eq!(grants(&restored), bucks(10_000));

        for _ in 100..500 {
            assert!(spawn_human(&mut ctx.g, house).is_some());
        }
        progression_update(&mut ctx.g);
        progression_update(&mut ctx.g);
        assert_eq!(ctx.g.read::<ProgressionState>().highest_milestone, 3);
        assert_eq!(grants(&ctx.g), bucks(60_000));
        assert!(progression_status(&ctx.g).next.is_none());
    }

    #[test]
    fn e3_objectives_use_consecutive_closed_days_and_ignore_clock_changes() {
        let mut sim = TestCtx::new().g;
        guide(&sim);
        assert_eq!(
            progression_status(&sim).objectives,
            [
                (
                    ObjectiveId::PositiveBalance,
                    ObjectiveProgress::InProgress { completed: 0, required: 2 },
                ),
                (ObjectiveId::StockedShops, ObjectiveProgress::Unavailable),
            ]
        );

        let record = |sim: &mut Simulation, day, revenue, served, due, filled| {
            let (_, resources) = sim.world_res();
            for (category, amount) in [
                (BudgetCategory::TradeExport, revenue),
                (BudgetCategory::RoadUpkeep, -100),
                (BudgetCategory::Construction, -500),
                (BudgetCategory::StarterInfrastructure, 500),
                (BudgetCategory::MilestoneGrant, 500),
            ] {
                Budget::apply(resources, category, Money::new_bucks(amount));
            }
            sim.write::<DailyEconomyMetrics>().days.insert(
                day,
                EconomyDayMetrics {
                    utility_served_demand: Some(served),
                    utility_total_demand: Some(100.0),
                    baskets_due: Some(due),
                    baskets_filled: Some(filled),
                    ..EconomyDayMetrics::default()
                },
            );
            progression_update(sim);
        };
        let close = |sim: &mut Simulation| {
            sim.write::<GameTime>().tick.0 += TICKS_PER_BUDGET_DAY - 1;
            progression_update(sim);
            sim.write::<GameTime>().tick.0 += 1;
        };
        let streak = |sim: &Simulation| {
            sim.read::<ProgressionState>().streaks[&ObjectiveId::PositiveBalance]
        };

        sim.write::<GameTime>().tick.0 = INITIAL_TICK + 1;
        record(&mut sim, 0, 101, 95.0, 19, 19);
        assert!(sim.read::<ProgressionState>().last_processed_day.is_none());
        close(&mut sim);
        assert_eq!(streak(&sim), 1);
        assert!(sim.read::<ProgressionState>().claimed.is_empty());

        for (day, revenue, served, filled) in [(1, 99, 100.0, 17), (2, 101, 94.0, 18)] {
            record(&mut sim, day, revenue, served, 20, filled);
            close(&mut sim);
            assert_eq!(streak(&sim), 0);
        }
        assert_eq!(
            sim.read::<ProgressionState>().claimed,
            BTreeSet::from([ObjectiveId::StockedShops])
        );
        assert_eq!(grants(&sim), bucks(11_500));

        record(&mut sim, 3, 101, 95.0, 20, 20);
        close(&mut sim);
        assert_eq!(streak(&sim), 1);
        let mut sim: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
        for tick in [0, INITIAL_TICK + 40 * TICKS_PER_BUDGET_DAY] {
            sim.apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(tick)))]);
            progression_update(&mut sim);
            assert_eq!(sim.read::<ProgressionState>().last_processed_day, Some(3));
            assert_eq!(streak(&sim), 1);
            assert_eq!(grants(&sim), bucks(12_000));
        }

        record(&mut sim, 4, 101, 95.0, 20, 20);
        close(&mut sim);
        assert_eq!(streak(&sim), 2);
        assert_eq!(sim.read::<ProgressionState>().claimed.len(), 2);
        assert_eq!(grants(&sim), bucks(22_500));
        progression_update(&mut sim);
        assert_eq!(grants(&sim), bucks(22_500));
    }

    #[test]
    fn e3_commands_enforce_named_capabilities_only_in_guided_mode() {
        drop(TestCtx::new());
        // Names a table may list before their prototypes ship resolve to nothing.
        for name in ["future-depot", "regional-freight-hub", ""] {
            assert_eq!(BuildCapability::prototype_from_name(name), None, "{name}");
        }
        let id = GoodsCompanyID::new("small-depot");
        let warehouse = GoodsCompanyID::new("large-warehouse");
        assert_eq!(
            BuildCapability::prototype_from_name("small-depot"),
            Some(BuildCapability::Prototype(id))
        );
        assert_eq!(
            BuildCapability::prototype_from_name("large-warehouse"),
            Some(BuildCapability::Prototype(warehouse))
        );
        let empty = Simulation::new_game(&NewGameOptions {
            progression: ProgressionMode::Guided,
            terrain_chunks: 16,
            ..NewGameOptions::default()
        })
        .unwrap();
        assert_eq!(
            progression_status(&empty).next.unwrap().2,
            ["Bus service", "Medium density"]
        );

        let mut sim = Simulation::new_game(&NewGameOptions {
            preset: crate::MapPreset::Flat,
            progression: ProgressionMode::Guided,
            terrain_chunks: 16,
            ..NewGameOptions::default()
        })
        .unwrap();
        let [a, b, c, d] = [
            vec3(400.0, 400.0, 0.0),
            vec3(600.0, 400.0, 0.0),
            vec3(600.0, 600.0, 0.0),
            vec3(400.0, 600.0, 0.0),
        ];
        sim.apply_commands(
            [a, b, c, d]
                .into_iter()
                .zip([b, c, d, a])
                .map(|(from, to)| WorldCommand::MapMakeConnection {
                    from: crate::map::MapProject::ground(from),
                    to: crate::map::MapProject::ground(to),
                    inter: None,
                    pat: crate::map::LanePatternBuilder::default().build(),
                })
                .collect::<Vec<_>>()
                .iter(),
        );
        let stops = {
            let map = sim.map();
            let lane = map
                .lanes()
                .values()
                .find(|lane| {
                    lane.kind == LaneKind::Driving
                        && lane.points.middle().xy().is_close(vec2(500.0, 400.0), 20.0)
                })
                .unwrap();
            [0.3, 0.7].map(|fraction| {
                let (pos, dir) = lane.points.point_dir_along(lane.points.length() * fraction);
                WorldCommand::TransitAddStop { pos, dir: dir.xy() }
            })
        };
        sim.apply_commands(&stops);
        assert_eq!(sim.read::<CommandOutcomes>().accepted.len(), 2);
        let size = id.prototype().size;
        let depot = WorldCommand::MapBuildSpecialBuilding {
            pos: OBB::new(vec2(520.0, 470.0), Vec2::X, size.w, size.h),
            kind: BuildingKind::GoodsCompany(id),
            gen: id.prototype().bgen,
            zone: None,
            connected_road: None,
        };
        let paint = WorldCommand::ZonePaint {
            area: OBB::new(vec2(520.0, 540.0), Vec2::X, 16.0, 16.0),
            kind: Some(ZoneKind::Residential),
            density: Density::Medium,
        };
        let stops = sim.read::<Transit>().stops.keys().collect();
        let line = WorldCommand::TransitCreateLine { stops };
        let commands = [depot, paint, line];
        let before = sim.hashes();
        sim.apply_commands(&commands);
        assert_eq!(
            sim.read::<CommandOutcomes>().last,
            [
                (
                    "MapBuildSpecialBuilding",
                    Rejection::Locked { milestone: 2 }
                ),
                ("ZonePaint", Rejection::Locked { milestone: 1 }),
                ("TransitCreateLine", Rejection::Locked { milestone: 1 }),
            ]
        );
        assert_eq!(sim.hashes(), before);

        for name in [
            "solar-panel",
            "coal-power-plant",
            "water-pump",
            "sewage-outlet",
            "fire-station",
            "clinic",
            "police-station",
            "school",
            "park",
            "garbage-depot",
        ] {
            assert_eq!(
                can_build(&sim, BuildCapability::prototype_from_name(name).unwrap()),
                Ok(())
            );
        }
        // Each real warehouse opens at its own milestone.
        for (highest, depot_lock, warehouse_lock, next) in [
            (0, Some(2), Some(3), None),
            (1, Some(2), Some(3), Some("Small depot")),
            (2, None, Some(3), Some("Large warehouse")),
            (3, None, None, None),
        ] {
            sim.write::<ProgressionState>().highest_milestone = highest;
            let lock = |proto: GoodsCompanyID| {
                can_build(&sim, BuildCapability::Prototype(proto))
                    .err()
                    .map(|locked| locked.milestone)
            };
            assert_eq!(lock(id), depot_lock, "milestone {highest}");
            assert_eq!(lock(warehouse), warehouse_lock, "milestone {highest}");
            if let Some(label) = next {
                assert!(progression_status(&sim)
                    .next
                    .unwrap()
                    .2
                    .iter()
                    .any(|name| name == label));
            }
        }

        sim.write::<ProgressionState>().mode = ProgressionMode::FreeBuild;
        sim.apply_commands(&commands);
        assert_eq!(sim.read::<CommandOutcomes>().last, []);
        assert_eq!(sim.read::<CommandOutcomes>().accepted.len(), 3);
    }
}
