use super::{ImportReport, LinkClass, NormalizedCity, LESCE_BOUNDS};
use crate::builders::{place_company, CommandSink};
use crate::ScenarioCtx;
use geom::{vec2, Vec2, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID, RollingStockID, Tick};
use simulation::map::import::{ImportedJunction, ImportedLink, ImportedNetwork};
use simulation::map::{
    BuildingID, BuildingKind, LaneID, LaneKind, LanePattern, LanePatternBuilder, Map, PathKind,
    Pathfinder, ProjectFilter, ProjectKind, RoadID, Traversable, TraverseDirection, TraverseKind,
};
use simulation::map_dynamic::{BuildingInfos, Itinerary};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{MapPreset, NewGameOptions, Simulation};
use std::collections::BTreeSet;

pub(super) const OPTIONS: NewGameOptions = NewGameOptions {
    preset: MapPreset::Flat,
    seed: 46_360_514_160,
    terrain_chunks: 16,
    starting_money_bucks: 250_000,
    progression: simulation::economy::progression::ProgressionMode::FreeBuild,
};
pub(super) const TARGET_OPENING_BEDS: usize = 2_900;
pub(super) const OPENING_POPULATION: std::ops::RangeInclusive<usize> = 2_500..=3_500;
pub(super) const STATION: Vec2 = vec2(3890.0, 4080.0);
const SUPPLEMENTAL_FIRE_SITE: Vec2 = vec2(4390.0, 3570.0);
pub(super) struct Layout {
    utility_road: usize,
    external_rail: usize,
    freight_rail: usize,
    mainline: [simulation::map::import::BoundaryStub; 2],
    motorway: Vec<simulation::map::import::BoundaryStub>,
}

pub(super) fn add_lesce_infrastructure(
    city: &mut NormalizedCity,
    report: &mut ImportReport,
) -> Result<Layout, String> {
    let road_branch = split_near(city, 168406420, vec2(2698.0, 4270.0))?;
    let siding_switch = split_near(city, 61695232, vec2(3754.0, 4395.0))?;
    let stubs = city.city.boundary_stubs.clone();
    let mainline: Vec<_> = stubs
        .iter()
        .copied()
        .filter(|s| city.classes[s.link] == LinkClass::MainRail)
        .collect();
    let mut mainline: [_; 2] = mainline.try_into().map_err(|stubs: Vec<_>| {
        format!(
            "expected two clipped Lesce mainline ends, found {}",
            stubs.len()
        )
    })?;
    let point = |city: &NormalizedCity, id: usize| city.city.junctions[id].position;
    mainline.sort_by(|a, b| {
        point(city, a.junction)
            .y
            .total_cmp(&point(city, b.junction).y)
    });
    let south_stub = mainline[0];
    let south = point(city, south_stub.junction);
    let line = &city.city.links[south_stub.link].points;
    let inside = if south_stub.at_start {
        line[1]
    } else {
        line[line.len() - 2]
    };
    let direction = (south - inside)
        .try_normalize()
        .ok_or("invalid mainline clip direction")?;
    let rail_end = south + direction * 390.0;
    let rail_end_id = add_junction(city, rail_end);
    let external_rail = add_link(
        city,
        south_stub.junction,
        rail_end_id,
        vec![south, rail_end],
        LinkClass::MainRail,
        LanePatternBuilder::new()
            .rail(true)
            .speed_limit(22.0)
            .build(),
    );
    report
        .synthetic
        .push("390 m mainline extension and external rail market in the southern buffer".into());

    let motorway: Vec<_> = stubs
        .iter()
        .copied()
        .filter(|s| city.classes[s.link] == LinkClass::Motorway)
        .collect();
    if motorway.len() != 4 {
        return Err(format!(
            "expected four clipped A2 carriageways, found {}",
            motorway.len()
        ));
    }
    let (min, max) = LESCE_BOUNDS.limits();
    for (y, outward) in [(min.y, -Vec2::Y), (max.y, Vec2::Y)] {
        let is_on_edge = |stub: &&simulation::map::import::BoundaryStub| {
            (point(city, stub.junction).y - y).abs() < 0.1
        };
        let pair: Vec<_> = motorway.iter().filter(is_on_edge).copied().collect();
        let [outbound, inbound] = if let [a, b] = pair.as_slice() {
            if a.at_start {
                [*b, *a]
            } else {
                [*a, *b]
            }
        } else {
            return Err(format!("could not pair A2 carriageways at y={y}"));
        };
        if outbound.at_start || !inbound.at_start {
            return Err(format!("A2 boundary at y={y} has incompatible directions"));
        }
        let a = point(city, outbound.junction);
        let b = point(city, inbound.junction);
        let side = (b - a).normalize();
        let points = vec![
            a,
            a + outward * 45.0,
            a + outward * 74.0 - side * 8.0,
            (a + b) * 0.5 + outward * 88.0,
            b + outward * 74.0 + side * 8.0,
            b + outward * 45.0,
            b,
        ];
        add_link(
            city,
            outbound.junction,
            inbound.junction,
            points,
            LinkClass::Motorway,
            LanePatternBuilder::new()
                .sidewalks(false)
                .parking(false)
                .n_lanes(2)
                .one_way(true)
                .speed_limit(10.0)
                .build(),
        );
        report.synthetic.push(format!(
            "A2 carriageway turnaround in the {} buffer",
            if y == min.y { "southern" } else { "northern" }
        ));
    }

    let siding_end = vec2(3652.0, 4709.0);
    let siding_end_id = add_junction(city, siding_end);
    let freight_rail = add_link(
        city,
        siding_switch,
        siding_end_id,
        vec![point(city, siding_switch), vec2(3756.0, 4466.0), siding_end],
        LinkClass::OtherRail,
        LanePatternBuilder::new()
            .rail(true)
            .speed_limit(14.0)
            .build(),
    );
    report
        .synthetic
        .push("freight siding parallel to the mainline north of Lesce-Bled".into());

    let from = point(city, road_branch);
    let end = vec2(2130.0, 4150.0);
    let end_id = add_junction(city, end);
    let utility_road = add_link(
        city,
        road_branch,
        end_id,
        vec![from, end],
        LinkClass::Street,
        LanePatternBuilder::new().build(),
    );
    report
        .synthetic
        .push("utility access spur west of Šobčeva cesta".into());
    Ok(Layout {
        utility_road,
        external_rail,
        freight_rail,
        mainline,
        motorway,
    })
}

fn add_junction(city: &mut NormalizedCity, position: Vec2) -> usize {
    let id = city.city.junctions.len();
    city.city.junctions.push(ImportedJunction {
        id,
        position,
        roundabout_radius: None,
    });
    id
}

fn add_link(
    city: &mut NormalizedCity,
    from: usize,
    to: usize,
    points: Vec<Vec2>,
    class: LinkClass,
    pattern: LanePattern,
) -> usize {
    let index = city.city.links.len();
    city.city.links.push(ImportedLink {
        source_way_ids: Vec::new(),
        from,
        to,
        points,
        pattern,
        frontage: false,
    });
    city.classes.push(class);
    index
}

fn split_near(city: &mut NormalizedCity, source: u64, target: Vec2) -> Result<usize, String> {
    let (index, offset, position) = city
        .city
        .links
        .iter()
        .enumerate()
        .filter(|(_, link)| link.source_way_ids.contains(&source))
        .flat_map(|(i, link)| {
            link.points
                .iter()
                .enumerate()
                .map(move |(j, &pos)| (i, j, pos))
        })
        .min_by(|a, b| a.2.distance2(target).total_cmp(&b.2.distance2(target)))
        .filter(|(_, _, pos)| pos.distance(target) < 20.0)
        .ok_or_else(|| format!("missing connection anchor on way/{source}"))?;
    let link = &city.city.links[index];
    if offset == 0 {
        return Ok(link.from);
    }
    if offset == link.points.len() - 1 {
        return Ok(link.to);
    }
    let mut tail = link.clone();
    let junction = add_junction(city, position);
    tail.points.drain(..offset);
    tail.from = junction;
    let first = &mut city.city.links[index];
    first.points.truncate(offset + 1);
    first.to = junction;
    let tail_index = city.city.links.len();
    for stub in &mut city.city.boundary_stubs {
        if stub.link == index && !stub.at_start {
            stub.link = tail_index;
        }
    }
    city.city.links.push(tail);
    city.classes.push(city.classes[index]);
    Ok(junction)
}

pub(super) struct Facilities {
    pub(super) plant: BuildingID,
    pub(super) utilities: Vec<BuildingID>,
    pub(super) terminal: BuildingID,
    pub(super) external: BuildingID,
}

pub(super) fn place_facilities(
    ctx: &mut ScenarioCtx,
    city: &NormalizedCity,
    network: &ImportedNetwork,
    layout: &Layout,
    report: &mut ImportReport,
) -> Result<Facilities, String> {
    let utility_road = network.roads[&layout.utility_road];
    let mut utilities = Vec::new();
    for (name, site) in [
        ("coal-power-plant", vec2(2527.0, 4328.0)),
        ("coal-mine", vec2(2300.0, 4270.0)),
        ("water-pump", vec2(2450.0, 4194.0)),
        ("sewage-outlet", vec2(2230.0, 4151.0)),
        ("water-pump", vec2(2350.0, 4233.0)),
        ("water-pump", vec2(2400.0, 4290.0)),
        ("sewage-outlet", vec2(2310.0, 4158.0)),
        ("sewage-outlet", vec2(2575.0, 4230.0)),
        // Size water and sewage for upgraded residences and employers too.
        ("water-pump", vec2(2175.0, 4130.0)),
        ("sewage-outlet", vec2(2175.0, 4188.0)),
        ("water-pump", vec2(2255.0, 4210.0)),
        ("sewage-outlet", vec2(2385.0, 4171.0)),
        ("water-pump", vec2(2515.0, 4203.0)),
        ("sewage-outlet", vec2(2630.0, 4230.0)),
        ("water-pump", vec2(2345.0, 4161.0)),
        ("sewage-outlet", vec2(2660.0, 4236.0)),
    ] {
        let id = place_company(ctx, name, site)
            .filter(|id| ctx.sim.map().buildings()[*id].connected_road == Some(utility_road))
            .ok_or_else(|| format!("could not place {name} on the utility access spur"))?;
        utilities.push(id);
        report
            .synthetic
            .push(format!("{name} on the utility access spur"));
    }
    let plant = utilities[0];

    let (external_site, train_lane, train_dist) = {
        let map = ctx.sim.map();
        let road = &map.roads()[network.roads[&layout.external_rail]];
        let train_lane = road.incoming_lanes_to(road.src)[0].0;
        let points = &map.lanes()[train_lane].points;
        let dist = points.length() * 0.5;
        let (middle, dir) = points.point_dir_along(dist);
        (
            OBB::new(middle.xy(), dir.xy(), 150.0, 72.2),
            train_lane,
            dist,
        )
    };
    let external = ctx
        .sim
        .write::<Map>()
        .build_special_building(
            &external_site,
            BuildingKind::ExternalTrading,
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            None,
            None,
        )
        .ok_or("could not build the external rail market")?;
    ctx.sim.write::<BuildingInfos>().insert(external);

    let (obb, access, door) = freight_site(
        &ctx.sim.map(),
        network.roads[&layout.freight_rail],
        utility_road,
        external_site.center().z0(),
    )?;
    let kind = BuildingKind::RailFreightStation(FreightStationPrototypeID::new("freight-station"));
    apply_checked(
        ctx,
        &[WorldCommand::MapBuildSpecialBuilding {
            pos: obb,
            kind,
            gen: BuildingGen::NoWalkway { door_pos: door },
            zone: None,
            connected_road: Some(access),
        }],
    )?;
    let terminal = ctx
        .sim
        .map()
        .buildings()
        .values()
        .find(|building| building.kind == kind && building.connected_road == Some(access))
        .map(|building| building.id)
        .ok_or("could not build the Lesce freight terminal")?;
    apply_checked(
        ctx,
        &[WorldCommand::SpawnTrain {
            wagons: std::iter::once(RollingStockID::new("locomotive"))
                .chain([RollingStockID::new("freight-wagon"); 3])
                .collect(),
            lane: train_lane,
            dist: train_dist,
        }],
    )?;
    if ctx.sim.world().trains.len() != 1 {
        return Err("freight consist was not spawned".into());
    }
    report.synthetic.push("road-connected freight terminal and locomotive with three freight wagons at the Lesce-Bled freight siding".into());

    let mut service_count = 0;
    let station = city
        .sites
        .iter()
        .filter(|site| site.kind == "passenger station")
        .min_by(|a, b| {
            (
                !a.source.starts_with("way/"),
                a.position.distance2(STATION).to_bits(),
                &a.source,
            )
                .cmp(&(
                    !b.source.starts_with("way/"),
                    b.position.distance2(STATION).to_bits(),
                    &b.source,
                ))
        })
        .ok_or("missing Lesce-Bled passenger site")?;
    place_passenger_station(&mut ctx.sim, station, plant, report)?;
    for site in city
        .sites
        .iter()
        .filter(|site| site.kind != "passenger station")
    {
        let before: BTreeSet<_> = ctx.sim.map().buildings().keys().collect();
        let placed = place_company(ctx, site.kind, site.position);
        let connected = placed.is_some_and(|id| {
            let map = ctx.sim.map();
            let building = &map.buildings()[id];
            building.obb.center().distance(site.position) <= 125.0
                && map.electricity.net_id(id) == map.electricity.net_id(plant)
                && !before.contains(&id)
        });
        if connected {
            service_count += 1;
            *report
                .feature_counts
                .entry(format!("placed_{}", site.kind))
                .or_default() += 1;
        } else {
            if let Some(id) = placed.filter(|id| !before.contains(id)) {
                apply_checked(ctx, &[WorldCommand::MapRemoveBuilding(id)])?;
            }
            report.skipped.push((
                site.source.clone(),
                format!("no clear, connected {} site within 125 m", site.kind),
            ));
        }
    }
    report
        .feature_counts
        .insert("placed_mapped_services".into(), service_count);
    // The mapped stations are about 2 km apart before accounting for road travel.
    let fire = place_company(ctx, "fire-station", SUPPLEMENTAL_FIRE_SITE)
        .filter(|&id| {
            let map = ctx.sim.map();
            map.electricity.net_id(id) == map.electricity.net_id(plant)
        })
        .ok_or("could not place the supplemental fire station on the utility network")?;
    let center = ctx.sim.map().buildings()[fire].obb.center();
    report.synthetic.push(format!(
        "supplemental fire station between Lesce and Radovljica at ({:.1}, {:.1})",
        center.x, center.y
    ));
    report
        .feature_counts
        .insert("placed_supplemental_fire_stations".into(), 1);
    Ok(Facilities {
        plant,
        utilities,
        terminal,
        external,
    })
}

fn place_passenger_station(
    sim: &mut Simulation,
    site: &super::Site,
    plant: BuildingID,
    report: &mut ImportReport,
) -> Result<(), String> {
    let (shape, road) = {
        let map = sim.map();
        let net = map.electricity.net_id(plant);
        let road = map
            .nearest_lane(site.position.z0(), LaneKind::Walking, Some(100.0))
            .map(|lane| map.lanes()[lane].parent)
            .filter(|&road| map.electricity.net_id(road) == net)
            .ok_or("no station access road")?;
        let shape = [18.0, 13.0, 8.0]
            .into_iter()
            .map(|size| OBB::new(site.position, Vec2::X, size * 0.7, size))
            .find(|&shape| !map.building_overlaps(shape))
            .ok_or("mapped station footprint is occupied")?;
        (shape, road)
    };
    let id = sim
        .write::<Map>()
        .build_special_building(
            &shape,
            BuildingKind::TrainStation,
            BuildingGen::House,
            None,
            Some(road),
        )
        .ok_or("could not place Lesce-Bled station marker")?;
    sim.write::<BuildingInfos>().insert(id);
    report
        .feature_counts
        .insert("placed_passenger_station_markers".into(), 1);
    report.synthetic.push(format!(
        "Lesce-Bled passenger station marker at {} ({:.1}, {:.1}); inspect as Train Station",
        site.source, site.position.x, site.position.y,
    ));
    Ok(())
}

fn freight_site(
    map: &Map,
    rail: RoadID,
    utility: RoadID,
    market: geom::Vec3,
) -> Result<(OBB, RoadID, Vec2), String> {
    let utility_net = map.electricity.net_id(utility);
    let mut best = None::<(f32, OBB, RoadID, Vec2)>;
    // The dispatch target is 75 m along the station and 40 m off its centerline.
    for (lane, _) in map.roads()[rail].lanes_iter() {
        let lane = &map.lanes()[lane];
        let points = &lane.points;
        for offset in (16..points.length() as usize).step_by(8) {
            let (berth, tangent) = points.point_dir_along(offset as f32);
            let Some(dir) = tangent.xy().try_normalize() else {
                continue;
            };
            let across = dir.perpendicular();
            let center = berth.xy() - dir * 75.0 + across * 40.0;
            if center.distance(STATION) > 515.0 {
                continue;
            }
            for width in [84.0, 64.0, 44.0] {
                let obb = OBB::new(center, dir, 156.0, width);
                if map
                    .spatial_map()
                    .query(obb, ProjectFilter::BUILDING | ProjectFilter::INTER)
                    .any(|project| match project {
                        ProjectKind::Building(_) => true,
                        ProjectKind::Intersection(id) => {
                            map.intersections()[id].roads.iter().any(|rid| {
                                map.roads()[*rid]
                                    .lanes_iter()
                                    .any(|(_, kind)| kind == LaneKind::Driving)
                            })
                        }
                        _ => false,
                    })
                {
                    continue;
                }
                let mut passes_over_rail = false;
                let road_overlap =
                    map.spatial_map()
                        .query(obb, ProjectFilter::ROAD)
                        .any(|project| {
                            let ProjectKind::Road(id) = project else {
                                return false;
                            };
                            passes_over_rail |= map.roads()[id]
                                .lanes_iter()
                                .any(|(_, kind)| kind == LaneKind::Rail);
                            map.roads()[id]
                                .lanes_iter()
                                .any(|(_, kind)| kind == LaneKind::Driving)
                        });
                if road_overlap || !passes_over_rail {
                    continue;
                }
                for project in map
                    .spatial_map()
                    .query_around(center, 130.0, ProjectFilter::ROAD)
                {
                    let ProjectKind::Road(id) = project else {
                        continue;
                    };
                    let access = &map.roads()[id];
                    if access.sidewalks(access.src).incoming.is_none()
                        || map.electricity.net_id(id) != utility_net
                    {
                        continue;
                    }
                    let near = access.points().project_2d(center).xy();
                    let to_road = near - center;
                    let door = vec2(
                        to_road.dot(across).clamp(-width * 0.5, width * 0.5),
                        to_road.dot(dir).clamp(-78.0, 78.0),
                    );
                    let door_world = center + across * door.x + dir * door.y;
                    let distance = door_world.distance(near);
                    let score = distance * 3.0 + center.distance(STATION) + (84.0 - width) * 0.5;
                    if distance <= 65.0
                        && best
                            .as_ref()
                            .is_none_or(|(s, _, rid, _)| (score, id) < (*s, *rid))
                        && Itinerary::route(Tick(0), berth, market, map, PathKind::Rail).is_some()
                        && Itinerary::route(Tick(0), market, berth, map, PathKind::Rail).is_some()
                    {
                        best = Some((score, obb, id, door));
                    }
                }
            }
        }
    }
    best.map(|(_, obb, road, door)| (obb, road, door))
        .ok_or_else(|| "no clear, routable freight footprint along the Lesce-Bled siding".into())
}

pub(super) fn apply_checked(
    ctx: &mut ScenarioCtx,
    commands: &[WorldCommand],
) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }
    ctx.submit(commands);
    ctx.flush();
    let result = ctx.sim.read::<CommandOutcomes>();
    if !result.last.is_empty() || result.accepted.len() != commands.len() {
        return Err(format!(
            "{} of {} city commands applied, rejections {:?}",
            result.accepted.len(),
            commands.len(),
            result.last
        ));
    }
    Ok(())
}

pub(super) fn check_routes(
    sim: &Simulation,
    city: &NormalizedCity,
    network: &ImportedNetwork,
    layout: &Layout,
    facilities: &Facilities,
    report: &mut ImportReport,
) -> Result<(), String> {
    let map = sim.map();
    let tick = Tick(sim.get_tick());
    let terminal = &map.buildings()[facilities.terminal];
    let external = &map.buildings()[facilities.external];
    let axis = terminal.obb.axis()[1].normalize();
    let berth =
        (terminal.obb.center() + axis * 75.0 - axis.perpendicular() * 40.0).z(terminal.height);
    let market = external.obb.center().z0();
    for (label, a, b) in [
        ("freight outbound", berth, market),
        ("freight inbound", market, berth),
    ] {
        route_result(
            report,
            label.to_owned(),
            Itinerary::route(tick, a, b, &map, PathKind::Rail).is_some(),
        )?;
    }
    let town = network
        .roads
        .iter()
        .filter(|&(index, _)| city.classes[*index] == LinkClass::Arterial)
        .map(|(_, id)| &map.roads()[*id])
        .filter(|road| {
            road.lanes_iter()
                .filter(|(_, kind)| *kind == LaneKind::Driving)
                .count()
                >= 2
        })
        .min_by(|a, b| {
            a.points()
                .project_2d(STATION)
                .xy()
                .distance2(STATION)
                .total_cmp(&b.points().project_2d(STATION).xy().distance2(STATION))
        })
        .ok_or("could not find a Lesce arterial for routing checks")?;
    let town_mid = town.points().point_along(town.length() * 0.5);
    for (label, a, b) in [
        ("freight road access inbound", town_mid, terminal.door_pos),
        ("freight road access outbound", terminal.door_pos, town_mid),
    ] {
        route_result(
            report,
            label.to_owned(),
            Itinerary::route(tick, a, b, &map, PathKind::Vehicle).is_some(),
        )?;
    }

    let town_lanes: Vec<_> = town
        .lanes_iter()
        .filter_map(|(id, kind)| (kind == LaneKind::Driving).then_some(id))
        .collect();
    for stub in &layout.motorway {
        let link = &city.city.links[stub.link];
        let label = format!(
            "A2 way/{} {}",
            link.source_way_ids[0],
            if stub.at_start {
                "into Lesce"
            } else {
                "out of Lesce"
            }
        );
        let road = &map.roads()[network.roads[&stub.link]];
        let lanes = if stub.at_start {
            road.outgoing_lanes_from(road.src)
        } else {
            road.incoming_lanes_to(road.dst)
        };
        let ok = lanes
            .iter()
            .filter(|(_, kind)| *kind == LaneKind::Driving)
            .all(|&(lane, _)| {
                town_lanes.iter().any(|&town| {
                    let (from, to) = if stub.at_start {
                        (lane, town)
                    } else {
                        (town, lane)
                    };
                    lane_route(&map, tick, PathKind::Vehicle, from, to)
                })
            });
        route_result(report, label, ok)?;
    }
    for (index, label) in [
        (0, "mainline south to north"),
        (1, "mainline north to south"),
    ] {
        let [a, b] = [layout.mainline[index], layout.mainline[1 - index]];
        let first = &map.roads()[network.roads[&a.link]];
        let last = &map.roads()[network.roads[&b.link]];
        let from = first.outgoing_lanes_from(network.junctions[&a.junction])[0].0;
        let to = last.incoming_lanes_to(network.junctions[&b.junction])[0].0;
        route_result(
            report,
            label.into(),
            lane_route(&map, tick, PathKind::Rail, from, to),
        )?;
    }
    let crossing = LESCE_BOUNDS.project(46.3622451, 14.1566347);
    let check_crossing = |way, lane_kind, path_kind| {
        let mut legs = network
            .roads
            .iter()
            .filter(|&(index, _)| city.city.links[*index].source_way_ids.contains(&way))
            .filter_map(|(_, id)| {
                let road = &map.roads()[*id];
                let joint = [road.src, road.dst]
                    .into_iter()
                    .find(|id| map.intersections()[*id].pos.xy().is_close(crossing, 0.1))?;
                Some((road, joint))
            });
        let ((first, a), (last, b)) = (legs.next()?, legs.next()?);
        if a != b || legs.next().is_some() {
            return None;
        }
        for (source, destination) in [(first, last), (last, first)] {
            let from = source
                .incoming_lanes_to(a)
                .iter()
                .find(|(_, kind)| *kind == lane_kind)?
                .0;
            let to = destination
                .outgoing_lanes_from(a)
                .iter()
                .find(|(_, kind)| *kind == lane_kind)?
                .0;
            if !lane_route(&map, tick, path_kind, from, to)
                || !map.intersections()[a]
                    .turns()
                    .any(|turn| turn.id.src == from && turn.id.dst == to)
            {
                return None;
            }
        }
        Some(a)
    };
    let road_crossing = check_crossing(1230075924, LaneKind::Driving, PathKind::Vehicle);
    let rail_crossing = check_crossing(61695232, LaneKind::Rail, PathKind::Rail);
    route_result(
        report,
        "independent road and rail through routes at node/253609833".into(),
        road_crossing
            .zip(rail_crossing)
            .is_some_and(|(road, rail)| road != rail),
    )?;
    Ok(())
}

fn lane_route(map: &Map, tick: Tick, kind: PathKind, from: LaneID, to: LaneID) -> bool {
    kind.path(
        map,
        tick,
        Traversable::new(TraverseKind::Lane(from), TraverseDirection::Forward),
        to,
    )
    .is_some()
}

fn route_result(report: &mut ImportReport, label: String, result: bool) -> Result<(), String> {
    report.route_checks.insert(label.clone(), result);
    if result {
        Ok(())
    } else {
        Err(format!("required route failed: {label}"))
    }
}
