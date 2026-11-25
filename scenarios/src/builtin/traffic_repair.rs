use crate::builders::{fill_houses, grant_money, grid_city, place_company};
use crate::traffic_repair::{Observation, Terminal};
use crate::ScenarioCtx;
use geom::vec2;
use prototypes::{ItemID, Money, RecTimeInterval};
use serde::Serialize;
use simulation::diagnostics::elapsed_tick;
use simulation::economy::cargo::{reserve_road_freight, CargoBook, ItemQuantity};
use simulation::economy::{Market, TICKS_PER_BUDGET_DAY};
use simulation::map::road_edit::{
    preview_road_upgrade, road_edit_stamp, RoadEditStatus, RoadUpgrade,
};
use simulation::map::{BuildingID, LanePatternBuilder, RoadID};
use simulation::map_dynamic::{BuildingInfos, Destination};
use simulation::souls::desire::{Work, WorkKind};
use simulation::souls::human::{spawn_human, HumanDecision, HumanDecisionKind};
use simulation::transportation::Location;
use simulation::world_command::{CommandOutcomes, PrepareBudget, WorldCommand};
use simulation::{HumanID, Simulation, SoulID};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureArm {
    Repair,
    NoLoss,
}

pub struct RepairFixture {
    pub simulation: Simulation,
    pub checkpoint: u64,
    pub repair_at: u64,
    pub primary_cutoff: u64,
    pub tail_cutoff: u64,
    pub road: RoadID,
    pub repair: WorldCommand,
    pub schedule: Vec<ScheduledOffer>,
}

#[derive(Debug, Serialize)]
pub struct ScheduledOffer {
    pub tag: u32,
    pub tick: u64,
    pub group: &'static str,
    pub offer: FixtureOffer,
}

#[derive(Debug, Serialize)]
#[serde(tag = "cohort", rename_all = "snake_case")]
pub enum FixtureOffer {
    Person {
        human: HumanID,
        home: BuildingID,
        destination: BuildingID,
    },
    Freight {
        sender: BuildingID,
        receiver: BuildingID,
        item: ItemID,
        units: u64,
    },
}

/// Builds the checkpoint and freezes offers by semantic corridor and facility.
/// Releases are consumed by the matched runner after both arms stage the checkpoint.
pub fn prepare_repair_fixture(arm: FixtureArm) -> Result<RepairFixture, String> {
    const PEOPLE: usize = 120;
    const LOADS: usize = 24;
    ScenarioCtx::initialize();
    let sim = Simulation::new_authored_flat(4, 6042).map_err(|error| error.to_string())?;
    let mut ctx = ScenarioCtx::from_sim(sim);
    grant_money(&mut ctx, Money::new_bucks(850_000));
    let origin = vec2(400.0, 520.0);
    let grid = grid_city(&mut ctx, origin, 4, 3, 260.0);
    if grid.roads.len() != 17 {
        return Err(format!(
            "repair fixture has {} roads, expected 17",
            grid.roads.len()
        ));
    }
    let road = {
        let map = ctx.sim.map();
        let center = origin + vec2(390.0, 260.0);
        grid.roads
            .iter()
            .copied()
            .find(|id| {
                map.roads()[*id]
                    .points()
                    .middle()
                    .xy()
                    .is_close(center, 1.0)
            })
            .ok_or("repair fixture is missing its middle link")?
    };
    let starting_speed = match arm {
        FixtureArm::Repair => 4.0,
        FixtureArm::NoLoss => 13.0,
    };
    let setup = speed_edit(&ctx.sim, road, starting_speed)?;
    ctx.sim.apply_commands([&setup]);
    if ctx.sim.read::<CommandOutcomes>().accepted != [0] {
        return Err("repair fixture could not set the opening speed".into());
    }

    let mine_west = place_company(&mut ctx, "coal-mine", origin + vec2(110.0, -70.0))
        .ok_or("repair fixture is missing the west coal mine")?;
    let mine_east = place_company(&mut ctx, "coal-mine", origin + vec2(670.0, 600.0))
        .ok_or("repair fixture is missing the east coal mine")?;
    let plant_west = place_company(&mut ctx, "coal-power-plant", origin + vec2(110.0, 610.0))
        .ok_or("repair fixture is missing the west receiver")?;
    let plant_east = place_company(&mut ctx, "coal-power-plant", origin + vec2(670.0, -100.0))
        .ok_or("repair fixture is missing the east receiver")?;
    let center = place_company(&mut ctx, "small-depot", origin + vec2(390.0, -50.0))
        .ok_or("repair fixture is missing the central destination")?;

    let mut houses = fill_houses(&mut ctx, PEOPLE + 2);
    if houses.len() != PEOPLE + 2 {
        return Err(format!(
            "repair fixture built {} houses, expected {}",
            houses.len(),
            PEOPLE + 2
        ));
    }
    let driver_homes = houses.split_off(PEOPLE);
    let resident = |sim: &mut Simulation, home| {
        let existing = sim.read::<BuildingInfos>().get(home).and_then(|info| {
            info.residents.iter().find_map(|&resident| match resident {
                SoulID::Human(human) => Some(human),
                _ => None,
            })
        });
        existing.or_else(|| spawn_human(sim, home))
    };
    houses.sort_by(|&a, &b| {
        let map = ctx.sim.map();
        map.buildings()[a]
            .obb
            .center()
            .x
            .total_cmp(&map.buildings()[b].obb.center().x)
            .then(a.cmp(&b))
    });
    let mut people = Vec::with_capacity(PEOPLE);
    for home in houses {
        let resident =
            resident(&mut ctx.sim, home).ok_or("repair fixture could not seed a resident")?;
        people.push((resident, home));
    }
    for (home, mine) in driver_homes.into_iter().zip([mine_west, mine_east]) {
        let driver = resident(&mut ctx.sim, home).ok_or("could not seed a freight driver")?;
        assign_fixture_driver(&mut ctx.sim, driver, home, mine)?;
    }

    let checkpoint = simulation::diagnostics::elapsed_tick(&ctx.sim).0;
    let directed = [
        (0, "west_to_center", center),
        (0, "west_to_east", plant_east),
        (1, "center_to_west", plant_west),
        (1, "center_to_east", plant_east),
        (2, "east_to_west", plant_west),
        (2, "east_to_center", center),
    ];
    let mut schedule = Vec::with_capacity(PEOPLE + LOADS);
    for i in 0..PEOPLE {
        let (region, group, destination) = directed[i % directed.len()];
        let (human, home) = people[region * 40 + i / directed.len() * 2 + i % 2];
        schedule.push(ScheduledOffer {
            tag: i as u32 + 1,
            tick: checkpoint + 100 + i as u64 * 50,
            group,
            offer: FixtureOffer::Person {
                human,
                home,
                destination,
            },
        });
    }
    let pairs = [
        ("west_mine_to_east_plant", mine_west, plant_east),
        ("east_mine_to_west_plant", mine_east, plant_west),
    ];
    for i in 0..LOADS {
        let (group, sender, receiver) = pairs[i % pairs.len()];
        schedule.push(ScheduledOffer {
            tag: (PEOPLE + i + 1) as u32,
            tick: checkpoint + 100 + i as u64 * 250,
            group,
            offer: FixtureOffer::Freight {
                sender,
                receiver,
                item: ItemID::new("coal"),
                units: 1,
            },
        });
    }
    schedule.sort_by_key(|offer| (offer.tick, offer.tag));
    let requested_speed = match arm {
        FixtureArm::Repair => 13.0,
        FixtureArm::NoLoss => 15.0,
    };
    let repair = speed_edit(&ctx.sim, road, requested_speed)?;
    Ok(RepairFixture {
        simulation: ctx.sim,
        checkpoint,
        repair_at: checkpoint + 1,
        primary_cutoff: checkpoint + 10_600,
        tail_cutoff: checkpoint + 144_000,
        road,
        repair,
        schedule,
    })
}

fn speed_edit(sim: &Simulation, road: RoadID, speed: f32) -> Result<WorldCommand, String> {
    let mut budget = PrepareBudget::for_preview(4_194_304);
    let expected = road_edit_stamp(sim, road, &mut budget)
        .map_err(|error| format!("road stamp: {error:?}"))?;
    let change = RoadUpgrade::Replace(LanePatternBuilder::new().speed_limit(speed).build());
    let preview = preview_road_upgrade(sim, road, expected, &change, &mut budget)
        .map_err(|error| format!("road quote: {error:?}"))?;
    Ok(WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        quote: preview.quote,
    })
}

fn assign_fixture_driver(
    sim: &mut Simulation,
    driver: HumanID,
    home: BuildingID,
    mine: BuildingID,
) -> Result<(), String> {
    let Some(SoulID::GoodsCompany(company)) = sim.read::<BuildingInfos>().owner(mine) else {
        return Err("mine has no operating company".into());
    };
    let door = sim.map().buildings()[mine].door_pos;
    let world = sim.world_mut_unchecked();
    let company = &mut world.companies[company];
    let truck = *company.comp.trucks.first().ok_or("mine has no truck")?;
    company.workers.0.push(driver);
    company.comp.driver = Some(driver);
    let mut work = Work::new(
        mine,
        WorkKind::Driver {
            truck,
            deliver_order: None,
            load: None,
        },
        0.0,
    );
    work.work_inter = RecTimeInterval::always();
    world.edit_human(driver, |human| {
        human.location = Location::Building(mine);
        human.trans.pos = door;
        human.work = Some(work);
    });
    let mut infos = sim.write::<BuildingInfos>();
    infos.get_out(home, SoulID::Human(driver));
    infos.get_in(mine, SoulID::Human(driver));
    Ok(())
}

/// Replays the pinned offers on a fresh copy of the checkpoint. Keep movement,
/// routing and cargo on the stock schedule while supplying only authored orders.
pub fn run_no_loss_arm(
    mut sim: Simulation,
    fixture: &RepairFixture,
    treatment: bool,
) -> Result<Vec<Observation>, String> {
    if elapsed_tick(&sim).0 != fixture.checkpoint {
        return Err("no-loss snapshot is at the wrong elapsed tick".into());
    }
    let mut schedule = Simulation::schedule();
    schedule.retain_systems(|name| {
        !matches!(
            name,
            "company_system" | "market_update" | "residence_update" | "growth_update"
        )
    });
    let mut reports: Vec<_> = fixture
        .schedule
        .iter()
        .map(|entry| Observation {
            tag: entry.tag,
            admitted_at: None,
            terminal: None,
        })
        .collect();
    let mut pending_people = BTreeMap::<usize, (HumanID, BuildingID)>::new();
    struct FreightTags {
        recipient: SoulID,
        pending: VecDeque<usize>,
        credited: u64,
    }
    let mut freight = BTreeMap::<(BuildingID, ItemID), FreightTags>::new();
    let mut receipt_day = fixture.checkpoint / TICKS_PER_BUDGET_DAY;
    let mut next_offer = 0;
    for tick in (fixture.checkpoint + 1)..=fixture.tail_cutoff {
        while let Some(entry) = fixture
            .schedule
            .get(next_offer)
            .filter(|entry| entry.tick == tick)
        {
            match entry.offer {
                FixtureOffer::Person {
                    human,
                    home,
                    destination,
                } => {
                    sim.world_mut_unchecked()
                        .edit_human(human, |human| {
                            if human.location != Location::Building(home) {
                                return Err(format!(
                                    "tag {} left home before its release",
                                    entry.tag
                                ));
                            }
                            human.decision = HumanDecision {
                                kind: HumanDecisionKind::GoTo(Destination::Building(destination)),
                                wait: 0,
                            };
                            Ok(())
                        })
                        .ok_or("missing tagged resident")??;
                    pending_people.insert(next_offer, (human, destination));
                }
                FixtureOffer::Freight {
                    sender,
                    receiver,
                    item,
                    units,
                } => {
                    let source = sim
                        .read::<BuildingInfos>()
                        .owner(sender)
                        .ok_or("missing sender")?;
                    let target = sim
                        .read::<BuildingInfos>()
                        .owner(receiver)
                        .ok_or("missing receiver")?;
                    if units != 1 || sim.read::<Market>().capital(source, item) != 0 {
                        return Err("unexpected authored freight supply".into());
                    }
                    sim.write::<Market>().produce(source, item, 1);
                    if !reserve_road_freight(
                        &mut sim,
                        sender,
                        receiver,
                        ItemQuantity { item, units: 1 },
                    ) {
                        return Err(format!("could not reserve freight tag {}", entry.tag));
                    }
                    freight
                        .entry((receiver, item))
                        .or_insert_with(|| FreightTags {
                            recipient: target,
                            pending: VecDeque::new(),
                            credited: 0,
                        })
                        .pending
                        .push_back(next_offer);
                }
            }
            reports[next_offer].admitted_at = Some(tick);
            next_offer += 1;
        }

        let commands = (treatment && tick == fixture.repair_at).then_some(&fixture.repair);
        sim.tick(&mut schedule, commands);
        if commands.is_some()
            && !sim
                .read::<CommandOutcomes>()
                .road_edit(0)
                .is_some_and(|result| {
                    result
                        .as_ref()
                        .is_ok_and(|receipt| receipt.status == RoadEditStatus::Applied)
                })
        {
            return Err("no-loss edit did not apply".into());
        }

        pending_people.retain(|&index, &mut (human, destination)| {
            if sim
                .world()
                .humans
                .get(human)
                .is_some_and(|entity| entity.location == Location::Building(destination))
            {
                reports[index].terminal = Some(Terminal::Completed {
                    tick,
                    destination: format!("{destination:?}"),
                    units: 1,
                });
                false
            } else {
                true
            }
        });

        let book = sim.read::<CargoBook>();
        let day = tick / TICKS_PER_BUDGET_DAY;
        if day != receipt_day {
            receipt_day = day;
            for tags in freight.values_mut() {
                tags.credited = 0;
            }
        }
        if let Some(receipts) = book.freight_day(day) {
            if receipts.cancelled_units > 0 {
                return Err(format!("no-loss shipment cancelled at elapsed tick {tick}"));
            }
            for (&(receiver, item), tags) in &mut freight {
                let total = receipts.receipt(tags.recipient, item).road_units;
                for _ in tags.credited..total {
                    let index = tags.pending.pop_front().ok_or("unexpected coal unload")?;
                    reports[index].terminal = Some(Terminal::Completed {
                        tick,
                        destination: format!("{receiver:?}"),
                        units: 1,
                    });
                }
                tags.credited = total;
            }
        }
        if next_offer == reports.len()
            && pending_people.is_empty()
            && freight.values().all(|tags| tags.pending.is_empty())
        {
            // All tagged outcomes are terminal at both this point and the tail.
            break;
        }
    }
    Ok(reports)
}
