use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicUsize, Ordering};

use geom::vec2;
use prototypes::{Money, Tick, TICKS_PER_HOUR};
use simulation::economy::{Budget, BudgetCategory, TransitStats, TripStats, TICKS_PER_BUDGET_DAY};
use simulation::map::{BuildingID, LaneKind};
use simulation::map_dynamic::transit::GIVE_UP_TICKS;
use simulation::map_dynamic::{Destination, LineID, RoutingStep, Transit};
use simulation::souls::desire::WorkKind;
use simulation::transportation::Location;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{HumanID, Simulation, SimulationOptions};

use crate::{Scenario, ScenarioCtx};

pub const SCENARIO: (Scenario, SimulationOptions) = (
    Scenario {
        name: "transit_basic",
        run,
    },
    super::growth_basic::SCENARIO.1,
);

// Commute durations are agent movement: cap at 12 real minutes.
const COMMUTE_TIMEOUT: u64 = 12 * 60 * prototypes::TICKS_PER_REALTIME_SECOND;
const FLEET_SIZE: u8 = 3;
// Changes in jobs and fire losses can shift which departures have a match.
const MAX_ALL_ATTEMPT_COMMUTE_RATIO: f64 = 1.05;

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    compare_branches(ctx).map(|_| ())
}

fn compare_branches(ctx: &mut ScenarioCtx) -> Result<Simulation, String> {
    (super::growth_basic::SCENARIO.0.run)(ctx)?;
    let start_tick = ctx.sim.get_tick();
    let start = elapsed_tick(&mut ctx.sim);
    let mut branches = fork(&ctx.sim, 2)?;
    ctx.sim = branches.pop().unwrap();
    let mut with_line = Commutes::new(&ctx.sim);
    let mut without_line = with_line.clone();
    let line = install_loop(&mut ctx.sim)?;

    let mut control = ScenarioCtx::new(SCENARIO.1);
    control.sim = branches.pop().unwrap();
    let control_start = totals(&control.sim);
    for now in start + 1..=start + TICKS_PER_BUDGET_DAY {
        control.run_ticks(1);
        without_line.observe(&control.sim, now, true);
    }
    let control_totals = totals(&control.sim).since(control_start);
    let control_sample = control.sample("without_transit");
    ctx.samples.push(control_sample);
    without_line.finish(&mut control, start + TICKS_PER_BUDGET_DAY);

    let transit_start = totals(&ctx.sim);
    let mut resume = None;
    let mut running_buses = 0;
    let mut bus_ticks = 0;
    let mut settled_bus_ticks = 0;
    for day_tick in 1..=TICKS_PER_BUDGET_DAY {
        ctx.run_ticks(1);
        let now = start + day_tick;
        with_line.observe(&ctx.sim, now, true);
        let buses = ctx.sim.read::<Transit>().runs.len();
        running_buses = running_buses.max(buses);
        bus_ticks += buses as u64;
        if now % TICKS_PER_HOUR == 0 {
            settled_bus_ticks = bus_ticks;
        }
        if resume.is_none()
            && day_tick < TICKS_PER_BUDGET_DAY
            && occupied_and_waiting(&ctx.sim, line)
        {
            resume = Some((day_tick, fork(&ctx.sim, 1)?.pop().unwrap()));
        }
    }
    let transit = totals(&ctx.sim).since(transit_start);
    let (boards, max_wait) = {
        let stats = ctx.sim.read::<TransitStats>();
        (
            stats.boardings.get(&line).copied().unwrap_or_default(),
            stats.max_wait_ticks,
        )
    };
    let day_end_hashes = ctx.sim.hashes();
    ctx.sample("with_transit");
    let transit_sample = ctx.samples.len() - 1;
    with_line.finish(ctx, start + TICKS_PER_BUDGET_DAY);
    let comparison = without_line.compare(&with_line)?;
    let [commute_a, commute_b] = comparison.means;
    let [matched_a, matched_b] = comparison.matched_means;
    let [missing_a, missing_b] = comparison.missing;
    let [pending_a, pending_b] = comparison.pending;
    let [expired_a, expired_b] = comparison.expired;
    let matched = comparison.matched;
    let wait_attempts = boards + transit.give_ups;
    let expected_upkeep = -i128::from(settled_bus_ticks) * i128::from(Money::new_bucks(25).inner())
        / i128::from(TICKS_PER_BUDGET_DAY);
    let summary = format!(
        "bus trips={} car trips={}/{} carless commute={commute_b:.1}/{commute_a:.1} ticks (matched={matched}, matched means={matched_b:.1}/{matched_a:.1}, missing={missing_b}/{missing_a}, pending={pending_b}/{pending_a}, expired={expired_b}/{expired_a}) boardings={boards} max wait={max_wait} give-ups={}/{wait_attempts} upkeep={}/{} (inner units)",
        transit.bus, transit.car, control_totals.car, transit.give_ups, transit.upkeep, expected_upkeep,
    );
    if running_buses != usize::from(FLEET_SIZE)
        || transit.bus < 50
        || transit.car >= control_totals.car
        || commute_b > commute_a * MAX_ALL_ATTEMPT_COMMUTE_RATIO
        || matched_b >= matched_a
        || boards == 0
        || boards < transit.bus
        || max_wait == 0
        || max_wait >= GIVE_UP_TICKS
        || transit.give_ups * 20 > wait_attempts
        || transit.upkeep >= 0
        || transit.upkeep != expected_upkeep
    {
        return Err(format!(
            "transit gate failed with {running_buses} buses ({summary})"
        ));
    }
    let (resume_tick, saved) = resume.ok_or("no checkpoint had both riders and a stop queue")?;
    let mut resumed = ScenarioCtx::new(SCENARIO.1);
    resumed.sim = saved;
    resumed.run_ticks(TICKS_PER_BUDGET_DAY - resume_tick);
    if resumed.sim.hashes() != day_end_hashes {
        return Err(format!(
            "transit continuation diverged after tick {}",
            start_tick + resume_tick
        ));
    }
    println!(
        "transit_basic: {summary} resumed from tick {}",
        start_tick + resume_tick
    );
    ctx.samples[transit_sample].extra.extend([
        ("bus_trips".into(), transit.bus as f64),
        ("car_trips".into(), transit.car as f64),
        (
            "car_trips_without_transit".into(),
            control_totals.car as f64,
        ),
        ("matched_carless_commutes".into(), matched as f64),
        ("mean_matched_carless_commute_ticks".into(), matched_b),
        (
            "mean_matched_carless_commute_ticks_without_transit".into(),
            matched_a,
        ),
        ("missing_carless_commutes".into(), missing_b as f64),
        (
            "missing_carless_commutes_without_transit".into(),
            missing_a as f64,
        ),
        ("pending_carless_commutes".into(), pending_b as f64),
        (
            "pending_carless_commutes_without_transit".into(),
            pending_a as f64,
        ),
        ("expired_carless_commutes".into(), expired_b as f64),
        (
            "expired_carless_commutes_without_transit".into(),
            expired_a as f64,
        ),
        ("mean_carless_commute_ticks".into(), commute_b),
        (
            "mean_carless_commute_ticks_without_transit".into(),
            commute_a,
        ),
        ("max_wait_ticks".into(), max_wait as f64),
        ("boardings".into(), boards as f64),
        ("transit_upkeep".into(), transit.upkeep as f64),
        ("give_ups".into(), transit.give_ups as f64),
        ("wait_attempts".into(), wait_attempts as f64),
    ]);
    Ok(control.sim)
}

fn elapsed_tick(sim: &mut Simulation) -> u64 {
    let (_, resources) = sim.unrestricted_world_res().unwrap();
    simulation::map_dynamic::transit::elapsed_ticks(resources).0
}

fn install_loop(sim: &mut Simulation) -> Result<LineID, String> {
    // Serve the residential and employment blocks on a short one-way loop.
    let legs = [
        ((1, 1), (2, 1)),
        ((3, 1), (4, 1)),
        ((4, 2), (4, 3)),
        ((4, 3), (3, 3)),
        ((2, 3), (1, 3)),
        ((1, 3), (1, 2)),
    ];
    let commands = {
        let map = sim.map();
        let junction = |(x, y): (u32, u32)| {
            let point = vec2(704.0 + 100.0 * x as f32, 704.0 + 100.0 * y as f32);
            map.intersections()
                .values()
                .find(|inter| inter.pos.xy().is_close(point, 1.0))
                .map(|inter| inter.id)
        };
        legs.into_iter()
            .map(|(src, dst)| {
                let lane = junction(src).zip(junction(dst)).and_then(|(src, dst)| {
                    let road = map
                        .find_road(src, dst)
                        .or_else(|| map.find_road(dst, src))?;
                    map.roads()[road].lanes_iter().find_map(|(id, kind)| {
                        let lane = &map.lanes()[id];
                        (kind == LaneKind::Driving && lane.src == src && lane.dst == dst)
                            .then_some(lane)
                    })
                });
                let lane =
                    lane.ok_or_else(|| format!("missing directed stop lane {src:?} to {dst:?}"))?;
                let (pos, dir) = lane.points.point_dir_along(lane.points.length() / 2.0);
                Ok(WorldCommand::TransitAddStop { pos, dir: dir.xy() })
            })
            .collect::<Result<Vec<_>, String>>()?
    };

    let mut stops = Vec::with_capacity(commands.len());
    for command in commands {
        sim.apply_commands([&command]);
        check_commands(sim)?;
        let stop = sim
            .read::<Transit>()
            .stops
            .keys()
            .find(|id| !stops.contains(id))
            .ok_or("accepted stop command did not create a stop")?;
        stops.push(stop);
    }
    sim.apply_commands([&WorldCommand::TransitCreateLine { stops }]);
    check_commands(sim)?;
    let line = sim
        .read::<Transit>()
        .lines
        .keys()
        .next()
        .ok_or("line was not created")?;
    sim.apply_commands([
        &WorldCommand::TransitSetVehicles {
            line,
            n: FLEET_SIZE,
        },
        &WorldCommand::TransitSetFare {
            line,
            fare: Money::ZERO,
        },
    ]);
    check_commands(sim)?;
    Ok(line)
}

fn check_commands(sim: &Simulation) -> Result<(), String> {
    let outcomes = sim.read::<CommandOutcomes>();
    if outcomes.last.is_empty() {
        Ok(())
    } else {
        Err(format!("transit command rejected: {:?}", outcomes.last))
    }
}

fn occupied_and_waiting(sim: &Simulation, line: LineID) -> bool {
    let mut occupied = false;
    let mut waiting = false;
    for human in sim.world().humans.values() {
        occupied |= matches!(human.location, Location::Transit(_));
        waiting |= matches!(human.router.cur_step(), Some(RoutingStep::WaitAtStop { line: id, .. }) if *id == line);
        if occupied && waiting {
            return true;
        }
    }
    false
}

fn fork(sim: &Simulation, count: usize) -> Result<Vec<Simulation>, String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = simulation::save_dir();
    fs::create_dir_all(root).map_err(|err| err.to_string())?;
    let (directory, path) = loop {
        let run = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = format!("transit-basic-{}-{run}", std::process::id());
        let path = root.join(&directory);
        match fs::create_dir(&path) {
            Ok(()) => break (directory, path),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        }
    };
    let name = format!("{directory}/snapshot");
    sim.save_to_disk(&name);
    let loaded = (0..count)
        .map(|_| Simulation::load_from_disk(&name))
        .collect::<Result<Vec<_>, _>>();
    fs::remove_dir_all(path).map_err(|err| err.to_string())?;
    let loaded = loaded.map_err(|err| err.to_string())?;
    let hashes = sim.hashes();
    if loaded.iter().any(|branch| branch.hashes() != hashes) {
        return Err(format!(
            "transit snapshot changed at tick {}",
            sim.get_tick()
        ));
    }
    Ok(loaded)
}

#[derive(Clone, Copy)]
struct Totals {
    car: u64,
    bus: u64,
    give_ups: u64,
    upkeep: i128,
}

fn totals(sim: &Simulation) -> Totals {
    let stats = sim.read::<TripStats>();
    Totals {
        car: stats.car.completed,
        bus: stats.transit.completed,
        give_ups: stats.give_ups,
        upkeep: sim.read::<Budget>().total(BudgetCategory::TransitUpkeep),
    }
}

impl Totals {
    fn since(self, before: Self) -> Self {
        Self {
            car: self.car - before.car,
            bus: self.bus - before.bus,
            give_ups: self.give_ups - before.give_ups,
            upkeep: self.upkeep - before.upkeep,
        }
    }
}

#[derive(Clone)]
struct Commute {
    home: BuildingID,
    job: BuildingID,
    was_home: bool,
    attempts: Vec<Attempt>,
}

#[derive(Clone, Copy)]
enum Attempt {
    Pending(Tick),
    Completed(u64),
    Expired,
}

#[derive(Default)]
struct CommuteComparison {
    means: [f64; 2],
    matched_means: [f64; 2],
    missing: [u64; 2],
    pending: [u64; 2],
    expired: [u64; 2],
    matched: u64,
}

#[derive(Clone)]
struct Commutes(BTreeMap<HumanID, Commute>);

impl Commutes {
    fn new(sim: &Simulation) -> Self {
        Self(
            sim.world()
                .humans
                .iter()
                .filter_map(|(id, human)| {
                    let work = human.work.as_ref()?;
                    (human.router.personal_car.is_none() && matches!(work.kind, WorkKind::Worker))
                        .then(|| {
                            let home = human.home.house;
                            (
                                id,
                                Commute {
                                    home,
                                    job: work.workplace,
                                    was_home: human.location == Location::Building(home),
                                    attempts: Vec::new(),
                                },
                            )
                        })
                })
                .collect(),
        )
    }

    /// Follows open attempts and, while `departures` is set, records new ones.
    fn observe(&mut self, sim: &Simulation, now: u64, departures: bool) {
        for (&id, commute) in &mut self.0 {
            let human = sim.get(id);
            if let Some((attempt, start)) =
                commute
                    .attempts
                    .last_mut()
                    .and_then(|attempt| match *attempt {
                        Attempt::Pending(start) => Some((attempt, start)),
                        _ => None,
                    })
            {
                let duration = now - start.0;
                if human.is_some_and(|human| human.location == Location::Building(commute.job)) {
                    *attempt = Attempt::Completed(duration.min(COMMUTE_TIMEOUT));
                } else if duration >= COMMUTE_TIMEOUT
                    || !human.is_some_and(|human| human.router.trip_start == Some(start))
                {
                    *attempt = Attempt::Expired;
                }
            }
            let dest = Some(Destination::Building(commute.job));
            if let Some(human) = human {
                if departures
                    && commute.was_home
                    && human.router.target_dest == dest
                    && human.router.trip_start == Some(Tick(now))
                {
                    commute.attempts.push(Attempt::Pending(Tick(now)));
                }
                commute.was_home = human.location == Location::Building(commute.home);
            }
        }
    }

    fn pending(&self) -> bool {
        self.0
            .values()
            .any(|commute| matches!(commute.attempts.last(), Some(Attempt::Pending(_))))
    }

    /// Runs past the end of the day until every attempt has finished or timed out. The
    /// morning shift starts in the last game hours of the day, about 8 real minutes at the
    /// 3x clock, so stopping at the day's end would score late departures as timeouts.
    fn finish(&mut self, branch: &mut ScenarioCtx, end: u64) {
        for now in end + 1..=end + COMMUTE_TIMEOUT {
            if !self.pending() {
                break;
            }
            branch.run_ticks(1);
            self.observe(&branch.sim, now, false);
        }
    }

    fn compare(&self, other: &Self) -> Result<CommuteComparison, String> {
        let mut comparison = CommuteComparison::default();
        let mut durations = [0, 0];
        let mut matched_durations = [0, 0];
        let mut count = 0;
        for (id, commute) in &self.0 {
            let branches = [&commute.attempts, &other.0[id].attempts];
            // Keep attempt indices aligned even when one branch has no departure.
            for index in 0..branches[0].len().max(branches[1].len()) {
                let pair = branches.map(|attempts| attempts.get(index));
                count += 1;
                let matched = pair.iter().all(Option::is_some);
                comparison.matched += u64::from(matched);
                for (branch, attempt) in pair.into_iter().enumerate() {
                    let duration = match attempt {
                        Some(Attempt::Completed(duration)) => *duration,
                        Some(Attempt::Expired) => {
                            comparison.expired[branch] += 1;
                            COMMUTE_TIMEOUT
                        }
                        Some(Attempt::Pending(_)) => {
                            comparison.pending[branch] += 1;
                            COMMUTE_TIMEOUT
                        }
                        None => {
                            comparison.missing[branch] += 1;
                            COMMUTE_TIMEOUT
                        }
                    };
                    durations[branch] += duration;
                    if matched {
                        matched_durations[branch] += duration;
                    }
                }
            }
        }
        if comparison.matched == 0 {
            return Err(format!(
                "no matched carless home-to-work commute attempts (missing {:?}, pending {:?})",
                comparison.missing, comparison.pending
            ));
        }
        comparison.means = durations.map(|ticks| ticks as f64 / count as f64);
        comparison.matched_means =
            matched_durations.map(|ticks| ticks as f64 / comparison.matched as f64);
        Ok(comparison)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p3c_commute_comparison_counts_missing_pending_and_expired_attempts() {
        let resident = HumanID::default();
        let branch = |attempts| {
            Commutes(BTreeMap::from([(
                resident,
                Commute {
                    home: BuildingID::default(),
                    job: BuildingID::default(),
                    was_home: true,
                    attempts,
                },
            )]))
        };
        let control = branch(vec![Attempt::Completed(100), Attempt::Expired]);
        let transit = branch(vec![
            Attempt::Completed(80),
            Attempt::Completed(200),
            Attempt::Pending(Tick(720)),
        ]);
        let pair = control.compare(&transit).unwrap();
        assert_eq!(pair.matched, 2);
        assert_eq!(pair.missing, [1, 0]);
        assert_eq!(pair.pending, [0, 1]);
        assert_eq!(pair.expired, [1, 0]);
        assert_eq!(
            pair.matched_means,
            [(100 + COMMUTE_TIMEOUT) as f64 / 2.0, 140.0]
        );
        assert_eq!(pair.means[0], (100 + 2 * COMMUTE_TIMEOUT) as f64 / 3.0);
    }

    #[test]
    fn p3c_transit_basic_is_deterministic() {
        let mut ctx = ScenarioCtx::new(SCENARIO.1);
        let mut first = None;
        ctx.verify_twice(|ctx| {
            let control = compare_branches(ctx)?;
            let hashes = control.hashes();
            if let Some(first) = &first {
                assert_eq!(first, &hashes, "no-line branch diverged");
            } else {
                first = Some(hashes);
            }
            Ok(())
        })
        .unwrap();
    }
}
