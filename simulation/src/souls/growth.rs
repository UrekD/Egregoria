use crate::economy::balance::{periodic_tick, CITY_UPDATE_INTERVAL_TICKS};
use crate::economy::{Budget, ZoneDemand};
use crate::map::import::growable_shell::{self, housing_slots, job_slots, realized_floors};
pub use crate::map::import::growable_shell::{GrowableForm, GrowableFormFit, ShellGeometry};
pub use crate::map::GrowableLotError;
use crate::map::{
    Building, BuildingID, BuildingKind, Density, GrowableBusinessState, GrowableDevelopment,
    GrowableSiteState, Lot, LotID, Map, PreparedGrowableAppearance, RoadID, ZoneCell, ZoneGrid,
    ZoneKind,
};
use crate::map_dynamic::{BuildingInfos, EnvGrid};
use crate::souls::{lifecycle, residence};
use crate::{GrowablePlan, Simulation};
use common::FastMap;
use geom::{Vec2, OBB};
use ordered_float::OrderedFloat;
use prototypes::{
    BuildingGen, GrowableChainKey, GrowableCompanyPrototype, GrowableMassing, GrowableZoneKind,
    Prototype, ResidencePrototype, Tick,
};

pub(crate) const GROWTH_INTERVAL: u64 = CITY_UPDATE_INTERVAL_TICKS;
const COHORTS: u8 = 14;

#[derive(Default)]
pub(crate) struct GrowthCache {
    zone_generation: u64,
    zones: FastMap<LotID, Option<ZoneCell>>,
    buildings: Vec<(BuildingID, Vec2)>,
    distances: FastMap<LotID, OrderedFloat<f32>>,
    zoned_lots: Vec<(LotID, ZoneCell)>,
    center_index: Vec<(BuildingID, Vec2)>,
    grade_revision: (u64, u64, u64, u64, u64),
    forms: FastMap<LotID, (ZoneCell, RoadID, Result<GrowableFormFit, GrowableLotError>)>,
    candidates: Vec<GrowthCandidate>,
}

struct GrowthCandidate {
    cohort: u8,
    distance: OrderedFloat<f32>,
    lot: LotID,
    cell: ZoneCell,
}

pub(crate) const fn due(tick: u64) -> bool {
    tick >= GROWTH_INTERVAL && tick % GROWTH_INTERVAL == 0
}

pub fn growth_update(sim: &mut Simulation) {
    let tick = periodic_tick(&sim.read::<Budget>(), Tick(sim.get_tick()));
    if !due(tick) {
        return;
    }
    profiling::scope!("souls::growth_pass");
    let cursor = {
        let mut zones = sim.write::<ZoneGrid>();
        let cursor = zones.growth_cohort();
        zones.advance_growth_cohort();
        cursor
    };

    let mut orphaned_companies = Vec::new();
    let mut orphaned_homes = Vec::new();
    let mut orphaned_profiled = Vec::new();
    {
        let map = sim.map();
        for (id, building) in map.buildings().iter() {
            if !building.is_orphaned() || map.lot_origin(id).is_none() {
                continue;
            }
            if building.development().is_some() {
                orphaned_profiled.push(id);
                if building.has_residential_use() {
                    orphaned_homes.push((id, building.door_pos));
                }
                continue;
            }
            match building.kind {
                BuildingKind::House => orphaned_homes.push((id, building.door_pos)),
                BuildingKind::GoodsCompany(_) => orphaned_companies.push(id),
                _ => {}
            }
        }
    }
    orphaned_profiled.sort_unstable();
    for id in orphaned_profiled {
        lifecycle::mark_site_abandoned(sim, id);
    }
    residence::abandon_orphaned_growables(sim, orphaned_homes);
    orphaned_companies.sort_unstable();
    for id in orphaned_companies {
        remove_growable(sim, id);
    }

    if sim.read::<ZoneGrid>().cells().is_empty() {
        return;
    }
    // A save from before staggering can reach the first growth phase without a demand sample.
    if !sim.read::<ZoneDemand>().has_sample() {
        return;
    }

    let mut zoned_lots = {
        let resources = &sim.resources;
        let map = resources.read::<Map>();
        let zones = resources.read::<ZoneGrid>();
        let mut cache = resources.write::<GrowthCache>();
        if cache.zone_generation != zones.generation() || cache.grade_revision.0 != map.identity() {
            cache.zone_generation = zones.generation();
            cache.zones.clear();
        }
        // A lot's shape is immutable for its ID, even when frontage changes.
        cache.zones.retain(|&id, _| map.get(id).is_some());
        cache.distances.retain(|&id, _| map.get(id).is_some());
        let GrowthCache {
            zones: lot_zones,
            zoned_lots,
            ..
        } = &mut *cache;
        zoned_lots.extend(map.lots().iter().filter_map(|(id, lot)| {
            lot_zones
                .entry(id)
                .or_insert_with(|| zones.lot_zone(&lot.shape))
                .map(|zone| (id, zone))
        }));
        std::mem::take(zoned_lots)
    };
    grow_zoned_lots(sim, cursor, zoned_lots.drain(..));
    sim.write::<GrowthCache>().zoned_lots = zoned_lots;
}

fn grow_zoned_lots(
    sim: &mut Simulation,
    cursor: u8,
    zoned_lots: impl IntoIterator<Item = (LotID, ZoneCell)>,
) {
    let quota = sector_quotas(&sim.read::<ZoneDemand>());
    let mut candidates = {
        let map = sim.map();
        let mut cache = sim.write::<GrowthCache>();
        let building_centers = || map.buildings().iter().map(|(id, b)| (id, b.obb.center()));
        if !building_centers().eq(cache.buildings.iter().copied()) {
            cache.buildings.clear();
            cache.buildings.extend(building_centers());
            cache.center_index.clear();
            cache.center_index.extend(building_centers());
            index_building_centers(&mut cache.center_index, 0);
            cache.distances.clear();
        }
        let revision = growable_shell::grade_revision(&map);
        if cache.grade_revision != revision {
            cache.grade_revision = revision;
            cache.forms.clear();
        }
        let GrowthCache {
            distances,
            center_index,
            candidates,
            forms,
            ..
        } = &mut *cache;
        forms.retain(|&id, _| map.get(id).is_some());
        for (id, cell) in zoned_lots {
            if growth_cost(cell)
                .iter()
                .zip(quota)
                .any(|(&cost, credits)| cost > 0 && credits == 0)
            {
                continue;
            }
            let Some(lot) = map.get(id) else {
                continue;
            };
            let Ok(site) = check_site(&map, lot, cell) else {
                continue;
            };
            if let Some(profile) = site.profile {
                let entry = forms
                    .entry(id)
                    .or_insert_with(|| (cell, lot.parent, initial_form(&map, site, profile, 0)));
                if (entry.0, entry.1) != (cell, lot.parent) {
                    *entry = (cell, lot.parent, initial_form(&map, site, profile, 0));
                }
                if entry.2.is_err() {
                    continue;
                }
            }
            let distance = *distances.entry(id).or_insert_with(|| {
                OrderedFloat(nearest_building_distance(
                    center_index,
                    lot.shape.center(),
                    0,
                    f32::INFINITY,
                ))
            });
            candidates.push(GrowthCandidate {
                cohort: cohort(cell),
                distance,
                lot: id,
                cell,
            });
        }
        std::mem::take(candidates)
    };

    arbitrate(&mut candidates, cursor, quota, |lot, cell| {
        let live_vote = {
            let map = sim.map();
            map.get(lot)
                .and_then(|lot| sim.read::<ZoneGrid>().lot_zone(&lot.shape))
        };
        live_vote == Some(cell) && sim.place_zoned_growable(lot, cell).is_ok()
    });
    sim.write::<GrowthCache>().candidates = candidates;
}

fn sector_quotas(demand: &ZoneDemand) -> [u8; 4] {
    ZoneKind::TAXABLE.map(|zone| {
        let score = demand.values.get(&zone).copied().unwrap_or_default();
        if score.is_finite() && score > 5.0 {
            (score / 20.0).ceil().min(4.0) as u8
        } else {
            0
        }
    })
}

fn cohort(cell: ZoneCell) -> u8 {
    let ordinal = cell.kind.ordinal() * 3 + cell.density.ordinal();
    ordinal - u8::from(cell.kind == ZoneKind::MixedUse)
}

fn growth_cost(cell: ZoneCell) -> [u8; 4] {
    let mut cost = [0; 4];
    let housing = match cell.density {
        Density::Low => 1,
        Density::Medium => 2,
        Density::High => 4,
    };
    match cell.kind {
        ZoneKind::MixedUse => {
            cost[0] = housing;
            cost[1] = 1;
        }
        ZoneKind::Residential => cost[0] = housing,
        workplace => {
            cost[usize::from(workplace.ordinal())] = 1 + u8::from(cell.density == Density::High)
        }
    }
    cost
}

fn arbitrate(
    candidates: &mut Vec<GrowthCandidate>,
    cursor: u8,
    quota: [u8; 4],
    mut commit: impl FnMut(LotID, ZoneCell) -> bool,
) {
    candidates.sort_unstable_by_key(|candidate| {
        let priority = (candidate.cohort + COHORTS - cursor) % COHORTS;
        (priority, candidate.distance, candidate.lot)
    });
    let mut remaining = quota;
    let mut mixed_placed = false;
    for GrowthCandidate { lot, cell, .. } in candidates.drain(..) {
        let mixed = cell.kind == ZoneKind::MixedUse;
        if mixed && mixed_placed {
            continue;
        }
        let cost = growth_cost(cell);
        let funded = cost
            .iter()
            .zip(remaining)
            .zip(quota)
            .all(|((&cost, left), initial)| cost <= left || (left > 0 && left == initial));
        if funded && commit(lot, cell) {
            for (left, cost) in remaining.iter_mut().zip(cost) {
                *left = left.saturating_sub(cost);
            }
            mixed_placed |= mixed;
        }
    }
}

/// Lay out a balanced k-d tree in place using stable tie ordering at each median.
fn index_building_centers(centers: &mut [(BuildingID, Vec2)], axis: usize) {
    if centers.len() > 1 {
        let (left, _, right) = centers
            .select_nth_unstable_by_key(centers.len() / 2, |&(id, pos)| {
                (OrderedFloat(coordinate(pos, axis)), id)
            });
        index_building_centers(left, axis ^ 1);
        index_building_centers(right, axis ^ 1);
    }
}

fn coordinate(point: Vec2, axis: usize) -> f32 {
    if axis == 0 {
        point.x
    } else {
        point.y
    }
}

fn nearest_building_distance(
    centers: &[(BuildingID, Vec2)],
    target: Vec2,
    axis: usize,
    best: f32,
) -> f32 {
    if centers.is_empty() {
        return best;
    }
    let (left, rest) = centers.split_at(centers.len() / 2);
    let ((_, center), right) = rest.split_first().unwrap();
    let delta = coordinate(target, axis) - coordinate(*center, axis);
    let (near, far) = if delta < 0.0 {
        (left, right)
    } else {
        (right, left)
    };
    let best =
        nearest_building_distance(near, target, axis ^ 1, best.min(center.distance2(target)));
    if delta * delta < best {
        nearest_building_distance(far, target, axis ^ 1, best)
    } else {
        best
    }
}

/// Resolved roles and frontage for an existing lot.
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub struct GrowableSite {
    pub lot: LotID,
    pub shape: OBB,
    pub frontage: RoadID,
    pub zone: ZoneCell,
    pub kind: BuildingKind,
    pub home: Option<&'static ResidencePrototype>,
    pub company: Option<&'static GrowableCompanyPrototype>,
    pub generator: BuildingGen,
    profile: Option<GrowableMassing>,
    initial_form: Option<GrowableFormFit>,
}

impl GrowableSite {
    /// Guaranteed feasible profiled form at the base value band.
    pub fn initial_form(&self) -> Option<GrowableFormFit> {
        self.initial_form
    }
}

pub fn growable_for_lot(
    map: &Map,
    lot: &Lot,
    cell: ZoneCell,
) -> Option<(BuildingKind, Option<&'static ResidencePrototype>)> {
    if cell.kind == ZoneKind::MixedUse {
        return None;
    }
    let site = check_growable_lot(map, lot, cell).ok()?;
    Some((site.kind, site.home))
}

/// Resolve the level-one roles and check the reserved parcel against live frontage and water.
pub fn check_growable_lot(
    map: &Map,
    lot: &Lot,
    zone: ZoneCell,
) -> Result<GrowableSite, GrowableLotError> {
    let mut site = check_site(map, lot, zone)?;
    if let Some(profile) = site.profile {
        site.initial_form = Some(initial_form(map, site, profile, 0)?);
    }
    Ok(site)
}

fn check_site(map: &Map, lot: &Lot, zone: ZoneCell) -> Result<GrowableSite, GrowableLotError> {
    if !zone.kind.supports_density(zone.density) {
        return Err(GrowableLotError::UnsupportedDensity);
    }
    if map.get(lot.parent).is_none() {
        return Err(GrowableLotError::MissingRoad);
    }
    let site = growable_that_fits(lot, zone)?;
    lot.check_growable_footprint(map, site.generator)?;
    if let Some(home) = site.home.filter(|_| site.company.is_some()) {
        if !Building::valid_footprint(&lot.shape, home.base.bgen) {
            return Err(GrowableLotError::Obstructed);
        }
    }
    Ok(site)
}

pub(crate) fn growable_that_fits(
    lot: &Lot,
    zone: ZoneCell,
) -> Result<GrowableSite, GrowableLotError> {
    let registry = prototypes::prototypes();
    let (home, company) = match zone.kind {
        ZoneKind::Residential => (
            Some(residence::prototype(zone.density, 1).ok_or(GrowableLotError::MissingPrototype)?),
            None,
        ),
        ZoneKind::MixedUse => {
            let pair = registry
                .mixed_use_pair(zone.density)
                .ok_or(GrowableLotError::MissingPrototype)?;
            (Some(pair.home), Some(pair.shop))
        }
        pure => {
            let key = GrowableChainKey {
                zone_kind: GrowableZoneKind::Pure(pure),
                density: zone.density,
                level: 1,
            };
            let company = registry
                .growable_company_for_chain(key)
                .or_else(|| {
                    GrowableCompanyPrototype::iter().find(|proto| {
                        proto.growable && proto.massing.is_none() && proto.chain_key() == key
                    })
                })
                .ok_or(GrowableLotError::MissingPrototype)?;
            (None, Some(company))
        }
    };
    let home_generator = home.map_or(BuildingGen::House, |proto| proto.base.bgen);
    let (kind, generator) = company.map_or((BuildingKind::House, home_generator), |proto| {
        (BuildingKind::GoodsCompany(proto.base.id), proto.base.bgen)
    });

    let [width, depth] = lot.shape.axis().map(Vec2::mag);
    let fits = |minimum: f32, profile: Option<GrowableMassing>| {
        if let Some(profile) = profile {
            let quantize = |v: f32| (v * 10.0).round() / 10.0;
            let (width, depth) = (quantize(width), quantize(depth));
            let axis_range = 0.1..=f32::from(u16::MAX) / 10.0;
            axis_range.contains(&width)
                && axis_range.contains(&depth)
                && width * depth + 0.001 >= minimum
                && width >= profile.min_plot_width_m
                && depth >= profile.min_plot_depth_m
        } else {
            width * depth + 0.001 >= minimum
        }
    };
    if home.is_some_and(|proto| !fits(proto.lot_size_min, proto.massing))
        || company.is_some_and(|proto| !fits(proto.lot_size_min, proto.massing))
    {
        return Err(GrowableLotError::TooSmall);
    }
    Ok(GrowableSite {
        lot: lot.id,
        shape: lot.shape,
        frontage: lot.parent,
        zone,
        kind,
        home,
        company,
        generator,
        profile: home
            .and_then(|home| home.massing)
            .or_else(|| company.and_then(|firm| firm.massing)),
        initial_form: None,
    })
}

fn initial_form(
    map: &Map,
    site: GrowableSite,
    profile: GrowableMassing,
    band: u8,
) -> Result<GrowableFormFit, GrowableLotError> {
    GrowableFormFit::check(
        map.environment(),
        site.shape,
        site.zone,
        profile.style,
        band,
    )
    .ok_or(GrowableLotError::InfeasibleGrade)
}

pub(crate) fn plan_growable(
    sim: &Simulation,
    lot: LotID,
    zone: ZoneCell,
) -> Result<GrowablePlan, GrowableLotError> {
    if !zone.kind.supports_density(zone.density) {
        return Err(GrowableLotError::UnsupportedDensity);
    }
    if zone.kind == ZoneKind::MixedUse
        && prototypes::prototypes()
            .mixed_use_pair(zone.density)
            .is_none()
    {
        return Err(GrowableLotError::MixedUseDisabled);
    }
    let map = sim.map();
    let lot = map.get(lot).ok_or(GrowableLotError::MissingLot)?;
    let site = check_growable_lot(&map, lot, zone)?;
    let value = site
        .profile
        .map_or(0, |_| sim.read::<EnvGrid>().initial_lot_value(&map, lot));
    prepare_site(&map, site, value)
}

fn prepare_site(
    map: &Map,
    site: GrowableSite,
    value: u8,
) -> Result<GrowablePlan, GrowableLotError> {
    let mut home = site.home.map(residence::Residence::from);
    let mut jobs = site.company.map_or(0, |company| company.n_workers);
    let appearance = if let Some(profile) = site.profile {
        let band = match value {
            0..=49 => 0,
            50..=69 => 1,
            _ => 2,
        };
        let mut fit = site.initial_form.ok_or(GrowableLotError::InfeasibleGrade)?;
        for candidate_band in (1..=band).rev() {
            if let Ok(form) = initial_form(map, site, profile, candidate_band) {
                fit = form;
                break;
            }
        }
        let nominal = profile
            .nominal_floors(1)
            .ok_or(GrowableLotError::MissingPrototype)?;
        let floors = realized_floors(profile, site.zone.density, 1, fit.form(), band)
            .filter(|&floors| floors > 0)
            .ok_or(GrowableLotError::MissingPrototype)?;
        if let Some(home) = &mut home {
            home.capacity = housing_slots(
                home.capacity,
                floors,
                nominal,
                site.zone.kind == ZoneKind::MixedUse,
            )
            .ok_or(GrowableLotError::MissingPrototype)?;
        }
        if site.company.is_some() {
            jobs = job_slots(site.zone.kind, jobs, floors, nominal)
                .ok_or(GrowableLotError::MissingPrototype)?;
        }
        let (mesh, roof_z) = fit
            .mesh(map.environment(), site.zone, profile.style, floors, 1)
            .ok_or(GrowableLotError::InfeasibleGrade)?;
        Some(PreparedGrowableAppearance {
            development: GrowableDevelopment {
                zone: site.zone,
                level: 1,
                floors,
                roof_z,
                approved_value_band: band,
                home: home.as_ref().map(|home| home.prototype),
                planned_jobs: jobs,
                site: GrowableSiteState::Active,
                business: site.company.map(|_| GrowableBusinessState::AwaitingSpawn),
            },
            base_z: fit.base_z(),
            door: fit.door(),
            mesh,
        })
    } else {
        None
    };
    GrowablePlan::prepare(map, site, appearance, home, jobs)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrowableLandValue {
    pub measured: Option<u8>,
    pub approved_band: u8,
}

impl Simulation {
    pub fn growable_land_value(&self, id: BuildingID) -> Option<GrowableLandValue> {
        let map = self.map();
        let development = map.get(id)?.development()?;
        Some(GrowableLandValue {
            measured: self.read::<EnvGrid>().land_value_sample(id),
            approved_band: development.approved_value_band,
        })
    }
}

/// Registers simulation state after successful map placement.
pub fn on_growable_placed(sim: &Simulation, building: BuildingID, kind: BuildingKind) {
    sim.write::<BuildingInfos>().insert(building);
    if kind == BuildingKind::House {
        residence::on_house_built(sim, building);
    }
}

/// Clears residence metadata after successful map removal.
pub fn on_growable_removed(sim: &mut Simulation, building: BuildingID) {
    residence::on_building_removed(sim, building);
}

pub(crate) fn remove_growable(sim: &mut Simulation, id: BuildingID) {
    lifecycle::remove_site(sim, id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{Market, ZoneDemand};
    use crate::map::{Density, LanePatternBuilder, Map, MapProject, RoadID};
    use crate::map_dynamic::{
        BuildingInfos, CoverageGrid, Destination, ParkingManagement, SpotReservation, UtilityFlow,
    };
    use crate::souls::desire::{Work, WorkKind};
    use crate::souls::{add_souls_to_empty_buildings, lifecycle};
    use crate::tests::TestCtx;
    use crate::{SoulID, WorldCommand};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, OBB};
    use prototypes::{GameTime, GoodsCompanyID, ItemID, ServiceKind, TICKS_PER_HOUR};

    fn street() -> TestCtx {
        let test = TestCtx::new();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
        test
    }

    #[test]
    fn zones2_z1_due_pass_advances_cursor_without_zones() {
        let mut test = street();
        for pass in 1..=15 {
            test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL * pass;
            growth_update(&mut test.g);
            let grid = test.g.read::<ZoneGrid>();
            assert_eq!(grid.growth_cohort(), (pass % 14) as u8);
            assert_eq!(grid.generation(), 0);
        }
    }

    #[test]
    fn zones2_z1_oversized_candidate_spends_a_single_remaining_credit() {
        let mut test = street();
        residential_demand(&test.g, 21.0);
        let zone = ZoneCell {
            kind: ZoneKind::Residential,
            density: Density::High,
        };
        let lots = {
            let map = test.g.map();
            map.lots().keys().map(|id| (id, zone)).collect::<Vec<_>>()
        };
        paint_growth_area(&test.g, zone);
        grow_zoned_lots(&mut test.g, 0, lots);
        assert_eq!(test.g.read::<residence::Residences>().by_building.len(), 1);
    }

    #[test]
    fn zones2_z1_repeated_contention_starts_every_cohort_across_reload() {
        let mut grid = ZoneGrid::default();
        let mut cohort_turns = [0; COHORTS as usize];
        for pass in 0..28 {
            if pass == 6 {
                grid = Bincode::decode(&Bincode::encode(&grid).unwrap()).unwrap();
            }
            let cursor = grid.growth_cohort();
            assert_eq!(cursor, pass % COHORTS);
            grid.advance_growth_cohort();

            let mut candidates = Vec::new();
            for kind in ZoneKind::ALL {
                for density in [Density::Low, Density::Medium, Density::High] {
                    if !kind.supports_density(density) {
                        continue;
                    }
                    let cell = ZoneCell { kind, density };
                    for distance in 0..32 {
                        candidates.push(GrowthCandidate {
                            cohort: cohort(cell),
                            distance: OrderedFloat(distance as f32),
                            lot: LotID::default(),
                            cell,
                        });
                    }
                }
            }
            let mut attempts = 0;
            let mut placements = Vec::new();
            arbitrate(&mut candidates, cursor, [1; 4], |_, cell| {
                if attempts == 0 {
                    attempts += 1;
                    assert_eq!(cohort(cell), cursor);
                    false
                } else {
                    placements.push(cohort(cell));
                    true
                }
            });
            assert_eq!(placements[0], cursor, "failed first attempt consumed quota");
            assert!(placements.iter().filter(|&&c| c >= 12).count() <= 1);
            cohort_turns[usize::from(placements[0])] += 1;
        }
        assert_eq!(cohort_turns, [2; COHORTS as usize]);
    }

    #[test]
    fn zones2_z1_rechecks_the_vote_before_spending_a_credit() {
        let mut test = street();
        residential_demand(&test.g, 10.0);
        let high = ZoneCell {
            kind: ZoneKind::Residential,
            density: Density::High,
        };
        let low = ZoneCell {
            density: Density::Low,
            ..high
        };
        paint_growth_area(&test.g, low);
        let (old, live) = {
            let map = test.g.map();
            let mut lots = map.lots().keys();
            (lots.next().unwrap(), lots.next().unwrap())
        };
        grow_zoned_lots(&mut test.g, cohort(high), [(old, high), (live, low)]);
        let map = test.g.map();
        assert!(map.get(old).is_some());
        assert!(map.get(live).is_none());
    }

    #[test]
    fn zones2_z1_profiled_commit_records_a_tower_and_its_realized_slots() {
        let mut test = street();
        test.g.map_mut().update();
        let zone = ZoneCell {
            kind: ZoneKind::Residential,
            density: Density::High,
        };
        let (plan, parcel) = {
            let map = test.g.map();
            let mut site =
                check_growable_lot(&map, map.get(front_lot(&test.g)).unwrap(), zone).unwrap();
            // The integration gate keeps the shipped registry unprofiled during Z1.
            let profile = GrowableMassing {
                style: prototypes::GrowableMassingStyle::Tower,
                floors_by_level: [12, 15, 18, 21, 24],
                max_floors: 28,
                min_plot_width_m: 20.0,
                min_plot_depth_m: 20.0,
            };
            site.profile = Some(profile);
            site.initial_form = Some(initial_form(&map, site, profile, 0).unwrap());
            (prepare_site(&map, site, 85).unwrap(), site.shape)
        };
        let placed = test.g.commit_prepared_growable(plan).unwrap();
        let summary = test.g.growable_summary(placed.building).unwrap();
        assert!(summary.floors >= 14);
        assert!(summary.height_above_base > 40.0);
        assert_eq!(summary.housing_slots, placed.housing_slots);
        assert_eq!(
            placed.housing_slots,
            (4 * u32::from(summary.floors) + 6) / 12
        );
        assert!(test
            .g
            .read::<BuildingInfos>()
            .get(placed.building)
            .is_some());
        assert_eq!(
            test.g.growable_land_value(placed.building),
            Some(GrowableLandValue {
                measured: None,
                approved_band: 2
            })
        );

        let mut loaded: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
        for sim in [&mut test.g, &mut loaded] {
            let map = sim.map();
            let building = map.get(placed.building).unwrap();
            assert_eq!(building.obb.corners, parcel.corners);
            let shell = building.growable_shell().unwrap();
            assert_eq!(shell.roof_z(), summary.roof_z);
            assert_eq!(shell.door(), building.door_pos);
            assert_eq!(shell.bounds().ur.z, summary.roof_z);
            assert!(shell.faces().map(|(face, _)| face.len()).sum::<usize>() <= 1536);
            assert!(shell.triangles().count() * 3 <= 2304);
            let ray = geom::Ray3::new(parcel.center().z(summary.roof_z + 20.0), -geom::Vec3::Z);
            assert!((shell.ray_depth(ray, 30.0).unwrap() - 20.0).abs() < 0.001);
            assert_eq!(shell.ray_depth(ray, 10.0), None);
        }
    }

    #[test]
    fn zones2_z1_import_reports_committed_jobs_and_checks_density() {
        let mut test = street();
        let lot = front_lot(&test.g);
        let shop = BuildingKind::GoodsCompany(GoodsCompanyID::new("commercial-medium-l1"));
        assert!(test
            .g
            .seed_imported_growable_with_slots(lot, shop, Density::Low)
            .is_none());
        assert!(test.g.map().get(lot).is_some());
        let placed = test
            .g
            .seed_imported_growable_with_slots(lot, shop, Density::Medium)
            .unwrap();
        assert_eq!((placed.housing_slots, placed.job_slots), (0, 18));
        add_souls_to_empty_buildings(&mut test.g);
        let Some(SoulID::GoodsCompany(company)) =
            test.g.read::<BuildingInfos>().owner(placed.building)
        else {
            panic!("expected a company at the imported site");
        };
        assert_eq!(
            test.g.get(company).unwrap().comp.max_workers,
            placed.job_slots
        );
    }

    fn paint_growth_area(sim: &Simulation, zone: ZoneCell) {
        sim.write::<ZoneGrid>()
            .paint_on_map(
                &sim.map(),
                &OBB::new(vec2(250.0, 100.0), Vec2::X, 380.0, 180.0),
                Some(zone.kind),
                zone.density,
            )
            .unwrap();
    }

    fn residential_demand(sim: &Simulation, demand: f32) {
        sim.write::<ZoneDemand>()
            .values
            .insert(ZoneKind::Residential, demand);
    }

    fn hourly_check(sim: &mut Simulation) {
        let tick = sim.get_tick();
        let mut next = tick - tick % TICKS_PER_HOUR + residence::UPDATE_OFFSET;
        if next <= tick {
            next += TICKS_PER_HOUR;
        }
        assert!(residence::due(next));
        sim.write::<GameTime>().tick.0 = next;
        residence::residence_update(sim);
        crate::utils::scheduler::drain_commands(sim);
    }

    fn serve_house(sim: &mut Simulation, house: BuildingID) {
        let road = sim.map().get(house).unwrap().connected_road.unwrap();
        for (name, x) in [
            ("coal-power-plant", 160.0),
            ("water-pump", 230.0),
            ("sewage-outlet", 300.0),
        ] {
            let proto = GoodsCompanyID::new(name).prototype();
            let site = sim
                .map_mut()
                .build_special_building(
                    &OBB::new(vec2(x, 70.0), geom::Vec2::X, 20.0, 20.0),
                    BuildingKind::GoodsCompany(proto.id),
                    proto.bgen,
                    None,
                    Some(road),
                )
                .unwrap();
            sim.write::<BuildingInfos>().insert(site);
            let Some(SoulID::GoodsCompany(owner)) =
                crate::souls::goods_company::company_soul(sim, site, proto.id)
            else {
                panic!("could not start {name}");
            };
            // The fixture only runs residence checks. Model staffing for the
            // utility snapshots without adding residents or running the labor market.
            sim.world.companies[owner]
                .workers
                .0
                .push(crate::HumanID::default());
        }
        let (world, resources) = sim.world_res();
        crate::map_dynamic::electricity_flow_system(world, resources);
        let tick = sim.get_tick();
        sim.write::<GameTime>().tick.0 = GROWTH_INTERVAL + UtilityFlow::UPDATE_OFFSET;
        assert!(UtilityFlow::due(sim.get_tick()));
        crate::map_dynamic::utility_flow_update(sim);
        sim.write::<GameTime>().tick.0 = tick;
        let utilities = sim.read::<UtilityFlow>();
        assert!(utilities.has_water(house) && utilities.has_sewage(house));
    }

    fn place_store(sim: &Simulation, lot: LotID) -> BuildingID {
        let kind = BuildingKind::GoodsCompany(prototypes::GoodsCompanyID::new("commercial-l1"));
        let building = sim.map_mut().place_growable(lot, kind).unwrap();
        on_growable_placed(sim, building, kind);
        building
    }

    #[test]
    fn p1bce_grows_high_density_company_zones() {
        for zone in [ZoneKind::Commercial, ZoneKind::Industrial, ZoneKind::Office] {
            let mut test = street();
            test.apply(&[WorldCommand::ZonePaint {
                area: OBB::new(vec2(250.0, 100.0), geom::Vec2::X, 320.0, 140.0),
                kind: Some(zone),
                density: Density::High,
            }]);
            test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL - 1;
            let (world, resources) = test.g.world_res();
            crate::economy::demand_update(world, resources);
            test.g.write::<ZoneDemand>().values.insert(zone, 40.0);
            assert!(due(GROWTH_INTERVAL));
            test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL;
            growth_update(&mut test.g);
            let map = test.g.map();
            assert!(
                map.buildings().values().any(|building| {
                    let Some(company) = building.kind.as_goods_company() else {
                        return false;
                    };
                    let proto = prototypes::GrowableCompanyID::from(company).prototype();
                    proto.density == Density::High && proto.zone_kind == zone && proto.level == 1
                }),
                "high-density {zone:?} lot did not grow"
            );
        }
    }

    fn grow_medium_zone(test: &mut TestCtx, kind: ZoneKind) {
        test.apply(&[WorldCommand::ZonePaint {
            area: OBB::new(vec2(250.0, 100.0), Vec2::X, 320.0, 140.0),
            kind: Some(kind),
            density: Density::Medium,
        }]);
        test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL - 1;
        let (world, resources) = test.g.world_res();
        crate::economy::demand_update(world, resources);
        test.g.write::<ZoneDemand>().values.insert(kind, 15.0);
        test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL;
        growth_update(&mut test.g);
    }

    #[test]
    fn u1_zone_medium_companies_fit_twenty_metres_and_high_needs_thirty() {
        let test = street();
        let map = test.g.map();
        let mut lot = map.lots().values().next().unwrap().clone();
        for kind in [ZoneKind::Commercial, ZoneKind::Industrial, ZoneKind::Office] {
            for (side, density, fits) in [
                (20.0, Density::Medium, true),
                (20.0, Density::High, false),
                (30.0, Density::High, true),
            ] {
                lot.shape = OBB::new(lot.shape.center(), geom::Vec2::X, side, side);
                assert_eq!(
                    growable_that_fits(&lot, ZoneCell { kind, density }).is_ok(),
                    fits,
                    "{kind:?} {density:?} on a {side} m lot"
                );
            }
        }
    }

    #[test]
    fn u1_zone_growth_uses_medium_prototypes_for_all_kinds() {
        for (kind, name) in [
            (ZoneKind::Residential, "residence-medium-l1"),
            (ZoneKind::Commercial, "commercial-medium-l1"),
            (ZoneKind::Industrial, "industrial-medium-l1"),
            (ZoneKind::Office, "office-medium-l1"),
        ] {
            let mut test = street();
            grow_medium_zone(&mut test, kind);
            let map = test.g.map();
            let building = map
                .buildings()
                .values()
                .find(|building| map.lot_origin(building.id).is_some())
                .unwrap_or_else(|| panic!("Medium {kind:?} lot did not grow"));
            if kind == ZoneKind::Residential {
                let residences = test.g.read::<residence::Residences>();
                let home = &residences.by_building[&building.id];
                assert_eq!(home.prototype, prototypes::ResidencePrototypeID::new(name));
                assert_eq!(
                    (home.density, home.level, home.capacity),
                    (Density::Medium, 1, 3)
                );
            } else {
                assert_eq!(
                    building.kind,
                    BuildingKind::GoodsCompany(GoodsCompanyID::new(name))
                );
            }
        }
    }

    #[test]
    fn u1_zone_medium_residence_upgrades_at_same_density() {
        let mut test = street();
        grow_medium_zone(&mut test, ZoneKind::Residential);
        let house = *test
            .g
            .read::<residence::Residences>()
            .by_building
            .keys()
            .next()
            .expect("Medium residence should grow before upgrading");
        let lot = test.g.map().lots().keys().next().unwrap();
        let store = place_store(&test.g, lot);
        residential_demand(&test.g, 40.0);
        serve_house(&mut test.g, house);
        for kind in [ServiceKind::School, ServiceKind::Health] {
            test.g
                .write::<CoverageGrid>()
                .by_kind
                .entry(kind)
                .or_default()
                .insert(house, 100.0);
        }
        test.g.write::<GameTime>().tick.0 =
            GROWTH_INTERVAL + crate::map_dynamic::EnvGrid::UPDATE_OFFSET;
        crate::map_dynamic::env_update(&mut test.g);
        for (capacity, level, next_capacity) in [(3, 2, 6), (6, 3, 9)] {
            while test.g.world().humans.len() < capacity {
                crate::souls::human::spawn_human(&mut test.g, house).unwrap();
            }
            for human in test.g.world_mut_unchecked().humans.values_mut() {
                human.work = Some(Work::new(store, WorkKind::Worker, 0.0));
            }
            for _ in 0..6 {
                hourly_check(&mut test.g);
            }
            let residences = test.g.read::<residence::Residences>();
            let home = &residences.by_building[&house];
            assert_eq!(
                (home.density, home.level, home.capacity),
                (Density::Medium, level, next_capacity)
            );
        }
    }

    #[test]
    fn p1bce_player_house_survives_low_occupancy() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        assert!(test.g.map().lot_origin(house).is_none());
        test.g.write::<GameTime>().tick.0 = 0;
        serve_house(&mut test.g, house);
        for _ in 0..36 {
            hourly_check(&mut test.g);
        }
        assert!(test.g.map().get(house).is_some());
        assert!(!test.g.read::<residence::Residences>().is_abandoned(house));
    }

    #[test]
    fn p1bce_unavailable_parking_keeps_car_if_parked() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        let car = test.g.get(human).unwrap().router.personal_car.unwrap();
        let destination = test.g.map().get(house).unwrap().door_pos;
        let spots = fill_parking(&test.g, destination, 400.0);
        test.g.world_mut_unchecked().humans[human]
            .router
            .target_dest = Some(Destination::Outside(destination));
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        crate::utils::scheduler::drain_commands(&mut test.g);
        let router = &test.g.get(human).unwrap().router;
        assert!(router.last_error.is_none());
        assert_eq!(router.personal_car, Some(car));
        assert!(test.g.get(car).is_some());
        for spot in spots {
            test.g.write::<ParkingManagement>().free(spot);
        }
    }

    #[test]
    fn p1bce_work_truck_uses_legacy_parking_search() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        let house_door = test.g.map().get(house).unwrap().door_pos;
        let truck = crate::transportation::spawn_parked_vehicle(
            &mut test.g,
            crate::transportation::VehicleKind::Truck,
            house_door,
        )
        .unwrap();
        let resident = &mut test.g.world_mut_unchecked().humans[human];
        resident.work = Some(Work::new(
            house,
            WorkKind::Driver {
                truck,
                deliver_order: None,
                load: None,
            },
            0.0,
        ));
        resident.router.use_vehicle(Some(truck));
        resident.router.target_dest = Some(Destination::Outside(vec3(175.0, 600.0, 0.0)));
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        assert!(test.g.get(human).unwrap().router.last_error.is_none());
    }

    #[test]
    fn p1b_growth_and_residence_lifecycle() {
        let mut test = street();
        residential_demand(&test.g, 0.0);
        let lot = front_lot(&test.g);
        test.apply(&[WorldCommand::MapBuildHouse(lot)]);
        let house = *test
            .g
            .read::<residence::Residences>()
            .by_building
            .keys()
            .next()
            .unwrap();
        add_souls_to_empty_buildings(&mut test.g);
        assert!(
            test.g.world().humans.is_empty(),
            "zero demand prevents immigration"
        );

        residential_demand(&test.g, 40.0);
        add_souls_to_empty_buildings(&mut test.g);
        assert_eq!(test.g.world().humans.len(), 1);
        add_souls_to_empty_buildings(&mut test.g);
        add_souls_to_empty_buildings(&mut test.g);
        assert_eq!(test.g.world().humans.len(), 2);

        let residents: Vec<_> = test.g.world().humans.keys().collect();
        let owner = test.g.read::<BuildingInfos>().owner(house);
        assert_eq!(owner, Some(SoulID::Human(residents[0])));
        assert_eq!(
            test.g
                .read::<BuildingInfos>()
                .building_owned_by(SoulID::Human(residents[1])),
            Some(house)
        );
        lifecycle::despawn_human(&mut test.g, residents[0]);
        crate::utils::scheduler::drain_commands(&mut test.g);
        assert_eq!(
            test.g.read::<BuildingInfos>().owner(house),
            Some(SoulID::Human(residents[1]))
        );
    }

    #[test]
    fn p1b_level_up_after_six_good_checks() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let mesh_top = |sim: &Simulation| {
            sim.map()
                .get(house)
                .unwrap()
                .mesh
                .faces
                .iter()
                .flat_map(|(face, _)| face.iter().map(|vertex| vertex.z))
                .fold(f32::NEG_INFINITY, f32::max)
        };
        let original_top = mesh_top(&test.g);
        residential_demand(&test.g, 40.0);
        for _ in 0..2 {
            add_souls_to_empty_buildings(&mut test.g);
        }
        let lot = test.g.map().lots().keys().next().unwrap();
        let store = place_store(&test.g, lot);

        for human in test.g.world_mut_unchecked().humans.values_mut() {
            human.work = Some(Work::new(store, WorkKind::Worker, 0.0));
        }
        test.g.write::<GameTime>().tick.0 = 0;
        serve_house(&mut test.g, house);
        test.g
            .write::<CoverageGrid>()
            .by_kind
            .entry(ServiceKind::School)
            .or_default()
            .insert(house, 30.0);
        for _ in 0..5 {
            hourly_check(&mut test.g);
        }
        let residences = test.g.read::<residence::Residences>();
        assert_eq!(residences.by_building.get(&house).map(|r| r.level), Some(1));
        drop(residences);
        hourly_check(&mut test.g);
        let residences = test.g.read::<residence::Residences>();
        let home = &residences.by_building[&house];
        assert_eq!((home.level, home.capacity), (2, 4));
        assert!(mesh_top(&test.g) > original_top);
    }

    #[test]
    fn p1b_orphan_rehomes_then_emigrates_and_reposts_job() {
        let mut test = street();
        residential_demand(&test.g, 40.0);
        let house = test.build_house_near(vec2(175.0, 125.0));
        add_souls_to_empty_buildings(&mut test.g);
        add_souls_to_empty_buildings(&mut test.g);
        let residents: Vec<_> = test.g.world().humans.keys().collect();
        assert_eq!(residents.len(), 2);

        let road = test.g.map().get(house).unwrap().connected_road.unwrap();
        test.apply(&[WorldCommand::MapRemoveRoad(road)]);
        test.build_roads(&[vec3(100.0, 300.0, 0.0), vec3(400.0, 300.0, 0.0)]);
        let neighbor = test.build_house_near(vec2(175.0, 325.0));
        add_souls_to_empty_buildings(&mut test.g);
        residential_demand(&test.g, 0.0);
        let lot = test.g.map().lots().keys().next().unwrap();
        place_store(&test.g, lot);
        add_souls_to_empty_buildings(&mut test.g);
        let company = test.g.world().companies.keys().next().unwrap();
        let workplace = test.g.get(company).unwrap().comp.building;
        let company_soul = SoulID::GoodsCompany(company);
        let job = ItemID::new("job-opening");
        let emigrant_soul = SoulID::Human(residents[1]);
        {
            let mut market = test.g.write::<Market>();
            market.produce(company_soul, job, -1);
            market.sell_all(company_soul, vec2(0.0, 0.0), job, 0);
        }
        let jobs_before = test
            .g
            .read::<Market>()
            .iter()
            .find(|(item, _)| **item == job)
            .unwrap()
            .1
            .sell_order(company_soul)
            .unwrap()
            .qty;
        if let Some(car) = test.g.get(residents[1]).unwrap().router.personal_car {
            test.g
                .read::<crate::ParCommandBuffer<crate::VehicleEnt>>()
                .kill(car);
            crate::utils::scheduler::drain_commands(&mut test.g);
        }
        let door = test.g.map().get(workplace).unwrap().door_pos;
        let car = crate::transportation::spawn_parked_vehicle(
            &mut test.g,
            crate::transportation::VehicleKind::Car,
            door,
        )
        .unwrap();
        let car_pos = test.g.get(car).unwrap().trans.pos;
        let spot = test
            .g
            .map()
            .parking
            .all_spots()
            .find_map(|(id, spot)| (spot.trans.pos == car_pos).then_some(id))
            .unwrap();
        assert!(!test.g.read::<ParkingManagement>().is_spot_free(spot));
        let world = test.g.world_mut_unchecked();
        world.companies[company].workers.0.push(residents[1]);
        world.humans[residents[1]].router = crate::map_dynamic::Router::new(Some(car));
        world.humans[residents[1]].work = Some(Work::new(workplace, WorkKind::Worker, 0.0));

        test.g.write::<GameTime>().tick.0 = 0;
        hourly_check(&mut test.g);
        assert_eq!(test.g.get(residents[0]).unwrap().home.house, neighbor);
        assert!(test.g.get(residents[1]).is_none());
        assert!(test.g.get(car).is_none());
        assert!(test.g.read::<ParkingManagement>().is_spot_free(spot));
        assert!(test.g.get(company).unwrap().workers.0.is_empty());
        let market = test.g.read::<Market>();
        for (_, item) in market.iter() {
            assert!(item.buy_order(emigrant_soul).is_none());
            assert!(item.sell_order(emigrant_soul).is_none());
            assert!(item.capital(emigrant_soul).is_none());
        }
        assert_eq!(
            market
                .iter()
                .find(|(item, _)| **item == job)
                .unwrap()
                .1
                .sell_order(company_soul)
                .unwrap()
                .qty,
            jobs_before + 1
        );
        drop(market);

        let infos = test.g.read::<BuildingInfos>();
        assert!(infos.get(house).unwrap().inside.is_empty());
        assert!(infos.owner(house).is_none());
        assert!(infos.building_owned_by(emigrant_soul).is_none());
        drop(infos);
        for _ in 0..12 {
            hourly_check(&mut test.g);
        }
        assert!(test.g.map().get(house).is_none());
        assert!(!test
            .g
            .read::<residence::Residences>()
            .by_building
            .contains_key(&house));
        assert!(test.g.read::<BuildingInfos>().get(house).is_none());
    }

    #[test]
    fn af2_orphaned_growable_house_is_abandoned_on_the_next_growth_pass() {
        let mut test = street();
        let lot = front_lot(&test.g);
        let home = grow_house(&mut test.g, lot);
        let road = test.g.map().get(home).unwrap().connected_road.unwrap();
        let resident = crate::souls::human::spawn_human(&mut test.g, home).unwrap();
        assert!(test.g.get(resident).is_some());
        let other = test.build_house_near(vec2(275.0, 125.0));
        assert_eq!(test.g.map().get(other).unwrap().connected_road, Some(road));

        test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL - 1;
        test.apply(&[WorldCommand::MapRemoveRoad(road)]);
        assert!(test.g.map().get(home).unwrap().is_orphaned());
        assert!(!test.g.read::<residence::Residences>().is_abandoned(home));
        assert!(!residence::due(GROWTH_INTERVAL));

        test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL;
        growth_update(&mut test.g);
        crate::utils::scheduler::drain_commands(&mut test.g);

        let residences = test.g.read::<residence::Residences>();
        assert!(residences.is_abandoned(home));
        assert!(!residences.is_abandoned(other));
        assert!(test.g.get(resident).is_none());
        let infos = test.g.read::<BuildingInfos>();
        assert!(infos.get(home).unwrap().residents.is_empty());
    }

    #[test]
    fn p1b_carless_resident_walks_to_work() {
        let mut test = street();
        residential_demand(&test.g, 40.0);
        let west = test.build_house_near(vec2(175.0, 125.0));
        let east = test.build_house_near(vec2(275.0, 125.0));
        for _ in 0..2 {
            add_souls_to_empty_buildings(&mut test.g);
        }
        let (human, home) = test
            .g
            .world()
            .humans
            .iter()
            .find_map(|(id, human)| {
                human
                    .router
                    .personal_car
                    .is_none()
                    .then_some((id, human.home.house))
            })
            .expect("a deterministic carless resident");
        let workplace = if home == west { east } else { west };
        let human_ent = &mut test.g.world_mut_unchecked().humans[human];
        human_ent.work = Some(Work::new(workplace, WorkKind::Worker, 0.0));
        human_ent.decision.kind =
            crate::souls::human::HumanDecisionKind::GoTo(Destination::Building(workplace));
        for _ in 0..10_000 {
            test.tick_fast();
            if test.g.get(human).unwrap().location
                == crate::transportation::Location::Building(workplace)
            {
                return;
            }
        }
        panic!("carless resident did not reach work");
    }

    #[test]
    fn p1b_parking_retry_keeps_car_when_a_spot_is_available_farther_away() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        let car = test.g.get(human).unwrap().router.personal_car.unwrap();
        let destination = test.g.map().get(house).unwrap().door_pos;
        let spots = fill_parking(&test.g, destination, 100.0);
        assert!(!spots.is_empty());
        test.g.world_mut_unchecked().humans[human]
            .router
            .target_dest = Some(Destination::Outside(destination));
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        crate::utils::scheduler::drain_commands(&mut test.g);
        let router = &test.g.get(human).unwrap().router;
        assert!(router.last_error.is_none());
        assert_eq!(router.personal_car, Some(car));
        assert!(test.g.get(car).is_some());
        for spot in spots {
            test.g.write::<ParkingManagement>().free(spot);
        }
    }

    fn fill_parking(
        sim: &Simulation,
        destination: geom::Vec3,
        radius: f32,
    ) -> Vec<SpotReservation> {
        let map = sim.map();
        let mut parking = sim.write::<ParkingManagement>();
        let mut spots = vec![];
        while let Ok(spot) = parking.reserve_near_radius(destination, &map, radius) {
            spots.push(spot);
        }
        spots
    }

    #[test]
    fn p1b_mid_trip_parking_failure_walks() {
        let mut test = street();
        residential_demand(&test.g, 40.0);
        let house = test.build_house_near(vec2(175.0, 125.0));
        let workplace = test.build_house_near(vec2(275.0, 125.0));
        add_souls_to_empty_buildings(&mut test.g);
        let (human, body) = test
            .g
            .world()
            .humans
            .iter()
            .find(|(_, h)| h.home.house == house)
            .unwrap();
        let car = body
            .router
            .personal_car
            .expect("fixture resident has a car");
        crate::transportation::unpark(&mut test.g, car);
        let car_pos = test.g.get(car).unwrap().trans.pos;
        test.g
            .write::<BuildingInfos>()
            .get_out(house, SoulID::Human(human));
        let body = &mut test.g.world_mut_unchecked().humans[human];
        body.location = crate::transportation::Location::Vehicle(car);
        body.router.target_dest = Some(Destination::Building(workplace));
        body.decision.kind =
            crate::souls::human::HumanDecisionKind::GoTo(Destination::Building(workplace));
        let destination = test.g.map().get(workplace).unwrap().door_pos;
        let spots = fill_parking(&test.g, destination, 400.0);
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        crate::utils::scheduler::drain_commands(&mut test.g);
        let body = test.g.get(human).unwrap();
        assert!(body.router.last_error.is_none());
        assert!(body.router.personal_car.is_none());
        assert_eq!(body.location, crate::transportation::Location::Outside);
        assert_eq!(body.trans.pos, car_pos);
        assert!(body.collider.is_some());
        assert!(test.g.get(car).is_none());
        for spot in spots {
            test.g.write::<ParkingManagement>().free(spot);
        }
        for _ in 0..10_000 {
            test.tick_fast();
            if test.g.get(human).unwrap().location
                == crate::transportation::Location::Building(workplace)
            {
                return;
            }
        }
        panic!("resident did not reach the destination after leaving the car");
    }

    fn front_lot(sim: &Simulation) -> LotID {
        sim.map()
            .lots()
            .values()
            .min_by_key(|lot| OrderedFloat(lot.shape.center().distance2(vec2(170.0, 125.0))))
            .unwrap()
            .id
    }

    fn grow_house(sim: &mut Simulation, lot: LotID) -> BuildingID {
        sim.place_zoned_growable(
            lot,
            ZoneCell {
                kind: ZoneKind::Residential,
                density: Density::Low,
            },
        )
        .unwrap()
        .building
    }

    fn restored_lot(map: &Map, origin: OBB) -> RoadID {
        map.lots()
            .values()
            .find(|lot| {
                lot.shape
                    .corners
                    .iter()
                    .zip(origin.corners)
                    .all(|(actual, expected)| actual.is_close(expected, 0.001))
            })
            .expect("lot restored at the original footprint")
            .parent
    }

    #[test]
    fn p1b_zoned_growth_obeys_demand_quota_and_clusters() {
        let mut test = street();
        let anchor = test.build_house_near(vec2(175.0, 125.0));
        let (nearest, total, zones) = {
            let map = test.g.map();
            let anchor = map.get(anchor).unwrap().obb.center();
            let mut zones = map
                .lots()
                .keys()
                .map(|id| {
                    (
                        id,
                        ZoneCell {
                            kind: ZoneKind::Residential,
                            density: Density::High,
                        },
                    )
                })
                .collect::<Vec<_>>();
            zones.sort_unstable_by_key(|&(id, _)| {
                (
                    OrderedFloat(map.get(id).unwrap().shape.center().distance2(anchor)),
                    id,
                )
            });
            (map.get(zones[0].0).unwrap().shape, map.lots().len(), zones)
        };
        let money = test.g.read::<crate::economy::Government>().money;
        paint_growth_area(&test.g, zones[0].1);
        residential_demand(&test.g, 5.0);
        grow_zoned_lots(&mut test.g, 0, zones.clone());
        assert_eq!(test.g.map().lots().len(), total);
        residential_demand(&test.g, 21.0);
        grow_zoned_lots(&mut test.g, 0, zones);
        let residences = test.g.read::<residence::Residences>();
        assert_eq!(residences.by_building.len(), 2);
        let map = test.g.map();
        assert_eq!(map.lots().len(), total - 1);
        assert!(map
            .buildings()
            .iter()
            .any(|(id, building)| { id != anchor && building.obb.center() == nearest.center() }));
        for (&id, home) in &residences.by_building {
            if id != anchor {
                assert_eq!(
                    (home.density, home.level, home.capacity),
                    (Density::High, 1, 4)
                );
                assert!(map.lot_origin(id).is_some());
                assert!(test.g.read::<BuildingInfos>().get(id).is_some());
            }
        }
        assert_eq!(test.g.read::<crate::economy::Government>().money, money);
    }

    #[test]
    fn p1b_collapse_restores_lot_after_exactly_twelve_abandoned_hours() {
        let mut test = street();
        let lot = front_lot(&test.g);
        let origin = test.g.map().get(lot).unwrap().shape;
        let house = grow_house(&mut test.g, lot);
        residential_demand(&test.g, 0.0);
        test.g.write::<GameTime>().tick.0 = 0;
        serve_house(&mut test.g, house);
        for _ in 0..23 {
            hourly_check(&mut test.g);
        }
        assert!(!test.g.read::<residence::Residences>().is_abandoned(house));
        hourly_check(&mut test.g);
        assert!(test.g.read::<residence::Residences>().is_abandoned(house));
        residential_demand(&test.g, 40.0);
        add_souls_to_empty_buildings(&mut test.g);
        assert!(test.g.world().humans.is_empty());
        for _ in 0..11 {
            hourly_check(&mut test.g);
        }
        assert!(test.g.map().get(house).is_some());
        hourly_check(&mut test.g);
        assert!(test.g.map().get(house).is_none());
        assert!(test.g.map().lot_origin(house).is_none());
        assert!(!test
            .g
            .read::<residence::Residences>()
            .by_building
            .contains_key(&house));
        restored_lot(&test.g.map(), origin);
    }

    #[test]
    fn p1b_player_removal_restores_saved_lot_after_road_split_or_merge() {
        for merge in [false, true] {
            let mut test = street();
            let lot = front_lot(&test.g);
            let origin = test.g.map().get(lot).unwrap().clone();
            let house = grow_house(&mut test.g, lot);
            let encoded = Bincode::encode(&test.g).unwrap();
            test.g = Bincode::decode(&encoded).unwrap();
            let road = {
                let mut map = test.g.map_mut();
                let (_, spur) = map
                    .make_connection(
                        MapProject {
                            pos: vec3(285.0, 100.0, 0.0),
                            kind: crate::map::ProjectKind::Road(origin.parent),
                        },
                        MapProject::ground(vec3(285.0, 40.0, 0.0)),
                        None,
                        &LanePatternBuilder::default().sidewalks(false).build(),
                    )
                    .unwrap();
                let junction = map.remove_road(spur).unwrap().src;
                if merge {
                    let roads = &map.get(junction).unwrap().roads;
                    let (a, b) = (roads[0], roads[1]);
                    map.merge_road(a, b).unwrap()
                } else {
                    map.get(house).unwrap().connected_road.unwrap()
                }
            };
            assert_ne!(road, origin.parent);
            test.apply(&[WorldCommand::MapRemoveBuilding(house)]);
            assert_eq!(restored_lot(&test.g.map(), origin.shape), road);
        }
    }

    #[test]
    fn p1b_orphaned_company_restores_lot_after_frontage_rebuilt() {
        let mut test = street();
        let lot = front_lot(&test.g);
        let origin = test.g.map().get(lot).unwrap().clone();
        let store = prototypes::GoodsCompanyID::new("commercial-l1");
        let kind = BuildingKind::GoodsCompany(store);
        let building = test.g.map_mut().place_growable(lot, kind).unwrap();
        on_growable_placed(&test.g, building, kind);
        add_souls_to_empty_buildings(&mut test.g);
        let company = test.g.read::<BuildingInfos>().owner(building).unwrap();
        test.apply(&[WorldCommand::MapRemoveRoad(origin.parent)]);
        let replacement = test
            .g
            .map_mut()
            .make_connection(
                MapProject::ground(vec3(100.0, 100.0, 0.0)),
                MapProject::ground(vec3(400.0, 100.0, 0.0)),
                None,
                &LanePatternBuilder::default().build(),
            )
            .unwrap()
            .1;
        assert_ne!(replacement, origin.parent);
        assert!(test.g.map().lots().values().all(|lot| {
            !lot.shape
                .corners
                .iter()
                .zip(origin.shape.corners)
                .all(|(actual, expected)| actual.is_close(expected, 0.001))
        }));
        assert!(test.g.read::<ZoneGrid>().cells().is_empty());
        assert!(due(GROWTH_INTERVAL));
        test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL;
        growth_update(&mut test.g);
        crate::utils::scheduler::drain_commands(&mut test.g);
        assert!(test.g.map().get(building).is_none());
        assert_eq!(restored_lot(&test.g.map(), origin.shape), replacement);
        assert!(test.g.world().companies.is_empty());
        assert!(test
            .g
            .read::<BuildingInfos>()
            .building_owned_by(company)
            .is_none());
        assert!(
            test.g.read::<GrowthCache>().zones.is_empty(),
            "orphan cleanup should leave the lot cache empty without zones"
        );
    }
}
