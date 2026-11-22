use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicUsize, Ordering};

use geom::vec2;
use prototypes::{Money, Tick, TICKS_PER_HOUR};
use simulation::economy::{TransitStats, TripStats, TICKS_PER_BUDGET_DAY};
use simulation::map::{BuildingID, LaneKind};
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

const COMMUTE_TIMEOUT: u64 = 2 * TICKS_PER_HOUR;
const FLEET_SIZE: u8 = 3;

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    compare_branches(ctx).map(|_| ())
}

fn compare_branches(ctx: &mut ScenarioCtx) -> Result<Simulation, String> {
    (super::growth_basic::SCENARIO.0.run)(ctx)?;
    let start_tick = ctx.sim.get_tick();
    let start = elapsed_tick(&mut ctx.sim);
    let mut with_line = Commutes::new(&ctx.sim);
    let mut without_line = with_line.clone();
    let snapshot = fork(&ctx.sim)?;
    let line = install_loop(&mut ctx.sim)?;

    let mut control = ScenarioCtx::new(SCENARIO.1);
    control.sim = snapshot;
    let control_start = totals(&control.sim);
    for now in start + 1..=start + TICKS_PER_BUDGET_DAY {
        control.run_ticks(1);
        without_line.observe(&control.sim, now);
    }
    let control_totals = totals(&control.sim).since(control_start);
    let control_sample = control.sample("without_transit");
    ctx.samples.push(control_sample);

    let transit_start = totals(&ctx.sim);
    let mut resume = None;
    let mut running_buses = 0;
    for day_tick in 1..=TICKS_PER_BUDGET_DAY {
        ctx.run_ticks(1);
        with_line.observe(&ctx.sim, start + day_tick);
        running_buses = running_buses.max(ctx.sim.read::<Transit>().runs.len());
        if resume.is_none()
            && day_tick < TICKS_PER_BUDGET_DAY
            && occupied_and_waiting(&ctx.sim, line)
        {
            resume = Some((day_tick, fork(&ctx.sim)?));
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
    let (commute_a, commute_b, matched) = without_line.matched_means(&with_line)?;
    let wait_attempts = boards + transit.give_ups;
    let summary = format!(
        "bus trips={} car trips={}/{} carless commute={commute_b:.1}/{commute_a:.1} ticks (n={matched}) max wait={max_wait} give-ups={}/{wait_attempts}",
        transit.bus, transit.car, control_totals.car, transit.give_ups,
    );
    if running_buses != usize::from(FLEET_SIZE)
        || transit.bus < 50
        || transit.car >= control_totals.car
        || commute_b >= commute_a
        || max_wait >= COMMUTE_TIMEOUT
        || transit.give_ups * 20 > wait_attempts
    {
        return Err(format!(
            "transit gate failed with {running_buses} buses ({summary})"
        ));
    }
    let (resume_tick, saved) = resume.ok_or("no checkpoint had both riders and a stop queue")?;
    let mut resumed = ScenarioCtx::new(SCENARIO.1);
    resumed.sim = saved;
    resumed.run_ticks(TICKS_PER_BUDGET_DAY - resume_tick);
    if resumed.sim.hashes() != ctx.sim.hashes() {
        return Err(format!(
            "transit continuation diverged after tick {}",
            start_tick + resume_tick
        ));
    }
    println!(
        "transit_basic: {summary} resumed from tick {}",
        start_tick + resume_tick
    );
    ctx.sample("with_transit");
    ctx.samples.last_mut().unwrap().extra.extend([
        ("bus_trips".into(), transit.bus as f64),
        ("car_trips".into(), transit.car as f64),
        (
            "car_trips_without_transit".into(),
            control_totals.car as f64,
        ),
        ("matched_carless_commutes".into(), matched as f64),
        ("mean_carless_commute_ticks".into(), commute_b),
        (
            "mean_carless_commute_ticks_without_transit".into(),
            commute_a,
        ),
        ("max_wait_ticks".into(), max_wait as f64),
        ("give_ups".into(), transit.give_ups as f64),
        ("wait_attempts".into(), wait_attempts as f64),
    ]);
    Ok(control.sim)
}

fn elapsed_tick(sim: &mut Simulation) -> u64 {
    let (_, resources) = sim.world_res();
    simulation::map_dynamic::transit::elapsed_ticks(resources).0
}

fn install_loop(sim: &mut Simulation) -> Result<LineID, String> {
    // Clockwise service around the residential and employment blocks of growth_basic.
    let legs = [
        ((1, 1), (2, 1)),
        ((2, 1), (3, 1)),
        ((3, 1), (4, 1)),
        ((4, 2), (4, 3)),
        ((4, 4), (3, 4)),
        ((2, 4), (1, 4)),
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

fn fork(sim: &Simulation) -> Result<Simulation, String> {
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
    let loaded = Simulation::load_from_disk(&name);
    fs::remove_dir_all(path).map_err(|err| err.to_string())?;
    let loaded = loaded.map_err(|err| err.to_string())?;
    if sim.hashes() != loaded.hashes() {
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
}

fn totals(sim: &Simulation) -> Totals {
    let stats = sim.read::<TripStats>();
    Totals {
        car: stats.car.completed,
        bus: stats.transit.completed,
        give_ups: stats.give_ups,
    }
}

impl Totals {
    fn since(self, before: Self) -> Self {
        Self {
            car: self.car - before.car,
            bus: self.bus - before.bus,
            give_ups: self.give_ups - before.give_ups,
        }
    }
}

#[derive(Clone)]
struct Commute {
    home: BuildingID,
    job: BuildingID,
    was_home: bool,
    pending: Option<Tick>,
    durations: Vec<u64>,
}

impl Commute {
    fn attempts(&self) -> impl Iterator<Item = u64> + '_ {
        self.durations
            .iter()
            .copied()
            .chain(self.pending.map(|_| COMMUTE_TIMEOUT))
    }
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
                                    pending: None,
                                    durations: Vec::new(),
                                },
                            )
                        })
                })
                .collect(),
        )
    }

    fn observe(&mut self, sim: &Simulation, now: u64) {
        for (&id, commute) in &mut self.0 {
            let human = sim.get(id);
            if let Some(start) = commute.pending {
                let duration = now - start.0;
                if human.is_some_and(|human| human.location == Location::Building(commute.job)) {
                    commute.durations.push(duration.min(COMMUTE_TIMEOUT));
                    commute.pending = None;
                } else if duration >= COMMUTE_TIMEOUT
                    || !human.is_some_and(|human| human.router.trip_start == Some(start))
                {
                    commute.durations.push(COMMUTE_TIMEOUT);
                    commute.pending = None;
                }
            }
            let dest = Some(Destination::Building(commute.job));
            if let Some(human) = human {
                if commute.was_home
                    && human.router.target_dest == dest
                    && human.router.trip_start == Some(Tick(now))
                {
                    commute.pending = Some(Tick(now));
                }
                commute.was_home = human.location == Location::Building(commute.home);
            }
        }
    }

    fn matched_means(&self, other: &Self) -> Result<(f64, f64, u64), String> {
        let mut count = 0;
        let mut left = 0;
        let mut right = 0;
        for (id, commute) in &self.0 {
            // Compare attempts by resident and order. An attempt still in progress
            // at the end of either branch receives the same timeout as a failed one.
            for (a, b) in commute.attempts().zip(other.0[id].attempts()) {
                count += 1;
                left += a;
                right += b;
            }
        }
        if count == 0 {
            return Err("no matched carless home-to-work commute attempts".into());
        }
        Ok((
            left as f64 / count as f64,
            right as f64 / count as f64,
            count,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
