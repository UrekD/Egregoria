use super::lesce::{Facilities, STATION, TARGET_OPENING_BEDS};
use super::normalize::{ring_area, ring_center};
use super::spatial::{point_bounds, points_bounds, BoundsIndex, PolygonWork};
use super::{Feature, ImportReport, LinkClass, NormalizedCity, LESCE_BOUNDS};
use geom::{Intersect, Polygon, Shape, Vec2, AABB, OBB};
use prototypes::GrowableCompanyPrototype;
use simulation::map::import::ImportedNetwork;
use simulation::map::{
    BuildingID, BuildingKind, Density, LaneKind, LotID, Map, ProjectFilter, ProjectKind, RoadID,
    ZoneCell, ZoneGrid, ZoneKind, ZONE_CELL_SIZE,
};
use simulation::souls::{growth, human::spawn_human, residence::Residences};
use simulation::Simulation;
use std::collections::BTreeMap;

const MIN_INFERRED_HOME_AREA_M2: f32 = 68.0;
const MAX_INFERRED_HOME_SHARE_PCT: usize = 15;

pub(super) fn zone_and_seed(
    sim: &mut Simulation,
    city: &NormalizedCity,
    network: &ImportedNetwork,
    facilities: &Facilities,
    report: &mut ImportReport,
) -> Result<(), String> {
    let features = feature_index(&city.features);
    let lots = mapped_lots(&sim.map(), &features, facilities.plant)?;
    let mut seeded_homes = Vec::new();

    // A single generated lot can span several real parcels. Place a footprint
    // first where this happens so every mapped home can be represented.
    let mut footprints = Vec::new();
    for feature in city
        .features
        .iter()
        .filter(|f| f.house && !f.inferred_house)
    {
        for ring in &feature.outers {
            let position = ring_center(ring);
            if !LESCE_BOUNDS.limits_aabb().contains(position) {
                continue;
            }
            footprints.push((feature, ring, position));
        }
    }
    report
        .feature_counts
        .insert("mapped_home_footprints".into(), footprints.len());
    for (feature, ring, position) in footprints {
        let seeded = mapped_home(sim, facilities.plant, feature.density, ring, position);
        match seeded {
            Some((home, shape)) => {
                seeded_homes.push(home);
                paint(sim, shape, ZoneKind::Residential, feature.density)?;
            }
            None => report.skipped.push((
                feature.source.clone(),
                "no clear residential footprint with connected road access".into(),
            )),
        }
    }
    report
        .feature_counts
        .insert("seeded_mapped_footprints".into(), seeded_homes.len());

    let mut mapped_jobs = 0;
    for feature in city
        .features
        .iter()
        .filter(|feature| feature.building && !feature.house)
    {
        let proto = GrowableCompanyPrototype::iter().find(|proto| {
            proto.growable
                && proto.zone_kind == feature.kind
                && proto.density == Density::Low
                && proto.level == 1
        });
        let Some(proto) = proto else { continue };
        for ring in &feature.outers {
            let center = ring_center(ring);
            if !LESCE_BOUNDS.limits_aabb().contains(center) {
                continue;
            }
            let site = mapped_site(&sim.map(), facilities.plant, ring, center);
            let id = site.and_then(|(road, shape)| {
                let kind = BuildingKind::GoodsCompany(proto.base.id);
                let id = sim.write::<Map>().build_special_building(
                    &shape,
                    kind,
                    proto.base.bgen,
                    None,
                    Some(road),
                )?;
                growth::on_growable_placed(sim, id, kind);
                Some((id, shape))
            });
            if let Some((_, shape)) = id {
                paint(sim, shape, feature.kind, proto.density)?;
                mapped_jobs += proto.base.n_workers as usize;
                *report
                    .feature_counts
                    .entry(format!("seeded_mapped_{:?}_footprints", feature.kind).to_lowercase())
                    .or_default() += 1;
            } else {
                report.skipped.push((
                    feature.source.clone(),
                    "no clear employment footprint with connected road access".into(),
                ));
            }
        }
    }
    let mut mapped_residential_lots = Vec::new();
    for (lot, feature) in lots {
        let Some(shape) = sim.map().lots().get(lot).map(|lot| lot.shape) else {
            continue;
        };
        paint(sim, shape, feature.kind, feature.density)?;
        *report
            .feature_counts
            .entry(format!("zoned_{:?}_{:?}_lots", feature.kind, feature.density).to_lowercase())
            .or_default() += 1;
        if feature.house {
            if let Some(home) =
                sim.seed_imported_growable(lot, BuildingKind::House, feature.density)
            {
                seeded_homes.push(home);
            }
        } else if feature.kind == ZoneKind::Residential {
            mapped_residential_lots.push((shape.center().distance2(STATION), lot));
        } else {
            mapped_jobs += seed_job(sim, lot, feature.kind).unwrap_or(0);
        }
    }

    let mut beds = {
        let residences = sim.read::<Residences>();
        seeded_homes
            .iter()
            .map(|id| residences.by_building[id].capacity as usize)
            .sum::<usize>()
    };
    let mut inferred = 0;
    let max_inferred =
        seeded_homes.len() * MAX_INFERRED_HOME_SHARE_PCT / (100 - MAX_INFERRED_HOME_SHARE_PCT);
    let bounds = LESCE_BOUNDS.limits_aabb();
    let mut work = PolygonWork::new("inferred home and residential landuse matching");
    'inference: for feature in city
        .features
        .iter()
        .filter(|feature| feature.inferred_house)
    {
        for ring in &feature.outers {
            if beds >= TARGET_OPENING_BEDS || inferred >= max_inferred {
                break 'inference;
            }
            if footprint_area(ring, &feature.holes, &mut work)? < MIN_INFERRED_HOME_AREA_M2 {
                continue;
            }
            work.charge(ring.len() * 2)?;
            let position = ring_center(ring);
            if !bounds.contains(position) {
                continue;
            }
            for &(_, area) in features.query(point_bounds(position)) {
                work.charge(1)?;
                if !area.building
                    && area.kind == ZoneKind::Residential
                    && work.contains(&area.outers, &area.holes, position)?
                {
                    if let Some((id, shape)) =
                        mapped_home(sim, facilities.plant, Density::Low, ring, position)
                    {
                        beds += sim.read::<Residences>().by_building[&id].capacity as usize;
                        seeded_homes.push(id);
                        paint(sim, shape, ZoneKind::Residential, Density::Low)?;
                        inferred += 1;
                    }
                    break;
                }
            }
        }
    }
    if inferred > 0 {
        report.synthetic.push(format!(
            "{inferred} homes inferred from unclassified building footprints inside mapped residential landuse"
        ));
    }
    report
        .feature_counts
        .insert("seeded_inferred_home_footprints".into(), inferred);

    let mut landuse_homes = 0;
    mapped_residential_lots.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, lot) in mapped_residential_lots {
        if beds >= TARGET_OPENING_BEDS {
            break;
        }
        if let Some(id) = sim.seed_imported_growable(lot, BuildingKind::House, Density::Low) {
            beds += sim.read::<Residences>().by_building[&id].capacity as usize;
            seeded_homes.push(id);
            landuse_homes += 1;
        }
    }
    if landuse_homes > 0 {
        report.synthetic.push(format!(
            "{landuse_homes} homes on remaining frontage lots inside mapped residential landuse"
        ));
    }
    report
        .feature_counts
        .insert("seeded_landuse_homes".into(), landuse_homes);

    for &home in &seeded_homes {
        while spawn_human(sim, home).is_some() {}
    }
    let population = sim.world().humans.len();
    report
        .feature_counts
        .insert("seeded_houses".into(), seeded_homes.len());
    report
        .feature_counts
        .insert("mapped_zone_jobs".into(), mapped_jobs);
    let synthetic_jobs = seed_synthetic_jobs(
        sim,
        city,
        network,
        facilities.plant,
        &features,
        (population * 108 / 100).saturating_sub(mapped_jobs),
        report,
    )?;
    report
        .feature_counts
        .insert("seeded_zone_jobs".into(), mapped_jobs + synthetic_jobs);
    Ok(())
}

fn footprint_area(
    outer: &[Vec2],
    holes: &[Vec<Vec2>],
    work: &mut PolygonWork,
) -> Result<f32, String> {
    work.charge(outer.len())?;
    let mut area = ring_area(outer).abs();
    for hole in holes {
        if area < MIN_INFERRED_HOME_AREA_M2 {
            break;
        }
        if work.ring_contains(outer, hole[0])? {
            work.charge(hole.len())?;
            area -= ring_area(hole).abs();
        }
    }
    Ok(area)
}

fn feature_index(features: &[Feature]) -> BoundsIndex<(usize, &Feature)> {
    BoundsIndex::new(features.iter().enumerate().map(|(i, feature)| {
        (
            points_bounds(feature.outers.iter().flatten().copied()),
            (i, feature),
        )
    }))
}

fn mapped_lots<'a>(
    map: &Map,
    features: &BoundsIndex<(usize, &'a Feature)>,
    plant: BuildingID,
) -> Result<BTreeMap<LotID, &'a Feature>, String> {
    let net = map.electricity.net_id(plant);
    let filter = ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING;
    let mut plans = BTreeMap::new();
    let mut work = PolygonWork::new("lot and polygon matching");
    for lot in map.lots().values() {
        if map.electricity.net_id(lot.parent) != net
            || map
                .spatial_map()
                .query(lot.shape, filter)
                .any(|object| object != ProjectKind::Road(lot.parent))
        {
            continue;
        }
        let Ok(cells) = ZoneGrid::bounded_paint_cells(&lot.shape, map.environment().bounds())
        else {
            continue;
        };
        let mut best = None;
        for &(index, feature) in features.query(lot.shape.bbox()) {
            work.charge(1)?;
            if feature.inferred_house {
                continue;
            }
            let mut samples = 0;
            for &(x, y) in &cells {
                let p = (geom::vec2(x as f32, y as f32) + Vec2::splat(0.5)) * ZONE_CELL_SIZE;
                samples += usize::from(work.contains(&feature.outers, &feature.holes, p)?);
            }
            let mut center = false;
            if feature.building {
                for ring in &feature.outers {
                    work.charge(ring.len() * 2)?;
                    let p = ring_center(ring);
                    if lot.shape.contains(p) && work.contains(&feature.outers, &feature.holes, p)? {
                        center = true;
                        break;
                    }
                }
            }
            let score = samples * 2 + usize::from(center) * 3;
            let key = (feature.building, score, std::cmp::Reverse(index));
            if score > 0 && best.is_none_or(|(previous, _)| key > previous) {
                best = Some((key, feature));
            }
        }
        if let Some((_, feature)) = best {
            plans.insert(lot.id, feature);
        }
    }
    Ok(plans)
}

fn mapped_home(
    sim: &mut Simulation,
    plant: BuildingID,
    density: Density,
    ring: &[Vec2],
    center: Vec2,
) -> Option<(BuildingID, OBB)> {
    let (road, shape) = mapped_site(&sim.map(), plant, ring, center)?;
    sim.seed_imported_residence(shape, road, density)
        .map(|id| (id, shape))
}

fn mapped_site(map: &Map, plant: BuildingID, ring: &[Vec2], center: Vec2) -> Option<(RoadID, OBB)> {
    let net = map.electricity.net_id(plant);
    let road = map
        .spatial_map()
        .query_around(center, 230.0, ProjectFilter::ROAD)
        .filter_map(|item| match item {
            ProjectKind::Road(id) => Some(id),
            _ => None,
        })
        .filter(|&id| map.electricity.net_id(id) == net)
        .filter(|&id| {
            let road = &map.roads()[id];
            road.lanes_iter().any(|(_, kind)| kind == LaneKind::Walking)
                && road.lanes_iter().any(|(_, kind)| kind == LaneKind::Driving)
        })
        .map(|id| {
            (
                map.roads()[id]
                    .points()
                    .project_2d(center)
                    .xy()
                    .distance2(center),
                id,
            )
        })
        .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))?
        .1;
    let dir = ring
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .max_by(|a, b| a.mag2().total_cmp(&b.mag2()))?
        .try_normalize()?;
    let across = dir.perpendicular();
    let bounds = points_bounds(ring.iter().map(|&p| {
        let p = p - center;
        geom::vec2(p.dot(dir), p.dot(across))
    }));
    let shape = [0.91, 0.75, 0.56, 0.42]
        .into_iter()
        .map(|scale| {
            OBB::new(
                center,
                dir,
                (bounds.w() * scale).max(5.0),
                (bounds.h() * scale).max(5.0),
            )
        })
        .find(|&shape| {
            if map.building_overlaps(shape) {
                return false;
            }
            let footprint = Polygon::from(shape.corners.as_slice());
            // The road's broad envelope includes simulated parking and sidewalks.
            // Keep mapped parcels off the running surface of roads and tracks.
            !map.spatial_map()
                .query(shape, ProjectFilter::ROAD)
                .any(|object| {
                    let ProjectKind::Road(id) = object else {
                        return false;
                    };
                    let road = &map.roads()[id];
                    let mut running = road.boldline();
                    running.radius = road
                        .lanes_iter()
                        .filter(|(_, kind)| matches!(kind, LaneKind::Driving | LaneKind::Rail))
                        .map(|(_, kind)| kind.width())
                        .sum::<f32>()
                        * 0.5;
                    running.intersects(&footprint)
                })
        })?;
    Some((road, shape))
}

fn paint(sim: &Simulation, shape: OBB, kind: ZoneKind, density: Density) -> Result<(), String> {
    let map = sim.map();
    sim.write::<ZoneGrid>()
        .paint_on_map(&map, &shape, Some(kind), density)
        .map_err(|error| format!("imported zoning stamp was rejected ({error:?})"))
}

fn seed_job(sim: &mut Simulation, id: LotID, kind: ZoneKind) -> Option<usize> {
    let (building, shape, density) = {
        let map = sim.map();
        let lot = map.lots().get(id)?;
        [Density::Medium, Density::Low]
            .into_iter()
            .find_map(|density| {
                let cell = ZoneCell { kind, density };
                growth::growable_for_lot(&map, lot, cell)
                    .map(|(building, _)| (building, lot.shape, density))
            })?
    };
    building.as_goods_company()?;
    let placed = sim.seed_imported_growable_with_slots(id, building, density)?;
    paint(sim, shape, kind, density).ok()?;
    Some(placed.job_slots as usize)
}

fn seed_synthetic_jobs(
    sim: &mut Simulation,
    city: &NormalizedCity,
    network: &ImportedNetwork,
    plant: BuildingID,
    features: &BoundsIndex<(usize, &Feature)>,
    target: usize,
    report: &mut ImportReport,
) -> Result<usize, String> {
    let candidates = {
        let map = sim.map();
        let net = map.electricity.net_id(plant);
        // Index individual arterial segments so a long, curved road does not
        // admit lots that are near its bounding box but far from the street.
        let arterials = BoundsIndex::new(
            network
                .roads
                .iter()
                .filter(|&(i, _)| city.classes[*i] == LinkClass::Arterial)
                .flat_map(|(_, id)| {
                    map.roads()[*id]
                        .points()
                        .iter()
                        .copied()
                        .collect::<Vec<_>>()
                        .windows(2)
                        .map(|p| {
                            let segment = geom::Segment::new(p[0].xy(), p[1].xy());
                            (segment.bbox(), segment)
                        })
                        .collect::<Vec<_>>()
                }),
        );
        let grid = sim.read::<ZoneGrid>();
        let mut candidates = Vec::new();
        let mut work = PolygonWork::new("synthetic job site and polygon hole matching");
        'lots: for lot in map.lots().values() {
            let p = lot.shape.center();
            let town = p.distance(STATION);
            if town > 2000.0
                || map.electricity.net_id(lot.parent) != net
                || grid.lot_zone(&lot.shape).is_some()
            {
                continue;
            }
            for &(_, feature) in features.query(point_bounds(p)) {
                work.charge(1)?;
                if work.any_ring_contains(&feature.holes, p)? {
                    continue 'lots;
                }
            }
            if let Some(distance) = arterials
                .query(AABB::centered(p, Vec2::splat(320.0)))
                .map(|line| line.project(p).distance(p))
                .min_by(f32::total_cmp)
                .filter(|&distance| distance <= 160.0)
            {
                candidates.push((distance * 2.0 + town, lot.id));
            }
        }
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        candidates
    };
    let mut candidates = candidates.into_iter();
    let mut total = 0;
    let mut kinds = BTreeMap::<ZoneKind, (usize, usize)>::new();
    for (kind, fraction) in [
        (ZoneKind::Commercial, 35),
        (ZoneKind::Industrial, 45),
        (ZoneKind::Office, 20),
    ] {
        let mut jobs = 0;
        let mut sites = 0;
        while jobs < target * fraction / 100 {
            let Some((_, id)) = candidates.next() else {
                break;
            };
            if let Some(positions) = seed_job(sim, id, kind) {
                jobs += positions;
                sites += 1;
            }
        }
        kinds.insert(kind, (sites, jobs));
        total += jobs;
    }
    for (kind, (sites, jobs)) in kinds {
        report.synthetic.push(format!(
            "{sites} {kind:?} sites near arterials, {jobs} job positions"
        ));
    }
    Ok(total)
}

pub(super) fn filter_vegetation(sim: &Simulation, city: &NormalizedCity) -> Result<(), String> {
    let bounds = LESCE_BOUNDS.limits_aabb();
    let woods = BoundsIndex::new(city.forests.iter().map(|forest| (forest.bounds(), forest)));
    let built = feature_index(&city.features);
    let grid = sim.read::<ZoneGrid>();
    let mut work = PolygonWork::new("woodland and vegetation matching");
    sim.filter_imported_trees(|p| {
        if !bounds.contains(p) {
            return Ok(true);
        }
        let cell = |coordinate: f32| (coordinate / ZONE_CELL_SIZE).floor() as i32;
        if grid.cells().contains_key(&(cell(p.x), cell(p.y))) {
            return Ok(false);
        }
        for &(_, feature) in built.query(point_bounds(p)) {
            if work.contains(&feature.outers, &feature.holes, p)? {
                return Ok(false);
            }
        }
        for forest in woods.query(point_bounds(p)) {
            if work.contains(&forest.outers, &forest.holes, p)? {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec2;

    #[test]
    fn osm_inferred_home_area_uses_the_footprint_and_excludes_courtyards() {
        let mut work = PolygonWork::new("test footprint");
        let origin = Vec2::ZERO;
        let triangle = [origin, vec2(10.0, 0.0), vec2(0.0, 10.0), origin];
        assert!(footprint_area(&triangle, &[], &mut work).unwrap() < MIN_INFERRED_HOME_AREA_M2);

        let square = [
            origin,
            vec2(10.0, 0.0),
            vec2(10.0, 10.0),
            vec2(0.0, 10.0),
            origin,
        ];
        let courtyard = vec![
            vec2(2.0, 2.0),
            vec2(8.0, 2.0),
            vec2(8.0, 8.0),
            vec2(2.0, 8.0),
            vec2(2.0, 2.0),
        ];
        assert_eq!(
            footprint_area(&square, &[courtyard], &mut work).unwrap(),
            64.0
        );
    }
}
