use crate::ScenarioCtx;
use geom::{vec2, Polygon, Vec2, OBB};
use prototypes::{
    try_prototype, BuildingGen, FreightStationPrototypeID, GoodsCompanyID, Money, ServiceID,
    ServiceKind, Size2D,
};
use simulation::economy::Government;
use simulation::map::{
    BuildingID, BuildingKind, Density, LanePatternBuilder, Map, ProjectFilter, ProjectKind, RoadID,
    Zone, ZoneKind,
};
use simulation::souls::human::spawn_human;
use simulation::world_command::WorldCommand;
use simulation::Simulation;
use std::collections::BTreeSet;

pub trait CommandSink {
    fn sim(&self) -> &Simulation;

    /// Queue commands for the next simulation tick.
    fn submit(&mut self, commands: &[WorldCommand]);

    /// Whether commands have been queued but have yet to be applied.
    fn has_pending_commands(&self) -> bool;

    /// Advance at least one tick and return after applying all queued commands.
    fn flush(&mut self);
}

#[derive(Debug, Default)]
pub struct GridCity {
    pub roads: Vec<RoadID>,
}

/// Construct a grid with `nx` by `ny` junction positions, starting at `origin`.
pub fn grid_city(
    ctx: &mut dyn CommandSink,
    origin: Vec2,
    nx: u32,
    ny: u32,
    spacing: f32,
) -> GridCity {
    if nx == 0 || ny == 0 || !origin.is_finite() || !spacing.is_finite() || spacing < 1.0 {
        return GridCity::default();
    }

    if ctx.has_pending_commands() {
        ctx.flush();
    }
    let before: BTreeSet<_> = ctx.sim().map().roads().keys().collect();
    let pattern = LanePatternBuilder::new().build();
    let mut connect = |x, y, next_x, next_y| {
        let (from, to) = {
            let map = ctx.sim().map();
            let project = |x, y| {
                let xy = origin + vec2(x as f32, y as f32) * spacing;
                let z = map.environment().height(xy).unwrap_or_default();
                map.project(xy.z(z), 0.0, ProjectFilter::INTER | ProjectFilter::ROAD)
            };
            let from = project(x, y);
            let to = project(next_x, next_y);
            if let (ProjectKind::Intersection(a), ProjectKind::Intersection(b)) =
                (from.kind, to.kind)
            {
                if map
                    .find_road(a, b)
                    .or_else(|| map.find_road(b, a))
                    .is_some()
                {
                    return;
                }
            }
            (from, to)
        };

        ctx.submit(&[WorldCommand::MapMakeConnection {
            from,
            to,
            inter: None,
            pat: pattern.clone(),
        }]);
        ctx.flush();
    };

    // Close each block before extending the next so the map keeps its junctions.
    if nx == 1 {
        for y in 1..ny {
            connect(0, y - 1, 0, y);
        }
    } else if ny == 1 {
        for x in 1..nx {
            connect(x - 1, 0, x, 0);
        }
    } else {
        for y in 0..ny - 1 {
            for x in 0..nx - 1 {
                if y == 0 {
                    connect(x, y, x + 1, y);
                }
                connect(x + 1, y, x + 1, y + 1);
                connect(x + 1, y + 1, x, y + 1);
                if x == 0 {
                    connect(x, y + 1, x, y);
                }
            }
        }
    }

    let mut roads: Vec<_> = ctx
        .sim()
        .map()
        .roads()
        .keys()
        .filter(|id| !before.contains(id))
        .collect();
    roads.sort_unstable();
    GridCity { roads }
}

/// Paint one zoning rectangle per block in a grid laid out by `grid_city`.
pub fn paint_zone_blocks(
    ctx: &mut dyn CommandSink,
    origin: Vec2,
    spacing: f32,
    blocks: impl IntoIterator<Item = (u32, u32, ZoneKind, Density)>,
) {
    let commands: Vec<_> = blocks
        .into_iter()
        .map(|(x, y, kind, density)| WorldCommand::ZonePaint {
            area: OBB::new(
                origin + vec2(x as f32 + 0.5, y as f32 + 0.5) * spacing,
                Vec2::X,
                spacing,
                spacing,
            ),
            kind: Some(kind),
            density,
        })
        .collect();
    if !commands.is_empty() {
        ctx.submit(&commands);
        ctx.flush();
    }
}

/// Place a freight station with a footprint sized to the available block.
pub fn place_freight_station(
    ctx: &mut dyn CommandSink,
    near: Vec2,
    size: Size2D,
) -> Option<BuildingID> {
    let proto = try_prototype(FreightStationPrototypeID::new("freight-station"))?;
    if ctx.has_pending_commands() {
        ctx.flush();
    }
    let (road, obb) = roadside_site(&ctx.sim().map(), near, size, false, |_, _| true)?;
    let kind = BuildingKind::RailFreightStation(proto.id);
    ctx.submit(&[WorldCommand::MapBuildSpecialBuilding {
        pos: obb,
        kind,
        gen: BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        },
        zone: None,
        connected_road: Some(road),
    }]);
    ctx.flush();
    let map = ctx.sim().map();
    let building = map
        .spatial_map()
        .query(obb.center(), ProjectFilter::BUILDING)
        .filter_map(|p| p.as_building())
        .find(|&id| map.buildings()[id].kind == kind);
    building
}

/// Add startup funds to a scenario fixture directly, outside the budget ledger.
/// Construction still goes through the normal affordability checks and ledger.
pub(crate) fn grant_money(ctx: &mut ScenarioCtx, amount: Money) {
    ctx.sim.write::<Government>().money += amount;
}

/// Build at most `max` houses on existing lots in lot ID order, returning their building IDs.
pub fn fill_houses(ctx: &mut dyn CommandSink, max: usize) -> Vec<BuildingID> {
    if max == 0 {
        return Vec::new();
    }

    if ctx.has_pending_commands() {
        ctx.flush();
    }
    let (mut lots, before) = {
        let map = ctx.sim().map();
        let lots: Vec<_> = map.lots().keys().collect();
        let buildings: BTreeSet<_> = map.buildings().keys().collect();
        (lots, buildings)
    };
    lots.sort_unstable();
    lots.truncate(max);
    if lots.is_empty() {
        return Vec::new();
    }

    let commands: Vec<_> = lots.into_iter().map(WorldCommand::MapBuildHouse).collect();
    ctx.submit(&commands);
    ctx.flush();

    let mut houses: Vec<_> = ctx
        .sim()
        .map()
        .buildings()
        .iter()
        .filter_map(|(id, building)| {
            (building.kind == BuildingKind::House && !before.contains(&id)).then_some(id)
        })
        .collect();
    houses.sort_unstable();
    houses
}

/// Populate the prebuilt P0 fixtures with one starting household per house.
pub(crate) fn seed_households(sim: &mut Simulation, houses: &[BuildingID]) -> Result<(), String> {
    for &house in houses {
        spawn_human(sim, house).ok_or_else(|| format!("could not seed a resident in {house:?}"))?;
    }
    Ok(())
}

/// Place a company at a clear, sidewalk-connected site near `near`.
pub fn place_company(
    ctx: &mut dyn CommandSink,
    proto_name: &str,
    near: Vec2,
) -> Option<BuildingID> {
    place_company_at_frontage(ctx, proto_name, near, None)
}

pub(crate) fn place_company_on_network(
    ctx: &mut dyn CommandSink,
    proto_name: &str,
    near: Vec2,
    utility_site: BuildingID,
) -> Option<BuildingID> {
    place_company_at_frontage(ctx, proto_name, near, Some(utility_site))
}

fn place_company_at_frontage(
    ctx: &mut dyn CommandSink,
    proto_name: &str,
    near: Vec2,
    utility_site: Option<BuildingID>,
) -> Option<BuildingID> {
    let proto = try_prototype(GoodsCompanyID::new(proto_name))?;
    if !near.is_finite() {
        return None;
    }

    if ctx.has_pending_commands() {
        ctx.flush();
    }
    let kind = BuildingKind::GoodsCompany(proto.id);
    let fire_station = try_prototype(ServiceID::from(proto.id))
        .is_some_and(|service| service.service_kind == ServiceKind::Fire);
    let command = |road, obb: OBB| WorldCommand::MapBuildSpecialBuilding {
        pos: obb,
        kind,
        gen: proto.bgen,
        zone: proto
            .zone
            .as_ref()
            .map(|_| Zone::new(Polygon::from(obb.corners.as_slice()), Vec2::X)),
        connected_road: Some(road),
    };
    let (road, obb) = {
        let sim = ctx.sim();
        let map = sim.map();
        let network = match utility_site {
            Some(site) => Some(map.electricity.net_id(site)?),
            None => None,
        };
        roadside_site(&map, near, proto.size, fire_station, |road, obb| {
            network.is_none_or(|net| map.electricity.net_id(road) == Some(net))
                && (!fire_station || command(road, obb).preview(sim).is_ok())
        })?
    };
    ctx.submit(&[command(road, obb)]);
    ctx.flush();

    let map = ctx.sim().map();
    let id = map
        .spatial_map()
        .query(obb.center(), ProjectFilter::BUILDING)
        .filter_map(|project| project.as_building())
        .find(|id| map.buildings()[*id].kind == kind);
    id
}

pub(crate) fn place_grid_power_plant(
    ctx: &mut ScenarioCtx,
    origin: Vec2,
) -> Result<BuildingID, String> {
    let spur = grid_city(ctx, origin + vec2(-260.0, 200.0), 2, 1, 260.0);
    let Some(&road) = spur.roads.first().filter(|_| spur.roads.len() == 1) else {
        return Err("could not connect a power spur to the grid".into());
    };
    place_company(ctx, "coal-power-plant", origin + vec2(-130.0, 100.0))
        .filter(|&id| ctx.sim.map().buildings()[id].connected_road == Some(road))
        .ok_or_else(|| "could not connect a power plant to the grid".into())
}

pub(crate) fn place_service_sites(
    ctx: &mut ScenarioCtx,
    origin: Vec2,
    sites: impl IntoIterator<Item = (&'static str, Vec2)>,
) -> Result<Vec<BuildingID>, String> {
    sites
        .into_iter()
        .map(|(name, offset)| {
            place_company(ctx, name, origin + offset)
                .ok_or_else(|| format!("could not place {name} at {offset:?}"))
        })
        .collect()
}

pub(crate) const UTILITY_SITES: [(&str, Vec2); 2] = [
    ("water-pump", vec2(-30.0, 50.0)),
    ("sewage-outlet", vec2(-30.0, 350.0)),
];
pub(crate) const SCHOOL_SITE: (&str, Vec2) = ("school", vec2(-30.0, 250.0));

fn roadside_site(
    map: &Map,
    near: Vec2,
    size: Size2D,
    fire_station: bool,
    mut suitable: impl FnMut(RoadID, OBB) -> bool,
) -> Option<(RoadID, OBB)> {
    let mut best: Option<(f32, RoadID, OBB)> = None;
    let filter = ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING;
    for project in map
        .spatial_map()
        .query_around(near, 250.0, ProjectFilter::ROAD)
    {
        let ProjectKind::Road(id) = project else {
            continue;
        };
        let road = &map.roads()[id];
        if road.sidewalks(road.src).incoming.is_none() {
            continue;
        }

        let points = road.points();
        let (closest, segment) = points.project_segment_2d(near);
        let Some(tangent) = points.segment_vec(segment.saturating_sub(1)) else {
            continue;
        };
        let midpoint = points.point_dir_along(points.length() * 0.5);
        let end = if fire_station { (points.length() as usize).saturating_sub(24) } else { 24 };
        let samples = (24..end).step_by(5).map(|offset| points.point_dir_along(offset as f32));
        let setbacks: &[f32] = if fire_station { &[1.0, 12.5] } else { &[1.0] };
        for (pos, dir) in [(closest, tangent), midpoint].into_iter().chain(samples) {
            let Some(dir) = dir.xy().try_normalize() else {
                continue;
            };
            let side = dir.perpendicular();
            let positions = setbacks.iter().flat_map(|&setback| [(side, setback), (-side, setback)]);
            for (outward, setback) in positions {
                let center = pos.xy() + outward * ((road.width + size.w) * 0.5 + setback);
                let distance = center.distance2(near);
                if best
                    .as_ref()
                    .is_some_and(|&(d, r, _)| (distance, id) >= (d, r))
                {
                    continue;
                }

                let obb = OBB::new(center, outward, size.w, size.h);
                if map.environment().height(center).is_none()
                    || map.spatial_map().query(obb, filter).any(|p| p != project)
                    || !suitable(id, obb)
                {
                    continue;
                }
                best = Some((distance, id, obb));
            }
        }
    }
    best.map(|(_, id, obb)| (id, obb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::smoke;

    #[test]
    fn command_sink_applies_queued_roads_on_flush() {
        let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
        let sink: &mut dyn CommandSink = &mut ctx;
        let start = sink.sim().get_tick();
        let roads = sink.sim().map().roads().len();
        let map = sink.sim().map();
        let project = |x, y| {
            let pos = vec2(x, y);
            let z = map.environment().height(pos).unwrap();
            map.project(pos.z(z), 0.0, ProjectFilter::INTER | ProjectFilter::ROAD)
        };
        let (from, to) = (project(700.0, 700.0), project(860.0, 700.0));
        drop(map);

        sink.submit(&[WorldCommand::MapMakeConnection {
            from,
            to,
            inter: None,
            pat: LanePatternBuilder::new().build(),
        }]);
        assert_eq!(sink.sim().get_tick(), start);
        assert_eq!(sink.sim().map().roads().len(), roads);

        sink.flush();
        assert_eq!(sink.sim().get_tick(), start + 1);
        assert_eq!(sink.sim().map().roads().len(), roads + 1);

        let grid = grid_city(sink, vec2(700.0, 700.0), 2, 2, 160.0);
        assert_eq!(grid.roads.len(), 3);
        assert_eq!(sink.sim().get_tick(), start + 4);
    }
}
