use super::network::{
    clean_network, Graph, JunctionSpec, LinkSpec, RoadClass, SourceKey, SpacingLimits,
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
    pub crossings: Vec<CrossingSpec>,
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
pub struct CrossingSpec {
    pub key: &'static str,
    #[serde(serialize_with = "serialize_point")]
    pub point: Vec2,
    #[serde(serialize_with = "serialize_points")]
    pub approaches: [Vec2; 2],
    pub river_way: u64,
    pub mapped_bridge: Option<u64>,
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
    let graph = clean_network(source_graph.clone(), limits, config)?;
    let station = elements
        .iter()
        .find(|element| element.kind == Kind::Node && element.id == 5685260194)
        .and_then(|element| element.point(config))
        .ok_or("missing Lesce-Bled station node")?;
    let features = feature_rings(elements, config);
    let (river, crossings) = inputs
        .sava
        .as_ref()
        .map(|sava| water_reservations(sava.elements(), config))
        .transpose()?
        .unwrap_or_default();

    Ok(TownPlan {
        config,
        limits,
        source_graph,
        graph,
        features,
        districts: districts(config),
        river,
        crossings,
        station,
    })
}

#[derive(Clone)]
struct Sample {
    key: SourceKey,
    point: Vec2,
}

const A2_MAINLINE: &[u64] = &[64017257, 476387831, 61232337, 475299897, 61233481];
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

fn water_reservations(
    elements: &[Element],
    config: TownConfig,
) -> Result<(Vec<RiverReach>, Vec<CrossingSpec>), String> {
    let mut river = Vec::new();
    for element in elements
        .iter()
        .filter(|element| element.tag("waterway") == "river")
    {
        let width = element
            .tag("width")
            .trim_end_matches(" m")
            .parse::<f32>()
            .ok()
            .map(|width| width * TownConfig::SCALE);
        for span in clipped_way(element, config, false) {
            river.push(RiverReach {
                way: element.id,
                downstream: span.into_iter().map(|sample| sample.point).collect(),
                source_width_scaled: width,
            });
        }
    }
    let reach = river
        .iter()
        .max_by(|a, b| {
            super::network::arc(&a.downstream).total_cmp(&super::network::arc(&b.downstream))
        })
        .ok_or("pinned Sava does not reach the owned rectangle")?;
    let crossings = [
        ("sava-town", vec2(320.0, 645.0)),
        ("sava-rim", vec2(245.0, 290.0)),
    ]
    .into_iter()
    .map(|(key, target)| {
        let target = design_point(target, config);
        let (point, tangent) = reach
            .downstream
            .windows(2)
            .map(|segment| {
                let point = super::network::project_on_segment(target, segment[0], segment[1]);
                (point, segment[1] - segment[0])
            })
            .min_by(|(a, _), (b, _)| a.distance2(target).total_cmp(&b.distance2(target)))
            .unwrap();
        let normal = vec2(-tangent.y, tangent.x).normalize();
        let approaches = [point - 110.0 * normal, point + 110.0 * normal];
        if approaches.iter().any(|&point| !inside(point, config)) {
            return Err(format!(
                "Sava reservation {key} has an approach beyond the start area"
            ));
        }
        let mapped_bridge = elements
            .iter()
            .filter(|element| !element.tag("bridge").is_empty())
            .find(|bridge| {
                bridge.geometry.windows(2).any(|pair| {
                    let a = pair[0].project(config);
                    let b = pair[1].project(config);
                    super::network::project_on_segment(point, a, b).distance(point) < 25.0
                })
            })
            .map(|bridge| bridge.id);
        Ok(CrossingSpec {
            key,
            point,
            approaches,
            river_way: reach.way,
            mapped_bridge,
        })
    })
    .collect::<Result<Vec<_>, String>>()?;
    Ok((river, crossings))
}

pub(crate) fn design_point(point: Vec2, config: TownConfig) -> Vec2 {
    let baseline = TownConfig::default();
    point + config.translation - baseline.translation
        + (config.owned_min + config.owned_max - baseline.owned_max) * 0.5
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
                (1250., 660.),
                (1570., 510.),
                (2060., 655.),
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
