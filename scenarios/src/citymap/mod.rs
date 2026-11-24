mod layout;
mod network;
mod source;

pub use layout::*;
pub use network::*;
pub use source::*;

use simulation::map::LotLayout;
use simulation::{MapPreset, NewGameOptions, Simulation};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn schematic_cli(args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let mut args = args;
    let mut out = None;
    let mut lesce = None;
    let mut sava = None;
    while let Some(arg) = args.next() {
        let (Some("--out" | "--input" | "--sava-input"), Some(path)) = (arg.to_str(), args.next())
        else {
            return Err(
                "usage: scenarios citymap-schematic --out DIR [--input PATH] [--sava-input PATH]"
                    .into(),
            );
        };
        let destination = match arg.to_str().unwrap() {
            "--out" => &mut out,
            "--input" => &mut lesce,
            _ => &mut sava,
        };
        if destination.is_some() || path.as_encoded_bytes().starts_with(b"--") {
            return Err(format!("invalid or repeated {arg:?}"));
        }
        *destination = Some(std::path::absolute(Path::new(&path)).map_err(|err| err.to_string())?);
    }
    let out = out.ok_or("citymap-schematic requires --out DIR")?;
    let lesce = lesce.unwrap_or_else(|| PathBuf::from(DEFAULT_LESCE));
    let sava = sava.unwrap_or_else(|| PathBuf::from(DEFAULT_SAVA));
    export_schematic(
        SourcePaths {
            lesce: &lesce,
            sava: Some(&sava),
        },
        &out,
    )
}

fn preview_world(config: TownConfig) -> Result<Simulation, String> {
    crate::ensure_repo_root_cwd();
    simulation::init::init();
    // Stock New Game requires at least 16 chunks. Strip its starter
    // infrastructure via commands so the preview uses ordinary lane trimming.
    let mut sim = Simulation::new_game(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: config.map_chunks.max(NewGameOptions::MIN_TERRAIN_CHUNKS),
        seed: 20261122,
        starting_money_bucks: NewGameOptions::MAX_STARTING_MONEY_BUCKS,
        progression: simulation::economy::progression::ProgressionMode::FreeBuild,
        lot_layout: LotLayout::Uniform,
    })
    .map_err(|error| error.to_string())?;
    use simulation::world_command::{CommandOutcomes, WorldCommand};
    let commands: Vec<_> = {
        let map = sim.map();
        map.buildings()
            .keys()
            .map(WorldCommand::MapRemoveBuilding)
            .chain(map.roads().keys().map(WorldCommand::MapRemoveRoad))
            .collect()
    };
    sim.apply_commands(&commands);
    if !sim.read::<CommandOutcomes>().last.is_empty() || !sim.map().is_empty() {
        return Err("could not clear stock infrastructure for CITYMAP preview".into());
    }
    Ok(sim)
}

pub fn export_schematic(paths: SourcePaths<'_>, out: &Path) -> Result<(), String> {
    let out = std::path::absolute(out).map_err(|err| err.to_string())?;
    let inputs = read_inputs(paths)?;
    let config = TownConfig::default();
    let plan = plan_town(&inputs, config, SpacingLimits::default())?;
    let mut sim = preview_world(config)?;
    let built = build_network(&mut sim, &plan.graph, plan.limits)?;
    let map = sim.map();
    let mut routes = route_checks(&map, &plan.graph, &built);
    routes.extend(river_routes(&map, &plan, &built));
    if let Some((name, _)) = routes.iter().find(|(_, passes)| !**passes) {
        return Err(format!("CITYMAP directed route failed: {name}"));
    }
    let lengths = live_lengths(&map, &plan, &built);
    if !lengths.has_retained_majority() {
        return Err(
            "CITYMAP requires a majority of named OSM alignment on collectors and locals".into(),
        );
    }
    let blocks = blocks(&plan.graph);
    let frontage = frontage(&plan, &map, &built);
    let rail = rail_crossings(&map, &plan, &built);
    let report = report(
        &plan, &inputs, &map, &blocks, &frontage, lengths, routes, rail,
    );
    let svg = out.join("citymap-schematic.svg");
    let files = [
        (
            out.join("citymap-plan.json"),
            serde_json::to_vec_pretty(&plan).map_err(|err| err.to_string())?,
        ),
        (
            out.join("citymap-stats.json"),
            serde_json::to_vec_pretty(&report).map_err(|err| err.to_string())?,
        ),
        (
            svg.clone(),
            schematic(&plan, &map, &built, &blocks, &frontage).into_bytes(),
        ),
    ];
    std::fs::create_dir_all(&out).map_err(|err| format!("{}: {err}", out.display()))?;
    for (path, bytes) in files {
        std::fs::write(&path, bytes).map_err(|err| format!("{}: {err}", path.display()))?;
    }
    write_png(&svg, &out.join("citymap-schematic.png"))?;
    Ok(())
}

use geom::{vec2, Vec2};
use serde::Serialize;
use simulation::map::{LaneKind, LightPolicy, Map};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::process::Command;

#[derive(Serialize)]
struct Distribution {
    count: usize,
    minimum: Option<f32>,
    p10: Option<f32>,
    median: Option<f32>,
    p90: Option<f32>,
    maximum: Option<f32>,
    under_20: usize,
    under_40: usize,
    under_60: usize,
}

impl Distribution {
    fn of(mut values: Vec<f32>) -> Self {
        values.sort_by(f32::total_cmp);
        let quantile = |fraction: f32| {
            if values.is_empty() {
                return None;
            }
            let rank = fraction * (values.len() - 1) as f32;
            let low = rank.floor() as usize;
            let high = rank.ceil() as usize;
            Some(values[low] + (values[high] - values[low]) * rank.fract())
        };
        Self {
            count: values.len(),
            minimum: quantile(0.0),
            p10: quantile(0.10),
            median: quantile(0.50),
            p90: quantile(0.90),
            maximum: quantile(1.0),
            under_20: values.partition_point(|&n| n < 20.0),
            under_40: values.partition_point(|&n| n < 40.0),
            under_60: values.partition_point(|&n| n < 60.0),
        }
    }
}

#[derive(Serialize)]
struct GeometryStats {
    junction_count: usize,
    all_junction_pairs: Distribution,
    adjacent_driving_junction_pairs: Distribution,
    link_chord: Distribution,
    link_arc: Distribution,
}

fn geometry_stats(graph: &Graph, scale: f32) -> GeometryStats {
    let nodes: Vec<_> = graph
        .junctions
        .values()
        .map(|node| node.position.xy())
        .collect();
    let pairs = nodes
        .iter()
        .enumerate()
        .flat_map(|(index, a)| {
            nodes[index + 1..]
                .iter()
                .map(move |&b| a.distance(b) / scale)
        })
        .collect();
    let chord = |link: &LinkSpec| {
        let [a, b] = link
            .endpoints
            .map(|key| graph.junctions[&key].position.xy());
        a.distance(b) / scale
    };
    GeometryStats {
        junction_count: nodes.len(),
        all_junction_pairs: Distribution::of(pairs),
        adjacent_driving_junction_pairs: Distribution::of(
            graph
                .links
                .values()
                .filter(|link| link.class.driving())
                .map(chord)
                .collect(),
        ),
        link_chord: Distribution::of(graph.links.values().map(chord).collect()),
        link_arc: Distribution::of(
            graph
                .links
                .values()
                .map(|link| network::arc(&link.points) / scale)
                .collect(),
        ),
    }
}

#[derive(Default, Serialize)]
struct AlignmentLengths {
    retained: f32,
    realigned: f32,
    authored: f32,
    retained_named_local_collector: f32,
    total_local_collector: f32,
}

impl AlignmentLengths {
    fn has_retained_majority(&self) -> bool {
        2.0 * self.retained_named_local_collector > self.total_local_collector
    }
}

fn live_lengths(map: &Map, plan: &TownPlan, built: &BuiltNetwork) -> AlignmentLengths {
    let mut lengths = AlignmentLengths::default();
    for (key, link) in &plan.graph.links {
        let road = map.get(built.links[key].road).unwrap();
        let length = road.length();
        let retained = if link.lineage.is_empty() {
            lengths.authored += length;
            0.0
        } else {
            let retained: f32 = road
                .points
                .iter()
                .zip(road.points.iter().skip(1))
                .map(|(&a, &b)| {
                    let length = a.distance(b);
                    let sections = (length / 4.0).ceil().max(1.0) as usize;
                    let within_source = (0..sections)
                        .filter(|&i| {
                            let midpoint =
                                (a + (b - a) * ((i as f32 + 0.5) / sections as f32)).xy();
                            link.samples.windows(2).any(|pair| {
                                network::project_on_segment(midpoint, pair[0], pair[1])
                                    .distance2(midpoint)
                                    <= plan.limits.alignment.powi(2)
                            })
                        })
                        .count();
                    length * within_source as f32 / sections as f32
                })
                .sum();
            lengths.retained += retained;
            lengths.realigned += length - retained;
            retained
        };
        if matches!(link.class, RoadClass::Local | RoadClass::Collector) {
            lengths.total_local_collector += length;
            if link.named {
                lengths.retained_named_local_collector += retained;
            }
        }
    }
    lengths
}

fn river_routes(map: &Map, plan: &TownPlan, built: &BuiltNetwork) -> BTreeMap<&'static str, bool> {
    let mut routes = BTreeMap::new();
    if let [west, east] = plan.crossings.as_slice() {
        for omitted in [west, east] {
            let [north, south] = omitted.approaches_keys.map(|key| built.junctions[&key]);
            let blocked = built.links[&omitted.bridge].road;
            routes.insert(
                omitted.without_bridge_check,
                [LaneKind::Driving, LaneKind::Walking]
                    .into_iter()
                    .all(|kind| {
                        network::can_route(map, north, south, kind, |road| road != blocked)
                            && network::can_route(map, south, north, kind, |road| road != blocked)
                    }),
            );
        }
    }
    routes
}

fn road_bounds(graph: &Graph) -> Option<[Vec2; 2]> {
    let mut points = graph
        .links
        .values()
        .filter(|link| {
            matches!(
                link.class,
                RoadClass::Local | RoadClass::Collector | RoadClass::Avenue
            )
        })
        .flat_map(|link| link.points.iter().copied());
    let first = points.next()?;
    Some(points.fold([first, first], |[lo, hi], point| {
        [lo.min(point), hi.max(point)]
    }))
}

/// Bounded road faces are sketch envelopes. Their area is based on road
/// centerlines and includes the streets, setbacks and existing footprint intent.
fn blocks(graph: &Graph) -> Vec<Vec<Vec2>> {
    let mut approaches = BTreeMap::<_, Vec<(_, _, f32)>>::new();
    for link in graph.links.values().filter(|link| link.frontage()) {
        for (index, key) in link.endpoints.into_iter().enumerate() {
            let points = &link.points;
            let dir = if index == 0 {
                points[1] - points[0]
            } else {
                points[points.len() - 2] - points[points.len() - 1]
            };
            approaches.entry(key).or_default().push((
                link.endpoints[1 - index],
                link.key,
                dir.y.atan2(dir.x),
            ));
        }
    }
    for options in approaches.values_mut() {
        options.sort_by(|a, b| a.2.total_cmp(&b.2));
    }
    let mut walked = BTreeSet::new();
    let mut faces = Vec::new();
    for (&start, options) in &approaches {
        for &(first, _, _) in options {
            let mut a = start;
            let mut b = first;
            let mut ring = Vec::new();
            while walked.insert((a, b)) {
                let around = &approaches[&b];
                let index = around.iter().position(|entry| entry.0 == a).unwrap();
                let link = &graph.links[&around[index].1];
                let take = link.points.len() - 1;
                if link.endpoints[0] == a {
                    ring.extend(link.points.iter().take(take).copied());
                } else {
                    ring.extend(link.points.iter().rev().take(take).copied());
                }
                (a, b) = (b, around[(index + around.len() - 1) % around.len()].0);
            }
            if (a, b) == (start, first) && ring.len() >= 3 && area(&ring) > 1_200.0 {
                faces.push(ring);
            }
        }
    }
    faces
}

fn area(ring: &[Vec2]) -> f64 {
    ring.iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(a, b)| f64::from(a.x) * f64::from(b.y) - f64::from(a.y) * f64::from(b.x))
        .sum::<f64>()
        * 0.5
}

#[derive(Serialize)]
struct Frontage {
    link: SourceKey,
    district: Option<&'static str>,
    infill_candidate: bool,
    left_of_source_direction: bool,
    river_facing: bool,
    polygon: [[f32; 2]; 4],
}

fn contains(ring: &[Vec2], point: Vec2) -> bool {
    ring.iter()
        .zip(ring.iter().cycle().skip(1))
        .fold(false, |inside, (&a, &b)| {
            if (a.y > point.y) == (b.y > point.y) {
                inside
            } else {
                inside ^ (point.x < a.x + (point.y - a.y) * (b.x - a.x) / (b.y - a.y))
            }
        })
}

fn frontage(plan: &TownPlan, map: &Map, built: &BuiltNetwork) -> Vec<Frontage> {
    let road_keys: BTreeMap<_, _> = built
        .links
        .iter()
        .filter(|(key, _)| plan.graph.links[key].frontage())
        .map(|(&key, span)| (span.road, key))
        .collect();
    let mut lots: Vec<_> = map
        .lots()
        .values()
        .filter(|lot| road_keys.contains_key(&lot.parent))
        .collect();
    lots.sort_by_key(|lot| lot.id);
    lots.into_iter()
        .filter_map(|lot| {
            let corners = lot.shape.corners;
            if corners.iter().any(|point| {
                point.x < plan.config.owned_min.x
                    || point.y < plan.config.owned_min.y
                    || point.x > plan.config.owned_max.x
                    || point.y > plan.config.owned_max.y
            }) {
                return None;
            }
            let on_water = plan.river.iter().any(|reach| {
                reach.downstream.windows(2).any(|pair| {
                    corners.iter().any(|&corner| {
                        network::project_on_segment(corner, pair[0], pair[1]).distance(corner)
                            < 50.0
                    })
                })
            });
            let in_park = plan.riverside_park.as_ref().is_some_and(|park| {
                corners
                    .iter()
                    .any(|&point| contains(&park.bank_strip, point))
                    || park
                        .bank_strip
                        .iter()
                        .any(|&point| contains(&corners, point))
            });
            if on_water || in_park {
                return None;
            }
            let district = plan.districts.iter().find(|district| {
                corners
                    .iter()
                    .all(|&point| contains(&district.polygon, point))
            });
            let centre = corners.into_iter().sum::<Vec2>() * 0.25;
            let road = map.get(lot.parent).unwrap();
            let (a, b) =
                road.points
                    .iter()
                    .zip(road.points.iter().skip(1))
                    .min_by(|&(a, b), &(c, d)| {
                        let distance = |a: &geom::Vec3, b: &geom::Vec3| {
                            network::project_on_segment(centre, a.xy(), b.xy()).distance2(centre)
                        };
                        distance(a, b).total_cmp(&distance(c, d))
                    })?;
            let left = (b.xy() - a.xy()).perp_dot(centre - a.xy()) > 0.0;
            let river_facing = district.is_some_and(|district| district.key == "south")
                && plan.river.iter().any(|reach| {
                    reach.downstream.windows(2).any(|pair| {
                        let near = network::project_on_segment(centre, pair[0], pair[1]);
                        let offset = centre - near;
                        (94.0..=180.0).contains(&offset.mag())
                            && (pair[1] - pair[0]).perp_dot(offset) > 0.0
                    })
                });
            Some(Frontage {
                link: road_keys[&lot.parent],
                district: district.map(|district| district.key),
                infill_candidate: district.is_some_and(|district| {
                    district.reservations.iter().any(|reservation| {
                        reservation.role == "infill"
                            && centre.distance(reservation.position) <= 160.0
                    })
                }),
                left_of_source_direction: left,
                river_facing,
                polygon: corners.map(source::xy),
            })
        })
        .collect()
}

fn block_depth(ring: &[Vec2]) -> f32 {
    ring.iter()
        .zip(ring.iter().cycle().skip(1))
        .filter_map(|(&a, &b)| (b - a).try_normalize())
        .map(|dir| {
            let (min, max) = ring
                .iter()
                .map(|point| point.dot(dir.perpendicular()))
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(min, max), value| {
                    (min.min(value), max.max(value))
                });
            max - min
        })
        .fold(f32::INFINITY, f32::min)
}

fn district_stats(
    plan: &TownPlan,
    blocks: &[Vec<Vec2>],
    frontage: &[Frontage],
) -> BTreeMap<&'static str, serde_json::Value> {
    plan.districts
        .iter()
        .map(|district| {
            let interior = |point| contains(&district.polygon, point);
            let lots = frontage
                .iter()
                .filter(|lot| lot.district == Some(district.key))
                .collect::<Vec<_>>();
            let counts = [true, false].map(|left| {
                lots.iter()
                    .filter(|lot| lot.left_of_source_direction == left)
                    .count()
            });
            let blocks = blocks
                .iter()
                .filter(|ring| interior(ring.iter().copied().sum::<Vec2>() / ring.len() as f32))
                .map(|ring| {
                    let lots_in_block = lots
                        .iter()
                        .filter(|lot| {
                            let centre =
                                lot.polygon.iter().map(|[x, y]| vec2(*x, *y)).sum::<Vec2>() * 0.25;
                            contains(ring, centre)
                        })
                        .count();
                    serde_json::json!({
                        "area_m2": area(ring),
                        "minimum_obb_width_m": block_depth(ring),
                        "fitting_uniform_lots": lots_in_block,
                    })
                })
                .collect::<Vec<_>>();
            let mut exits = BTreeSet::new();
            let mut exit_pairs = Vec::new();
            for link in plan
                .graph
                .links
                .values()
                .filter(|link| link.class.driving() && link.class <= RoadClass::Avenue)
            {
                let [a, b] = link
                    .endpoints
                    .map(|key| interior(plan.graph.junctions[&key].position.xy()));
                if a != b {
                    let outer = link.endpoints[usize::from(a)];
                    let inner = link.endpoints[usize::from(b)];
                    exits.insert(outer);
                    exit_pairs.push((inner, outer));
                }
            }
            (
                district.key,
                serde_json::json!({
                    "bounded_blocks": blocks,
                    "left_right_uniform_lot_counts": counts,
                    "river_facing_lots": lots.iter().filter(|lot| lot.river_facing).count(),
                    "distinct_external_road_junctions": exits,
                    "two_distributed_exits": exit_pairs.iter().enumerate().any(|(i, (a, b))| {
                        exit_pairs[i + 1..].iter().any(|(c, d)| a != c && b != d)
                    }),
                }),
            )
        })
        .collect()
}

fn rail_crossings(map: &Map, plan: &TownPlan, built: &BuiltNetwork) -> Vec<serde_json::Value> {
    let live = |key| map.get(built.links[&key].road).unwrap();
    let mut crossings = Vec::new();
    for rail in plan
        .graph
        .links
        .values()
        .filter(|link| link.class == RoadClass::Rail)
    {
        let track = live(rail.key);
        for road in plan
            .graph
            .links
            .values()
            .filter(|link| link.class.driving())
        {
            let street = live(road.key);
            for (a, b) in track.points.iter().zip(track.points.iter().skip(1)) {
                for (c, d) in street.points.iter().zip(street.points.iter().skip(1)) {
                    let Some((t, u)) = network::segment_parameters(a.xy(), b.xy(), c.xy(), d.xy())
                    else {
                        continue;
                    };
                    let rail_z = a.z + t * (b.z - a.z);
                    let road_z = c.z + u * (d.z - c.z);
                    crossings.push(serde_json::json!({
                        "track": rail.key,
                        "street": road.key,
                        "position_m": source::xy((a + (b - a) * t).xy()),
                        "rail_running_surface_m": rail_z,
                        "road_running_surface_m": road_z,
                        "vertical_separation_m": (rail_z - road_z).abs(),
                    }));
                }
            }
        }
    }
    crossings
}

fn report(
    plan: &TownPlan,
    inputs: &PinnedInputs,
    map: &Map,
    blocks: &[Vec<Vec2>],
    frontage: &[Frontage],
    lengths: AlignmentLengths,
    routes: BTreeMap<&'static str, bool>,
    rail: Vec<serde_json::Value>,
) -> serde_json::Value {
    let lane_stats = |kind| {
        Distribution::of(
            map.lanes()
                .values()
                .filter(|lane| lane.kind == kind)
                .map(|lane| lane.points.length())
                .collect(),
        )
    };
    let mut classes = BTreeMap::<_, usize>::new();
    for link in plan.graph.links.values() {
        *classes.entry(link.class).or_default() += 1;
    }
    let extent = plan.config.owned_max - plan.config.owned_min;
    let filled = blocks.iter().map(|ring| area(ring)).sum::<f64>();
    serde_json::json!({
        "attribution": ATTRIBUTION,
        "provenance": { "lesce": inputs.lesce.provenance, "sava": inputs.sava.as_ref().map(|extract| &extract.provenance) },
        "transform": { "uniform_scale": TownConfig::SCALE, "frame": plan.config },
        "limits_m": plan.limits,
        "source_crop_projected_osm_metres": geometry_stats(&plan.source_graph, TownConfig::SCALE),
        "before_cleaning_scaled_source": geometry_stats(&plan.source_graph, 1.0),
        "after_cleaning_live_endpoints_and_fitted_centrelines": geometry_stats(&plan.graph, 1.0),
        "existing_lesce_osm_cm0_reference": {
            "adjacent_driving_junction_median_m": 57,
            "adjacent_junction_pairs_under_40_m": 315,
            "driving_lanes": 1792,
            "driving_lanes_under_20_m": 703
        },
        "live_road_arc": Distribution::of(map.roads().values().map(|road| road.length()).collect()),
        "live_driving_lanes": lane_stats(LaneKind::Driving),
        "live_rail_lanes": lane_stats(LaneKind::Rail),
        "live_road_rail_crossings": rail,
        "length_by_alignment_m": lengths,
        "link_counts": classes,
        "directed_turn_connectivity": routes,
        "source_movement_audit": {
            "scope": "Separate OSM API bbox snapshot for the Lesce core, inspected 2026-11-24",
            "source": "https://api.openstreetmap.org/api/0.6/map?bbox=14.145,46.348,14.171,46.370",
            "sha256": "ec7ba0154587caf5b89656ea2930645761c878eb7423769d17acf4c906bf781a",
            "relations_read": 18,
            "kept_as_live_turn_checks": [2465892, 18225139],
            "superseded_or_outside_live_approaches": [1014411, 5516533, 5516534, 5516535, 5516536, 6650471, 8451567, 11569290, 11569298, 11569299, 11569300, 17691026, 17810892, 18140559, 18140560, 18140561]
        },
        "signals": plan.graph.junctions.values().filter(|node| node.light == LightPolicy::Lights).map(|node| node.key).collect::<Vec<_>>(),
        "town_road_bounds_m": road_bounds(&plan.graph).map(|bounds| bounds.map(source::xy)),
        "bounded_road_face_count": blocks.len(),
        "bounded_road_face_share": filled / (f64::from(extent.x) * f64::from(extent.y)),
        "sava_crossings": plan.crossings,
        "authored_sava": plan.river,
        "source_sava": plan.source_river,
        "mapped_sava_bridges": plan.mapped_sava_bridges,
        "outer_corridor_traces": plan.outer_corridor_traces,
        "riverside_park": plan.riverside_park,
        "district_block_depth_and_frontage": district_stats(plan, blocks, frontage),
        "districts": plan.districts,
        "dry_uniform_frontage_candidates": frontage,
        "district_lot_counts": plan.districts.iter().map(|district| (district.key, frontage.iter().filter(|lot| lot.district == Some(district.key)).count())).collect::<BTreeMap<_,_>>(),
        "decisions": plan.graph.decisions,
        "scope": "Dry geometry preview on an empty stock 16-chunk Flat validation world. Crossing and facility markers are reservations."
    })
}

fn schematic(
    plan: &TownPlan,
    map: &Map,
    built: &BuiltNetwork,
    blocks: &[Vec<Vec2>],
    frontage: &[Frontage],
) -> String {
    let mut svg = String::new();
    let lo = plan.config.owned_min;
    let hi = plan.config.owned_max;
    let local = |point: Vec2| vec2(point.x - lo.x, hi.y - point.y);
    let points = |line: &[Vec2]| {
        line.iter()
            .map(|&point| {
                let p = local(point);
                format!("{:.1},{:.1}", p.x, p.y)
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let width = hi.x - lo.x;
    let height = hi.y - lo.y;
    writeln!(svg, r##"<svg xmlns="http://www.w3.org/2000/svg" width="1600" height="{canvas_px_h:.0}" viewBox="0 0 {canvas_w} {canvas_h}" font-family="sans-serif">
<rect width="100%" height="100%" fill="#faf8f2"/>
<text x="84" y="120" font-size="78" font-weight="bold" fill="#212b2a">Lesce Town</text>
<text x="88" y="185" font-size="37" fill="#475652">CM1 · Dry street network · 2× projected source scale</text>
<defs><clipPath id="frame"><rect width="{width}" height="{height}"/></clipPath></defs>
<g transform="translate(84 245)" clip-path="url(#frame)" stroke-linejoin="round" stroke-linecap="round">
<rect width="{width}" height="{height}" fill="#f2eee3"/>"##, canvas_w=width+1050.0, canvas_h=height+410.0, canvas_px_h=1600.0 * (height+410.0) / (width+1050.0)).unwrap();
    for feature in &plan.features {
        let fill = if feature.hole {
            "#f2eee3"
        } else if feature.role.starts_with("building=") {
            "#d7cdb8"
        } else {
            "#e8e3cd"
        };
        writeln!(
            svg,
            r#"<polygon points="{}" fill="{fill}" opacity=".70"/>"#,
            points(&feature.geometry)
        )
        .unwrap();
    }
    for block in blocks {
        writeln!(
            svg,
            r##"<polygon points="{}" fill="#e4ce92" opacity=".30"/>"##,
            points(block)
        )
        .unwrap();
    }
    for (district, colour) in plan
        .districts
        .iter()
        .zip(["#bc674b", "#6c945e", "#a98048", "#50898a", "#7b75a1"])
    {
        writeln!(svg,r#"<polygon points="{}" fill="{colour}" fill-opacity=".13" stroke="{colour}" stroke-width="5" stroke-dasharray="15 13"/>"#,points(&district.polygon)).unwrap();
    }
    for lot in frontage {
        let polygon = lot.polygon.map(|[x, y]| vec2(x, y));
        let fill = if lot.river_facing {
            "#708c52"
        } else if lot.infill_candidate {
            "#c3a14d"
        } else {
            "#9eaa79"
        };
        writeln!(
            svg,
            r#"<polygon points="{}" fill="{fill}" opacity=".55"/>"#,
            points(&polygon)
        )
        .unwrap();
    }
    if let Some(park) = &plan.riverside_park {
        writeln!(svg, r##"<polygon points="{}" fill="#79a75d" fill-opacity=".48" stroke="#5c8745" stroke-width="4"/>"##,
            points(&park.bank_strip)).unwrap();
    }
    for reach in &plan.source_river {
        writeln!(svg, r##"<polyline points="{}" fill="none" stroke="#6495a3" stroke-width="8" stroke-dasharray="20 15"/>"##,
            points(&reach.downstream)).unwrap();
    }
    for reach in &plan.river {
        writeln!(svg,r##"<polyline points="{}" fill="none" stroke="#78b3c8" stroke-opacity=".55" stroke-width="86"/>
<polyline points="{}" fill="none" stroke="#3380a2" stroke-width="9"/>"##,points(&reach.downstream),points(&reach.downstream)).unwrap();
    }
    for link in plan.source_graph.links.values() {
        writeln!(svg,r##"<polyline points="{}" fill="none" stroke="#a8a99d" stroke-width="4" stroke-dasharray="13 9"/>"##,points(&link.samples)).unwrap();
    }
    for class in [
        RoadClass::Walk,
        RoadClass::Local,
        RoadClass::Collector,
        RoadClass::Avenue,
        RoadClass::Ramp,
        RoadClass::Motorway,
        RoadClass::Rail,
    ] {
        for (&key, link) in plan
            .graph
            .links
            .iter()
            .filter(|(_, link)| link.class == class)
        {
            let road = map.get(built.links[&key].road).unwrap();
            let line: Vec<_> = road.points.iter().map(|point| point.xy()).collect();
            let (colour, width, _) = styling(link.class);
            writeln!(
                svg,
                r#"<polyline points="{}" fill="none" stroke="{colour}" stroke-width="{width}"/>"#,
                points(&line)
            )
            .unwrap();
        }
    }
    for junction in plan
        .graph
        .junctions
        .values()
        .filter(|node| node.light == LightPolicy::Lights)
    {
        let p = local(junction.position.xy());
        writeln!(svg,r##"<circle cx="{:.1}" cy="{:.1}" r="13" fill="#e8ba59" stroke="#57392e" stroke-width="5"/>"##,p.x,p.y).unwrap();
    }
    for node in plan
        .source_graph
        .junctions
        .values()
        .filter(|node| matches!(node.key, SourceKey::Cut { .. }))
    {
        let p = local(node.position.xy())
            .max(vec2(10., 10.))
            .min(vec2(width - 10., height - 10.));
        writeln!(svg,r##"<circle cx="{:.1}" cy="{:.1}" r="9" fill="#faf8f2" stroke="#77796f" stroke-width="4"/>"##,p.x,p.y).unwrap();
    }
    for node in
        plan.graph.junctions.values().filter(|node| {
            matches!(node.key, SourceKey::Cut { .. })
                && plan.graph.links.values().any(|link| {
                    link.endpoints.contains(&node.key) && link.class >= RoadClass::Motorway
                })
        })
    {
        let p = local(node.position.xy());
        writeln!(svg,r##"<rect x="{:.1}" y="{:.1}" width="22" height="22" fill="#1c3335" stroke="#faf8f2" stroke-width="3"/>"##,p.x-11.,p.y-11.).unwrap();
    }
    for crossing in &plan.crossings {
        let p = local(crossing.point);
        writeln!(svg,r##"<polyline points="{}" fill="none" stroke="#f8f4dd" stroke-width="22"/>
<polyline points="{}" fill="none" stroke="#205a79" stroke-width="8" stroke-dasharray="15 10"/>
<circle cx="{:.1}" cy="{:.1}" r="12" fill="#205a79"/><text x="{:.1}" y="{:.1}" font-size="28" fill="#183f58" paint-order="stroke" stroke="#f5f4e8" stroke-width="4">{}</text>"##,points(&crossing.approaches),points(&crossing.approaches),p.x,p.y,p.x+22.0,p.y-16.0,crossing.key).unwrap();
    }
    let station = local(plan.station);
    writeln!(svg,r##"<circle cx="{:.1}" cy="{:.1}" r="17" fill="#493d63" stroke="#f7f4e9" stroke-width="5"/>
<text x="{:.1}" y="{:.1}" font-size="31" fill="#3d3352" paint-order="stroke" stroke="#f5f4e8" stroke-width="7">Lesce-Bled station</text>"##,station.x,station.y,station.x-270.0,station.y+45.0).unwrap();
    for (index, district) in plan.districts.iter().enumerate() {
        let label =
            local(district.polygon.iter().copied().sum::<Vec2>() / district.polygon.len() as f32);
        writeln!(svg,r##"<circle cx="{:.1}" cy="{:.1}" r="25" fill="#44433a" stroke="#faf8ee" stroke-width="4"/>
<text x="{:.1}" y="{:.1}" font-size="31" text-anchor="middle" fill="#fffdf6">{}</text>"##,label.x,label.y,label.x,label.y+11.0,index+1).unwrap();
        for reservation in &district.reservations {
            let p = local(reservation.position);
            writeln!(svg,r##"<circle cx="{:.1}" cy="{:.1}" r="8" fill="#6e634b"/><text x="{:.1}" y="{:.1}" font-size="22" fill="#574e3d" paint-order="stroke" stroke="#f7f4e9" stroke-width="5">{}</text>"##,p.x,p.y,p.x+13.0,p.y-11.0,reservation.role).unwrap();
        }
    }
    let lx = width + 145.0;
    writeln!(svg,r##"</g><rect x="84" y="245" width="{width}" height="{height}" fill="none" stroke="#a8a99b" stroke-width="4"/>
<g transform="translate({lx} 285)" fill="#293432">
<text y="0" font-weight="bold" font-size="44">Road hierarchy</text>"##).unwrap();
    for (index, class) in [
        RoadClass::Motorway,
        RoadClass::Ramp,
        RoadClass::Avenue,
        RoadClass::Collector,
        RoadClass::Local,
        RoadClass::Walk,
        RoadClass::Rail,
    ]
    .into_iter()
    .enumerate()
    {
        let (colour, line, label) = styling(class);
        let y = 77 + index * 65;
        writeln!(svg,r#"<path d="M0 {y}h100" stroke="{colour}" stroke-width="{line}"/><text x="134" y="{}" font-size="36">{label}</text>"#,y+12).unwrap();
    }
    svg.push_str(r##"<path d="M0 550h100" stroke="#a8a99d" stroke-width="6" stroke-dasharray="13 9"/><text x="134" y="562" font-size="36">OSM alignment</text>
<circle cx="50" cy="615" r="13" fill="#e8ba59" stroke="#57392e" stroke-width="5"/><text x="134" y="627" font-size="36">Signal</text>
<path d="M0 680h100" stroke="#3380a2" stroke-width="12"/><text x="134" y="692" font-size="36">Authored Sava</text>
<path d="M0 745h100" stroke="#205a79" stroke-width="8" stroke-dasharray="15 10"/><text x="134" y="757" font-size="36">Crossing reservation</text>
<circle cx="50" cy="810" r="9" fill="#faf8f2" stroke="#77796f" stroke-width="4"/><text x="134" y="822" font-size="36">Source crop</text>
<rect x="39" y="864" width="22" height="22" fill="#1c3335"/><text x="134" y="887" font-size="36">Rim / rail gateway</text>
<rect x="29" y="931" width="42" height="25" fill="#a6ac85"/><text x="134" y="957" font-size="36">Dry frontage candidate</text>
<text y="1035" font-weight="bold" font-size="44">Districts</text>
"##);
    for (index, district) in plan.districts.iter().enumerate() {
        writeln!(
            svg,
            r#"<text y="{}" font-size="36">{} · {}</text>"#,
            1112 + index * 65,
            index + 1,
            district.name
        )
        .unwrap();
    }
    svg.push_str(r##"<path d="M0 1490h100" stroke="#6495a3" stroke-width="8" stroke-dasharray="20 15"/><text x="134" y="1502" font-size="33">Mapped Sava</text>
<rect x="29" y="1530" width="42" height="25" fill="#79a75d" opacity=".65"/><text x="134" y="1554" font-size="33">Riverside park strip</text>
<rect x="29" y="1590" width="42" height="25" fill="#708c52"/><text x="134" y="1614" font-size="33">River-facing lots</text>
<path d="M30 1830v-105m-23 28 23-33 23 33" fill="none" stroke="#293432" stroke-width="7"/>
<text x="16" y="1700" font-size="40">N</text>
<path d="M0 1980h500m-500 13v-26m250 26v-26m250 26v-26" fill="none" stroke="#293432" stroke-width="6"/>
<text y="2040" font-size="33">0</text><text x="226" y="2040" font-size="33">250</text><text x="463" y="2040" font-size="33">500 m</text>
</g>"##);
    writeln!(svg,r##"<text x="84" y="{}" font-size="29" fill="#4d5650">{ATTRIBUTION} · Draft schematic</text></svg>"##,height+335.0).unwrap();
    svg
}

fn styling(class: RoadClass) -> (&'static str, u8, &'static str) {
    match class {
        RoadClass::Motorway => ("#263a3f", 24, "A2 rim"),
        RoadClass::Ramp => ("#6e5542", 16, "A2 access ramp"),
        RoadClass::Avenue => ("#ad4d35", 19, "Avenue"),
        RoadClass::Collector => ("#b47b2d", 14, "Collector"),
        RoadClass::Local => ("#586b65", 10, "Local"),
        RoadClass::Walk => ("#698954", 7, "Walking"),
        RoadClass::Rail => ("#665482", 13, "Mainline rail"),
    }
}

fn write_png(svg: &Path, png: &Path) -> Result<(), String> {
    let result = Command::new("rsvg-convert")
        .arg(svg)
        .arg("--output")
        .arg(png)
        .status();
    match result {
        Ok(status) if status.success() => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => Err(format!(
            "could not render PNG using rsvg-convert: {result:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulation::map::{MapProject, ProjectKind};
    use simulation::world_command::{CommandOutcomes, WorldCommand};
    use simulation::{SlotId, SnapshotRevision};

    fn fixture_graph() -> Graph {
        let mut graph = Graph::default();
        let centre = SourceKey::Authored("fixture-centre");
        let points = [
            (centre, vec2(500.0, 500.0)),
            (SourceKey::Authored("fixture-west"), vec2(300.0, 490.0)),
            (SourceKey::Authored("fixture-east"), vec2(685.0, 535.0)),
            (SourceKey::Authored("fixture-north"), vec2(470.0, 685.0)),
        ];
        for (key, point) in points {
            graph.junctions.insert(key, JunctionSpec::new(key, point));
        }
        for (key, point) in points.into_iter().skip(1) {
            graph.links.insert(
                key,
                LinkSpec {
                    key,
                    endpoints: [centre, key],
                    class: RoadClass::Local,
                    layer: 0,
                    one_way: false,
                    roundabout: false,
                    named: false,
                    lineage: BTreeSet::new(),
                    samples: Vec::new(),
                    points: vec![points[0].1, point],
                    elbow: None,
                    realigned: false,
                    inherited: Vec::new(),
                },
            );
        }
        graph
    }

    fn reload_fixture(sim: &Simulation) -> Simulation {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("egr-cm1-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let id = SlotId::new("fixture").unwrap();
        let loaded = (|| {
            sim.save_to_slot(&path, &id, SnapshotRevision::new(1))?;
            Simulation::load_from_slot(&path, &id)
        })();
        std::fs::remove_dir_all(&path).unwrap();
        loaded.unwrap()
    }

    fn pinned_inputs() -> PinnedInputs {
        read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap()
    }

    #[test]
    fn citymap_reframes_owned_area_in_banded_terrain() {
        let inputs = pinned_inputs();
        let mut baseline_cut = None;
        for offset in [vec2(0.0, 0.0), vec2(-100.0, -100.0), vec2(-256.0, -128.0)] {
            let min = vec2(512.0, 512.0) + offset;
            let config = TownConfig {
                map_chunks: 8,
                owned_min: min,
                owned_max: min + vec2(3072.0, 3072.0),
                translation: TownConfig::default().translation - offset,
                ..TownConfig::default()
            };
            let plan = plan_town(&inputs, config, SpacingLimits::default())
                .unwrap_or_else(|err| panic!("{offset:?} planning: {err}"));
            let east_cut = plan.graph.links[&SourceKey::Authored("hraska-east")].endpoints[1];
            if offset == Vec2::ZERO {
                baseline_cut = Some(east_cut);
            }
            if offset.x == -256.0 {
                assert_ne!(baseline_cut, Some(east_cut), "the geographic cut must move");
            }
            let terrain_edge = |point: Vec2| {
                let cap = f32::from(config.map_chunks) * 512.0;
                [point.x, point.y, cap - point.x, cap - point.y]
                    .iter()
                    .any(|value| value.abs() < 0.1)
            };
            for class in [RoadClass::Motorway, RoadClass::Rail] {
                let edge_points = plan
                    .outer_corridor_traces
                    .iter()
                    .filter(|trace| trace.class == class)
                    .flat_map(|trace| [trace.points[0], *trace.points.last().unwrap()])
                    .filter(|&point| terrain_edge(point))
                    .count();
                assert!(
                    edge_points >= 2,
                    "{offset:?}: incomplete outer {class:?} trace"
                );
            }
            let sava = &plan.river[0].downstream;
            assert!(terrain_edge(sava[0]) && terrain_edge(*sava.last().unwrap()));
            let mut sim = preview_world(config).unwrap();
            let built = build_network(&mut sim, &plan.graph, plan.limits)
                .unwrap_or_else(|err| panic!("{offset:?} live build: {err}"));
            let routes = route_checks(&sim.map(), &plan.graph, &built);
            assert!(
                routes.values().all(|pass| *pass),
                "{offset:?} routes: {routes:?}"
            );
        }
    }

    #[test]
    fn citymap_reframes_to_seven_compact_chunks() {
        let config = TownConfig {
            map_chunks: 7,
            owned_max: vec2(3584.0, 3584.0),
            ..TownConfig::default()
        };
        let plan = plan_town(&pinned_inputs(), config, SpacingLimits::default()).unwrap();
        let mut sim = preview_world(config).unwrap();
        let built = build_network(&mut sim, &plan.graph, plan.limits).unwrap();
        assert!(route_checks(&sim.map(), &plan.graph, &built)
            .values()
            .all(|pass| *pass));
    }

    #[test]
    fn citymap_neighbourhood_density_and_frontage() {
        let plan = plan_town(
            &pinned_inputs(),
            TownConfig::default(),
            SpacingLimits::default(),
        )
        .unwrap();
        let mut sim = preview_world(plan.config).unwrap();
        let built = build_network(&mut sim, &plan.graph, plan.limits).unwrap();
        let map = sim.map();
        assert!(plan
            .graph
            .junctions
            .values()
            .any(|node| node.light == LightPolicy::Lights));
        let stats = geometry_stats(&plan.graph, 1.0);
        let median = stats.adjacent_driving_junction_pairs.median.unwrap();
        assert!((110.0..=140.5).contains(&median), "median {median:.1} m");
        let lots = frontage(&plan, &map, &built);
        let faces = blocks(&plan.graph);
        let routes = route_checks(&map, &plan.graph, &built);
        assert!(routes.values().all(|pass| *pass), "{routes:?}");
        let lengths = live_lengths(&map, &plan, &built);
        assert!(
            lengths.has_retained_majority(),
            "named alignment {}/{}",
            lengths.retained_named_local_collector,
            lengths.total_local_collector
        );
        let summaries = district_stats(&plan, &faces, &lots);
        assert!(summaries["south"]["river_facing_lots"]
            .as_u64()
            .is_some_and(|n| n >= 2));
        for (name, summary) in &summaries {
            let fronted_blocks = summary["bounded_blocks"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|block| {
                    block["fitting_uniform_lots"]
                        .as_u64()
                        .is_some_and(|count| count >= 2)
                })
                .count();
            assert!(
                fronted_blocks >= 2,
                "{name} needs two fronted blocks: {summary}"
            );
            let sides = summary["left_right_uniform_lot_counts"].as_array().unwrap();
            assert!(
                sides.iter().all(|side| side.as_u64().unwrap() >= 2),
                "{name}: {summary}"
            );
            assert_eq!(
                summary["two_distributed_exits"], true,
                "{name} lacks two exits: {summary}"
            );
        }
        for district in ["south", "west"] {
            let area = &plan
                .districts
                .iter()
                .find(|d| d.key == district)
                .unwrap()
                .polygon;
            let faces = faces
                .iter()
                .filter(|ring| {
                    contains(area, ring.iter().copied().sum::<Vec2>() / ring.len() as f32)
                })
                .count();
            assert!(faces >= 2, "{district}: {faces} block faces");
            let lots = lots
                .iter()
                .filter(|lot| lot.district == Some(district))
                .count();
            assert!(lots >= 12, "{district}: {lots} lots");
        }
    }

    #[test]
    fn citymap_river_crossings_have_live_approaches() {
        let plan = plan_town(
            &pinned_inputs(),
            TownConfig::default(),
            SpacingLimits::default(),
        )
        .unwrap();
        assert_eq!(plan.crossings.len(), 2);
        let mut sim = preview_world(plan.config).unwrap();
        let built = build_network(&mut sim, &plan.graph, plan.limits).unwrap();
        let map = sim.map();
        let routes = river_routes(&map, &plan, &built);
        assert_eq!(routes.len(), 2);
        assert!(routes.values().all(|&pass| pass), "{routes:?}");
        assert_eq!(
            plan.crossings
                .iter()
                .map(|bridge| bridge.class)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([RoadClass::Collector, RoadClass::Avenue])
        );
        assert!(plan
            .riverside_park
            .as_ref()
            .is_some_and(|park| park.bank_strip.len() >= 12));
        for crossing in &plan.crossings {
            let bridge = map.get(built.links[&crossing.bridge].road).unwrap();
            assert!(bridge.has_sidewalks());
            assert!(bridge
                .lanes_iter()
                .filter(|&(_, kind)| kind == LaneKind::Driving)
                .all(|(id, _)| map.get(id).unwrap().points.length() >= plan.limits.arterial_lane));
            for end in crossing.approaches {
                assert!(
                    plan.graph
                        .junctions
                        .values()
                        .any(|node| node.position.xy().distance(end) < 0.5),
                    "{} has no road approach at {end:?}",
                    crossing.key
                );
            }
        }
        drop(map);
        let loaded = reload_fixture(&sim);
        let map = loaded.map();
        let rebuilt = resolve_network(&map, &plan.graph).unwrap();
        validate_live(&map, &plan.graph, &rebuilt, plan.limits).unwrap();
        assert!(river_routes(&map, &plan, &rebuilt)
            .values()
            .all(|&pass| pass));
        assert!(route_checks(&map, &plan.graph, &rebuilt)
            .values()
            .all(|&pass| pass));
    }

    #[test]
    fn citymap_resolver_rejects_wrong_elbow() {
        let mut graph = fixture_graph();
        let mut sim = preview_world(TownConfig::default()).unwrap();
        let limits = SpacingLimits::default();
        let built = build_network(&mut sim, &graph, limits).unwrap();
        let link = graph.links.values_mut().next().unwrap();
        let [a, b] = link
            .endpoints
            .map(|key| graph.junctions[&key].position.xy());
        link.elbow = Some((a + b) * 0.5 + vec2(0.0, 90.0));
        let map = sim.map();
        assert!(resolve_network(&map, &graph)
            .unwrap_err()
            .contains("centreline"));
        assert!(validate_live(&map, &graph, &built, limits)
            .unwrap_err()
            .contains("centreline"));
    }

    fn edit_connection(
        sim: &mut Simulation,
        from: MapProject,
        to: Vec2,
        elbow: Option<Vec2>,
        class: RoadClass,
    ) {
        sim.apply_commands([&WorldCommand::MapMakeConnection {
            from,
            to: MapProject::ground(to.z(10.0)),
            inter: elbow,
            pat: class.pattern(false, 0.0),
        }]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    }

    #[test]
    fn citymap_validation_rejects_shared_approach_overlap_after_edit() {
        let graph = fixture_graph();
        let mut sim = preview_world(TownConfig::default()).unwrap();
        let limits = SpacingLimits::default();
        let built = build_network(&mut sim, &graph, limits).unwrap();
        let centre = built.junctions[&SourceKey::Authored("fixture-centre")];
        edit_connection(
            &mut sim,
            MapProject {
                kind: ProjectKind::Intersection(centre),
                pos: graph.junctions[&SourceKey::Authored("fixture-centre")].position,
            },
            vec2(690.0, 430.0),
            Some(vec2(590.0, 645.0)),
            RoadClass::Local,
        );
        assert!(validate_live(&sim.map(), &graph, &built, limits)
            .unwrap_err()
            .contains("swept overlap"));
    }

    #[test]
    fn citymap_validation_rejects_unsafe_live_rail_crossing() {
        let graph = fixture_graph();
        let mut sim = preview_world(TownConfig::default()).unwrap();
        let limits = SpacingLimits::default();
        let built = build_network(&mut sim, &graph, limits).unwrap();
        edit_connection(
            &mut sim,
            MapProject::ground(vec2(600.0, 370.0).z(10.0)),
            vec2(600.0, 650.0),
            None,
            RoadClass::Rail,
        );
        assert!(validate_live(&sim.map(), &graph, &built, limits)
            .unwrap_err()
            .contains("rail clearance"));
    }

    #[test]
    fn citymap_validation_rejects_disconnected_neighbourhood_after_edit() {
        let graph = fixture_graph();
        let mut sim = preview_world(TownConfig::default()).unwrap();
        let limits = SpacingLimits::default();
        let built = build_network(&mut sim, &graph, limits).unwrap();
        edit_connection(
            &mut sim,
            MapProject::ground(vec2(825.0, 870.0).z(10.0)),
            vec2(1015.0, 850.0),
            None,
            RoadClass::Local,
        );
        assert!(validate_live(&sim.map(), &graph, &built, limits)
            .unwrap_err()
            .contains("Driving connectivity"));
    }

    #[test]
    fn citymap_live_resolution_survives_an_adjacent_command_and_reload() {
        let graph = fixture_graph();
        let limits = SpacingLimits::default();
        let mut sim = preview_world(TownConfig::default()).unwrap();
        let built = build_network(&mut sim, &graph, limits).unwrap();
        let excessive = SpacingLimits {
            local_lane: 500.0,
            ..limits
        };
        assert!(validate_live(&sim.map(), &graph, &built, excessive)
            .unwrap_err()
            .contains("requires 500.0 m"));
        let west = built.junctions[&SourceKey::Authored("fixture-west")];
        let pos = sim.map().get(west).unwrap().pos;
        sim.apply_commands([&WorldCommand::MapMakeConnection {
            from: simulation::map::MapProject {
                kind: ProjectKind::Intersection(west),
                pos,
            },
            to: simulation::map::MapProject::ground(vec2(235.0, 280.0).z(10.0)),
            inter: None,
            pat: RoadClass::Local.pattern(false, 0.0),
        }]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
        let resolved = resolve_network(&sim.map(), &graph).unwrap();
        validate_live(&sim.map(), &graph, &resolved, limits).unwrap();

        let loaded = reload_fixture(&sim);
        let map = loaded.map();
        let rebuilt = resolve_network(&map, &graph).unwrap();
        assert_eq!(resolved.junctions, rebuilt.junctions);
        validate_live(&map, &graph, &rebuilt, limits).unwrap();

        let mut wrong_direction = graph.clone();
        wrong_direction.links.values_mut().next().unwrap().one_way = true;
        assert!(resolve_network(&map, &wrong_direction)
            .unwrap_err()
            .contains("lane pattern changed"));
    }
}
