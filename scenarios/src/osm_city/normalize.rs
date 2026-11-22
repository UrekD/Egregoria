use super::source::{Coordinate, Element, ElementKind, OverpassExtract};
use super::spatial::{point_bounds, points_bounds, ring_contains, BoundsIndex, PolygonWork};
use super::ImportError;
use geom::{vec2, Vec2, AABB};
use serde::Serialize;
use simulation::map::import::{BoundaryStub, ImportedCity, ImportedJunction, ImportedLink};
use simulation::map::{Density, LaneKind, LanePattern, LanePatternBuilder, LightPolicy, ZoneKind};
use std::collections::{BTreeMap, BTreeSet};

const METRES_NORTH: f64 = 111_158.360;
const LESCE_LATITUDE: f64 = 46.3605;
const LESCE_METRES_EAST: f64 = 76_958.685;
const MIN_LINK_LENGTH: f32 = 10.0;
const MAX_LINKS: usize = 6000;
/// Mapped signals usually sit on the approaches, a few metres before the junction node.
const SIGNAL_RADIUS: f32 = 25.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GeoBounds {
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

impl GeoBounds {
    pub fn project(self, lat: f64, lon: f64) -> Vec2 {
        let mid_latitude = (self.south + self.north) * 0.5;
        let metres_east =
            LESCE_METRES_EAST * mid_latitude.to_radians().cos() / LESCE_LATITUDE.to_radians().cos();
        let east = (lon - (self.west + self.east) * 0.5) * metres_east;
        let north = (lat - (self.south + self.north) * 0.5) * METRES_NORTH;
        vec2((4096.0 + east) as f32, (4096.0 + north) as f32)
    }

    pub(super) fn limits(self) -> (Vec2, Vec2) {
        (
            self.project(self.south, self.west),
            self.project(self.north, self.east),
        )
    }

    pub(super) fn limits_aabb(self) -> AABB {
        let (ll, ur) = self.limits();
        AABB { ll, ur }
    }

    fn point(self, point: Coordinate) -> Vec2 {
        self.project(point.lat, point.lon)
    }

    fn contains(self, point: Vec2) -> bool {
        let (min, max) = self.limits();
        point.x >= min.x && point.x <= max.x && point.y >= min.y && point.y <= max.y
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkClass {
    Motorway,
    Ramp,
    Arterial,
    Street,
    Foot,
    MainRail,
    OtherRail,
}

impl LinkClass {
    pub(super) fn rail(self) -> bool {
        matches!(self, Self::MainRail | Self::OtherRail)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    pub source: String,
    pub kind: ZoneKind,
    pub density: Density,
    pub building: bool,
    pub house: bool,
    pub inferred_house: bool,
    pub outers: Vec<Vec<Vec2>>,
    pub holes: Vec<Vec<Vec2>>,
}

impl Feature {
    pub fn contains(&self, point: Vec2) -> bool {
        self.outers.iter().any(|ring| ring_contains(ring, point))
            && !self.holes.iter().any(|ring| ring_contains(ring, point))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Forest {
    pub source: String,
    pub outers: Vec<Vec<Vec2>>,
    pub holes: Vec<Vec<Vec2>>,
}

impl Forest {
    pub fn contains(&self, point: Vec2) -> bool {
        self.outers.iter().any(|ring| ring_contains(ring, point))
            && !self.holes.iter().any(|ring| ring_contains(ring, point))
    }

    pub(super) fn bounds(&self) -> AABB {
        points_bounds(self.outers.iter().flatten().copied())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Site {
    pub source: String,
    pub name: String,
    pub kind: &'static str,
    pub position: Vec2,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NormalizedCity {
    pub city: ImportedCity,
    /// OSM origin of each junction and the reason for its traffic control, indexed by junction id.
    pub junction_sources: Vec<(String, &'static str)>,
    pub classes: Vec<LinkClass>,
    pub features: Vec<Feature>,
    pub forests: Vec<Forest>,
    pub sites: Vec<Site>,
    pub skipped: Vec<(String, String)>,
    pub flat_crossings: Vec<(String, String)>,
    pub source_elements: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Anchor {
    Node(bool, u64),
    Roundabout(u64),
    Cut(u64, usize, bool),
}

#[derive(Clone, Copy, Debug)]
struct Vertex {
    key: Anchor,
    point: Vec2,
}

struct Way<'a> {
    source: &'a Element,
    class: LinkClass,
    grade: i8,
    roundabout: bool,
    pattern: LanePattern,
    vertices: Vec<Vertex>,
}

struct Edge {
    way: u64,
    class: LinkClass,
    grade: i8,
    frontage: bool,
    pattern: LanePattern,
    vertices: Vec<Vertex>,
}

pub fn normalize(
    extract: OverpassExtract,
    bounds: GeoBounds,
) -> Result<NormalizedCity, ImportError> {
    let (min, max) = bounds.limits();
    if !(bounds.south < bounds.north && bounds.west < bounds.east)
        || !min.is_finite()
        || !max.is_finite()
        || min.x <= 0.0
        || min.y <= 0.0
        || max.x >= 8192.0
        || max.y >= 8192.0
    {
        return Err("OSM bounds must fit in the 16-chunk city map".into());
    }

    let mut normalized = NormalizedCity {
        city: ImportedCity::default(),
        junction_sources: Vec::new(),
        classes: Vec::new(),
        features: Vec::new(),
        forests: Vec::new(),
        sites: Vec::new(),
        skipped: Vec::new(),
        flat_crossings: Vec::new(),
        source_elements: extract.elements.len(),
    };
    let ways: Vec<_> = extract
        .elements
        .iter()
        .filter(|e| e.kind == ElementKind::Way)
        .filter_map(|e| {
            let result = make_way(e, bounds);
            if result.is_none() && (!e.tag("highway").is_empty() || !e.tag("railway").is_empty()) {
                normalized
                    .skipped
                    .push((e.label(), "road or rail class omitted".into()));
            }
            result
        })
        .collect();

    let roundabouts = roundabout_anchors(&ways, bounds);
    let mut road_nodes = BTreeSet::new();
    let mut rail_nodes = BTreeSet::new();
    let mut counts = BTreeMap::<Anchor, usize>::new();
    for way in &ways {
        for vertex in &way.vertices {
            *counts.entry(vertex.key).or_default() += 1;
            if let Anchor::Node(rail, node) = vertex.key {
                if rail {
                    rail_nodes.insert(node);
                } else if way.class != LinkClass::Foot {
                    road_nodes.insert(node);
                }
            }
        }
    }

    let crossings: BTreeSet<_> = road_nodes.intersection(&rail_nodes).copied().collect();
    for node in &crossings {
        normalized.flat_crossings.push((
            format!("node/{node}"),
            "at-grade road/rail crossing with independent through routes".into(),
        ));
    }

    let mut raw_edges = Vec::new();
    for way in &ways {
        if way.roundabout {
            continue;
        }
        let mut vertices: Vec<Vertex> = Vec::new();
        for vertex in &way.vertices {
            let vertex = roundabouts.get(&vertex.key).map_or(*vertex, |(v, _)| *v);
            if vertices.last().is_none_or(|last| last.key != vertex.key) {
                vertices.push(vertex);
            }
        }
        let spans = clip_way(&vertices, way.source.id, min, max);
        if spans.is_empty() {
            normalized
                .skipped
                .push((way.source.label(), "link outside import bounds".into()));
        }
        for span in spans {
            let mut first = 0;
            for i in 1..span.len() {
                let end = span[i];
                let split = match end.key {
                    Anchor::Node(_, id) => counts[&end.key] > 1 || crossings.contains(&id),
                    _ => true,
                };
                if split || i + 1 == span.len() {
                    split_edge(
                        way,
                        &span[first..=i],
                        &mut raw_edges,
                        &mut normalized.skipped,
                    );
                    first = i;
                }
            }
        }
        if raw_edges.len() > MAX_LINKS {
            return Err(format!("OSM extract exceeds the {MAX_LINKS}-link import limit").into());
        }
    }
    let mut raw_edges = contract_short_edges(raw_edges, &mut normalized.skipped)?;
    raw_edges.retain_mut(|edge| {
        let first = edge.vertices[0].point;
        let last = edge.vertices.last().unwrap().point;
        // Match the engine's sample cleanup before testing the minimum length.
        let end = edge.vertices.pop().unwrap();
        let mut samples = edge.vertices.drain(..);
        let start = samples.next().unwrap();
        let mut cleaned = vec![start];
        for sample in samples {
            if sample.point.distance2(first) >= 1.0
                && sample.point.distance2(last) >= 1.0
                && sample.point.distance2(cleaned.last().unwrap().point) >= 0.04
            {
                cleaned.push(sample);
            }
        }
        cleaned.push(end);
        edge.vertices = cleaned;
        let valid = first.distance(last) >= 1.0
            && edge.vertices.len() >= 2
            && edge_length(&edge.vertices) >= MIN_LINK_LENGTH;
        if !valid {
            normalized.skipped.push((
                format!("way/{}", edge.way),
                "unusable endpoints or cleaned geometry".into(),
            ));
        }
        valid
    });
    report_grade_crossings(&raw_edges, &mut normalized.flat_crossings)?;

    let anchors: BTreeMap<_, _> = raw_edges
        .iter()
        .flat_map(|edge| [edge.vertices[0], *edge.vertices.last().unwrap()])
        .map(|vertex| (vertex.key, vertex.point))
        .collect();
    let keys: BTreeMap<_, _> = anchors
        .keys()
        .enumerate()
        .map(|(id, &key)| (key, id))
        .collect();
    let radii: BTreeMap<_, _> = roundabouts
        .values()
        .map(|(v, radius)| (v.key, *radius))
        .collect();
    let mut legs = BTreeMap::<Anchor, Vec<LinkClass>>::new();
    for edge in raw_edges
        .iter()
        .filter(|edge| !edge.class.rail() && edge.class != LinkClass::Foot)
    {
        for vertex in [edge.vertices[0], *edge.vertices.last().unwrap()] {
            legs.entry(vertex.key).or_default().push(edge.class);
        }
    }
    let signalled = signalled_junctions(
        &extract.elements,
        bounds,
        &anchors,
        &legs,
        &mut normalized.skipped,
    );
    let mut controls = Vec::with_capacity(anchors.len());
    normalized.city.junctions = anchors
        .iter()
        .map(|(&key, &position)| {
            let legs = legs.get(&key).map_or(&[][..], Vec::as_slice);
            let (light_policy, reason) = junction_control(key, signalled.contains(&key), legs);
            controls.push(reason);
            ImportedJunction {
                id: keys[&key],
                position,
                roundabout_radius: radii.get(&key).copied(),
                light_policy,
            }
        })
        .collect();
    normalized.junction_sources = anchors
        .keys()
        .zip(controls)
        .map(|(key, reason)| {
            let source = match *key {
                Anchor::Node(_, id) => format!("node/{id}"),
                Anchor::Roundabout(id) => format!("node/{id} roundabout"),
                Anchor::Cut(way, ..) => format!("way/{way} boundary"),
            };
            (source, reason)
        })
        .collect();

    for edge in raw_edges {
        let from = edge.vertices[0].key;
        let to = edge.vertices.last().unwrap().key;
        let link = normalized.city.links.len();
        for (key, at_start) in [(from, true), (to, false)] {
            if matches!(key, Anchor::Cut(..)) {
                normalized.city.boundary_stubs.push(BoundaryStub {
                    junction: keys[&key],
                    link,
                    at_start,
                });
            }
        }
        normalized.classes.push(edge.class);
        normalized.city.links.push(ImportedLink {
            source_way_ids: vec![edge.way],
            from: keys[&from],
            to: keys[&to],
            points: edge.vertices.iter().map(|v| v.point).collect(),
            pattern: edge.pattern,
            frontage: edge.frontage,
        });
    }
    if normalized.city.links.is_empty() {
        return Err("no usable road or rail links inside the bounds".into());
    }
    collect_features(&extract.elements, bounds, &mut normalized)?;
    Ok(normalized)
}

/// Traffic control for a junction with the given road legs, and the reason for the report.
/// Junctions default to priority signage. Vehicles do not accept gaps in a busy through
/// stream, so streets crossing an arterial are signalled. Roundabouts yield on entry.
fn junction_control(
    key: Anchor,
    signalled: bool,
    legs: &[LinkClass],
) -> (LightPolicy, &'static str) {
    let arterials = legs
        .iter()
        .filter(|&&class| class == LinkClass::Arterial)
        .count();
    if signalled {
        (LightPolicy::Lights, "mapped traffic signals")
    } else if matches!(key, Anchor::Node(true, _)) {
        (LightPolicy::Auto, "rail")
    } else if matches!(key, Anchor::Roundabout(_)) {
        (LightPolicy::NoLights, "roundabout")
    } else if legs.len() >= 4 && arterials >= 2 && legs.contains(&LinkClass::Street) {
        (LightPolicy::Lights, "arterial crossroads")
    } else {
        (LightPolicy::NoLights, "priority signage")
    }
}

/// Road junctions with mapped traffic signals at the node or on an approach.
fn signalled_junctions(
    elements: &[Element],
    bounds: GeoBounds,
    anchors: &BTreeMap<Anchor, Vec2>,
    legs: &BTreeMap<Anchor, Vec<LinkClass>>,
    skipped: &mut Vec<(String, String)>,
) -> BTreeSet<Anchor> {
    // Pedestrian-only signals do not control the junction.
    let mut signals: Vec<_> = elements
        .iter()
        .filter(|element| {
            element.kind == ElementKind::Node
                && element.tag("highway") == "traffic_signals"
                && matches!(element.tag("traffic_signals"), "" | "signal")
        })
        .collect();
    signals.sort_by_key(|element| element.id);
    let mut signalled = BTreeSet::new();
    for element in signals {
        let Some(point) = element.point().map(|point| bounds.point(point)) else {
            continue;
        };
        let nearest = anchors
            .iter()
            .filter(|(key, _)| legs.get(key).is_some_and(|legs| legs.len() >= 3))
            .map(|(key, position)| (position.distance(point), *key))
            .filter(|&(distance, _)| distance <= SIGNAL_RADIUS)
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        match nearest {
            Some((_, key)) => {
                signalled.insert(key);
            }
            None if bounds.contains(point) => skipped.push((
                element.label(),
                "traffic signal not at a road junction".into(),
            )),
            None => {}
        }
    }
    signalled
}

fn make_way(element: &Element, bounds: GeoBounds) -> Option<Way<'_>> {
    let highway = element.tag("highway");
    let class = if element.tag("railway") == "rail" {
        if element.tag("service").is_empty() {
            LinkClass::MainRail
        } else {
            LinkClass::OtherRail
        }
    } else {
        if matches!(element.tag("access"), "private" | "no") {
            return None;
        }
        match highway {
            "motorway" => LinkClass::Motorway,
            "motorway_link" => LinkClass::Ramp,
            "primary" | "secondary" | "tertiary" | "primary_link" | "secondary_link"
            | "tertiary_link" => LinkClass::Arterial,
            "residential" | "living_street" | "unclassified" => LinkClass::Street,
            "service" if !matches!(element.tag("service"), "driveway" | "parking_aisle") => {
                LinkClass::Street
            }
            "pedestrian" => LinkClass::Foot,
            _ => return None,
        }
    };
    let roundabout = element.tag("junction") == "roundabout" && !class.rail();
    let reverse = element.tag("oneway") == "-1" && !class.rail();
    let one_way = !class.rail()
        && match element.tag("oneway") {
            "yes" | "true" | "1" | "-1" => true,
            "no" | "false" | "0" => false,
            _ => roundabout || class == LinkClass::Motorway,
        };
    let implicit_grade = if !matches!(element.tag("bridge"), "" | "no") {
        1
    } else if !matches!(element.tag("tunnel"), "" | "no") {
        -1
    } else {
        0
    };
    let grade = element.tag("layer").parse().unwrap_or(implicit_grade);
    let mut vertices: Vec<_> = element
        .nodes
        .iter()
        .zip(&element.geometry)
        .map(|(&id, &point)| Vertex {
            key: Anchor::Node(class.rail(), id),
            point: bounds.point(point),
        })
        .collect();
    if reverse {
        vertices.reverse();
    }
    Some(Way {
        source: element,
        class,
        grade,
        roundabout,
        pattern: lane_pattern(element, class, one_way, reverse),
        vertices,
    })
}

fn lane_pattern(element: &Element, class: LinkClass, one_way: bool, reverse: bool) -> LanePattern {
    let default_speed = match class {
        LinkClass::Motorway => 130.0,
        LinkClass::Ramp => 60.0,
        LinkClass::Arterial => 50.0,
        LinkClass::MainRail | LinkClass::OtherRail => 80.0,
        _ => 30.0,
    } / 3.6;
    let tagged = element.tag("maxspeed").trim();
    let (value, scale) = tagged
        .strip_suffix("mph")
        .map_or((tagged.trim_end_matches("km/h"), 1.0 / 3.6), |mph| {
            (mph, 0.44704)
        });
    let speed = value
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .map_or(default_speed, |v| (v * scale).clamp(1.0, 60.0));
    if class.rail() || class == LinkClass::Foot {
        return LanePatternBuilder::new()
            .rail(class.rail())
            .n_lanes(u32::from(class.rail()))
            .speed_limit(speed)
            .build();
    }
    let lane_tag = |tag| {
        element
            .tag(tag)
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(|n| n.min(6))
    };
    let default = if class == LinkClass::Motorway { 2 } else { 1 };
    let total = lane_tag("lanes").unwrap_or(default * if one_way { 1 } else { 2 });
    let (forward_tag, backward_tag) = if reverse {
        ("lanes:backward", "lanes:forward")
    } else {
        ("lanes:forward", "lanes:backward")
    };
    let forward = lane_tag(forward_tag);
    let backward = lane_tag(backward_tag);
    let forward_count = forward.unwrap_or_else(|| {
        if one_way {
            total
        } else {
            total.saturating_sub(backward.unwrap_or(total / 2)).max(1)
        }
    });
    let backward_count = if one_way {
        0
    } else {
        backward.unwrap_or(total.saturating_sub(forward_count).max(1))
    };
    let sidewalks = matches!(class, LinkClass::Street | LinkClass::Arterial)
        && !element.tag("highway").ends_with("_link");
    let side = |count| {
        let mut lanes = vec![(LaneKind::Driving, speed); count];
        if class == LinkClass::Street && count > 0 && element.tag("highway") != "service" {
            lanes.push((LaneKind::Parking, speed));
        }
        if sidewalks {
            lanes.push((LaneKind::Walking, speed));
        }
        lanes
    };
    LanePattern {
        lanes_forward: side(forward_count),
        lanes_backward: side(backward_count),
    }
}

fn roundabout_anchors(ways: &[Way<'_>], bounds: GeoBounds) -> BTreeMap<Anchor, (Vertex, f32)> {
    let mut neighbours = BTreeMap::<Anchor, Vec<Anchor>>::new();
    let mut positions = BTreeMap::new();
    for way in ways.iter().filter(|way| way.roundabout) {
        for vertex in &way.vertices {
            positions.insert(vertex.key, vertex.point);
        }
        for pair in way.vertices.windows(2) {
            neighbours.entry(pair[0].key).or_default().push(pair[1].key);
            neighbours.entry(pair[1].key).or_default().push(pair[0].key);
        }
    }
    let mut remaining: BTreeSet<_> = positions.keys().copied().collect();
    let mut result = BTreeMap::new();
    while let Some(first) = remaining.pop_first() {
        let mut component = BTreeSet::from([first]);
        let mut queue = vec![first];
        while let Some(node) = queue.pop() {
            for next in &neighbours[&node] {
                if remaining.remove(next) {
                    component.insert(*next);
                    queue.push(*next);
                }
            }
        }
        let position = component
            .iter()
            .map(|n| positions[n])
            .fold(Vec2::ZERO, |a, b| a + b)
            / component.len() as f32;
        if !bounds.contains(position) {
            continue;
        }
        let radius = component
            .iter()
            .map(|n| positions[n].distance(position))
            .sum::<f32>()
            / component.len() as f32;
        let Anchor::Node(_, id) = first else {
            unreachable!()
        };
        for node in component {
            result.insert(
                node,
                (
                    Vertex {
                        key: Anchor::Roundabout(id),
                        point: position,
                    },
                    radius.clamp(10.0, 50.0),
                ),
            );
        }
    }
    result
}

fn clip_way(vertices: &[Vertex], way: u64, min: Vec2, max: Vec2) -> Vec<Vec<Vertex>> {
    let mut spans = Vec::new();
    let mut span: Vec<Vertex> = Vec::new();
    for (index, pair) in vertices.windows(2).enumerate() {
        let [a, b] = [pair[0], pair[1]];
        let delta = b.point - a.point;
        if let Some((enter, leave)) = clip_segment(a.point, delta, min, max) {
            let start = if enter == 0.0 {
                a
            } else {
                Vertex {
                    key: Anchor::Cut(way, index, true),
                    point: a.point + delta * enter,
                }
            };
            let end = if leave == 1.0 {
                b
            } else {
                Vertex {
                    key: Anchor::Cut(way, index, false),
                    point: a.point + delta * leave,
                }
            };
            if span.last().is_some_and(|v| v.key != start.key) {
                spans.push(std::mem::take(&mut span));
            }
            if span.is_empty() {
                span.push(start);
            }
            span.push(end);
        } else if !span.is_empty() {
            spans.push(std::mem::take(&mut span));
        }
    }
    if !span.is_empty() {
        spans.push(span);
    }
    spans
}

fn clip_segment(start: Vec2, delta: Vec2, min: Vec2, max: Vec2) -> Option<(f32, f32)> {
    let mut enter: f32 = 0.0;
    let mut leave: f32 = 1.0;
    for (position, velocity, lo, hi) in [
        (start.x, delta.x, min.x, max.x),
        (start.y, delta.y, min.y, max.y),
    ] {
        if velocity == 0.0 {
            if position < lo || position > hi {
                return None;
            }
        } else {
            let a = (lo - position) / velocity;
            let b = (hi - position) / velocity;
            enter = enter.max(a.min(b));
            leave = leave.min(a.max(b));
        }
    }
    (enter < leave).then_some((enter, leave))
}

fn split_edge(
    way: &Way<'_>,
    span: &[Vertex],
    edges: &mut Vec<Edge>,
    skipped: &mut Vec<(String, String)>,
) {
    if span[0].key == span.last().unwrap().key {
        if span.len() < 3 {
            skipped.push((way.source.label(), "degenerate closed link".into()));
        } else {
            let mid = (span.len() - 1) / 2;
            if span[mid].key == span[0].key {
                skipped.push((
                    way.source.label(),
                    "closed link has no distinct midpoint".into(),
                ));
            } else {
                split_edge(way, &span[..=mid], edges, skipped);
                split_edge(way, &span[mid..], edges, skipped);
            }
        }
        return;
    }
    edges.push(Edge {
        way: way.source.id,
        class: way.class,
        grade: way.grade,
        frontage: way.grade == 0
            && way
                .pattern
                .lanes()
                .any(|(kind, _, _)| kind == LaneKind::Driving)
            && way
                .pattern
                .lanes()
                .any(|(kind, _, _)| kind == LaneKind::Walking),
        pattern: way.pattern.clone(),
        vertices: span.to_vec(),
    });
}

fn edge_length(vertices: &[Vertex]) -> f32 {
    vertices
        .windows(2)
        .map(|pair| pair[0].point.distance(pair[1].point))
        .sum()
}

fn contract_short_edges(
    edges: Vec<Edge>,
    skipped: &mut Vec<(String, String)>,
) -> Result<Vec<Edge>, ImportError> {
    let mut incident = BTreeMap::<Anchor, BTreeSet<usize>>::new();
    let mut pending = BTreeSet::new();
    let mut edges: Vec<_> = edges.into_iter().map(Some).collect();
    for (i, edge) in edges.iter().enumerate() {
        let edge = edge.as_ref().unwrap();
        for anchor in [edge.vertices[0].key, edge.vertices.last().unwrap().key] {
            incident.entry(anchor).or_default().insert(i);
        }
        if edge_length(&edge.vertices) < MIN_LINK_LENGTH {
            pending.insert(i);
        }
    }
    let mut rewired = 0;
    while let Some(index) = pending.pop_first() {
        let edge = edges[index].take().unwrap();
        let from = edge.vertices[0];
        let to = *edge.vertices.last().unwrap();
        incident.get_mut(&from.key).unwrap().remove(&index);
        incident.get_mut(&to.key).unwrap().remove(&index);
        let attached = |key| incident[&key].iter().map(|&id| edges[id].as_ref().unwrap());
        let forward = edge
            .pattern
            .lanes_forward
            .iter()
            .any(|(kind, _)| matches!(kind, LaneKind::Driving | LaneKind::Rail));
        let backward = edge
            .pattern
            .lanes_backward
            .iter()
            .any(|(kind, _)| matches!(kind, LaneKind::Driving | LaneKind::Rail));
        let directed_join = |anchor: Anchor, incoming: bool| {
            let mut incident = attached(anchor);
            let Some(other) = incident.next() else {
                return false;
            };
            if incident.next().is_some() {
                return false;
            }
            let starts_here = other.vertices[0].key == anchor;
            let lanes = if starts_here != incoming {
                &other.pattern.lanes_forward
            } else {
                &other.pattern.lanes_backward
            };
            let reverse_lanes = if starts_here != incoming {
                &other.pattern.lanes_backward
            } else {
                &other.pattern.lanes_forward
            };
            let moving = |lanes: &[(LaneKind, f32)]| {
                lanes
                    .iter()
                    .any(|(kind, _)| matches!(kind, LaneKind::Driving | LaneKind::Rail))
            };
            moving(lanes) && !moving(reverse_lanes)
        };
        let parallel = attached(from.key).any(|other| {
            [other.vertices[0].key, other.vertices.last().unwrap().key].contains(&to.key)
        });
        let safe = !parallel
            && attached(from.key).next().is_some()
            && attached(to.key).next().is_some()
            && ((forward && backward)
                || (forward && (directed_join(from.key, true) || directed_join(to.key, false))));
        if safe
            && !matches!(from.key, Anchor::Cut(..))
            && !matches!(to.key, Anchor::Cut(..))
            && !matches!(
                (from.key, to.key),
                (Anchor::Roundabout(_), Anchor::Roundabout(_))
            )
        {
            let (keep, discard) = if matches!(to.key, Anchor::Roundabout(_)) {
                (to, from)
            } else {
                (from, to)
            };
            let moved = std::mem::take(incident.get_mut(&discard.key).unwrap());
            let retained = incident.get_mut(&keep.key).unwrap();
            for i in moved {
                let other = edges[i].as_mut().unwrap();
                rewired += other.vertices.len();
                if rewired > 1_000_000 {
                    return Err(
                        "OSM short-link contraction exceeded the geometry work limit".into(),
                    );
                }
                let last = other.vertices.len() - 1;
                for endpoint in [0, last] {
                    if other.vertices[endpoint].key == discard.key {
                        other.vertices[endpoint] = keep;
                    }
                }
                retained.insert(i);
                if edge_length(&other.vertices) < MIN_LINK_LENGTH {
                    pending.insert(i);
                } else {
                    pending.remove(&i);
                }
            }
            skipped.push((
                format!("way/{}", edge.way),
                "sub-10 m connector contracted".into(),
            ));
        } else {
            skipped.push((format!("way/{}", edge.way), "sub-10 m link dropped".into()));
        }
    }
    Ok(edges.into_iter().flatten().collect())
}

fn report_grade_crossings(
    edges: &[Edge],
    report: &mut Vec<(String, String)>,
) -> Result<(), ImportError> {
    let segments: Vec<_> = edges
        .iter()
        .flat_map(|edge| {
            edge.vertices
                .windows(2)
                .map(|pair| (edge.way, edge.grade, pair[0].point, pair[1].point))
        })
        .collect();
    let index = BoundsIndex::new(
        segments
            .iter()
            .enumerate()
            .map(|(i, &(_, _, p, q))| (points_bounds([p, q].into_iter()), i)),
    );
    let mut pairs = BTreeSet::new();
    let mut work = 0;
    for (i, &(way, grade, p, end)) in segments.iter().enumerate() {
        for &j in index.query(points_bounds([p, end].into_iter())) {
            work += 1;
            if work > 4_000_000 {
                return Err("OSM grade crossing candidates exceeded the spatial work limit".into());
            }
            if j >= i {
                continue;
            }
            let (other, layer, q, other_end) = segments[j];
            if grade == layer || way == other {
                continue;
            }
            let pair = (way.min(other), way.max(other));
            if pairs.contains(&pair) {
                continue;
            }
            let r = end - p;
            let s = other_end - q;
            let cross = r.perp_dot(s);
            if cross.abs() >= 0.0001 {
                let t = (q - p).perp_dot(s) / cross;
                let u = (q - p).perp_dot(r) / cross;
                if (0.001..0.999).contains(&t) && (0.001..0.999).contains(&u) {
                    pairs.insert(pair);
                }
            }
        }
    }
    report.extend(pairs.into_iter().map(|(a, b)| {
        (
            format!("way/{a},way/{b}"),
            "grade-separated in OSM; rendered flat, potential z-fighting".into(),
        )
    }));
    Ok(())
}

fn ordered_coordinate(value: f64) -> u64 {
    if value == 0.0 {
        0
    } else {
        value.to_bits()
    }
}

type Rings = (Vec<Vec<Vec2>>, Vec<Vec<Vec2>>);

fn collect_features(
    elements: &[Element],
    bounds: GeoBounds,
    city: &mut NormalizedCity,
) -> Result<(), String> {
    let mut members = BTreeSet::new();
    let mut forest_members = BTreeSet::new();
    let mut area_sites = Vec::new();
    let mut point_sites = Vec::new();
    // Relations are considered first so member ways do not duplicate their area.
    for element in elements.iter().rev() {
        let class = zone_class(element);
        let forest = element.tag("landuse") == "forest" || element.tag("natural") == "wood";
        let site_kind = match element.tag("amenity") {
            "school" => Some("school"),
            "clinic" | "hospital" => Some("clinic"),
            "fire_station" => Some("fire-station"),
            "police" => Some("police-station"),
            _ if matches!(element.tag("railway"), "station" | "halt")
                || element.tag("building") == "train_station" =>
            {
                Some("passenger station")
            }
            _ => None,
        };
        if class.is_none() && site_kind.is_none() && !forest {
            if !element.tag("amenity").is_empty() {
                city.skipped.push((
                    element.label(),
                    format!("unsupported amenity {}", element.tag("amenity")),
                ));
            }
            continue;
        }
        if let Some(point) = element.point() {
            if let Some(kind) = site_kind {
                let pos = bounds.point(point);
                if bounds.contains(pos) {
                    point_sites.push(make_site(element, kind, pos));
                }
            }
            continue;
        }
        let Some((outers, holes)) = rings(element, bounds) else {
            city.skipped.push((
                element.label(),
                "could not assemble closed footprint rings".into(),
            ));
            continue;
        };
        let (min, max) = bounds.limits();
        if !outers.iter().any(|ring| {
            let lo = ring
                .iter()
                .copied()
                .fold(Vec2::splat(f32::INFINITY), Vec2::min);
            let hi = ring
                .iter()
                .copied()
                .fold(Vec2::splat(f32::NEG_INFINITY), Vec2::max);
            lo.x <= max.x && hi.x >= min.x && lo.y <= max.y && hi.y >= min.y
        }) {
            continue;
        }
        if let Some(kind) = site_kind {
            let largest = outers
                .iter()
                .map(|ring| (ring_area(ring).abs(), ring))
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap()
                .1;
            let center = ring_center(largest);
            if bounds.contains(center) {
                area_sites.push((
                    make_site(element, kind, center),
                    outers.clone(),
                    holes.clone(),
                ));
            } else {
                city.skipped.push((
                    element.label(),
                    "amenity center outside the import bounds".into(),
                ));
            }
        }
        if forest && !(element.kind == ElementKind::Way && forest_members.contains(&element.id)) {
            if element.kind == ElementKind::Relation {
                forest_members.extend(
                    element
                        .members
                        .iter()
                        .filter(|m| m.kind == ElementKind::Way)
                        .map(|m| m.id),
                );
            }
            city.forests.push(Forest {
                source: element.label(),
                outers: outers.clone(),
                holes: holes.clone(),
            });
        }
        if let Some((kind, density, building, house)) = class {
            if element.kind == ElementKind::Way && members.contains(&(element.id, kind, building)) {
                continue;
            }
            if element.kind == ElementKind::Relation {
                members.extend(
                    element
                        .members
                        .iter()
                        .filter(|m| m.kind == ElementKind::Way)
                        .map(|m| (m.id, kind, building)),
                );
            }
            city.features.push(Feature {
                source: element.label(),
                kind,
                density,
                building,
                house,
                inferred_house: element.tag("building") == "yes" && house,
                outers,
                holes,
            });
        }
    }
    city.features.sort_by(|a, b| a.source.cmp(&b.source));
    city.forests.sort_by(|a, b| a.source.cmp(&b.source));
    let site_index = BoundsIndex::new(area_sites.iter().map(|entry| {
        let bounds = points_bounds(entry.1.iter().flatten().copied());
        (bounds, entry)
    }));
    let mut work = PolygonWork::new("amenity polygon matching");
    'points: for site in point_sites {
        for (area, outer, holes) in site_index.query(point_bounds(site.position)).copied() {
            work.charge(1)?;
            if area.kind == site.kind && work.contains(outer, holes, site.position)? {
                continue 'points;
            }
        }
        city.sites.push(site);
    }
    drop(site_index);
    city.sites
        .extend(area_sites.into_iter().map(|(site, _, _)| site));
    city.sites.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(())
}

fn make_site(element: &Element, kind: &'static str, position: Vec2) -> Site {
    Site {
        source: element.label(),
        name: element.tag("name").into(),
        kind,
        position,
    }
}

fn zone_class(element: &Element) -> Option<(ZoneKind, Density, bool, bool)> {
    use ZoneKind::*;
    let building = element.tag("building");
    let levels = element.tag("building:levels").parse::<u32>().unwrap_or(0);
    let (kind, density, house) = match building {
        "apartments" if levels >= 3 => (Residential, Density::High, true),
        "apartments" | "terrace" | "terraced_house" => (Residential, Density::Medium, true),
        "house" | "detached" | "semidetached_house" | "bungalow" | "residential" => {
            let density = match levels {
                0 | 1 => Density::Low,
                2 => Density::Medium,
                _ => Density::High,
            };
            (Residential, density, true)
        }
        "retail" | "commercial" | "supermarket" | "hotel" => (Commercial, Density::Low, false),
        "industrial" | "warehouse" | "manufacture" => (Industrial, Density::Low, false),
        "office" => (Office, Density::Low, false),
        "" => {
            let kind = match element.tag("landuse") {
                "residential" => Residential,
                "retail" | "commercial" => Commercial,
                "industrial" => Industrial,
                _ => return None,
            };
            return Some((kind, Density::Low, false, false));
        }
        _ if !element.tag("office").is_empty() => (Office, Density::Low, false),
        _ if !element.tag("shop").is_empty() => (Commercial, Density::Low, false),
        "yes" if can_infer_home(element) => (Residential, Density::Low, true),
        _ => return None,
    };
    Some((kind, density, true, house))
}

fn can_infer_home(element: &Element) -> bool {
    matches!(
        element.tag("building:use"),
        "" | "residential" | "house" | "apartments"
    ) && matches!(element.tag("building:part"), "" | "no")
        && [
            "amenity",
            "craft",
            "emergency",
            "garage",
            "industrial",
            "leisure",
            "man_made",
            "power",
            "public_transport",
            "shed",
            "tourism",
        ]
        .into_iter()
        .all(|key| matches!(element.tag(key), "" | "no"))
}

fn rings(element: &Element, bounds: GeoBounds) -> Option<Rings> {
    let project = |ring: Vec<Coordinate>| ring.into_iter().map(|c| bounds.point(c)).collect();
    match element.kind {
        ElementKind::Way
            if element.nodes.first() == element.nodes.last() && element.geometry.len() >= 4 =>
        {
            Some((vec![project(element.geometry.clone())], Vec::new()))
        }
        ElementKind::Relation if element.tag("type") == "multipolygon" => {
            let assemble = |role| {
                let mut segments: Vec<_> = element
                    .members
                    .iter()
                    .filter(|member| {
                        member.kind == ElementKind::Way
                            && (member.role == role || (role == "outer" && member.role.is_empty()))
                    })
                    .collect();
                segments.sort_by_key(|member| member.id);
                let key = |p: Coordinate| (ordered_coordinate(p.lat), ordered_coordinate(p.lon));
                let mut endpoints = BTreeMap::<_, BTreeSet<_>>::new();
                for (i, member) in segments.iter().enumerate() {
                    if member.geometry.len() < 2 {
                        return None;
                    }
                    for &p in [member.geometry.first()?, member.geometry.last()?] {
                        endpoints.entry(key(p)).or_default().insert(i);
                    }
                }
                let mut unused: BTreeSet<_> = (0..segments.len()).collect();
                let take = |i: usize,
                            unused: &mut BTreeSet<_>,
                            endpoints: &mut BTreeMap<_, BTreeSet<_>>| {
                    let segment = &segments[i].geometry;
                    unused.remove(&i);
                    for &p in [segment.first().unwrap(), segment.last().unwrap()] {
                        endpoints.get_mut(&key(p)).unwrap().remove(&i);
                    }
                    segment
                };
                let mut result = Vec::new();
                while let Some(&first) = unused.first() {
                    let mut ring = take(first, &mut unused, &mut endpoints).clone();
                    while ring.first() != ring.last() {
                        let point = *ring.last()?;
                        let index = *endpoints.get(&key(point))?.first()?;
                        let segment = take(index, &mut unused, &mut endpoints);
                        if segment.first() == Some(&point) {
                            ring.extend(segment.iter().skip(1).copied());
                        } else {
                            ring.extend(segment.iter().rev().skip(1).copied());
                        }
                    }
                    if ring.len() < 4 {
                        return None;
                    }
                    result.push(project(ring));
                }
                Some(result)
            };
            let outers = assemble("outer")?;
            (!outers.is_empty()).then_some((outers, assemble("inner")?))
        }
        _ => None,
    }
}

pub(super) fn ring_area(ring: &[Vec2]) -> f32 {
    let origin = ring[0];
    ring.windows(2)
        .map(|pair| (pair[0] - origin).perp_dot(pair[1] - origin))
        .sum::<f32>()
        * 0.5
}

pub(super) fn ring_center(ring: &[Vec2]) -> Vec2 {
    let origin = ring[0];
    let sum = ring
        .windows(2)
        .map(|pair| {
            let a = pair[0] - origin;
            let b = pair[1] - origin;
            (a + b) * a.perp_dot(b)
        })
        .fold(Vec2::ZERO, |a, b| a + b);
    let area = ring_area(ring);
    if area.abs() < 0.01 {
        origin
    } else {
        origin + sum / (6.0 * area)
    }
}
