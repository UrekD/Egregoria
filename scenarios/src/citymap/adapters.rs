use super::contains;
use geom::{Polygon, Vec2, OBB};
use prototypes::{try_prototype, BuildingGen, GoodsCompanyID};
use simulation::map::{
    river_site_summary, water_provider_site, water_service_site, BuildingID, BuildingKind, Map,
    ProjectFilter, RoadID, WaterSiteRequest, ZoneCell,
};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::Simulation;
use std::collections::BTreeSet;

use super::{BuiltNetwork, TownPlan};

/// The dry opening expresses downtown mixed-use intent as distinct High lots.
/// A future ZONES2 integration can replace this assignment at the parcel seam.
pub(super) fn downtown_cells() -> (ZoneCell, ZoneCell) {
    use simulation::map::{Density::High, ZoneKind::*};
    (
        ZoneCell {
            kind: Residential,
            density: High,
        },
        ZoneCell {
            kind: Commercial,
            density: High,
        },
    )
}

/// The W3 site solver resolves legacy road connections on dry terrain and
/// enforces live shore contact and intake/outlet order on configured terrain.
pub(super) fn water_sites(
    sim: &mut Simulation,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
    anchor: Vec2,
) -> Result<[BuildingID; 2], String> {
    let place = |sim: &mut Simulation, name: &str| {
        let kind = BuildingKind::GoodsCompany(GoodsCompanyID::new(name));
        let (building, _) =
            place_scored_company(sim, plan, roads, name, |map, road, footprint| {
                let request = WaterSiteRequest::for_placement(kind, footprint, Some(road))?;
                water_service_site(map, request, &river_site_summary(map)).ok()?;
                Some(footprint.center().distance2(anchor))
            })?
            .ok_or_else(|| format!("no clear W3 site for {name}"))?;
        water_provider_site(&sim.map(), building)
            .ok_or_else(|| format!("missing W3 provider site for {name}"))?
            .map_err(|error| format!("invalid W3 provider site for {name}: {error}"))?;
        Ok::<_, String>(building)
    };
    Ok([place(sim, "water-pump")?, place(sim, "sewage-outlet")?])
}

pub(super) fn frontage_roads(plan: &TownPlan, built: &BuiltNetwork) -> BTreeSet<RoadID> {
    built
        .links
        .iter()
        .filter_map(|(key, span)| plan.graph.links[key].frontage().then_some(span.road))
        .collect()
}

pub(super) fn apply(
    sim: &mut Simulation,
    context: &str,
    commands: &[WorldCommand],
) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }
    sim.apply_commands(commands);
    let outcomes = sim.read::<CommandOutcomes>();
    if outcomes.accepted.len() != commands.len() {
        let index = (0..commands.len())
            .find(|i| !outcomes.accepted.contains(i))
            .unwrap();
        return Err(format!(
            "{context}, command {index}: {:?}",
            outcomes.rejections_for(index)
        ));
    }
    Ok(())
}

pub(super) fn place_company(
    sim: &mut Simulation,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
    name: &str,
    anchor: Vec2,
) -> Result<(BuildingID, RoadID), String> {
    place_scored_company(sim, plan, roads, name, |_, _, footprint| {
        Some(footprint.center().distance2(anchor))
    })?
    .ok_or_else(|| format!("no clear fitting site for {name}"))
}

pub(super) fn place_scored_company(
    sim: &mut Simulation,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
    name: &str,
    mut score: impl FnMut(&Map, RoadID, OBB) -> Option<f32>,
) -> Result<Option<(BuildingID, RoadID)>, String> {
    let id = GoodsCompanyID::new(name);
    let proto = try_prototype(id).ok_or_else(|| format!("missing CITYMAP prototype {name}"))?;
    let kind = BuildingKind::GoodsCompany(id);
    let (obb, road) = {
        let map = sim.map();
        let mut best = None;
        for &id in roads {
            let road = &map.roads()[id];
            if map.electricity.net_id(id).is_none() || road.sidewalks(road.src).incoming.is_none() {
                continue;
            }
            let points = road.points();
            for offset in (24..(points.length() as usize).saturating_sub(24)).step_by(10) {
                let (point, tangent) = points.point_dir_along(offset as f32);
                if (point.z - 10.0).abs() > 1.0 {
                    continue;
                }
                let Some(across) = tangent.xy().perpendicular().try_normalize() else {
                    continue;
                };
                for outward in [across, -across] {
                    let center =
                        point.xy() + outward * (road.width * 0.5 + proto.size.h * 0.5 + 2.0);
                    let footprint = OBB::new(center, outward, proto.size.h, proto.size.w);
                    let Some(score) = score(&map, id, footprint) else {
                        continue;
                    };
                    if best.as_ref().is_some_and(|(old, _, _)| score >= *old) {
                        continue;
                    }
                    if site_clear(&map, plan, footprint) {
                        best = Some((score, footprint, id));
                    }
                }
            }
        }
        let Some((_, footprint, road)) = best else {
            return Ok(None);
        };
        (footprint, road)
    };
    let gen = BuildingGen::NoWalkway {
        door_pos: geom::vec2(0.0, -proto.size.h * 0.5),
    };
    // A road-facing door is also the search anchor for the real parking and
    // delivery paths, including for the large power plant.
    let building = place_special(sim, name, kind, obb, gen, road)?;
    Ok(Some((building, road)))
}

pub(super) fn site_clear(map: &Map, plan: &TownPlan, footprint: OBB) -> bool {
    let bounds = geom::AABB::new_ll_ur(plan.config.owned_min, plan.config.owned_max);
    footprint
        .corners
        .iter()
        .all(|&corner| bounds.contains(corner))
        && !map.building_overlaps(footprint)
        && map
            .spatial_map()
            .query(footprint, ProjectFilter::INTER | ProjectFilter::ROAD)
            .next()
            .is_none()
        && !map
            .environment()
            .intersects_water(&Polygon(footprint.corners.to_vec()))
        && !crosses_reserved_bank(plan, footprint)
}

pub(super) fn crosses_reserved_bank(plan: &TownPlan, footprint: OBB) -> bool {
    plan.river.iter().any(|reach| {
        reach.downstream.windows(2).any(|pair| {
            let shore = reach.source_width_scaled.unwrap_or(88.0) * 0.5 + 6.0;
            footprint
                .corners
                .iter()
                .chain([footprint.center()].iter())
                .any(|&p| {
                    super::network::project_on_segment(p, pair[0], pair[1]).distance(p) < shore
                })
        })
    }) || plan.riverside_park.as_ref().is_some_and(|park| {
        footprint
            .corners
            .iter()
            .any(|&p| contains(&park.bank_strip, p))
            || park
                .bank_strip
                .iter()
                .any(|&p| contains(&footprint.corners, p))
    })
}

pub(super) fn place_special(
    sim: &mut Simulation,
    key: &str,
    kind: BuildingKind,
    footprint: OBB,
    gen: BuildingGen,
    road: RoadID,
) -> Result<BuildingID, String> {
    apply(
        sim,
        key,
        &[WorldCommand::MapBuildSpecialBuilding {
            pos: footprint,
            kind,
            gen,
            zone: None,
            connected_road: Some(road),
        }],
    )?;
    sim.map()
        .buildings()
        .values()
        .find(|building| {
            building.kind == kind
                && building.connected_road == Some(road)
                && building.obb.center().distance2(footprint.center()) < 0.01
        })
        .map(|building| building.id)
        .ok_or_else(|| format!("{key}: placed building did not resolve"))
}
