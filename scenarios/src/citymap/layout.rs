use super::network::{
    calibrate_storage, clean_network, connect_infill, finish_network, main_components,
    retire_outer_road_fragments, segment_parameters, Graph, JunctionSpec, LinkSpec, RoadClass,
    SourceKey, SpacingLimits,
};
use super::source::{
    serialize_point, serialize_points, Coordinate, Element, Kind, PinnedInputs, TownConfig,
};
use geom::{vec2, Vec2};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
pub struct TownPlan {
    pub config: TownConfig,
    pub limits: SpacingLimits,
    pub source_graph: Graph,
    pub graph: Graph,
    pub features: Vec<Feature>,
    pub districts: Vec<District>,
    pub river: Vec<RiverReach>,
    pub source_river: Vec<RiverReach>,
    pub riverside_park: Option<RiversidePark>,
    pub crossings: Vec<CrossingSpec>,
    pub outer_corridor_traces: Vec<CorridorTrace>,
    pub mapped_sava_bridges: Vec<Feature>,
    #[serde(serialize_with = "serialize_point")]
    pub station: Vec2,
}

#[derive(Debug, Serialize)]
pub struct Feature {
    pub source: u64,
    pub kind: KindLabel,
    pub role: String,
    #[serde(serialize_with = "serialize_points")]
    pub geometry: Vec<Vec2>,
    pub hole: bool,
}

#[derive(Debug, Serialize)]
pub enum KindLabel {
    Way,
    Relation,
}

#[derive(Debug, Serialize)]
pub struct District {
    pub key: &'static str,
    pub name: &'static str,
    #[serde(serialize_with = "serialize_points")]
    pub polygon: Vec<Vec2>,
    pub reservations: Vec<Reservation>,
}

#[derive(Debug, Serialize)]
pub struct Reservation {
    pub role: &'static str,
    #[serde(serialize_with = "serialize_point")]
    pub position: Vec2,
}

#[derive(Debug, Serialize)]
pub struct RiverReach {
    pub way: u64,
    /// OSM way order is downstream.
    #[serde(serialize_with = "serialize_points")]
    pub downstream: Vec<Vec2>,
    pub source_width_scaled: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct RiversidePark {
    #[serde(serialize_with = "serialize_points")]
    pub bank_strip: Vec<Vec2>,
    pub far_bank_access: SourceKey,
}

#[derive(Debug, Serialize)]
pub struct CorridorTrace {
    pub way: u64,
    pub class: RoadClass,
    #[serde(serialize_with = "serialize_points")]
    pub points: Vec<Vec2>,
}

#[derive(Debug, Serialize)]
pub struct CrossingSpec {
    pub key: &'static str,
    #[serde(serialize_with = "serialize_point")]
    pub point: Vec2,
    #[serde(serialize_with = "serialize_points")]
    pub approaches: [Vec2; 2],
    pub river_way: u64,
    pub bridge: SourceKey,
    pub class: RoadClass,
    pub approaches_keys: [SourceKey; 2],
    pub approaches_links: [SourceKey; 2],
    pub deck_width_m: f32,
    pub max_approach_grade: f32,
    pub without_bridge_check: &'static str,
}

pub fn plan_town(
    inputs: &PinnedInputs,
    config: TownConfig,
    limits: SpacingLimits,
) -> Result<TownPlan, String> {
    config.validate()?;
    limits.validate()?;
    let elements = inputs.lesce.elements();
    let source_graph = source_graph(elements, config)?;
    let mut graph = clean_network(source_graph.clone(), limits, config)?;
    let preview = super::preview_world(config)?;
    let map = preview.map();
    let station = elements
        .iter()
        .find(|element| element.kind == Kind::Node && element.id == 5685260194)
        .and_then(|element| element.point(config))
        .ok_or("missing Lesce-Bled station node")?;
    let features = feature_rings(elements, config);
    let districts = districts(config);
    let infill: Vec<_> = ["south", "west", "hraska", "station", "employment"]
        .iter()
        .filter_map(|key| districts.iter().find(|district| district.key == *key))
        .map(|district| district.polygon.as_slice())
        .collect();
    let mut source_river = Vec::new();
    let mut river = Vec::new();
    let mut crossings = Vec::new();
    let mut riverside_park = None;
    let mut mapped_sava_bridges = Vec::new();
    if let Some(sava) = &inputs.sava {
        source_river = real_river(sava.elements(), config);
        mapped_sava_bridges = sava
            .elements()
            .iter()
            .filter(|element| element.kind == Kind::Way && !element.tag("bridge").is_empty())
            .map(|element| Feature {
                source: element.id,
                kind: KindLabel::Way,
                role: format!("bridge={}", element.tag("highway")),
                geometry: element
                    .geometry
                    .iter()
                    .map(|point| point.project(config))
                    .collect(),
                hole: false,
            })
            .collect();
        let reach = authored_river(&source_river, config)?;
        let drowned_walks: Vec<_> = graph
            .links
            .values()
            .filter(|link| {
                (link.class == RoadClass::Walk || link.lineage.contains(&168406420))
                    && !water_intersections(&reach, link).is_empty()
            })
            .map(|link| link.key)
            .collect();
        graph.links.retain(|key, _| !drowned_walks.contains(key));
        graph.remove_isolated();
        graph.record("trails and the outer Šobčeva spur intercepted by the authored Sava reroute through the bridges",
            &drowned_walks);
        let (bridges, park) = water_reservations(&reach, &mut graph, config)?;
        main_components(&mut graph);
        connect_neighbourhoods(&mut graph, config)?;
        retire_outer_road_fragments(&mut graph, &infill)?;
        finish_network(&mut graph, limits)?;
        calibrate_storage(&mut graph, config, limits, map.environment())?;
        main_components(&mut graph);
        connect_infill(
            &mut graph,
            limits,
            &infill,
            &reach.downstream,
            map.environment(),
        );
        prioritize_east_connector(&mut graph);
        finish_network(&mut graph, limits)?;
        validate_river_crossings(&reach, &bridges, &graph)?;
        river.push(reach);
        crossings = bridges;
        riverside_park = Some(park);
    } else {
        retire_outer_road_fragments(&mut graph, &infill)?;
        prioritize_east_connector(&mut graph);
        calibrate_storage(&mut graph, config, limits, map.environment())?;
    }

    Ok(TownPlan {
        config,
        limits,
        source_graph,
        graph,
        features,
        districts,
        river,
        source_river,
        riverside_park,
        crossings,
        outer_corridor_traces: outer_corridor_traces(elements, config),
        mapped_sava_bridges,
        station,
    })
}

fn prioritize_east_connector(graph: &mut Graph) {
    let mut upgraded = Vec::new();
    for link in graph
        .links
        .values_mut()
        .filter(|link| link.class == RoadClass::Local)
    {
        let class = if link.lineage.contains(&10575173) {
            RoadClass::Avenue
        } else if link.lineage.contains(&10521528) {
            RoadClass::Collector
        } else {
            continue;
        };
        link.class = class;
        upgraded.push(link.key);
    }
    if !upgraded.is_empty() {
        graph.record(
            "Dežmanova ulica and lower Hraška cesta connect Alpska to the Begunjska employment-side collector",
            &upgraded,
        );
    }
}

#[derive(Clone)]
struct Sample {
    key: SourceKey,
    point: Vec2,
}

const A2_MAINLINE: &[u64] = &[
    61696631, 344507323, 344507322, 37849482, 64017257, 476387831, 61232337, 475299897, 61233481,
];
const HRASKA_SPLITTERS: &[u64] = &[
    26270881, 26398035, 358555158, 358555279, 1288727380, 1288727382, 1288727383, 1288727384,
];

fn road_class(element: &Element) -> Option<RoadClass> {
    if element.kind != Kind::Way || matches!(element.tag("access"), "private" | "no") {
        return None;
    }
    if element.tag("railway") == "rail" {
        return (element.tag("ref") == "20" && element.tag("service").is_empty())
            .then_some(RoadClass::Rail);
    }
    Some(match element.tag("highway") {
        "motorway" if A2_MAINLINE.contains(&element.id) => RoadClass::Motorway,
        "primary" => RoadClass::Avenue,
        "secondary" | "tertiary" if element.tag("name") == "Alpska cesta" => RoadClass::Avenue,
        "secondary" | "tertiary" | "unclassified" => RoadClass::Collector,
        "residential" | "living_street" => RoadClass::Local,
        "footway" | "cycleway" | "path" | "steps" if element.tag("foot") != "no" => RoadClass::Walk,
        _ => return None,
    })
}

fn direction(element: &Element, class: RoadClass) -> i8 {
    if class == RoadClass::Motorway
        || class == RoadClass::Walk
        || HRASKA_SPLITTERS.contains(&element.id)
    {
        return 0;
    }
    match element.tag("oneway") {
        "yes" | "1" | "true" => 1,
        "-1" => -1,
        _ => 0,
    }
}

fn layer(element: &Element) -> i8 {
    element.tag("layer").parse().unwrap_or_else(|_| {
        if matches!(element.tag("bridge"), "" | "no") {
            -i8::from(!matches!(element.tag("tunnel"), "" | "no"))
        } else {
            1
        }
    })
}

fn inside(point: Vec2, config: TownConfig) -> bool {
    point.x >= config.owned_min.x
        && point.y >= config.owned_min.y
        && point.x <= config.owned_max.x
        && point.y <= config.owned_max.y
}

/// Liang–Barsky interval in source segment order. Corner-only touches are discarded.
fn clip(a: Vec2, b: Vec2, lo: Vec2, hi: Vec2) -> Option<(f32, f32)> {
    let delta = b - a;
    let mut enter = 0.0_f32;
    let mut exit = 1.0_f32;
    for (origin, movement, min, max) in [(a.x, delta.x, lo.x, hi.x), (a.y, delta.y, lo.y, hi.y)] {
        if movement == 0.0 {
            if origin < min || origin > max {
                return None;
            }
            continue;
        }
        let t1 = (min - origin) / movement;
        let t2 = (max - origin) / movement;
        enter = enter.max(t1.min(t2));
        exit = exit.min(t1.max(t2));
    }
    (enter < exit).then_some((enter, exit))
}

fn clipped_way(element: &Element, config: TownConfig, rail: bool) -> Vec<Vec<Sample>> {
    let points: Vec<_> = element
        .geometry
        .iter()
        .map(|point| point.project(config))
        .collect();
    let mut spans = Vec::new();
    let mut span = Vec::new();
    for (index, segment) in points.windows(2).enumerate() {
        let [a, b] = [segment[0], segment[1]];
        if let Some((start, end)) = clip(a, b, config.owned_min, config.owned_max) {
            let sample = |fraction: f32, side: u8, node: usize| Sample {
                key: if fraction == f32::from(side) {
                    SourceKey::Node {
                        id: element.nodes[node],
                        rail,
                    }
                } else {
                    SourceKey::Cut {
                        way: element.id,
                        segment: index as u32,
                        side,
                    }
                },
                point: a + fraction * (b - a),
            };
            if span.is_empty() {
                span.push(sample(start, 0, index));
            }
            span.push(sample(end, 1, index + 1));
            if end == 1.0 {
                continue;
            }
        }
        if !span.is_empty() {
            spans.push(std::mem::take(&mut span));
        }
    }
    if !span.is_empty() {
        spans.push(span);
    }
    spans
}

fn outer_corridor_traces(elements: &[Element], config: TownConfig) -> Vec<CorridorTrace> {
    let max = Vec2::splat(f32::from(config.map_chunks) * 512.0);
    let outer = TownConfig {
        owned_min: Vec2::ZERO,
        owned_max: max,
        translation: config.translation + (config.owned_min + config.owned_max - max) * 0.5,
        ..config
    };
    elements
        .iter()
        .filter_map(|element| {
            let class = road_class(element)?;
            matches!(class, RoadClass::Motorway | RoadClass::Rail).then_some((element, class))
        })
        .flat_map(|(element, class)| {
            clipped_way(element, outer, class == RoadClass::Rail)
                .into_iter()
                .map(move |span| CorridorTrace {
                    way: element.id,
                    class,
                    points: span.into_iter().map(|sample| sample.point).collect(),
                })
        })
        .collect()
}

fn source_graph(elements: &[Element], config: TownConfig) -> Result<Graph, String> {
    let mut graph = Graph::default();
    let mut uses = BTreeMap::<SourceKey, usize>::new();
    let mut clipped = Vec::new();
    for element in elements {
        let Some(class) = road_class(element) else {
            continue;
        };
        for span in clipped_way(element, config, class == RoadClass::Rail) {
            for sample in &span {
                *uses.entry(sample.key).or_default() += 1;
            }
            clipped.push((element, class, span));
        }
    }
    for (element, class, span) in clipped {
        let mut start = 0;
        for end in 1..span.len() {
            if end + 1 != span.len() && uses[&span[end].key] == 1 {
                continue;
            }
            let samples = &span[start..=end];
            start = end;
            let mut endpoints = [samples[0].key, samples.last().unwrap().key];
            let mut points: Vec<_> = samples.iter().map(|sample| sample.point).collect();
            let mut one_way = direction(element, class);
            if one_way < 0 {
                endpoints.reverse();
                points.reverse();
                one_way = 1;
            }
            let key = SourceKey::Way {
                id: element.id,
                span: graph
                    .links
                    .range(
                        SourceKey::Way {
                            id: element.id,
                            span: 0,
                        }..=SourceKey::Way {
                            id: element.id,
                            span: u32::MAX,
                        },
                    )
                    .count() as u32,
            };
            if endpoints[0] == endpoints[1] && element.tag("junction") != "roundabout" {
                graph.record("closed source spur omitted", &[key]);
                continue;
            }
            for (endpoint, point) in endpoints
                .into_iter()
                .zip([points[0], *points.last().unwrap()])
            {
                let junction = graph
                    .junctions
                    .entry(endpoint)
                    .or_insert(JunctionSpec::new(endpoint, point));
                junction.required |= matches!(endpoint, SourceKey::Cut { .. })
                    && matches!(class, RoadClass::Motorway | RoadClass::Rail);
            }
            graph.links.insert(
                key,
                LinkSpec {
                    key,
                    endpoints,
                    class,
                    layer: layer(element),
                    one_way: one_way != 0,
                    roundabout: element.tag("junction") == "roundabout",
                    lineage: BTreeSet::from([element.id]),
                    named: !element.tag("name").is_empty() || !element.tag("ref").is_empty(),
                    samples: points.clone(),
                    points,
                    elbow: None,
                    realigned: false,
                    inherited: Vec::new(),
                },
            );
        }
    }
    for relation in elements
        .iter()
        .filter(|element| element.tag("type") == "restriction")
    {
        let incident = relation.members.iter().any(|member| {
            member.kind == Kind::Way
                && graph
                    .links
                    .values()
                    .any(|link| link.lineage.contains(&member.id))
        });
        if incident {
            return Err(format!(
                "unresolved turn restriction relation/{}",
                relation.id
            ));
        }
    }
    if graph.links.is_empty() {
        return Err("no supported source roads intersect the owned rectangle".into());
    }
    graph.record(
        "A2 source carriageway consolidated to a two-way rim; paired carriageway and unmapped ramp movements require rebuilt access",
        &[SourceKey::Way { id: 61233481, span: 0 }, SourceKey::Way { id: 61233482, span: 0 }],
    );
    graph.record(
        "Hraška splitter approaches become two-way approaches to consolidated junctions",
        &HRASKA_SPLITTERS
            .iter()
            .map(|&id| SourceKey::Way { id, span: 0 })
            .collect::<Vec<_>>(),
    );
    let dropped: Vec<_> = elements
        .iter()
        .filter(|element| {
            matches!(element.tag("highway"), "service" | "track")
                && element
                    .geometry
                    .iter()
                    .any(|point| inside(point.project(config), config))
        })
        .map(|element| SourceKey::Way {
            id: element.id,
            span: 0,
        })
        .collect();
    graph.record(
        "service/track and driveway frontage omitted from the playable network",
        &dropped,
    );
    Ok(graph)
}

fn clip_ring(coordinates: &[Coordinate], config: TownConfig) -> Vec<Vec2> {
    let mut ring: Vec<_> = coordinates
        .iter()
        .map(|point| point.project(config))
        .collect();
    if ring.first() != ring.last() {
        return Vec::new();
    }
    ring.pop();
    for (axis, bound, sign) in [
        (Vec2::X, config.owned_min.x, 1.0),
        (Vec2::X, config.owned_max.x, -1.0),
        (Vec2::Y, config.owned_min.y, 1.0),
        (Vec2::Y, config.owned_max.y, -1.0),
    ] {
        let previous = std::mem::take(&mut ring);
        let Some(mut from) = previous.last().copied() else {
            break;
        };
        for to in previous {
            let a = sign * (axis.dot(from) - bound);
            let b = sign * (axis.dot(to) - bound);
            if (a >= 0.0) != (b >= 0.0) {
                ring.push(from + (to - from) * (a / (a - b)));
            }
            if b >= 0.0 {
                ring.push(to);
            }
            from = to;
        }
    }
    ring.dedup();
    if ring.len() >= 3 {
        ring.push(ring[0]);
        ring
    } else {
        Vec::new()
    }
}

/// Assemble relation members by exact endpoint identity before clipping each ring.
fn relation_rings(element: &Element, role: &str) -> Vec<Vec<Coordinate>> {
    let mut parts: Vec<_> = element
        .members
        .iter()
        .filter(|member| {
            member.kind == Kind::Way && member.role == role && member.geometry.len() >= 2
        })
        .map(|member| member.geometry.clone())
        .collect();
    let equal = |a: Coordinate, b: Coordinate| a.lat == b.lat && a.lon == b.lon;
    let mut rings = Vec::new();
    while let Some(mut ring) = parts.pop() {
        while !equal(ring[0], *ring.last().unwrap()) {
            let end = *ring.last().unwrap();
            let Some(index) = parts
                .iter()
                .position(|part| equal(part[0], end) || equal(*part.last().unwrap(), end))
            else {
                break;
            };
            let mut part = parts.remove(index);
            if !equal(part[0], end) {
                part.reverse();
            }
            ring.extend(part.into_iter().skip(1));
        }
        if equal(ring[0], *ring.last().unwrap()) {
            rings.push(ring);
        }
    }
    rings
}

fn feature_rings(elements: &[Element], config: TownConfig) -> Vec<Feature> {
    let mut features = Vec::new();
    for element in elements {
        let Some(role) = ["building", "landuse", "natural"]
            .into_iter()
            .find(|tag| !element.tag(tag).is_empty())
        else {
            continue;
        };
        let mut add = |kind, geometry: &[Coordinate], hole| {
            let geometry = clip_ring(geometry, config);
            if !geometry.is_empty() {
                features.push(Feature {
                    source: element.id,
                    kind,
                    role: format!("{role}={}", element.tag(role)),
                    geometry,
                    hole,
                });
            }
        };
        match element.kind {
            Kind::Node => {}
            Kind::Way => add(KindLabel::Way, &element.geometry, false),
            Kind::Relation => {
                for role in ["outer", "inner"] {
                    for ring in relation_rings(element, role) {
                        add(KindLabel::Relation, &ring, role == "inner");
                    }
                }
            }
        }
    }
    features
}

fn real_river(elements: &[Element], config: TownConfig) -> Vec<RiverReach> {
    elements
        .iter()
        .filter(|element| element.tag("waterway") == "river")
        .map(|element| RiverReach {
            way: element.id,
            downstream: element
                .geometry
                .iter()
                .map(|point| point.project(config))
                .collect(),
            source_width_scaled: element
                .tag("width")
                .trim_end_matches(" m")
                .parse::<f32>()
                .ok()
                .map(|width| width * TownConfig::SCALE),
        })
        .collect()
}

fn authored_river(source: &[RiverReach], config: TownConfig) -> Result<RiverReach, String> {
    let real = source
        .iter()
        .find(|reach| reach.way == 863152620)
        .ok_or("missing Sava Dolinka trace")?;
    // Use the real local bend to modulate the authored guide. An along-reach
    // coordinate preserves downstream order and the smoothed transverse drift
    // carries the mapped bend's shape into the new alignment.
    let start = design_point(vec2(-428.0, 1187.0), config);
    let end = design_point(vec2(970.0, -521.0), config);
    let nearest = |target: Vec2| {
        real.downstream
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.distance2(target).total_cmp(&b.distance2(target)))
            .map(|(index, _)| index)
            .unwrap()
    };
    let source = &real.downstream[nearest(start)..=nearest(end)];
    let start = source[0];
    let end = *source.last().unwrap();
    let direction = (end - start).normalize();
    let mut offset = Vec::with_capacity(source.len());
    let mut length = 0.0;
    for (i, point) in source.iter().enumerate() {
        if i > 0 {
            length += source[i - 1].distance(*point);
        }
        offset.push((length, (*point - start).dot(direction.perpendicular())));
    }
    let guide = [
        (-1300.0, 1590.0),
        (-550.0, 1440.0),
        (0.0, 1290.0),
        (360.0, 1110.0),
        (650.0, 1000.0),
        (900.0, 860.0),
        (1180.0, 720.0),
        (1410.0, 560.0),
        (1510.0, 400.0),
        (1570.0, 210.0),
        (1600.0, -50.0),
        (1660.0, -1300.0),
    ]
    .map(|(x, y)| vec2(x, y));
    let smooth = |index: usize, t: f32| {
        let a = guide[index];
        let b = guide[index + 1];
        let before = guide[index.saturating_sub(1)];
        let after = guide[(index + 2).min(guide.len() - 1)];
        let dx = b.x - a.x;
        let da = dx * (b.y - before.y) / (b.x - before.x);
        let db = dx * (after.y - a.y) / (after.x - a.x);
        let y = a.y
            + t * (da
                + t * (3.0 * (b.y - a.y) - 2.0 * da - db + t * (2.0 * (a.y - b.y) + da + db)));
        vec2(a.x + t * dx, y)
    };
    let mut points = Vec::new();
    for (i, segment) in guide.windows(2).enumerate() {
        let steps = (segment[0].distance(segment[1]) / 22.0).ceil() as usize;
        for j in 0..steps {
            let point = smooth(i, j as f32 / steps as f32);
            let progress = (point.x - guide[0].x) / (guide.last().unwrap().x - guide[0].x);
            let d = length * progress;
            let next = offset
                .partition_point(|&(along, _)| along <= d)
                .clamp(1, offset.len() - 1);
            let (a, ya) = offset[next - 1];
            let (b, yb) = offset[next];
            let drift = ya + (yb - ya) * ((d - a) / (b - a)).clamp(0.0, 1.0);
            points.push(design_point(point + vec2(0.0, drift * 0.055), config));
        }
    }
    points.push(design_point(*guide.last().unwrap(), config));
    // Retain the entire terrain reach for TILES. The schematic clips it to
    // the owned rectangle, and WATER can use the same line past that frame.
    let max = Vec2::splat(f32::from(config.map_chunks) * 512.0);
    let mut downstream = Vec::new();
    for segment in points.windows(2) {
        let (a, b) = (segment[0], segment[1]);
        if let Some((start, end)) = clip(a, b, Vec2::ZERO, max) {
            if downstream.is_empty() {
                downstream.push(a + start * (b - a));
            }
            downstream.push(a + end * (b - a));
        }
    }
    if downstream.len() < 2 {
        return Err("authored Sava misses the terrain".into());
    }
    Ok(RiverReach {
        way: real.way,
        downstream,
        source_width_scaled: real.source_width_scaled,
    })
}

pub(super) fn extend_gateways(plan: &mut TownPlan) -> Result<(), String> {
    plan.graph.wet = true;
    let extent = f32::from(plan.config.map_chunks) * 512.0;
    for (class, names) in [
        (
            RoadClass::Motorway,
            ["a2-outer-north", "a2-outer-southeast"],
        ),
        (RoadClass::Rail, ["rail-outer-north", "rail-outer-south"]),
    ] {
        let mut cuts: Vec<_> = plan
            .graph
            .links
            .values()
            .filter(|link| link.class == class)
            .flat_map(|link| link.endpoints)
            .filter(|key| matches!(key, SourceKey::Cut { .. }))
            .collect();
        cuts.sort_by(|a, b| {
            plan.graph.junctions[b]
                .position
                .y
                .total_cmp(&plan.graph.junctions[a].position.y)
        });
        let [north, south] = cuts.as_slice() else {
            return Err(format!("expected two {class:?} gateways"));
        };
        for (&cut, name) in [north, south].into_iter().zip(names) {
            let start = plan.graph.junctions[&cut].position.xy();
            let axis = [
                (start.x - plan.config.owned_min.x, -Vec2::X),
                (start.y - plan.config.owned_min.y, -Vec2::Y),
                (plan.config.owned_max.x - start.x, Vec2::X),
                (plan.config.owned_max.y - start.y, Vec2::Y),
            ]
            .into_iter()
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap()
            .1;
            let (way, edge, toward) = plan
                .outer_corridor_traces
                .iter()
                .filter(|trace| trace.class == class)
                .flat_map(|trace| {
                    let points = &trace.points;
                    [
                        (trace.way, points[0], points[1]),
                        (
                            trace.way,
                            points[points.len() - 1],
                            points[points.len() - 2],
                        ),
                    ]
                })
                .filter(|&(_, edge, _)| {
                    let boundary = if axis.x + axis.y > 0.0 { extent } else { 0.0 };
                    (edge.dot(axis).abs() - boundary).abs() < 0.1
                })
                .min_by(|a, b| a.1.distance2(start).total_cmp(&b.1.distance2(start)))
                .ok_or_else(|| format!("no outer OSM alignment for {name}"))?;
            // Match the TILES gateway inset while keeping the terminal sweep on the map.
            let end = edge + (toward - edge) * (8.0 / (edge - toward).dot(axis));
            let key = SourceKey::Authored(name);
            let mut terminal = JunctionSpec::new(key, end);
            terminal.position.z = plan.graph.junctions[&cut].position.z;
            terminal.turn = plan.graph.junctions[&cut].turn;
            plan.graph.junctions.insert(key, terminal);
            add_authored_road(&mut plan.graph, key, [cut, key], class, None, 0);
            plan.graph.links.get_mut(&key).unwrap().lineage.insert(way);
            plan.graph.record(
                "continue the gateway to the terrain edge on the OSM alignment",
                &[cut, key],
            );
        }
    }
    Ok(())
}

pub(super) fn fit_wet_approaches(plan: &mut TownPlan) -> Result<(), String> {
    let graph = &mut plan.graph;
    let [rim, town] = plan.crossings.as_mut_slice() else {
        return Err("wet CITYMAP needs two Sava crossings".into());
    };
    let parkway = SourceKey::Authored("sava-far-bank-parkway");
    let rim_ramp = SourceKey::Authored("sava-rim-park-ramp");
    let town_ramp = SourceKey::Authored("sava-town-park-ramp");
    let [park_west, park_east] = [
        (SourceKey::Authored("sava-park-west"), vec2(953.0, 720.0)),
        (SourceKey::Authored("sava-park-east"), vec2(1160.0, 607.0)),
    ]
    .map(|(key, point)| {
        let junction = JunctionSpec::new(key, design_point(point, plan.config));
        graph.junctions.insert(key, junction);
        key
    });
    for (crossing, ramp, bank) in [(rim, rim_ramp, park_west), (town, town_ramp, park_east)] {
        add_authored_road(
            graph,
            ramp,
            [crossing.approaches_keys[1], bank],
            RoadClass::Collector,
            None,
            0,
        );
        crossing.approaches_links[1] = ramp;
        for link in crossing.approaches_links {
            for key in graph.links[&link].endpoints {
                let height = &mut graph.junctions.get_mut(&key).unwrap().position.z;
                *height = height.max(10.6);
            }
        }
    }
    add_authored_road(
        graph,
        parkway,
        [park_west, park_east],
        RoadClass::Collector,
        Some(design_point(vec2(1056.0, 650.0), plan.config)),
        0,
    );
    graph.record(
        "return the far-bank parkway to ground level between the bridge ramps",
        &[rim_ramp, parkway, town_ramp],
    );
    Ok(())
}

fn water_reservations(
    river: &RiverReach,
    graph: &mut Graph,
    config: TownConfig,
) -> Result<(Vec<CrossingSpec>, RiversidePark), String> {
    let node = |id| SourceKey::Node { id, rail: false };
    let specs = [
        (
            "sava-rim",
            RoadClass::Collector,
            895.0,
            "sava-rim-town",
            "sava-rim-park",
            "sava-rim-approach",
            [node(1796759067), node(1796759070), node(4193639599)],
        ),
        (
            "sava-town",
            RoadClass::Avenue,
            1360.0,
            "sava-town-north",
            "sava-town-park",
            "sava-town-approach",
            [node(1800677047), node(1796759046), node(862116469)],
        ),
    ];
    let mut crossings = Vec::new();
    let bank_link = SourceKey::Authored("sava-far-bank-parkway");
    for (key, class, x, town, park, approach, anchors) in specs {
        let x = design_point(vec2(x, 0.0), config).x;
        let [a, b] = river
            .downstream
            .windows(2)
            .find(|pair| pair[0].x <= x && pair[1].x >= x)
            .map(|pair| [pair[0], pair[1]])
            .ok_or("Sava crossing lies outside the authored reach")?;
        let point = a + (b - a) * ((x - a.x) / (b.x - a.x));
        let normal = -(b - a).normalize().perpendicular();
        let approaches = [point + 104.0 * normal, point - 104.0 * normal];
        if approaches.iter().any(|&point| !inside(point, config)) {
            return Err(format!(
                "Sava reservation {key} has an approach outside the owned area"
            ));
        }
        let [town, park] = [town, park].map(SourceKey::Authored);
        for (key, point) in [town, park].into_iter().zip(approaches) {
            graph.junctions.insert(key, JunctionSpec::new(key, point));
        }
        let anchor = anchors
            .into_iter()
            .find(|key| graph.junctions.contains_key(key))
            .ok_or_else(|| format!("missing {key} neighbourhood connection"))?;
        let bridge = SourceKey::Authored(key);
        add_authored_road(graph, bridge, [town, park], class, None, 1);
        let approach = SourceKey::Authored(approach);
        let bend = (class == RoadClass::Avenue).then(|| design_point(vec2(1370.0, 857.0), config));
        add_authored_road(graph, approach, [anchor, town], class, bend, 0);
        crossings.push(CrossingSpec {
            key,
            point,
            approaches,
            river_way: river.way,
            bridge,
            class,
            approaches_keys: [town, park],
            approaches_links: [approach, bank_link],
            deck_width_m: class.pattern(false, 0.0).width(),
            max_approach_grade: 0.08,
            without_bridge_check: if class == RoadClass::Collector {
                "far_bank_reachable_without_sava_rim"
            } else {
                "far_bank_reachable_without_sava_town"
            },
        });
    }
    let far_bank = [
        crossings[0].approaches_keys[1],
        crossings[1].approaches_keys[1],
    ];
    add_authored_road(
        graph,
        bank_link,
        far_bank,
        RoadClass::Collector,
        Some(design_point(vec2(1000.0, 590.0), config)),
        0,
    );
    let sobceva = node(275779742);
    if graph.junctions.contains_key(&sobceva) {
        let key = SourceKey::Authored("sava-sobceva-access");
        add_authored_road(
            graph,
            key,
            [far_bank[0], sobceva],
            RoadClass::Collector,
            None,
            0,
        );
        graph.record(
            "collector ties the mapped Šobčeva far-bank streets to the western bridge",
            &[key, sobceva],
        );
    }
    graph.record(
        "two Sava reservations connect neighbourhood roads to the far-bank parkway",
        &[crossings[0].bridge, crossings[1].bridge, bank_link],
    );
    let park_start = design_point(vec2(970.0, 0.0), config).x;
    let park_end = design_point(vec2(1510.0, 0.0), config).x;
    let bank: Vec<_> = river
        .downstream
        .windows(3)
        .filter(|points| points[1].x >= park_start && points[1].x <= park_end)
        .map(|points| {
            (
                points[1],
                -(points[2] - points[0]).normalize().perpendicular(),
            )
        })
        .collect();
    let bank_strip = bank
        .iter()
        .map(|&(point, normal)| point + normal * 52.0)
        .chain(
            bank.iter()
                .rev()
                .map(|&(point, normal)| point + normal * 86.0),
        )
        .collect();
    Ok((
        crossings,
        RiversidePark {
            bank_strip,
            far_bank_access: bank_link,
        },
    ))
}

fn connect_neighbourhoods(graph: &mut Graph, config: TownConfig) -> Result<(), String> {
    let node = |id| SourceKey::Node { id, rail: false };
    let local = SourceKey::Authored;
    let h_west = node(4193639703);
    let h_east = node(4193639677);
    let h_school = node(13109951568);
    let e_north = node(4193639635);
    let e_south = node(4445164883);
    let station = node(11950257454);
    let alpska = node(781857034);
    for key in [h_west, h_east, h_school, station, alpska] {
        if !graph.junctions.contains_key(&key) {
            return Err(format!("missing source entrance for neighbourhood {key:?}"));
        }
    }
    let north = [
        ("hraska-elm", 1790.0, 2410.0),
        ("hraska-orchard", 1905.0, 2452.0),
        ("hraska-school-bend", 2033.0, 2430.0),
        ("hraska-meadow", 2163.0, 2386.0),
        ("hraska-grove", 1750.0, 2517.0),
        ("hraska-north-orchard", 1858.0, 2581.0),
        ("hraska-north-meadow", 1989.0, 2556.0),
        ("hraska-high-grove", 2100.0, 2548.0),
        ("hraska-high-meadow", 2198.0, 2489.0),
        ("hraska-west-grove", 1637.0, 2510.0),
        ("hraska-west-park", 1636.0, 2398.0),
        ("hraska-ridge", 1953.0, 2680.0),
        ("hraska-school-gate", 1912.0, 2330.0),
        ("hraska-school-approach", 2028.0, 2335.0),
    ];
    let employment = [
        ("freight-north-yard", 2673.0, 2363.0),
        ("freight-rim-bend", 2788.0, 2308.0),
        ("freight-east-yard", 2849.0, 2191.0),
        ("freight-west-yard", 2642.0, 2242.0),
    ];
    let fit_employment = [e_north, e_south]
        .iter()
        .all(|key| graph.junctions.contains_key(key))
        && employment.iter().all(|&(_, x, y)| {
            let point = design_point(vec2(x, y), config);
            inside(point + Vec2::splat(24.0), config) && inside(point - Vec2::splat(24.0), config)
        });
    for (name, x, y) in north
        .into_iter()
        .chain(employment.into_iter().filter(|_| fit_employment))
    {
        let key = local(name);
        graph.junctions.insert(
            key,
            JunctionSpec::new(key, design_point(vec2(x, y), config)),
        );
    }
    let n = |index: usize| local(north[index].0);
    let e = |index: usize| local(employment[index].0);
    let streets = [
        ("hraska-elm-entry", h_west, n(0), 5.0),
        ("hraska-orchard-street", n(0), n(1), -7.0),
        ("hraska-school-frontage", n(1), n(2), 4.0),
        ("hraska-meadow-street", n(2), n(3), -6.0),
        ("hraska-east-entry", n(3), h_east, 7.0),
        ("hraska-grove-approach", n(0), n(4), -5.0),
        ("hraska-upper-orchard", n(4), n(5), 6.0),
        ("hraska-upper-meadow", n(5), n(6), -7.0),
        ("hraska-high-grove-street", n(6), n(7), 5.0),
        ("hraska-high-meadow-street", n(7), n(8), -5.0),
        ("hraska-meadow-turn", n(8), n(3), 4.0),
        ("hraska-orchard-link", n(1), n(5), -4.0),
        ("hraska-school-link", n(2), n(6), 4.0),
        ("hraska-west-grove-bend", n(4), n(9), -6.0),
        ("hraska-west-park-frontage", n(9), n(10), 5.0),
        ("hraska-park-approach", n(10), h_west, -5.0),
        ("hraska-north-ridge", n(5), n(11), 9.0),
        ("hraska-ridge-meadow", n(11), n(6), -7.0),
        ("hraska-school-south", n(1), n(12), 5.0),
        ("hraska-school-curve", n(12), n(13), -6.0),
        ("hraska-school-entry", n(13), h_school, 4.0),
    ];
    let freight_streets = [
        ("freight-north-entry", e_north, e(0), 5.0),
        ("freight-rim-frontage", e(0), e(1), 9.0),
        ("freight-east-bend", e(1), e(2), -8.0),
        ("freight-south-entry", e(2), e_south, 4.0),
        ("freight-west-frontage", e(0), e(3), -5.0),
        ("freight-yard-link", e(3), e_south, 6.0),
    ];
    for (name, a, b, bend) in streets
        .into_iter()
        .chain(freight_streets.into_iter().filter(|_| fit_employment))
    {
        let [from, to] = [a, b].map(|key| graph.junctions[&key].position.xy());
        let elbow = (from + to) * 0.5 + (to - from).normalize().perpendicular() * bend;
        add_authored_road(graph, local(name), [a, b], RoadClass::Local, Some(elbow), 0);
    }
    let station_bend = local("station-civic-block");
    graph.junctions.insert(
        station_bend,
        JunctionSpec::new(station_bend, design_point(vec2(2210.0, 1633.0), config)),
    );
    add_authored_road(
        graph,
        local("station-civic-entry"),
        [station, station_bend],
        RoadClass::Local,
        Some(design_point(vec2(2158.0, 1643.0), config)),
        0,
    );
    add_authored_road(
        graph,
        local("station-alpska-entry"),
        [station_bend, alpska],
        RoadClass::Local,
        Some(design_point(vec2(2253.0, 1645.0), config)),
        0,
    );
    graph.record(
        "short civic frontage closes the station block and adds an Alpska exit",
        &[station, station_bend, alpska],
    );
    graph.record(
        "curved Hraška residential streets join three source entrances",
        &[h_west, h_east, h_school],
    );
    if fit_employment {
        graph.record(
            "freight frontage loops between two source entrances",
            &[e_north, e_south],
        );
    }
    Ok(())
}

fn add_authored_road(
    graph: &mut Graph,
    key: SourceKey,
    endpoints: [SourceKey; 2],
    class: RoadClass,
    elbow: Option<Vec2>,
    layer: i8,
) {
    let points = endpoints
        .map(|key| graph.junctions[&key].position.xy())
        .to_vec();
    graph.links.insert(
        key,
        LinkSpec {
            key,
            endpoints,
            class,
            layer,
            elbow,
            points,
            one_way: false,
            roundabout: false,
            named: false,
            lineage: BTreeSet::new(),
            samples: Vec::new(),
            realigned: false,
            inherited: Vec::new(),
        },
    );
}

fn water_intersections(river: &RiverReach, link: &LinkSpec) -> Vec<Vec2> {
    link.points
        .windows(2)
        .flat_map(|road| {
            river.downstream.windows(2).filter_map(move |water| {
                let (along, _) = segment_parameters(road[0], road[1], water[0], water[1])?;
                Some(road[0] + along * (road[1] - road[0]))
            })
        })
        .collect()
}

fn validate_river_crossings(
    river: &RiverReach,
    crossings: &[CrossingSpec],
    graph: &Graph,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for link in graph.links.values() {
        let intersections = water_intersections(river, link);
        let expected = crossings
            .iter()
            .find(|crossing| crossing.bridge == link.key);
        match expected {
            Some(crossing)
                if intersections.len() == 1 && intersections[0].distance(crossing.point) < 1.0 => {}
            None if intersections.is_empty() => {}
            _ => errors.push(format!("{:?}: {intersections:?}", link.key)),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "inconsistent Sava crossings: {}",
            errors.join(", ")
        ))
    }
}

pub(crate) fn design_point(point: Vec2, config: TownConfig) -> Vec2 {
    let baseline = TownConfig::default();
    let (lat, lon) = (baseline.latitude, baseline.longitude);
    point + (config.project(lat, lon) - baseline.project(lat, lon))
}

pub(super) fn fit_wet_frontage(plan: &mut TownPlan) {
    // Use existing town-bank frontage for the northern and western reserves.
    // In South, exchange the isolated river fringe for the streets closer to
    // the town's jobs and suppliers on the east side.
    for (district, edge, points) in [
        (
            "hraska",
            0..1,
            &[(1050., 2840.), (950., 2380.), (1500., 2340.)][..],
        ),
        (
            "west",
            7..8,
            &[(690., 2210.), (535., 2090.), (550., 1730.)][..],
        ),
        (
            "south",
            0..9,
            &[
                (1370., 790.),
                (1750., 760.),
                (2245., 770.),
                (2610., 820.),
                (2960., 1050.),
                (3040., 1650.),
                (2660., 1660.),
                (2340., 1470.),
                (1840., 1330.),
                (1480., 1190.),
            ][..],
        ),
        (
            "employment",
            0..2,
            &[(2230., 2070.), (2300., 1940.), (2460., 1890.)][..],
        ),
    ] {
        let area = plan
            .districts
            .iter_mut()
            .find(|area| area.key == district)
            .unwrap();
        area.polygon.splice(
            edge,
            points
                .iter()
                .map(|&(x, y)| design_point(vec2(x, y), plan.config)),
        );
    }

    // Keep curb maneuvers off the two through links where the wet trace
    // found a stopped tail and an upstream conflict.
    for (id, reason) in [
        (
            350992954,
            "keep the South through link clear of curb parking",
        ),
        (
            1426155807,
            "keep the Hraška through link clear of curb parking",
        ),
    ] {
        let key = SourceKey::Way { id, span: 0 };
        if let Some(link) = plan.graph.links.get_mut(&key) {
            link.class = RoadClass::Collector;
            plan.graph.record(reason, &[key]);
        }
    }
}

fn districts(config: TownConfig) -> Vec<District> {
    [
        (
            "station",
            "Station / old centre",
            &[
                (1630., 1470.),
                (1810., 1370.),
                (2290., 1470.),
                (2470., 1710.),
                (2225., 2020.),
                (1780., 1840.),
            ][..],
            &[
                ("civic stop", 2020., 1610.),
                ("infill", 2200., 1730.),
                ("pocket park", 1850., 1550.),
            ][..],
        ),
        (
            "hraska",
            "North / Hraška",
            &[
                (1490., 2580.),
                (1640., 2020.),
                (1970., 1900.),
                (2240., 2060.),
                (2235., 2680.),
                (1860., 2870.),
            ][..],
            &[("school", 1910., 2290.), ("infill", 1780., 2490.)][..],
        ),
        (
            "south",
            "South / river-facing homes",
            &[
                (1370., 720.),
                (1620., 525.),
                (1680., 130.),
                (2060., 95.),
                (2170., 610.),
                (2590., 1040.),
                (2340., 1470.),
                (1840., 1330.),
                (1480., 1190.),
            ][..],
            &[
                ("school", 1810., 1090.),
                ("park", 1510., 880.),
                ("infill", 2100., 1110.),
            ][..],
        ),
        (
            "west",
            "West / Plana and Tovarniška",
            &[
                (810., 1530.),
                (995., 1200.),
                (1250., 1115.),
                (1480., 1200.),
                (1630., 1470.),
                (1510., 1820.),
                (1630., 2190.),
                (1300., 2330.),
                (1020., 2140.),
            ][..],
            &[("infill", 1170., 1740.), ("pocket park", 1150., 1940.)][..],
        ),
        (
            "employment",
            "A2 / rail employment edge",
            &[
                (2230., 2070.),
                (2460., 1930.),
                (2870., 2110.),
                (2900., 2600.),
                (2630., 2920.),
                (2245., 2770.),
            ][..],
            &[
                ("depot / utilities", 2580., 2420.),
                ("freight access", 2380., 2590.),
                ("siding study", 1680., 2150.),
            ][..],
        ),
    ]
    .into_iter()
    .map(|(key, name, polygon, reservations)| District {
        key,
        name,
        polygon: polygon
            .iter()
            .map(|&(x, y)| design_point(vec2(x, y), config))
            .collect(),
        reservations: reservations
            .iter()
            .map(|&(role, x, y)| Reservation {
                role,
                position: design_point(vec2(x, y), config),
            })
            .collect(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citymap_clip_preserves_entry_and_exit_of_an_outside_segment() {
        assert_eq!(
            clip(vec2(-4.0, 5.0), vec2(12.0, 5.0), Vec2::ZERO, vec2(8.0, 8.0)),
            Some((0.25, 0.75))
        );
        assert_eq!(
            clip(vec2(-4.0, 9.0), vec2(12.0, 9.0), Vec2::ZERO, vec2(8.0, 8.0)),
            None
        );
    }

    #[test]
    fn citymap_reentering_way_keeps_distinct_stable_cut_keys() {
        let way: Element = serde_json::from_str(r#"{"type":"way","id":7,"nodes":[1,2,3],
            "geometry":[{"lat":46.36,"lon":14.14},{"lat":46.36,"lon":14.16},{"lat":46.37,"lon":14.16}]}"#).unwrap();
        let spans = clipped_way(&way, TownConfig::default(), false);
        assert_eq!(spans.len(), 1);
        assert!(matches!(
            spans[0][0].key,
            SourceKey::Cut {
                way: 7,
                segment: 0,
                side: 0
            }
        ));
        assert!(matches!(
            spans[0][2].key,
            SourceKey::Cut {
                way: 7,
                segment: 1,
                side: 1
            }
        ));
    }

    #[test]
    fn citymap_clips_feature_rings_and_keeps_them_closed() {
        let ring = [
            (46.359, 14.14),
            (46.362, 14.14),
            (46.362, 14.16),
            (46.359, 14.16),
            (46.359, 14.14),
        ]
        .map(|(lat, lon)| Coordinate { lat, lon });
        let config = TownConfig::default();
        let result = clip_ring(&ring, config);
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], result[4]);
        assert!(result.into_iter().all(|point| inside(point, config)));
    }
}
