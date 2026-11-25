use std::collections::BTreeMap;

use geom::Vec3;

use prototypes::{
    try_prototype, GrowableChainKey, GrowableCompanyID, GrowableZoneKind, ItemID, ServiceKind,
    TICKS_PER_HOUR,
};

use crate::economy::Market;
use crate::map::import::growable_shell::{
    housing_slots, job_slots, realized_floors, GrowableFormFit,
};
use crate::map::{
    Building, BuildingID, ElectricityNetworkID, GrowableBusinessState, GrowableSiteState, Map,
    PreparedGrowableAppearance,
};
use crate::map_dynamic::{
    active_business, connected_site_network, projected_site_load, utility_network_readout,
    ActiveBusiness, ActiveRetailIndex, BuildingInfos, CoverageGrid, ElectricityFlow, EnvGrid,
    NetworkLoad, NetworkReservations, SiteLoad, UtilityAdmission, UtilityFlow,
};
use crate::{CompanyID, Simulation, SoulID, World};

use super::lifecycle;
use super::residence::{self, Requirement, Residence, Residences, UPGRADE_CHECKS};

const OUTAGE_HOURS: u8 = 24;
const STAFFING_LOSS_HOURS: u8 = 24;
const BUSINESS_RETRY_TICKS: u64 = 24 * TICKS_PER_HOUR;
const COLLAPSE_TICKS: u64 = 12 * TICKS_PER_HOUR;
const EDUCATED_HOURS: u8 = 24;
const SCHOOL_COVERAGE: f32 = 30.0;

// Profiled L4/L5 and pure firm upgrades are gated independently for Wave D.
pub(super) const MAX_HOUSING_LEVEL: u8 = 3;
const MAX_FIRM_LEVEL: u8 = 1;

#[derive(Clone, Copy)]
pub enum RemovalCause {
    Bulldoze,
    Fire,
    Collapse,
}

pub fn remove_site(sim: &mut Simulation, building: BuildingID, _cause: RemovalCause) -> bool {
    lifecycle::remove_site(sim, building)
}

pub fn close_business(sim: &mut Simulation, company: CompanyID) -> bool {
    lifecycle::close_business(sim, company)
}

#[derive(Debug)]
pub struct UpgradeStatus {
    pub level: u8,
    pub next_level: Option<u8>,
    pub max_level: u8,
    pub eligible_hours: u8,
    pub required_hours: u8,
    pub requirements: Vec<Requirement>,
    pub admission_blocker: Option<UtilityAdmission>,
}

pub fn requirements(sim: &Simulation, id: BuildingID) -> Option<UpgradeStatus> {
    requirements_with_retail(sim, id, None)
}

pub(crate) fn requirements_with_retail(
    sim: &Simulation,
    id: BuildingID,
    retail_access: Option<bool>,
) -> Option<UpgradeStatus> {
    let map = sim.map();
    let building = map.get(id)?;
    let site = building.development()?;
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let context = Assessment {
        map: &map,
        infos: &infos,
        residences: &residences,
        world: sim.world(),
        coverage: &sim.read::<CoverageGrid>(),
        environment: &sim.read::<EnvGrid>(),
    };
    let max_level = max_level(building, MAX_HOUSING_LEVEL, MAX_FIRM_LEVEL);
    let next_level =
        (!building.is_abandoned_growable() && site.level < max_level).then(|| site.level + 1);
    let eligible_hours = residences
        .checks
        .get(&id)
        .map_or(0, |checks| checks.upgrade_streak);
    let mut requirements = Vec::new();
    let mut admission_blocker = None;
    if let Some(level) = next_level {
        let business = active_business(building, &infos, sim.world());
        let services = network_services(sim, &map, building, &mut BTreeMap::new());
        requirements = context.requirements(
            building,
            business,
            level,
            services,
            retail_access.unwrap_or_else(|| {
                ActiveRetailIndex::build(&map, &infos, sim.world())
                    .has_access(building.door_pos.xy())
            }),
        );
        if requirements.iter().all(|requirement| requirement.met) {
            if let Some(upgrade) = prepare_upgrade(&context, building, business, level) {
                admission_blocker = NetworkReservations::new(&map)
                    .try_reserve(sim, id, upgrade.load)
                    .err();
            } else {
                requirements.push(Requirement::available("Compatible shell", false));
            }
        }
    }
    Some(UpgradeStatus {
        level: site.level,
        next_level,
        max_level,
        eligible_hours,
        required_hours: UPGRADE_CHECKS,
        requirements,
        admission_blocker,
    })
}

#[derive(Clone, Copy, Default)]
pub(super) struct Services {
    power: bool,
    pub(super) water: bool,
    pub(super) sewage: bool,
}

pub(super) fn network_services(
    sim: &Simulation,
    map: &Map,
    building: &Building,
    cache: &mut BTreeMap<ElectricityNetworkID, Services>,
) -> Services {
    let Some(network) = connected_site_network(map, building) else {
        return Services::default();
    };
    *cache.entry(network).or_insert_with(|| {
        let Some(live) = NetworkLoad::live(
            map,
            &sim.read::<Residences>(),
            &sim.read::<BuildingInfos>(),
            sim.world(),
            network,
        ) else {
            return Services::default();
        };
        let mut services = Services {
            power: live.has_power(),
            ..Services::default()
        };
        let utilities = sim.read::<UtilityFlow>();
        if !utilities.water_pending(map) {
            if let Some(sample) = utility_network_readout(
                map,
                &sim.read::<ElectricityFlow>(),
                &utilities,
                Some(network),
            )
            .utility_totals
            {
                let mut pipes = live.pipes;
                pipes.water_production = pipes.water_production.min(sample.water_production);
                pipes.sewage_capacity = pipes.sewage_capacity.min(sample.sewage_capacity);
                // A zero-draw network can serve pipes through a zero-watt pump.
                services.water =
                    live.power_demand_watts <= live.power_supply_watts && pipes.has_water();
                services.sewage = services.water && pipes.has_sewage();
            }
        }
        services
    })
}

struct Assessment<'a> {
    map: &'a Map,
    infos: &'a BuildingInfos,
    residences: &'a Residences,
    world: &'a World,
    coverage: &'a CoverageGrid,
    environment: &'a EnvGrid,
}

fn member_ids<'a>(
    infos: &'a BuildingInfos,
    id: BuildingID,
) -> impl Iterator<Item = crate::HumanID> + 'a {
    infos
        .get(id)
        .into_iter()
        .flat_map(|info| &info.residents)
        .filter_map(move |&soul| {
            let SoulID::Human(human) = soul else {
                return None;
            };
            (infos.residence_of(soul) == Some(id)).then_some(human)
        })
}

impl Assessment<'_> {
    fn housing_counts(&self, id: BuildingID) -> (usize, usize, usize) {
        let mut count = (0, 0, 0);
        for resident in member_ids(self.infos, id).filter_map(|id| self.world.get(id)) {
            if resident.home.house != id {
                continue;
            }
            count.0 += 1;
            count.1 += usize::from(resident.work.as_ref().is_some_and(|work| {
                let Some(workplace) = self.map.get(work.workplace) else {
                    return false;
                };
                active_business(workplace, self.infos, self.world)
                    .is_some_and(|employer| work.employer.is_none_or(|id| id == employer.company))
            }));
            count.2 += usize::from(resident.household.education_hours() >= EDUCATED_HOURS);
        }
        count
    }

    fn requirements(
        &self,
        building: &Building,
        business: Option<ActiveBusiness>,
        level: u8,
        services: Services,
        retail: bool,
    ) -> Vec<Requirement> {
        let id = building.id;
        let site = building.development().unwrap();
        let mut requirements = vec![
            Requirement::available("Power", services.power),
            Requirement::available("Water", services.water),
            Requirement::available("Sewage", services.sewage),
        ];
        let (educated, members) = if let Some(home) = self.residences.by_building.get(&id) {
            let (occupied, employed, educated) = self.housing_counts(id);
            requirements.extend([
                Requirement::percent("Occupancy", occupied, home.capacity as usize, 80),
                Requirement::percent("Employment", employed, occupied, 70),
                Requirement::available("Retail access", retail),
            ]);
            if building.is_mixed_use() {
                requirements.push(Requirement::available(
                    "Staffed storefront",
                    business.is_some_and(|role| role.filled > 0),
                ));
            }
            (educated, occupied)
        } else {
            let (filled, slots, educated) = business.map_or((0, 0, 0), |role| {
                let educated = self.world.companies[role.company]
                    .workers
                    .0
                    .iter()
                    .filter_map(|&id| self.world.get(id))
                    .filter(|worker| worker.household.education_hours() >= EDUCATED_HOURS)
                    .count();
                (role.filled, role.slots as usize, educated)
            });
            requirements.push(Requirement::percent("Staffing", filled, slots, 80));
            (educated, filled)
        };

        let coverage_threshold = match level {
            4 => 35.0,
            5 => 45.0,
            _ => SCHOOL_COVERAGE,
        };
        let value_threshold = match (level, site.zone.kind, site.zone.density) {
            (4, crate::map::ZoneKind::Industrial, crate::map::Density::High) => 50.0,
            (5, crate::map::ZoneKind::Industrial, crate::map::Density::High) => 60.0,
            (4, _, _) => 60.0,
            (5, _, _) => 70.0,
            _ => 50.0,
        };
        requirements.extend(
            residence::amenity_requirements(
                self.coverage,
                self.environment,
                id,
                level,
                coverage_threshold,
                value_threshold,
            )
            .into_iter()
            .flatten(),
        );
        if level >= 4 {
            requirements.push(Requirement::percent(
                "Education",
                educated,
                members,
                if level == 4 { 25 } else { 50 },
            ));
        }
        requirements
    }
}

fn home_prototype(
    building: &Building,
    level: u8,
) -> Option<&'static prototypes::ResidencePrototype> {
    let site = building.development()?;
    site.home?;
    prototypes::prototypes().residence_for_chain(GrowableChainKey {
        zone_kind: if building.is_mixed_use() {
            GrowableZoneKind::MixedUse
        } else {
            GrowableZoneKind::Pure(site.zone.kind)
        },
        density: site.zone.density,
        level,
    })
}

fn max_level(building: &Building, housing_limit: u8, firm_limit: u8) -> u8 {
    let site = building.development().unwrap();
    if site.home.is_some() {
        (site.level + 1..=housing_limit)
            .take_while(|&level| home_prototype(building, level).is_some())
            .last()
            .unwrap_or(site.level)
    } else {
        firm_limit.max(site.level)
    }
}

struct Upgrade {
    appearance: PreparedGrowableAppearance,
    home: Option<Residence>,
    opening: Option<(CompanyID, i32)>,
    load: SiteLoad,
}

fn prepare_upgrade(
    context: &Assessment<'_>,
    building: &Building,
    business: Option<ActiveBusiness>,
    level: u8,
) -> Option<Upgrade> {
    let site = building.development()?;
    let profile = if let Some(home) = site.home {
        try_prototype(home)?.massing?
    } else {
        try_prototype(GrowableCompanyID::from(business?.prototype.id))?.massing?
    };
    let env = context.map.environment();
    let parcel = context.map.lot_origin(building.id)?.shape;
    // Recover the chosen inset from the existing, authoritative geometry.
    // The initial fit can use a lower band than the approved floors.
    let fit = (0..=site.approved_value_band).find_map(|band| {
        let fit = GrowableFormFit::check(env, parcel, site.zone, profile.style, band)?;
        if fit.base_z() != building.height || fit.door() != building.door_pos {
            return None;
        }
        let (mesh, roof) = fit.mesh(env, site.zone, profile.style, site.floors, site.level)?;
        let same_vertices = mesh.faces.len() == building.mesh.faces.len()
            && mesh
                .faces
                .iter()
                .zip(&building.mesh.faces)
                .all(|(a, b)| a.0 == b.0);
        (roof == site.roof_z && same_vertices).then_some(fit)
    })?;
    let band = site
        .approved_value_band
        .max(match context.environment.land_value(building.id) {
            0..=49 => 0,
            50..=69 => 1,
            _ => 2,
        });
    let floors = site.floors.max(realized_floors(
        profile,
        site.zone.density,
        level,
        fit.form(),
        band,
    )?);
    let nominal = profile.nominal_floors(level)?;
    let home = if let Some(current) = context.residences.by_building.get(&building.id) {
        let prototype = home_prototype(building, level)?;
        let capacity = housing_slots(prototype.capacity, floors, nominal, building.is_mixed_use())?;
        Some(Residence {
            capacity: capacity.max(current.capacity),
            ..Residence::from(prototype)
        })
    } else {
        None
    };
    let mut jobs = site.planned_jobs;
    let mut opening = None;
    let mut projected_business = business;
    if !building.has_residential_use() {
        let role = business?;
        jobs = jobs.max(job_slots(
            site.zone.kind,
            role.prototype.n_workers,
            floors,
            profile.nominal_floors(1)?,
        )?);
        opening = Some((role.company, role.job_slot_increase(jobs)?));
        projected_business = Some(ActiveBusiness {
            slots: jobs,
            ..role
        });
    }
    let load = projected_site_load(building, home.as_ref(), projected_business);
    let (mesh, roof_z) = fit.mesh(env, site.zone, profile.style, floors, level)?;
    let mut next = site.clone();
    next.level = level;
    next.floors = floors;
    next.roof_z = roof_z;
    next.home = home.as_ref().map(|home| home.prototype);
    next.planned_jobs = jobs;
    next.approved_value_band = band;
    Some(Upgrade {
        appearance: PreparedGrowableAppearance {
            development: next,
            base_z: building.height,
            door: building.door_pos,
            mesh,
        },
        home,
        opening,
        load,
    })
}

impl Upgrade {
    fn commit(self, sim: &mut Simulation, id: BuildingID) {
        let door = self.appearance.door;
        let slots = self.appearance.development.planned_jobs;
        if !sim
            .map_mut()
            .update_growable_appearance(id, self.appearance)
        {
            return;
        }
        if let Some(home) = self.home {
            sim.write::<Residences>().replace(id, home);
            sim.write::<BuildingInfos>().mark_vacancy_changed(id);
        }
        if let Some((company, delta)) = self.opening {
            sim.world.companies[company].comp.max_workers = slots;
            if delta > 0 {
                let seller = SoulID::GoodsCompany(company);
                let item = ItemID::new("job-opening");
                let mut market = sim.write::<Market>();
                market.produce(seller, item, delta);
                market.sell_all(seller, door.xy(), item, 0);
            }
        }
    }
}

fn accrue_education(sim: &mut Simulation) {
    let (world, resources) = sim.world_res();
    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let homes = resources.read::<Residences>();
    let coverage = resources.read::<CoverageGrid>();
    for &id in homes.by_building.keys() {
        if homes.is_abandoned(id)
            || coverage.coverage(id, ServiceKind::School) < SCHOOL_COVERAGE
            || !map
                .get(id)
                .is_some_and(|home| !home.is_orphaned() && !home.is_abandoned_growable())
        {
            continue;
        }
        for member in member_ids(&infos, id) {
            world.edit_human(member, |human| {
                if human.home.house == id {
                    human.household.earn_education_hour();
                }
            });
        }
    }
}

pub(crate) fn update(
    sim: &mut Simulation,
    phase: u64,
    legacy_abandoned: Vec<(BuildingID, Vec3)>,
) -> ActiveRetailIndex {
    update_with_limits(
        sim,
        phase,
        legacy_abandoned,
        MAX_HOUSING_LEVEL,
        MAX_FIRM_LEVEL,
    )
}

fn update_with_limits(
    sim: &mut Simulation,
    phase: u64,
    legacy_abandoned: Vec<(BuildingID, Vec3)>,
    housing_limit: u8,
    firm_limit: u8,
) -> ActiveRetailIndex {
    let retail_index = |sim: &Simulation| {
        ActiveRetailIndex::build(&sim.map(), &sim.read::<BuildingInfos>(), sim.world())
    };
    let hour = phase / TICKS_PER_HOUR;
    let consecutive = {
        let mut residences = sim.write::<Residences>();
        if residences
            .last_profile_hour
            .is_some_and(|previous| previous >= hour)
        {
            drop(residences);
            residence::settle_abandonment(sim, &mut Vec::new(), legacy_abandoned);
            return retail_index(sim);
        }
        residences
            .last_profile_hour
            .replace(hour)
            .is_some_and(|previous| previous + 1 == hour)
    };
    if housing_limit >= 4 || firm_limit >= 4 {
        accrue_education(sim);
    }
    let now = crate::map_dynamic::transit::elapsed_ticks(&sim.resources).0;
    let mut abandoned = Vec::new();
    let mut collapse = Vec::new();
    let mut close = Vec::new();

    {
        let map = sim.map();
        let homes = sim.read::<Residences>();
        let infos = sim.read::<BuildingInfos>();
        let context = Assessment {
            map: &map,
            infos: &infos,
            residences: &homes,
            world: sim.world(),
            coverage: &sim.read::<CoverageGrid>(),
            environment: &sim.read::<EnvGrid>(),
        };
        let mut services = BTreeMap::new();
        let mut checks_by_id = Vec::new();
        for building in map
            .buildings()
            .values()
            .filter(|building| building.development().is_some())
        {
            let id = building.id;
            let site = building.development().unwrap();
            if let GrowableSiteState::Abandoned { since_elapsed_tick } = site.site {
                if now.saturating_sub(since_elapsed_tick) >= COLLAPSE_TICKS {
                    collapse.push(id);
                }
                continue;
            }
            let mut checks = homes.checks.get(&id).copied().unwrap_or_default();
            if !consecutive {
                checks.outage_streak = 0;
                checks.poor = 0;
                checks.low_staffing_hours = 0;
                checks.upgrade_streak = 0;
            }
            let utility = network_services(sim, &map, building, &mut services);
            let residence = homes.by_building.get(&id);
            let served = utility.water && utility.sewage && (residence.is_some() || utility.power);
            checks.outage_streak = if served {
                0
            } else {
                checks.outage_streak.saturating_add(1)
            };
            let outage_limit = if residence.is_some() {
                residence::UNSERVED_CHECKS
            } else {
                OUTAGE_HOURS
            };
            let poor = residence.is_some_and(|home| {
                let (occupied, employed, _) = context.housing_counts(id);
                occupied * 5 < home.capacity as usize
                    && (occupied == 0 || employed * 10 < occupied * 3)
            });
            checks.poor = if poor {
                checks.poor.saturating_add(1)
            } else {
                0
            };
            let business = active_business(building, &infos, sim.world());
            if checks.staffing_company != business.map(|role| role.company) {
                checks.staffing_company = business.map(|role| role.company);
                checks.was_staffed = false;
                checks.low_staffing_hours = 0;
            }
            if let Some(role) = business {
                let staffing = role.rates().staffed_fraction;
                checks.was_staffed |= staffing >= 0.5;
                checks.low_staffing_hours = if checks.was_staffed && staffing < 0.2 {
                    checks.low_staffing_hours.saturating_add(1)
                } else {
                    0
                };
            } else {
                checks.was_staffed = false;
                checks.low_staffing_hours = 0;
            }
            let staffing_lost = checks.low_staffing_hours >= STAFFING_LOSS_HOURS;
            if building
                .connected_road
                .is_none_or(|road| map.get(road).is_none())
                || checks.outage_streak >= outage_limit
                || checks.poor >= 24
                || (staffing_lost && !building.is_mixed_use())
            {
                abandoned.push((id, building.door_pos));
            } else if site.business == Some(GrowableBusinessState::Open)
                && (business.is_none() || staffing_lost)
            {
                close.push((id, business.map(|role| role.company)));
                checks.was_staffed = false;
                checks.low_staffing_hours = 0;
            }
            checks_by_id.push((id, checks));
        }
        drop(homes);
        sim.write::<Residences>().checks.extend(checks_by_id);
    }

    // Mark every whole-site loss before evictions can look for a vacancy.
    abandoned.sort_unstable_by_key(|&(id, _)| id);
    for &(id, _) in &abandoned {
        sim.map_mut().mark_growable_abandoned(id, now);
    }
    for &(id, _) in &abandoned {
        let owner = sim.read::<BuildingInfos>().owner(id);
        if let Some(SoulID::GoodsCompany(company)) = owner {
            close_business(sim, company);
        }
    }
    close.sort_unstable_by_key(|&(id, _)| id);
    for (id, company) in close {
        if let Some(company) = company {
            close_business(sim, company);
        } else {
            sim.map_mut().set_growable_business_state(
                id,
                GrowableBusinessState::ClosedUntil {
                    retry_elapsed_tick: now.saturating_add(BUSINESS_RETRY_TICKS),
                },
            );
            sim.write::<BuildingInfos>().clear_owner(id);
        }
    }
    residence::settle_abandonment(sim, &mut abandoned, legacy_abandoned);
    collapse.sort_unstable();
    for id in collapse {
        remove_site(sim, id, RemovalCause::Collapse);
    }

    let retail = retail_index(sim);
    let ready = {
        let map = sim.map();
        let homes = sim.read::<Residences>();
        let infos = sim.read::<BuildingInfos>();
        let context = Assessment {
            map: &map,
            infos: &infos,
            residences: &homes,
            world: sim.world(),
            coverage: &sim.read::<CoverageGrid>(),
            environment: &sim.read::<EnvGrid>(),
        };
        let mut services = BTreeMap::new();
        let mut streaks = Vec::new();
        let mut ready = Vec::new();
        for building in map.buildings().values().filter(|building| {
            building.development().is_some() && !building.is_abandoned_growable()
        }) {
            let id = building.id;
            let level = building.development().unwrap().level;
            if level >= max_level(building, housing_limit, firm_limit) {
                continue;
            }
            let next = level + 1;
            let business = active_business(building, &infos, sim.world());
            let services = network_services(sim, &map, building, &mut services);
            let eligible = context
                .requirements(
                    building,
                    business,
                    next,
                    services,
                    retail.has_access(building.door_pos.xy()),
                )
                .iter()
                .all(|requirement| requirement.met);
            let streak = if eligible {
                homes
                    .checks
                    .get(&id)
                    .map_or(1, |checks| checks.upgrade_streak.saturating_add(1))
            } else {
                0
            };
            if streak >= UPGRADE_CHECKS {
                streaks.push((id, 0));
                if let Some(upgrade) = prepare_upgrade(&context, building, business, next) {
                    ready.push((id, upgrade));
                }
            } else {
                streaks.push((id, streak));
            }
        }
        drop(homes);
        let mut homes = sim.write::<Residences>();
        for (id, streak) in streaks {
            homes.checks.entry(id).or_default().upgrade_streak = streak;
        }
        ready
    };

    let mut ready = ready;
    ready.sort_unstable_by_key(|(id, _)| *id);
    let mut reservations = NetworkReservations::new(&sim.map());
    for (id, upgrade) in ready {
        if reservations.try_reserve(sim, id, upgrade.load).is_ok() {
            upgrade.commit(sim, id);
        }
    }
    retail
}

#[cfg(test)]
mod tests {
    use crate::economy::balance::CITY_UPDATE_INTERVAL_TICKS;
    use crate::map::{BuildingID, BuildingKind, Density, Lot, RoadID, ZoneCell, ZoneKind};
    use crate::map_dynamic::{
        electricity_flow_system, env_update, utility_flow_update, BuildingInfos, CoverageGrid,
        EnvGrid, UtilityFlow,
    };
    use crate::souls::desire::{Work, WorkKind};
    use crate::souls::goods_company::company_soul;
    use crate::souls::human::spawn_human;
    use crate::souls::{add_souls_to_empty_buildings, residence};
    use crate::tests::TestCtx;
    use crate::{CompanyID, Simulation, SoulID};
    use geom::{vec2, vec3, OBB};
    use prototypes::{GameTime, GoodsCompanyID, ServiceKind, Tick, TICKS_PER_HOUR};

    // Staged prototype data must not replace the registry of other unit tests.
    fn with_profiles(name: &str, test: impl FnOnce()) {
        const CHILD: &str = "EGREGORIA_Z2_PROFILE_TEST";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            test();
        } else {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
                .env(CHILD, name)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    fn street() -> (TestCtx, RoadID) {
        street_with_prototypes("")
    }

    fn street_with_prototypes(overrides: &str) -> (TestCtx, RoadID) {
        let test = TestCtx::new_fast();
        let mut lua = String::from(
            r#"
            package.path = "../base_mod/?.lua"
            zones2_profiles_enabled = true
            require("data")
        "#,
        );
        if !mixed_enabled() {
            lua.push_str(
                r#"
                for i = #data, 1, -1 do
                    if data[i].zone_kind == "mixed-use" then
                        table.remove(data, i)
                    end
                end
            "#,
            );
        }
        lua.push_str(overrides);
        prototypes::test_prototypes(&lua);
        test.build_roads(&[vec3(70.0, 100.0, 0.0), vec3(830.0, 100.0, 0.0)]);
        let road = test
            .g
            .map()
            .roads()
            .values()
            .find(|road| {
                road.points()
                    .project_2d(vec2(450.0, 100.0))
                    .xy()
                    .distance2(vec2(450.0, 100.0))
                    < 1.0
            })
            .unwrap()
            .id;
        (test, road)
    }

    fn mixed_enabled() -> bool {
        std::env::var_os("EGREGORIA_Z2_STAGED_MIXED_USE").is_some()
    }

    fn place(sim: &mut Simulation, zone: ZoneKind, x: f32) -> BuildingID {
        place_at_density(sim, zone, Density::Medium, x)
    }

    fn place_at_density(
        sim: &mut Simulation,
        zone: ZoneKind,
        density: Density,
        x: f32,
    ) -> BuildingID {
        let cell = ZoneCell {
            kind: zone,
            density,
        };
        let lot = {
            let map = sim.map();
            map.lots()
                .values()
                .filter(|lot| !Lot::footprint_obstructed(&map, lot.shape, false))
                .min_by_key(|lot| lot.shape.center().distance2(vec2(x, 125.0)) as i32)
                .unwrap()
                .id
        };
        sim.place_zoned_growable(lot, cell).unwrap().building
    }

    fn provider(sim: &mut Simulation, road: RoadID, name: &str, x: f32) -> CompanyID {
        let proto = GoodsCompanyID::new(name).prototype();
        let pos = {
            let map = sim.map();
            vec2(x, 100.0 - map.get(road).unwrap().width * 0.5 - 10.25)
        };
        let building = sim
            .map_mut()
            .build_special_building(
                &OBB::new(pos, -geom::Vec2::Y, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto.id),
                proto.bgen,
                None,
                Some(road),
            )
            .unwrap();
        sim.write::<BuildingInfos>().insert(building);
        let Some(SoulID::GoodsCompany(company)) = company_soul(sim, building, proto.id) else {
            panic!("could not spawn {name}");
        };
        company
    }

    #[test]
    fn zones2_z2_idle_grid_serves_homes_until_the_live_pump_closes() {
        use crate::souls::happiness::{happiness_update, Happiness, UPDATE_OFFSET};

        with_profiles(
            concat!(
                module_path!(),
                "::zones2_z2_idle_grid_serves_homes_until_the_live_pump_closes"
            ),
            || {
                let (mut test, road) = street_with_prototypes(
                    r#"
                    for _, proto in ipairs(data) do
                        if proto.type == "residence" or proto.name == "water-pump" then
                            proto.power_consumption = 0
                        end
                    end
                    "#,
                );
                let sim = &mut test.g;
                let home = place(sim, ZoneKind::Residential, 240.0);
                let worker = spawn_human(sim, home).unwrap();
                let pump = provider(sim, road, "water-pump", 450.0);
                let outlet = provider(sim, road, "sewage-outlet", 510.0);
                for company in [pump, outlet] {
                    sim.world.companies[company].workers.0.push(worker);
                }
                sim.write::<GameTime>().tick.0 = TICKS_PER_HOUR + UtilityFlow::UPDATE_OFFSET;
                let (world, resources) = sim.world_res();
                electricity_flow_system(world, resources);
                utility_flow_update(sim);
                assert!(sim.read::<UtilityFlow>().has_water(home));
                assert!(sim.read::<UtilityFlow>().has_sewage(home));

                let status = super::requirements(sim, home).unwrap();
                for (name, met) in [("Power", false), ("Water", true), ("Sewage", true)] {
                    assert_eq!(
                        status
                            .requirements
                            .iter()
                            .find(|r| r.name == name)
                            .unwrap()
                            .met,
                        met,
                        "{name} requirement on an idle grid"
                    );
                }
                sim.write::<GameTime>().tick.0 = TICKS_PER_HOUR + UPDATE_OFFSET;
                happiness_update(sim);
                let served = sim.read::<Happiness>().score(home).unwrap();
                for hour in 2..=13 {
                    sim.write::<GameTime>().tick.0 = hour * TICKS_PER_HOUR - 2;
                    residence::residence_update(sim);
                }
                assert!(!sim.map().get(home).unwrap().is_abandoned_growable());

                crate::souls::lifecycle::despawn_company(sim, pump);
                sim.apply_commands([]);
                assert!(sim.get(pump).is_none());
                assert!(sim.read::<UtilityFlow>().has_water(home));
                sim.write::<GameTime>().tick.0 = 13 * TICKS_PER_HOUR + UPDATE_OFFSET;
                happiness_update(sim);
                assert_eq!(sim.read::<Happiness>().score(home).unwrap(), served - 10);
            },
        );
    }

    #[test]
    fn zones2_z2_vacancy_admits_both_roles_once() {
        with_profiles(
            concat!(module_path!(), "::zones2_z2_vacancy_admits_both_roles_once"),
            || {
                let (mut test, mixed) = if mixed_enabled() {
                    let (mut test, _) = street();
                    let mixed = place(&mut test.g, ZoneKind::MixedUse, 280.0);
                    (test, mixed)
                } else {
                    let (test, mixed, _) = crate::map_dynamic::accounting_site(true);
                    (test, mixed)
                };
                test.set_residential_demand(50.0);
                add_souls_to_empty_buildings(&mut test.g);
                let first = test.g.growable_summary(mixed).unwrap();
                assert_eq!(first.active_job_slots, 12);
                assert_eq!(
                    first.occupants, 1,
                    "the shop must not consume the home vacancy"
                );
                let owner = test.g.read::<BuildingInfos>().owner(mixed).unwrap();
                for _ in 0..16 {
                    add_souls_to_empty_buildings(&mut test.g);
                }
                let full = test.g.growable_summary(mixed).unwrap();
                assert_eq!(full.occupants, full.housing_slots as usize);
                assert_eq!(test.g.read::<BuildingInfos>().owner(mixed), Some(owner));
                assert_eq!(test.g.world.companies.len(), 1);
                let proto = test
                    .g
                    .map()
                    .get(mixed)
                    .unwrap()
                    .kind
                    .as_goods_company()
                    .unwrap();
                assert!(company_soul(&mut test.g, mixed, proto).is_none());
                if mixed_enabled() {
                    let other = place(&mut test.g, ZoneKind::MixedUse, 345.0);
                    test.g.map_mut().set_growable_business_state(
                        other,
                        super::GrowableBusinessState::ClosedUntil {
                            retry_elapsed_tick: u64::MAX,
                        },
                    );
                    add_souls_to_empty_buildings(&mut test.g);
                    assert_eq!(test.g.growable_summary(other).unwrap().occupants, 1);
                    assert_eq!(test.g.read::<BuildingInfos>().owner(other), None);
                    test.g.map_mut().set_growable_business_state(
                        other,
                        super::GrowableBusinessState::AwaitingSpawn,
                    );
                    add_souls_to_empty_buildings(&mut test.g);
                    assert!(matches!(
                        test.g.read::<BuildingInfos>().owner(other),
                        Some(SoulID::GoodsCompany(_))
                    ));
                    assert_eq!(test.g.growable_summary(other).unwrap().occupants, 2);
                }
            },
        );
    }

    #[test]
    fn zones2_z2_downstairs_deposit_is_local_and_the_closed_shop_leaves_housing() {
        use crate::economy::{DailyEconomyMetrics, Market, TripPurpose};
        use crate::map::GrowableBusinessState;
        use crate::map_dynamic::ActiveRetailIndex;
        use crate::transportation::Location;
        use prototypes::ItemID;

        with_profiles(
            concat!(
                module_path!(),
                "::zones2_z2_downstairs_deposit_is_local_and_the_closed_shop_leaves_housing"
            ),
            || {
                let (mut test, mixed, company) = if mixed_enabled() {
                    let (mut test, _) = street();
                    let mixed = place(&mut test.g, ZoneKind::MixedUse, 280.0);
                    add_souls_to_empty_buildings(&mut test.g);
                    let Some(SoulID::GoodsCompany(company)) =
                        test.g.read::<BuildingInfos>().owner(mixed)
                    else {
                        panic!("expected an open storefront");
                    };
                    (test, mixed, company)
                } else {
                    crate::map_dynamic::accounting_site(true)
                };
                let shopper = spawn_human(&mut test.g, mixed).unwrap();
                let door = test.g.map().get(mixed).unwrap().door_pos.xy();
                let access = |sim: &Simulation| {
                    ActiveRetailIndex::build(&sim.map(), &sim.read::<BuildingInfos>(), sim.world())
                        .has_access(door)
                };
                assert!(!access(&test.g));
                test.g.world.companies[company].workers.0.push(shopper);
                assert!(
                    access(&test.g),
                    "a staffed shop with empty shelves provides retail access"
                );
                let groceries = ItemID::new("groceries");
                let seller = SoulID::GoodsCompany(company);
                {
                    let mut market = test.g.write::<Market>();
                    market.produce(seller, groceries, 3);
                    market.sell_all(seller, door, groceries, 0);
                }
                test.g.world.edit_human(shopper, |human| {
                    human.household.next_shop = 0;
                    human.decision.wait = 0;
                });
                test.tick_fast();
                let human = test.g.get(shopper).unwrap();
                assert_eq!(human.location, Location::Building(mixed));
                assert_eq!(human.household.on_hand.get(&groceries), Some(&3));
                assert!(human.household.carried.is_empty());
                {
                    let metrics = test.g.read::<DailyEconomyMetrics>();
                    let day = &metrics.days[&0];
                    assert_eq!(day.local_pickups, 1);
                    assert!(!day.trips_by_purpose.contains_key(&TripPurpose::Shopping));
                }
                assert!(super::close_business(&mut test.g, company));
                assert!(!super::close_business(&mut test.g, company));
                assert!(!access(&test.g));
                test.g.apply_commands([]);
                test.set_residential_demand(50.0);
                add_souls_to_empty_buildings(&mut test.g);
                let unowned = test.g.growable_summary(mixed).unwrap();
                assert_eq!(unowned.occupants, 2);
                assert_eq!(unowned.active_job_slots, 0);
                assert_eq!(test.g.read::<BuildingInfos>().owner(mixed), None);
                assert_eq!(
                    test.g
                        .get(shopper)
                        .unwrap()
                        .household
                        .on_hand
                        .get(&groceries),
                    Some(&3)
                );
                if mixed_enabled() {
                    let Some(GrowableBusinessState::ClosedUntil { retry_elapsed_tick }) =
                        unowned.business
                    else {
                        panic!("expected a retry timer");
                    };
                    test.g.write::<GameTime>().tick.0 =
                        retry_elapsed_tick + crate::INITIAL_TICK - 1;
                    add_souls_to_empty_buildings(&mut test.g);
                    assert_eq!(test.g.read::<BuildingInfos>().owner(mixed), None);
                    test.g.write::<GameTime>().tick.0 += 1;
                    add_souls_to_empty_buildings(&mut test.g);
                    let Some(SoulID::GoodsCompany(reopened)) =
                        test.g.read::<BuildingInfos>().owner(mixed)
                    else {
                        panic!("expected the vacancy pass to reopen the storefront");
                    };
                    assert_ne!(reopened, company);
                    assert_eq!(test.g.growable_summary(mixed).unwrap().active_job_slots, 12);
                }
            },
        );
    }

    #[test]
    fn zones2_z2_collapse_is_due_after_twelve_elapsed_hours() {
        use crate::map::GrowableSiteState;

        with_profiles(
            concat!(
                module_path!(),
                "::zones2_z2_collapse_is_due_after_twelve_elapsed_hours"
            ),
            || {
                let (mut test, _) = street();
                let use_kind = if mixed_enabled() {
                    ZoneKind::MixedUse
                } else {
                    ZoneKind::Residential
                };
                let id = place(&mut test.g, use_kind, 280.0);
                add_souls_to_empty_buildings(&mut test.g);
                let occupant = spawn_human(&mut test.g, id).unwrap();
                let parcel = test.g.map().get(id).unwrap().obb;
                let run_hour = |sim: &mut Simulation, hour| {
                    sim.write::<GameTime>().tick.0 = hour * TICKS_PER_HOUR - 2;
                    residence::residence_update(sim);
                    sim.apply_commands([]);
                };
                for hour in 1..12 {
                    run_hour(&mut test.g, hour);
                }
                assert!(test.g.get(occupant).is_some());
                assert_eq!(
                    test.g.growable_summary(id).unwrap().site,
                    GrowableSiteState::Active
                );
                run_hour(&mut test.g, 12);
                assert!(matches!(
                    test.g.growable_summary(id).unwrap().site,
                    GrowableSiteState::Abandoned { .. },
                ));
                assert!(test.g.get(occupant).is_none());
                assert_eq!(test.g.growable_summary(id).unwrap().active_job_slots, 0);
                run_hour(&mut test.g, 23);
                assert!(test.g.map().get(id).is_some());
                run_hour(&mut test.g, 24);
                assert!(test.g.map().get(id).is_none());
                assert!(!super::remove_site(
                    &mut test.g,
                    id,
                    super::RemovalCause::Collapse
                ));
                assert_eq!(
                    test.g
                        .map()
                        .lots()
                        .values()
                        .filter(|lot| lot.shape.corners == parcel.corners)
                        .count(),
                    1
                );
            },
        );
    }

    #[test]
    fn zones2_z2_capped_firm_advances_without_reposting_openings() {
        use crate::economy::{budget_update, Budget, BudgetCategory, Market};
        use crate::map_dynamic::active_business;
        use prototypes::ItemID;

        with_profiles(
            concat!(
                module_path!(),
                "::zones2_z2_capped_firm_advances_without_reposting_openings"
            ),
            || {
                let (mut test, _) = street();
                let sim = &mut test.g;
                let id = place_at_density(sim, ZoneKind::Commercial, Density::Low, 280.0);
                add_souls_to_empty_buildings(sim);
                let Some(seller @ SoulID::GoodsCompany(company)) =
                    sim.read::<BuildingInfos>().owner(id)
                else {
                    panic!("expected an active firm");
                };
                let home = place(sim, ZoneKind::Residential, 350.0);
                let worker = spawn_human(sim, home).unwrap();
                sim.world.companies[company].workers.0.push(worker);
                sim.world.companies[company].comp.progress = 0.5;
                let jobs = ItemID::new("job-opening");
                let stock = ItemID::new("groceries");
                {
                    let mut market = sim.write::<Market>();
                    market.produce(seller, jobs, -1);
                    market.produce(seller, stock, 2);
                }
                let upgrade = |sim: &mut Simulation, level| {
                    let prepared = {
                        let map = sim.map();
                        let infos = sim.read::<BuildingInfos>();
                        let homes = sim.read::<residence::Residences>();
                        let building = map.get(id).unwrap();
                        let assessment = super::Assessment {
                            map: &map,
                            infos: &infos,
                            residences: &homes,
                            world: sim.world(),
                            coverage: &sim.read::<CoverageGrid>(),
                            environment: &sim.read::<EnvGrid>(),
                        };
                        super::prepare_upgrade(
                            &assessment,
                            building,
                            active_business(building, &infos, sim.world()),
                            level,
                        )
                        .unwrap()
                    };
                    let delta = prepared.opening.unwrap().1;
                    prepared.commit(sim, id);
                    delta
                };
                let tax = |sim: &mut Simulation| {
                    let category = BudgetCategory::Tax(ZoneKind::Commercial);
                    let before = sim.read::<Budget>().total(category);
                    sim.write::<GameTime>().tick.0 += TICKS_PER_HOUR;
                    let (world, res) = sim.world_res();
                    budget_update(world, res);
                    sim.read::<Budget>().total(category) - before
                };
                let nominal = sim.growable_summary(id).unwrap().planned_jobs;
                assert_eq!(upgrade(sim, 2), nominal as i32);
                let capped = sim.growable_summary(id).unwrap();
                assert_eq!(capped.planned_jobs, 2 * nominal);
                let openings = sim.read::<Market>().inner()[&jobs]
                    .sell_order(seller)
                    .unwrap()
                    .qty;
                let level_two_tax = tax(sim);
                assert!(level_two_tax > 0);
                assert_eq!(upgrade(sim, 3), 0);
                let promoted = sim.growable_summary(id).unwrap();
                assert_eq!(promoted.level, 3);
                assert_eq!(promoted.planned_jobs, capped.planned_jobs);
                assert_eq!(promoted.floors, capped.floors);
                assert!(promoted.roof_z > capped.roof_z);
                assert!((2 * tax(sim) - 3 * level_two_tax).abs() <= 5);
                let entity = &sim.world.companies[company];
                assert_eq!(entity.workers.0, [worker]);
                assert_eq!(entity.comp.progress, 0.5);
                let market = sim.read::<Market>();
                assert_eq!(
                    market.inner()[&jobs].sell_order(seller).unwrap().qty,
                    openings
                );
                assert_eq!(market.capital(seller, stock), 2);
            },
        );
    }

    #[test]
    fn zones2_z2_progression_reaches_l3_with_a_matched_health_control() {
        with_profiles(
            concat!(
                module_path!(),
                "::zones2_z2_progression_reaches_l3_with_a_matched_health_control"
            ),
            || {
                let (mut test, road) = street();
                let sim = &mut test.g;
                let pure = place(sim, ZoneKind::Residential, 240.0);
                let mixed = mixed_enabled().then(|| place(sim, ZoneKind::MixedUse, 285.0));
                let control = place(
                    sim,
                    if mixed_enabled() {
                        ZoneKind::MixedUse
                    } else {
                        ZoneKind::Residential
                    },
                    330.0,
                );
                let retailer = mixed.unwrap_or_else(|| place(sim, ZoneKind::Commercial, 285.0));
                let homes: Vec<_> = [Some(pure), mixed, Some(control)]
                    .into_iter()
                    .flatten()
                    .collect();
                let pump = provider(sim, road, "water-pump", 450.0);
                let sewage = provider(sim, road, "sewage-outlet", 510.0);
                provider(sim, road, "solar-panel", 570.0);
                add_souls_to_empty_buildings(sim);
                let owner = |sim: &Simulation, id| {
                    let Some(SoulID::GoodsCompany(company)) = sim.read::<BuildingInfos>().owner(id)
                    else {
                        panic!("expected a live company at {id:?}");
                    };
                    company
                };
                let shop = owner(sim, retailer);
                let control_shop = if mixed_enabled() {
                    owner(sim, control)
                } else {
                    shop
                };
                for &id in &homes {
                    let mut coverage = sim.write::<CoverageGrid>();
                    for kind in [ServiceKind::School, ServiceKind::Health, ServiceKind::Park] {
                        let score = if id == control && kind == ServiceKind::Health {
                            29.0
                        } else {
                            100.0
                        };
                        coverage.by_kind.entry(kind).or_default().insert(id, score);
                    }
                }
                let entry = sim.map().get(pure).unwrap().door_pos;
                let initial_roof = sim.growable_summary(pure).unwrap().roof_z;
                let run_hour = |sim: &mut Simulation, hour, late| {
                    for &id in &homes {
                        let employer = if id == control { control_shop } else { shop };
                        while let Some(human) = spawn_human(sim, id) {
                            let (world, _) = sim.world_res();
                            let workplace = world.companies[employer].comp.building;
                            let mut work = Work::new(workplace, WorkKind::Worker, 0.0);
                            work.employer = Some(employer);
                            world.edit_human(human, |human| human.work = Some(work));
                            if world.companies[employer].workers.0.len() < 1 {
                                world.companies[employer].workers.0.push(human);
                            }
                        }
                    }
                    let worker = sim.world.humans.keys().next().unwrap();
                    for company in [pump, sewage] {
                        sim.world.companies[company].workers.0 = vec![worker];
                    }
                    let boundary = hour * TICKS_PER_HOUR - CITY_UPDATE_INTERVAL_TICKS;
                    *sim.write::<GameTime>() =
                        GameTime::new(Tick(boundary + UtilityFlow::UPDATE_OFFSET));
                    let (world, resources) = sim.world_res();
                    electricity_flow_system(world, resources);
                    utility_flow_update(sim);
                    *sim.write::<GameTime>() =
                        GameTime::new(Tick(boundary + EnvGrid::UPDATE_OFFSET));
                    env_update(sim);
                    *sim.write::<GameTime>() = GameTime::new(Tick(hour * TICKS_PER_HOUR - 2));
                    if late {
                        let phase = hour * TICKS_PER_HOUR - 2;
                        super::update_with_limits(sim, phase, Vec::new(), 5, 1);
                        super::update_with_limits(sim, phase, Vec::new(), 5, 1);
                    } else {
                        residence::residence_update(sim);
                    }
                };
                for hour in 1..=12 {
                    run_hour(sim, hour, false);
                }
                assert_eq!(sim.growable_summary(pure).unwrap().level, 3);
                assert!(sim.growable_summary(pure).unwrap().roof_z > initial_roof);
                assert_eq!(sim.map().get(pure).unwrap().door_pos, entry);
                if let Some(mixed) = mixed {
                    let upgraded = sim.growable_summary(mixed).unwrap();
                    assert_eq!(upgraded.level, 3);
                    assert_eq!(upgraded.planned_jobs, 12);
                    assert_eq!(owner(sim, mixed), shop);
                }
                assert_eq!(sim.growable_summary(control).unwrap().level, 2);
                let unmet: Vec<_> = residence::requirements(sim, control)
                    .into_iter()
                    .filter(|requirement| !requirement.met)
                    .map(|requirement| requirement.name)
                    .collect();
                assert_eq!(unmet, ["Health coverage"]);
                let resident = sim
                    .write::<BuildingInfos>()
                    .get(pure)
                    .unwrap()
                    .residents
                    .iter()
                    .find_map(|soul| match soul {
                        SoulID::Human(id) => Some(*id),
                        _ => None,
                    })
                    .unwrap();
                for hour in 13..=35 {
                    run_hour(sim, hour, true);
                }
                assert_eq!(sim.get(resident).unwrap().household.education_hours(), 23);
                assert_eq!(sim.growable_summary(pure).unwrap().level, 3);
                for hour in 36..=48 {
                    run_hour(sim, hour, true);
                }
                assert_eq!(sim.growable_summary(pure).unwrap().level, 5);
                assert_eq!(sim.growable_summary(control).unwrap().level, 2);
                if let Some(mixed) = mixed {
                    assert_eq!(sim.growable_summary(mixed).unwrap().level, 5);
                    assert_eq!(sim.growable_summary(mixed).unwrap().planned_jobs, 12);
                }
            },
        );
    }
}
