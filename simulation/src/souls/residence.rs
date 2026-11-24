use crate::economy::balance::{periodic_tick, RETAIL_ACCESS_RADIUS_M};
use crate::economy::Budget;
use crate::map::{BuildingID, BuildingKind, Density, Map};
use crate::map_dynamic::{BuildingInfos, CoverageGrid, ElectricityFlow, EnvGrid, UtilityFlow};
use crate::souls::happiness::Happiness;
use crate::souls::lifecycle;
use crate::{Simulation, SoulID};
use geom::{Vec2, Vec3};
use ordered_float::OrderedFloat;
use prototypes::{
    try_prototype, CompanyKind, GrowableChainKey, GrowableZoneKind, ItemID, ResidencePrototype,
    ResidencePrototypeID, ServiceKind, Tick, TICKS_PER_HOUR,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

const UPGRADE_CHECKS: u8 = 6;
const UPGRADE_COVERAGE: f32 = 30.0;
const UPGRADE_LAND_VALUE: f32 = 50.0;
const UNSERVED_CHECKS: u8 = 12;
pub(crate) const UPDATE_OFFSET: u64 = TICKS_PER_HOUR - 2;

#[derive(Serialize, Deserialize)]
pub struct Residence {
    pub prototype: ResidencePrototypeID,
    pub density: Density,
    pub level: u8,
    pub capacity: u32,
}

impl From<&ResidencePrototype> for Residence {
    fn from(proto: &ResidencePrototype) -> Self {
        Self {
            prototype: proto.id,
            density: proto.density,
            level: proto.level,
            capacity: proto.capacity,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ResidenceChecks {
    upgrade_streak: u8,
    outage_streak: u8,
    poor: u8,
    abandoned_hours: Option<u8>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Residences {
    #[serde(rename = "by_building")]
    entries: ResidenceEntries,
    checks: BTreeMap<BuildingID, ResidenceChecks>,
    #[serde(skip, default = "next_residence_generation")]
    residence_generation: u64,
}

/// Read access for callers that use `residences.by_building`.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResidenceEntries {
    pub by_building: BTreeMap<BuildingID, Residence>,
}

impl Deref for Residences {
    type Target = ResidenceEntries;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

// Existing unit fixtures edit individual residence fields directly.
#[cfg(test)]
impl std::ops::DerefMut for Residences {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.residence_generation = next_residence_generation();
        &mut self.entries
    }
}

fn next_residence_generation() -> u64 {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

impl Residences {
    pub(crate) fn generation(&self) -> u64 {
        self.residence_generation
    }

    pub(crate) fn insert(&mut self, building: BuildingID, residence: Residence) {
        self.entries.by_building.insert(building, residence);
        self.residence_generation = next_residence_generation();
    }

    fn remove(&mut self, building: BuildingID) {
        if self.entries.by_building.remove(&building).is_some() {
            self.residence_generation = next_residence_generation();
        }
        self.checks.remove(&building);
    }

    pub(crate) fn upgrade(&mut self, building: BuildingID, proto: &ResidencePrototype) {
        let residence = self
            .entries
            .by_building
            .get_mut(&building)
            .expect("registered residence");
        *residence = proto.into();
        self.residence_generation = next_residence_generation();
    }

    pub fn is_abandoned(&self, building: BuildingID) -> bool {
        self.checks
            .get(&building)
            .is_some_and(|checks| checks.abandoned_hours.is_some())
    }

    /// Returns completed consecutive eligible hours and the required total for the next level.
    pub fn upgrade_progress(&self, building: BuildingID) -> Option<(u8, u8)> {
        let residence = self.by_building.get(&building)?;
        if self.is_abandoned(building) {
            return None;
        }
        next_prototype(residence)?;
        let completed = self
            .checks
            .get(&building)
            .map_or(0, |checks| checks.upgrade_streak);
        Some((completed, UPGRADE_CHECKS))
    }
}

pub(crate) fn prototype(density: Density, level: u8) -> Option<&'static ResidencePrototype> {
    let registry = prototypes::prototypes();
    registry
        .residence_for_chain(GrowableChainKey {
            zone_kind: GrowableZoneKind::default(),
            density,
            level,
        })
        .or_else(|| registry.legacy_residence(density, level))
}

pub fn on_house_built(sim: &Simulation, building: BuildingID) {
    if let Some(proto) = prototype(Density::Low, 1) {
        register(sim, building, proto);
    }
}

pub(crate) fn register(sim: &Simulation, building: BuildingID, proto: &ResidencePrototype) {
    sim.write::<Residences>().insert(building, proto.into());
    sim.write::<BuildingInfos>().mark_vacancy_changed(building);
}

pub fn on_building_removed(sim: &mut Simulation, building: BuildingID) {
    sim.write::<Residences>().remove(building);
    sim.write::<Happiness>().by_building.remove(&building);
}

pub(crate) fn evict_residents(sim: &mut Simulation, building: BuildingID, door: Vec3) {
    evict_from_homes(sim, &[(building, door)]);
}

pub(crate) fn abandon_orphaned_growables(sim: &mut Simulation, mut homes: Vec<(BuildingID, Vec3)>) {
    if homes.is_empty() {
        return;
    }
    {
        let mut residences = sim.write::<Residences>();
        homes.retain(|&(id, _)| {
            let hours = &mut residences.checks.entry(id).or_default().abandoned_hours;
            if hours.is_some() {
                return false;
            }
            *hours = Some(0);
            true
        });
    }
    homes.sort_unstable_by_key(|&(id, _)| id);
    evict_abandoned_homes(sim, &homes);
}

fn evict_abandoned_homes(sim: &mut Simulation, homes: &[(BuildingID, Vec3)]) {
    // Mark every home before looking for vacancies.
    if !homes.is_empty() {
        evict_from_homes(sim, homes);
        lifecycle::clear_abandoned_home_references(sim, homes);
    }
}

fn evict_from_homes(sim: &mut Simulation, homes: &[(BuildingID, Vec3)]) {
    let displaced = {
        let mut infos = sim.write::<BuildingInfos>();
        let mut displaced = Vec::new();
        for &(building, door) in homes {
            infos.mark_vacancy_changed(building);
            if let Some(info) = infos
                .get(building)
                .filter(|info| !info.residents.is_empty())
            {
                let residents = info.residents.clone();
                for &soul in &residents {
                    infos.remove_resident(soul);
                }
                displaced.push((door, residents));
            }
        }
        displaced
    };
    if displaced.is_empty() {
        return;
    }

    let mut vacancies = {
        let residences = sim.read::<Residences>();
        let map = sim.map();
        let infos = sim.read::<BuildingInfos>();
        let mut vacancies = BTreeMap::new();
        for (&id, residence) in &residences.by_building {
            if residences.is_abandoned(id) || homes.binary_search_by_key(&id, |&(id, _)| id).is_ok()
            {
                continue;
            }
            let Some(home) = map.get(id).filter(|home| !home.is_orphaned()) else {
                continue;
            };
            let Some(info) = infos.get(id) else {
                continue;
            };
            let free = (residence.capacity as usize).saturating_sub(info.residents.len());
            if free > 0 {
                vacancies.insert(id, (home.door_pos, free));
            }
        }
        vacancies
    };

    let mut nearest = Vec::new();
    let (world, resources) = sim.world_res();
    let mut infos = resources.write::<BuildingInfos>();
    let departures = resources.read::<crate::ParCommandBuffer<crate::HumanEnt>>();
    for (door, residents) in displaced {
        // Source homes and their residents arrive in ID order. Rank currently
        // vacant homes by distance, breaking ties by building ID.
        nearest.clear();
        nearest.extend(
            vacancies
                .iter()
                .map(|(&id, &(position, _))| (OrderedFloat(position.distance2(door)), id)),
        );
        nearest.sort_unstable();
        let mut destinations = nearest.iter().map(|&(_, id)| id).peekable();
        for soul in residents {
            let SoulID::Human(id) = soul else { continue };
            let Some(human) = world.humans.get_mut(id) else {
                continue;
            };
            if let Some(&home) = destinations.peek() {
                human.home.house = home;
                infos.add_resident(home, soul);
                let (_, free) = vacancies.get_mut(&home).unwrap();
                *free -= 1;
                if *free == 0 {
                    vacancies.remove(&home);
                    destinations.next();
                }
            } else {
                departures.kill(id);
            }
        }
    }
}

pub fn is_retail_store(kind: BuildingKind) -> bool {
    let Some(id) = kind.as_goods_company().filter(|id| !id.is_service()) else {
        return false;
    };
    let retail_goods = ItemID::new("retail-goods");
    try_prototype(id).is_some_and(|proto| {
        proto.kind == CompanyKind::Store
            && proto.recipe.as_ref().is_some_and(|recipe| {
                recipe
                    .production
                    .iter()
                    .any(|item| item.id == retail_goods && item.amount > 0)
            })
    })
}

pub fn retail_store_positions(map: &Map) -> Vec<Vec2> {
    map.buildings()
        .values()
        .filter(|building| is_retail_store(building.kind))
        .map(|building| building.door_pos.xy())
        .collect()
}

pub fn has_retail_access(stores: &[Vec2], door: Vec2) -> bool {
    stores
        .iter()
        .any(|store| store.distance2(door) <= RETAIL_ACCESS_RADIUS_M.powi(2))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerStatus {
    Available,
    MissingNetwork,
    Blackout,
}

/// State of the electrical graph at this building. A disconnected building
/// can still have its own isolated node in the graph.
pub fn power_status(map: &Map, electricity: &ElectricityFlow, building: BuildingID) -> PowerStatus {
    match map.electricity.net_id(building) {
        None => PowerStatus::MissingNetwork,
        Some(network) if electricity.blackout(network) => PowerStatus::Blackout,
        Some(_) => PowerStatus::Available,
    }
}

pub fn has_power(map: &Map, electricity: &ElectricityFlow, building: BuildingID) -> bool {
    power_status(map, electricity, building) == PowerStatus::Available
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Requirement {
    pub name: &'static str,
    pub met: bool,
    pub value: f32,
    pub threshold: f32,
}

impl Requirement {
    fn available(name: &'static str, met: bool) -> Self {
        Self {
            name,
            met,
            value: f32::from(met),
            threshold: 1.0,
        }
    }

    fn minimum(name: &'static str, value: f32, threshold: f32) -> Self {
        Self {
            name,
            met: value >= threshold,
            value,
            threshold,
        }
    }

    fn percent(name: &'static str, count: usize, total: usize, threshold: usize) -> Self {
        Self {
            name,
            met: total > 0 && count * 100 >= total * threshold,
            value: count as f32 * 100.0 / total.max(1) as f32,
            threshold: threshold as f32,
        }
    }
}

struct Progression<'a> {
    map: &'a Map,
    electricity: &'a ElectricityFlow,
    utilities: &'a UtilityFlow,
    coverage: &'a CoverageGrid,
    environment: &'a EnvGrid,
    stores: &'a [Vec2],
}

impl Progression<'_> {
    fn requirements(
        &self,
        building: &crate::map::Building,
        next_level: u8,
        occupied: usize,
        employed: usize,
        capacity: usize,
    ) -> impl Iterator<Item = Requirement> {
        let id = building.id;
        let coverage = |name, kind| {
            Requirement::minimum(name, self.coverage.coverage(id, kind), UPGRADE_COVERAGE)
        };
        let school = coverage("School coverage", ServiceKind::School);
        let health = coverage("Health coverage", ServiceKind::Health);
        let land_value = Requirement::minimum(
            "Land value",
            f32::from(self.environment.land_value(id)),
            UPGRADE_LAND_VALUE,
        );
        let amenities = if next_level >= 3 {
            [Some(school), Some(health), Some(land_value)]
        } else {
            let park = coverage("Park coverage", ServiceKind::Park);
            // Show the closest option and its actual score alongside the list of alternatives.
            let closest = [health, park, land_value].into_iter().fold(school, |best, option| {
                if option.value / option.threshold > best.value / best.threshold {
                    option
                } else {
                    best
                }
            });
            [
                Some(Requirement::available(
                    "Level 2 needs 30% school, health or park coverage, or 50 land value",
                    closest.met,
                )),
                Some(closest),
                None,
            ]
        };
        [
            Requirement::available("Power", has_power(self.map, self.electricity, id)),
            Requirement::available("Water", self.utilities.has_water(id)),
            Requirement::available("Sewage", self.utilities.has_sewage(id)),
            Requirement::percent("Occupancy", occupied, capacity, 80),
            Requirement::percent("Employment", employed, occupied, 70),
            Requirement::available(
                "Retail access",
                has_retail_access(self.stores, building.door_pos.xy()),
            ),
        ]
        .into_iter()
        .chain(amenities.into_iter().flatten())
    }
}

fn employed_residents(sim: &Simulation, residents: &BTreeSet<SoulID>) -> usize {
    residents
        .iter()
        .filter(|&&soul| {
            matches!(soul, SoulID::Human(human) if sim.get(human).is_some_and(|h| h.work.is_some()))
        })
        .count()
}

fn next_prototype(residence: &Residence) -> Option<&'static ResidencePrototype> {
    residence
        .level
        .checked_add(1)
        .and_then(|level| prototype(residence.density, level))
}

/// Requirements for a residence's next level. Coverage and resident ratios are
/// percentages, land value is on its 0–100 scale, and availability is 0 or 1.
pub fn requirements(sim: &Simulation, building: BuildingID) -> Vec<Requirement> {
    requirements_with_stores(sim, building, None)
}

pub(crate) fn requirements_with_stores(
    sim: &Simulation,
    building: BuildingID,
    stores: Option<&[Vec2]>,
) -> Vec<Requirement> {
    let residences = sim.read::<Residences>();
    let Some(residence) = residences.by_building.get(&building) else {
        return Vec::new();
    };
    let Some(next) = next_prototype(residence) else {
        return Vec::new();
    };
    let map = sim.map();
    let infos = sim.read::<BuildingInfos>();
    let Some((building, info)) = map.get(building).zip(infos.get(building)) else {
        return Vec::new();
    };
    let positions;
    let stores = match stores {
        Some(stores) => stores,
        None => {
            positions = retail_store_positions(&map);
            &positions
        }
    };
    Progression {
        map: &map,
        electricity: &sim.read::<ElectricityFlow>(),
        utilities: &sim.read::<UtilityFlow>(),
        coverage: &sim.read::<CoverageGrid>(),
        environment: &sim.read::<EnvGrid>(),
        stores,
    }
    .requirements(
        building,
        next.level,
        info.residents.len(),
        employed_residents(sim, &info.residents),
        residence.capacity as usize,
    )
    .collect()
}

/// Whether the residence's current prototype consumes water.
pub fn uses_water(sim: &Simulation, building: BuildingID) -> bool {
    sim.read::<Residences>()
        .by_building
        .get(&building)
        .and_then(|residence| try_prototype(residence.prototype))
        .and_then(|proto| proto.water_consumption)
        .is_some_and(|consumption| consumption > 0.0)
}

pub(crate) const fn due(tick: u64) -> bool {
    tick % TICKS_PER_HOUR == UPDATE_OFFSET
}

pub fn residence_update(sim: &mut Simulation) {
    // Use the preceding elapsed-time utility and environment snapshots.
    if !due(periodic_tick(&sim.read::<Budget>(), Tick(sim.get_tick()))) {
        return;
    }

    let mut abandon = Vec::new();
    let mut collapse = Vec::new();
    let mut upgrades = Vec::new();
    {
        let map = sim.map();
        let infos = sim.read::<BuildingInfos>();
        let mut residences = sim.write::<Residences>();
        let progression = Progression {
            map: &map,
            electricity: &sim.read::<ElectricityFlow>(),
            utilities: &sim.read::<UtilityFlow>(),
            coverage: &sim.read::<CoverageGrid>(),
            environment: &sim.read::<EnvGrid>(),
            stores: &retail_store_positions(&map),
        };
        let Residences {
            entries, checks, ..
        } = &mut *residences;

        for (&id, residence) in &entries.by_building {
            let Some(building) = map.get(id) else {
                continue;
            };
            let Some(info) = infos.get(id) else { continue };
            let checks = checks.entry(id).or_default();
            if let Some(hours) = &mut checks.abandoned_hours {
                *hours = hours.saturating_add(1);
                if *hours >= 12 {
                    collapse.push(id);
                }
                continue;
            }

            let occupied = info.residents.len();
            let employed = employed_residents(sim, &info.residents);
            let capacity = residence.capacity as usize;
            let poor = map.lot_origin(id).is_some()
                && occupied * 5 < capacity
                && (occupied == 0 || employed * 10 < occupied * 3);
            checks.poor = if poor {
                checks.poor.saturating_add(1)
            } else {
                0
            };
            let utilities_ok =
                progression.utilities.has_water(id) && progression.utilities.has_sewage(id);
            checks.outage_streak = if utilities_ok {
                0
            } else {
                checks.outage_streak.saturating_add(1)
            };
            if building.is_orphaned()
                || checks.poor >= 24
                || checks.outage_streak >= UNSERVED_CHECKS
            {
                checks.abandoned_hours = Some(0);
                abandon.push((id, building.door_pos));
                continue;
            }
            let Some(next) = next_prototype(residence).filter(|_| utilities_ok) else {
                checks.upgrade_streak = 0;
                continue;
            };
            let good = progression
                .requirements(building, next.level, occupied, employed, capacity)
                .all(|requirement| requirement.met);
            checks.upgrade_streak = if good {
                checks.upgrade_streak.saturating_add(1)
            } else {
                0
            };
            if checks.upgrade_streak >= UPGRADE_CHECKS {
                checks.upgrade_streak = 0;
                upgrades.push((id, next));
            }
        }
        for &(id, proto) in &upgrades {
            residences.upgrade(id, proto);
        }
    }

    evict_abandoned_homes(sim, &abandon);
    for building in collapse {
        super::growth::remove_growable(sim, building);
    }
    if !upgrades.is_empty() {
        let mut map = sim.map_mut();
        let mut infos = sim.write::<BuildingInfos>();
        for (id, proto) in upgrades {
            infos.mark_vacancy_changed(id);
            map.regenerate_house_mesh(id, proto.level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestCtx;
    use geom::{vec2, vec3};
    use prototypes::GameTime;

    #[test]
    fn qw1_residence_progress_reads_the_streak_and_hides_unavailable_upgrades() {
        let mut test = TestCtx::new();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
        let house = test.build_house_near(vec2(175.0, 125.0));
        let progress = |sim: &Simulation| sim.read::<Residences>().upgrade_progress(house);

        assert_eq!(progress(&test.g), Some((0, 6)));
        test.g
            .write::<Residences>()
            .checks
            .entry(house)
            .or_default()
            .upgrade_streak = 3;
        assert_eq!(progress(&test.g), Some((3, 6)));

        // The house lacks utilities, so the hourly check resets its saved streak.
        *test.g.write::<GameTime>() = GameTime::new(Tick(UPDATE_OFFSET));
        residence_update(&mut test.g);
        assert_eq!(progress(&test.g), Some((0, 6)));

        let mut residences = test.g.write::<Residences>();
        let checks = residences.checks.get_mut(&house).unwrap();
        checks.upgrade_streak = 3;
        checks.abandoned_hours = Some(0);
        assert_eq!(residences.upgrade_progress(house), None);

        residences.checks.get_mut(&house).unwrap().abandoned_hours = None;
        let highest = ResidencePrototype::iter()
            .filter(|proto| proto.density == Density::Low)
            .max_by_key(|proto| proto.level)
            .unwrap();
        residences.upgrade(house, highest);
        assert_eq!(residences.upgrade_progress(house), None);
        assert_eq!(residences.upgrade_progress(BuildingID::default()), None);
    }
}
