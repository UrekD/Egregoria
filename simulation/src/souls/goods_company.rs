use serde::{Deserialize, Serialize};

use egui_inspect::Inspect;
use geom::{Transform, Vec2};
use prototypes::{
    try_prototype, CompanyKind, GoodsCompanyID, GoodsCompanyPrototype, ItemID, Power, Recipe,
};

use crate::economy::{
    cargo::{uses_road_freight, CargoBook},
    find_trade_place, record_coal_stockout, Market,
};
use crate::map::{
    Building, BuildingID, BuildingKind, GrowableBusinessState, Map, Zone, MAX_ZONE_AREA,
};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{BuildingInfos, ElectricityFlow};
use crate::souls::desire::WorkKind;
use crate::transportation::{spawn_parked_vehicle, spawn_truck_at_depot, VehicleKind};
use crate::utils::resources::Resources;
use crate::world::{CompanyEnt, HumanID, HumanStorage, VehicleID};
use crate::{ParCommandBuffer, SoulID, VehicleEnt};
use crate::{Simulation, World};

use super::desire::Work;
use super::residence::{power_status, PowerStatus};

pub fn recipe_init(recipe: &Recipe, soul: SoulID, near: Vec2, market: &mut Market) {
    for item in &recipe.consumption {
        market.buy_until(soul, near, item.id, item.amount as u32)
    }
    for item in &recipe.production {
        market.register(soul, item.id);
    }
}

pub fn recipe_should_produce(recipe: &Recipe, soul: SoulID, market: &Market) -> bool {
    recipe_blockers(recipe, soul, market).next().is_none()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeBlocker {
    MissingInput {
        item: ItemID,
        stock: i32,
        required: i32,
    },
    OutputFull {
        item: ItemID,
        stock: i32,
        limit: i32,
    },
    Empty,
}

pub fn recipe_blockers<'a>(
    recipe: &'a Recipe,
    soul: SoulID,
    market: &'a Market,
) -> impl Iterator<Item = RecipeBlocker> + 'a {
    let inputs = recipe.consumption.iter().filter_map(move |item| {
        let stock = market.capital(soul, item.id);
        (stock < item.amount).then_some(RecipeBlocker::MissingInput {
            item: item.id,
            stock,
            required: item.amount,
        })
    });
    let outputs = recipe.production.iter().filter_map(move |item| {
        let stock = market.capital(soul, item.id);
        let limit = item.amount * (recipe.storage_multiplier + 1);
        (stock >= limit).then_some(RecipeBlocker::OutputFull {
            item: item.id,
            stock,
            limit,
        })
    });
    inputs.chain(outputs).chain(
        (recipe.consumption.is_empty() && recipe.production.is_empty())
            .then_some(RecipeBlocker::Empty),
    )
}

pub fn recipe_act(recipe: &Recipe, soul: SoulID, near: Vec2, market: &mut Market) {
    run_recipe(recipe, soul, near, market, |_| false, |_| false);
}

/// Inputs with a truck on its way are not ordered from a station: the recipe
/// waits for the delivery.
fn run_recipe(
    recipe: &Recipe,
    soul: SoulID,
    near: Vec2,
    market: &mut Market,
    road_freight: impl Fn(ItemID) -> bool,
    trucked_in: impl Fn(ItemID) -> bool,
) {
    for item in &recipe.consumption {
        market.produce(soul, item.id, -item.amount);
        if !trucked_in(item.id) {
            market.buy_until(soul, near, item.id, item.amount as u32);
        }
    }
    for item in &recipe.production {
        market.produce(soul, item.id, item.amount);
        if !road_freight(item.id) {
            market.sell_all(
                soul,
                near,
                item.id,
                (item.amount * recipe.storage_multiplier) as u32,
            );
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Inspect)]
pub struct GoodsCompanyState {
    pub proto: GoodsCompanyID,
    pub building: BuildingID,
    pub max_workers: u32,
    /// In [0; 1] range, to show how much has been made until new product
    pub progress: f32,
    pub driver: Option<HumanID>,
    pub trucks: Vec<VehicleID>,
}

impl CompanyEnt {
    /// Returns the productivity of the company, in [0; 1] range _before_ taking electricity into account
    pub fn raw_productivity(&self, proto: &GoodsCompanyPrototype, zone: Option<&Zone>) -> f32 {
        let mut p = 1.0;
        if proto.n_workers > 0 {
            p = self.workers.0.len() as f32 / proto.n_workers as f32;
        }
        if let Some(z) = zone {
            p *= z.area / MAX_ZONE_AREA
        }

        p
    }

    /// Returns the productivity of the company, in [0; 1] range
    pub fn productivity(
        &self,
        proto: &GoodsCompanyPrototype,
        zone: Option<&Zone>,
        map: &Map,
        elec_flow: &ElectricityFlow,
    ) -> f32 {
        let p = self.raw_productivity(proto, zone);

        if proto.power_consumption > Some(Power::ZERO)
            && power_status(map, elec_flow, self.comp.building) == PowerStatus::Blackout
        {
            return 0.0;
        }

        p
    }
}

pub fn company_soul(
    sim: &mut Simulation,
    build_id: BuildingID,
    proto: GoodsCompanyID,
) -> Option<SoulID> {
    let map = sim.map();
    let b = map.buildings().get(build_id)?;
    let planned = b.development().map(|site| site.planned_jobs);
    if (planned.is_some() || BuildingKind::GoodsCompany(proto).is_mixed_growable())
        && !sim.profiled_business_can_spawn(build_id, proto)
    {
        return None;
    }
    let proto = prototypes::try_prototype(proto)?;
    let door_pos = b.door_pos;
    let obb = b.obb;
    let height = b.height;
    drop(map);

    let mut trucks = vec![];
    if proto.storage.is_some()
        || (proto.id.is_growable() && sim.read::<CargoBook>().growable_freight)
    {
        trucks.extend((0..proto.n_trucks).map(|_| spawn_truck_at_depot(sim, door_pos)));
    } else if proto.kind == CompanyKind::Factory && !proto.id.is_growable() && proto.n_trucks > 0 {
        trucks.push(spawn_parked_vehicle(sim, VehicleKind::Truck, door_pos)?);
        trucks.extend((1..proto.n_trucks).map(|_| spawn_truck_at_depot(sim, door_pos)));
    }

    let comp = GoodsCompanyState {
        proto: proto.id,
        building: build_id,
        max_workers: planned.unwrap_or(proto.n_workers),
        progress: 0.0,
        driver: None,
        trucks,
    };

    let id = sim.world.insert(CompanyEnt {
        trans: Transform::new(obb.center().z(height)),
        comp,
        workers: Default::default(),
        sold: Default::default(),
        bought: Default::default(),
    });

    let company = &sim.world.get(id).unwrap().comp;

    let soul = SoulID::GoodsCompany(id);

    let job_opening = ItemID::new("job-opening");

    {
        let m = &mut *sim.write::<Market>();
        m.produce(soul, job_opening, company.max_workers as i32);
        m.sell_all(soul, door_pos.xy(), job_opening, 0);

        if let Some(r) = proto.recipe.as_ref().filter(|_| proto.storage.is_none()) {
            recipe_init(r, soul, door_pos.xy(), m);
        }
    }

    sim.write::<BuildingInfos>()
        .set_owner(company.building, soul);

    if planned.is_some() {
        sim.map_mut()
            .set_growable_business_state(build_id, GrowableBusinessState::Open);
    }
    Some(soul)
}

/// Enables zoned road freight in a scenario, including for firms already on the map.
pub fn enable_growable_freight(sim: &mut Simulation) {
    if sim.read::<CargoBook>().growable_freight {
        return;
    }
    sim.write::<CargoBook>().growable_freight = true;
    let existing: Vec<_> = {
        let map = sim.map();
        sim.world()
            .companies
            .iter()
            .filter_map(|(id, company)| {
                let proto = company.comp.proto.prototype();
                let building = map.get(company.comp.building)?;
                (proto.id.is_growable() && proto.n_trucks > 0 && company.comp.trucks.is_empty())
                    .then_some((id, building.door_pos, proto.n_trucks))
            })
            .collect()
    };
    for (company, door, count) in existing {
        let trucks = (0..count)
            .map(|_| spawn_truck_at_depot(sim, door))
            .collect();
        sim.world.companies[company].comp.trucks = trucks;
    }
}

pub fn is_special_building_company(proto: GoodsCompanyID) -> bool {
    try_prototype(proto).is_some() && !proto.is_growable()
}

pub fn company_system(world: &mut World, res: &mut Resources) {
    profiling::scope!("souls::company_system");
    let cbuf: &ParCommandBuffer<CompanyEnt> = &res.read();
    let mut market = res.write::<Market>();
    market.reconcile_loaded_shelf_limits(world);
    let market: &Market = &market;
    let map: &Map = &res.read();
    let elec_flow: &ElectricityFlow = &res.read();
    let job_opening = ItemID::new("job-opening");
    let coal_plant = GoodsCompanyID::new("coal-power-plant");
    let coal = ItemID::new("coal");
    let growable_freight = res.read::<CargoBook>().growable_freight;
    let trucked_in = res
        .read::<CargoBook>()
        .inputs_in_transit(elapsed_ticks(res).0);

    world.companies.iter_mut().for_each(|(me, c)| {
        let soul = SoulID::GoodsCompany(me);
        let b: &Building = unwrap_or!(map.buildings.get(c.comp.building), {
            cbuf.kill(me);
            return;
        });

        let proto = c.comp.proto.prototype();
        if proto.storage.is_none()
            && !c.comp.trucks.is_empty()
            && (c.comp.trucks.len() as u32) < proto.n_trucks
        {
            // Existing companies keep their trucks and receive any new slots at the depot.
            let missing = proto.n_trucks - c.comp.trucks.len() as u32;
            let door = b.door_pos;
            cbuf.exec_ent(me, move |sim| {
                if !sim.world.companies.contains_key(me) {
                    return;
                }
                let trucks: Vec<_> = (0..missing)
                    .map(|_| spawn_truck_at_depot(sim, door))
                    .collect();
                sim.world.companies[me].comp.trucks.extend(trucks);
            });
        }

        // Keep already-settled legacy dispatches from a pre-batching snapshot.
        // New physical matches live in CargoBook. Virtual matches finish at the market.
        c.sold
            .0
            .retain(|trade| uses_road_freight(proto, trade.kind, growable_freight));
        for trades in c.bought.0.values_mut() {
            trades.clear();
        }

        if let Some(recipe) = proto.recipe.as_ref().filter(|_| proto.storage.is_none()) {
            if recipe_should_produce(recipe, soul, market) {
                let productivity = c.productivity(proto, b.zone.as_ref(), map, elec_flow);

                c.comp.progress += productivity / recipe.batch_ticks() as f32;
            } else {
                if proto.id == coal_plant && market.capital(soul, coal) == 0 {
                    record_coal_stockout(res, coal);
                }
                // Once no truck is bringing a missing input, a live station may fill it.
                let reorder: Vec<_> = recipe
                    .consumption
                    .iter()
                    .filter(|item| {
                        market.capital(soul, item.id) < item.amount
                            && !trucked_in.contains(&(soul, item.id))
                            && market.inner()[&item.id].buy_order(soul).is_none()
                    })
                    .map(|item| (item.id, item.amount as u32))
                    .collect();
                if !reorder.is_empty() {
                    let near = b.door_pos.xy();
                    cbuf.exec_on(me, move |market: &mut Market| {
                        for (item, amount) in reorder {
                            market.buy_until(soul, near, item, amount);
                        }
                    });
                }
            }

            if c.comp.progress >= 1.0 {
                c.comp.progress -= 1.0;
                let kind = c.comp.proto;
                let bpos = b.door_pos;
                let waiting: Vec<_> = recipe
                    .consumption
                    .iter()
                    .map(|item| item.id)
                    .filter(|&item| trucked_in.contains(&(soul, item)))
                    .collect();

                cbuf.exec_on(me, move |market| {
                    let proto = kind.prototype();
                    let recipe = proto.recipe.as_ref().unwrap();
                    run_recipe(
                        recipe,
                        soul,
                        bpos.xy(),
                        market,
                        |item| uses_road_freight(proto, item, growable_freight),
                        |item| waiting.contains(&item),
                    );
                });
                return;
            }
        }

        for &worker in &c.workers.0 {
            if world
                .humans
                .get(worker)
                .is_some_and(|human| human.work.is_none())
            {
                world.humans.edit(worker, |human| {
                    let offset = common::rand::randu(common::hash_u64(worker) as u32);
                    let mut work = Work::new(c.comp.building, WorkKind::Worker, offset);
                    if b.development().is_some() {
                        work.employer = Some(me);
                    }
                    human.work = Some(work);
                    human.bought.0.remove(&job_opening);
                });
            }
        }
        if proto.storage.is_some() {
            assign_fleet_drivers(c, &mut world.humans, &world.vehicles);
            return;
        }
        if c.comp.driver.is_some_and(|driver| {
            !c.workers.0.contains(&driver)
                || !world
                    .humans
                    .get(driver)
                    .and_then(|human| human.work.as_ref())
                    .is_some_and(|work| {
                        work.workplace == c.comp.building
                            && matches!(work.kind, WorkKind::Driver { truck, .. }
                            if c.comp.trucks.contains(&truck) && world.vehicles.contains_key(truck))
                    })
        }) {
            c.comp.driver = None;
        }
        if proto.kind == CompanyKind::Factory && (c.comp.driver.is_none() || proto.n_trucks > 1) {
            let driver = assign_fleet_drivers(c, &mut world.humans, &world.vehicles);
            c.comp.driver = c.comp.driver.or(driver);
        }

        if let Some(driver) = c.comp.driver.filter(|_| !c.sold.0.is_empty()) {
            world.humans.edit(driver, |human| {
                let Some(WorkKind::Driver {
                    truck,
                    deliver_order,
                    load,
                }) = human.work.as_mut().map(|work| &mut work.kind)
                else {
                    return;
                };
                if deliver_order.is_none()
                    && load.is_none()
                    && world
                        .vehicles
                        .get(*truck)
                        .is_some_and(|vehicle| vehicle.vehicle.state.is_parked())
                {
                    let trade = c.sold.0.pop().unwrap();
                    *deliver_order = find_trade_place(trade.buyer, &res.read::<BuildingInfos>());
                }
            });
        }
    });
}

/// Pairs idle staff with trucks that have no driver, both in ID order.
fn assign_fleet_drivers(
    company: &CompanyEnt,
    humans: &mut HumanStorage,
    vehicles: &slotmapd::HopSlotMap<VehicleID, VehicleEnt>,
) -> Option<HumanID> {
    let building = company.comp.building;
    let work_at = |worker: HumanID| {
        humans
            .get(worker)
            .and_then(|human| human.work.as_ref())
            .filter(|work| work.workplace == building)
    };
    let mut idle: Vec<_> = company
        .workers
        .0
        .iter()
        .copied()
        .filter(|&worker| work_at(worker).is_some_and(|work| matches!(work.kind, WorkKind::Worker)))
        .collect();
    let driven: Vec<_> = company
        .workers
        .0
        .iter()
        .filter_map(|&worker| match work_at(worker)?.kind {
            WorkKind::Driver { truck, .. }
                if company.comp.trucks.contains(&truck) && vehicles.contains_key(truck) =>
            {
                Some((truck, worker))
            }
            WorkKind::Driver { .. } => None,
            WorkKind::Worker => None,
        })
        .collect();
    let mut trucks: Vec<_> = company
        .comp
        .trucks
        .iter()
        .copied()
        .filter(|&truck| vehicles.contains_key(truck) && !driven.iter().any(|&(id, _)| id == truck))
        .collect();
    idle.sort_unstable();
    trucks.sort_unstable();
    let mut first = driven.iter().map(|&(_, driver)| driver).min();
    for (worker, truck) in idle.into_iter().zip(trucks) {
        first = Some(first.map_or(worker, |driver| driver.min(worker)));
        humans.edit(worker, |human| {
            human.work.as_mut().unwrap().kind = WorkKind::Driver {
                truck,
                deliver_order: None,
                load: None,
            };
        });
    }
    first
}
