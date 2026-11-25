use geom::{Intersect, Polygon, Segment, Vec2, OBB};
use prototypes::{try_prototype, BuildingGen, GoodsCompanyID};
use simulation::map::{
    river_site_summary, water_provider_site, water_service_site, BuildingID, BuildingKind,
    LaneKind, Map, ProjectFilter, ProjectKind, RoadID, WaterSiteRequest, Zone, ZoneCell,
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
    place_scored_company(sim, plan, roads, name, |map, road, footprint| {
        map.roads()[road]
            .lanes_iter()
            .any(|(_, kind)| kind == LaneKind::Parking)
            .then(|| footprint.center().distance2(anchor))
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
                    if site_clear(&map, plan, footprint, None) {
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
    let building = place_special(sim, name, kind, obb, gen, road)?;
    Ok(Some((building, road)))
}

pub(super) fn site_clear(
    map: &Map,
    plan: &TownPlan,
    footprint: OBB,
    own_building: Option<BuildingID>,
) -> bool {
    let bounds = geom::AABB::new_ll_ur(plan.config.owned_min, plan.config.owned_max);
    footprint
        .corners
        .iter()
        .all(|&corner| bounds.contains(corner))
        && !map
            .spatial_map()
            .query(
                footprint,
                ProjectFilter::BUILDING | ProjectFilter::INTER | ProjectFilter::ROAD,
            )
            .any(|site| Some(site) != own_building.map(ProjectKind::Building))
        && !map
            .environment()
            .intersects_water(&Polygon(footprint.corners.to_vec()))
        && !crosses_reserved_bank(plan, footprint)
}

pub(super) fn crosses_reserved_bank(plan: &TownPlan, footprint: OBB) -> bool {
    let edges = footprint.segments();
    plan.river.iter().any(|reach| {
        let shore = reach.source_width_scaled.unwrap_or(88.0) * 0.5 + 6.0;
        reach.downstream.windows(2).any(|pair| {
            let centerline = Segment::new(pair[0], pair[1]);
            footprint.intersects(&centerline)
                || edges.iter().any(|edge| edge.distance(&centerline) <= shore)
        })
    }) || plan.riverside_park.as_ref().is_some_and(|park| {
        Polygon(park.bank_strip.clone()).intersects(&footprint)
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
    let zone = kind
        .as_goods_company()
        .and_then(try_prototype)
        .and_then(|company| company.zone.as_ref())
        .map(|_| Zone::new(Polygon(footprint.corners.to_vec()), footprint.axis()[0]));
    apply(
        sim,
        key,
        &[WorldCommand::MapBuildSpecialBuilding {
            pos: footprint,
            kind,
            gen,
            zone,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::citymap::{
        build_dry_town, plan_town, read_inputs, SourcePaths, SpacingLimits, TownConfig,
        DEFAULT_LESCE, DEFAULT_SAVA,
    };
    use std::path::Path;

    #[test]
    fn cm2_adapter_reserves_edge_only_bank_and_park_overlaps() {
        use crate::citymap::layout::{RiverReach, RiversidePark};
        use crate::citymap::SourceKey;
        use geom::vec2;

        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let mut plan = plan_town(&inputs, TownConfig::default(), SpacingLimits::default()).unwrap();
        let footprint = OBB::new(Vec2::ZERO, vec2(1.0, 0.0), 200.0, 20.0);
        plan.river = vec![RiverReach {
            way: 1,
            downstream: vec![vec2(55.0, 14.0), vec2(65.0, 14.0)],
            source_width_scaled: Some(2.0),
        }];
        plan.riverside_park = None;
        assert!(crosses_reserved_bank(&plan, footprint));

        plan.river.clear();
        plan.riverside_park = Some(RiversidePark {
            bank_strip: vec![
                vec2(55.0, -35.0), vec2(65.0, -35.0),
                vec2(65.0, 35.0), vec2(55.0, 35.0),
            ],
            far_bank_access: SourceKey::Authored("test"),
        });
        assert!(crosses_reserved_bank(&plan, footprint));
        let outside = OBB::new(vec2(0.0, 60.0), vec2(1.0, 0.0), 200.0, 20.0);
        assert!(!crosses_reserved_bank(&plan, outside));
    }

    #[test]
    fn cm2_adapter_places_coal_on_truck_frontage() {
        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let plan = plan_town(&inputs, TownConfig::default(), SpacingLimits::default()).unwrap();
        let (mut sim, built) = build_dry_town(&plan).unwrap();
        let roads = frontage_roads(&plan, &built);
        let (plant, road) = place_company(
            &mut sim,
            &plan,
            &roads,
            "coal-power-plant",
            geom::vec2(2630.0, 2440.0) + geom::vec2(130.0, 320.0),
        )
        .unwrap();
        let map = sim.map();
        assert_eq!(map.buildings()[plant].connected_road, Some(road));
        assert!(map.roads()[road]
            .lanes_iter()
            .any(|(_, kind)| kind == LaneKind::Parking));
        let door = map.buildings()[plant].door_pos;
        assert!(
            map.roads()[road]
                .points()
                .project_2d(door.xy())
                .xy()
                .distance(door.xy())
                < map.roads()[road].width * 0.5 + 5.0
        );
    }

    #[test]
    fn cm2_adapter_places_a_cereal_farm_with_a_field() {
        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let plan = plan_town(&inputs, TownConfig::default(), SpacingLimits::default()).unwrap();
        let (mut sim, built) = build_dry_town(&plan).unwrap();
        let roads = frontage_roads(&plan, &built);
        let (farm, road) = place_company(
            &mut sim,
            &plan,
            &roads,
            "cereal-farm",
            geom::vec2(1030.0, 1460.0),
        )
        .unwrap();
        let map = sim.map();
        let farm = &map.buildings()[farm];
        assert_eq!(farm.connected_road, Some(road));
        assert!(farm.zone.as_ref().is_some_and(|field| field.area > 0.0));
    }
}
