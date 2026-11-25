use crate::builders::{fill_houses, grant_money, grid_city, place_company};
use crate::ScenarioCtx;
use geom::vec2;
use prototypes::{ItemID, Money};
use serde::Serialize;
use simulation::map::road_edit::{
    preview_road_edit, road_edit_stamp, RoadEditRequest, RoadUpgrade,
};
use simulation::map::{BuildingID, LanePatternBuilder, RoadID};
use simulation::map_dynamic::BuildingInfos;
use simulation::souls::human::spawn_human;
use simulation::world_command::{CommandOutcomes, PrepareBudget, WorldCommand};
use simulation::{HumanID, Simulation, SoulID};

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

    let mut houses = fill_houses(&mut ctx, PEOPLE);
    if houses.len() != PEOPLE {
        return Err(format!(
            "repair fixture built {} houses, expected {PEOPLE}",
            houses.len()
        ));
    }
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
        let existing = ctx.sim.read::<BuildingInfos>().get(home).and_then(|info| {
            info.residents.iter().find_map(|&resident| match resident {
                SoulID::Human(human) => Some(human),
                _ => None,
            })
        });
        let resident = existing
            .or_else(|| spawn_human(&mut ctx.sim, home))
            .ok_or("repair fixture could not seed a resident")?;
        people.push((resident, home));
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
    let repair = speed_edit(&ctx.sim, road, 13.0)?;
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
    let request = RoadEditRequest::Upgrade {
        road,
        expected,
        change: change.clone(),
    };
    let preview = preview_road_edit(sim, &request, &mut budget)
        .map_err(|error| format!("road quote: {error:?}"))?;
    Ok(WorldCommand::MapUpgradeRoad {
        road,
        expected,
        change,
        quote: preview.quote,
    })
}
