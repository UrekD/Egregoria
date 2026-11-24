use super::source::{serialize_optional_point, serialize_points, TownConfig};
use geom::{vec2, Spline, Vec2, Vec3};
use serde::Serialize;
use simulation::map::{
    IntersectionID, LaneID, LaneKind, LanePattern, LanePatternBuilder, LightPolicy, Map,
    MapProject, Road, RoadID, RoadSegmentKind, TurnPolicy,
};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::Simulation;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum SourceKey {
    Node { id: u64, rail: bool },
    Cut { way: u64, segment: u32, side: u8 },
    Way { id: u64, span: u32 },
    Authored(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum RoadClass {
    Walk,
    Local,
    Collector,
    Avenue,
    Ramp,
    Motorway,
    Rail,
}

impl RoadClass {
    pub fn driving(self) -> bool {
        !matches!(self, Self::Rail | Self::Walk)
    }

    pub fn pattern(self, one_way: bool, speed_adjustment: f32) -> LanePattern {
        let (lanes, speed, sidewalks, parking) = match self {
            Self::Walk => (0, 4.0, true, false),
            Self::Local => (1, 9.0, true, true),
            Self::Collector => (1, 11.1, true, false),
            Self::Avenue => (2, 13.9, true, false),
            Self::Ramp => (1, 13.9, true, false),
            Self::Motorway => (2, 25.0, false, false),
            Self::Rail => (1, 25.0, false, false),
        };
        LanePatternBuilder::new()
            .n_lanes(lanes)
            .speed_limit(speed + speed_adjustment)
            .sidewalks(sidewalks)
            .parking(parking)
            .one_way(one_way)
            .rail(self == Self::Rail)
            .build()
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SpacingLimits {
    pub junction: f32,
    pub median_driving: f32,
    pub local_lane: f32,
    pub collector_lane: f32,
    pub arterial_lane: f32,
    pub rail_lane: f32,
    pub alignment: f32,
    pub max_cluster: f32,
}

impl Default for SpacingLimits {
    fn default() -> Self {
        Self {
            junction: 40.0,
            median_driving: 100.0,
            local_lane: 65.0,
            collector_lane: 90.0,
            arterial_lane: 110.0,
            rail_lane: 110.0,
            alignment: 8.0,
            max_cluster: 80.0,
        }
    }
}

impl SpacingLimits {
    pub(crate) fn validate(self) -> Result<(), String> {
        if [
            self.junction,
            self.median_driving,
            self.local_lane,
            self.collector_lane,
            self.arterial_lane,
            self.rail_lane,
            self.alignment,
            self.max_cluster,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
            && self.max_cluster >= self.junction
        {
            Ok(())
        } else {
            Err("invalid CITYMAP spacing limits".into())
        }
    }

    pub fn lane(self, class: RoadClass) -> f32 {
        match class {
            RoadClass::Walk => 0.0,
            RoadClass::Local => self.local_lane,
            RoadClass::Collector | RoadClass::Ramp => self.collector_lane,
            RoadClass::Avenue | RoadClass::Motorway => self.arterial_lane,
            RoadClass::Rail => self.rail_lane,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct JunctionSpec {
    pub key: SourceKey,
    pub position: Vec3,
    pub required: bool,
    pub light: LightPolicy,
    pub turn: TurnPolicy,
    #[serde(skip)]
    members: Vec<Vec2>,
}

impl JunctionSpec {
    pub fn new(key: SourceKey, point: Vec2) -> Self {
        Self {
            key,
            position: point.z(10.0),
            required: matches!(key, SourceKey::Authored(_)),
            light: LightPolicy::NoLights,
            turn: TurnPolicy {
                back_turns: false,
                left_turns: true,
                crosswalks: true,
                roundabout: None,
            },
            members: vec![point],
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LinkSpec {
    pub key: SourceKey,
    pub endpoints: [SourceKey; 2],
    pub class: RoadClass,
    pub layer: i8,
    pub one_way: bool,
    pub roundabout: bool,
    pub named: bool,
    pub lineage: BTreeSet<u64>,
    #[serde(serialize_with = "serialize_points")]
    pub samples: Vec<Vec2>,
    #[serde(serialize_with = "serialize_points")]
    pub points: Vec<Vec2>,
    #[serde(serialize_with = "serialize_optional_point")]
    pub elbow: Option<Vec2>,
    pub realigned: bool,
    pub inherited: Vec<SourceKey>,
}

impl LinkSpec {
    pub fn frontage(&self) -> bool {
        self.layer == 0
            && matches!(
                self.class,
                RoadClass::Local | RoadClass::Collector | RoadClass::Avenue
            )
    }

    fn direction_at(&self, end: SourceKey) -> Vec2 {
        let [a, b] = [self.points[0], *self.points.last().unwrap()];
        if end == self.endpoints[0] {
            (self.elbow.unwrap_or(b) - a).normalize()
        } else {
            (self.elbow.unwrap_or(a) - b).normalize()
        }
    }

    fn reversed(mut self) -> Self {
        self.endpoints.reverse();
        self.points.reverse();
        self.samples.reverse();
        self
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub reason: &'static str,
    pub keys: Vec<SourceKey>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Graph {
    #[serde(serialize_with = "values")]
    pub junctions: BTreeMap<SourceKey, JunctionSpec>,
    #[serde(serialize_with = "values")]
    pub links: BTreeMap<SourceKey, LinkSpec>,
    pub decisions: Vec<Decision>,
}

fn values<S: serde::Serializer, T: Serialize>(
    map: &BTreeMap<SourceKey, T>,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.collect_seq(map.values())
}

impl Graph {
    pub(crate) fn record(&mut self, reason: &'static str, keys: &[SourceKey]) {
        self.decisions.push(Decision {
            reason,
            keys: keys.to_vec(),
        });
    }

    fn adjacency(&self) -> BTreeMap<SourceKey, Vec<SourceKey>> {
        let mut result = BTreeMap::<_, Vec<_>>::new();
        for link in self.links.values() {
            for endpoint in link.endpoints {
                result.entry(endpoint).or_default().push(link.key);
            }
        }
        result
    }

    fn remove_isolated(&mut self) {
        let adjacency = self.adjacency();
        self.junctions.retain(|key, _| adjacency.contains_key(key));
    }

    fn neighbours(&self, key: SourceKey) -> impl Iterator<Item = &LinkSpec> {
        self.links
            .values()
            .filter(move |link| link.endpoints.contains(&key))
    }

    fn priority(&self, key: SourceKey) -> (RoadClass, usize, SourceKey) {
        let class = self
            .neighbours(key)
            .map(|link| link.class)
            .max()
            .unwrap_or(RoadClass::Walk);
        (class, self.neighbours(key).count(), key)
    }
}

pub(crate) fn arc(points: &[Vec2]) -> f32 {
    points
        .windows(2)
        .map(|pair| pair[0].distance(pair[1]))
        .sum()
}

pub(crate) fn project_on_segment(point: Vec2, a: Vec2, b: Vec2) -> Vec2 {
    let line = b - a;
    let squared = line.mag2();
    if squared == 0.0 {
        return a;
    }
    a + line * ((point - a).dot(line) / squared).clamp(0.0, 1.0)
}

pub(crate) fn close_pairs<K: Copy + Ord>(
    nodes: impl IntoIterator<Item = (K, Vec2)>,
    floor: f32,
) -> Vec<(f32, K, K)> {
    let mut grid = BTreeMap::<(i32, i32), Vec<(K, Vec2)>>::new();
    let mut pairs = Vec::new();
    for (key, point) in nodes {
        let cell = (
            (point.x / floor).floor() as i32,
            (point.y / floor).floor() as i32,
        );
        for x in cell.0 - 1..=cell.0 + 1 {
            for y in cell.1 - 1..=cell.1 + 1 {
                for &(other, pos) in grid.get(&(x, y)).into_iter().flatten() {
                    let length = point.distance(pos);
                    if length < floor {
                        pairs.push((length, key.min(other), key.max(other)));
                    }
                }
            }
        }
        grid.entry(cell).or_default().push((key, point));
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    pairs
}

fn ordered([a, b]: [SourceKey; 2]) -> [SourceKey; 2] {
    [a.min(b), a.max(b)]
}

/// Remap endpoints in a candidate so a failed merge never changes the working graph.
fn contract(
    graph: &mut Graph,
    keep: SourceKey,
    retire: SourceKey,
    limit: f32,
    roundabout: bool,
) -> bool {
    let anchor = &graph.junctions[&keep];
    let member = &graph.junctions[&retire];
    if member.required
        || anchor.members.iter().chain(&member.members).any(|a| {
            anchor
                .members
                .iter()
                .chain(&member.members)
                .any(|b| a.distance(*b) > limit)
        })
    {
        return false;
    }
    let incident: Vec<_> = graph
        .links
        .values()
        .filter(|link| link.endpoints.contains(&keep) || link.endpoints.contains(&retire))
        .collect();
    let Some(bridge) = incident
        .iter()
        .find(|link| link.endpoints.contains(&keep) && link.endpoints.contains(&retire))
    else {
        return false;
    };
    if incident.iter().any(|link| {
        link.layer != bridge.layer
            || (link.class == RoadClass::Rail) != (bridge.class == RoadClass::Rail)
            || ((link.one_way || link.roundabout) && !(roundabout && link.roundabout))
    }) {
        return false;
    }
    let mut candidate = graph.clone();
    let mut pairs = BTreeSet::new();
    for link in &incident {
        let edited = candidate.links.get_mut(&link.key).unwrap();
        let [a, b] = &mut edited.endpoints;
        if *a == retire {
            *a = keep;
        }
        if *b == retire {
            *b = keep;
        }
        if *a == *b {
            if !roundabout && link.roundabout {
                return false;
            }
            candidate.links.remove(&link.key);
            continue;
        }
        if !pairs.insert(ordered(edited.endpoints)) {
            return false;
        }
        if link.endpoints.contains(&retire) {
            let index = if link.endpoints[0] == retire {
                0
            } else {
                edited.points.len() - 1
            };
            edited.points[index] = anchor.position.xy();
            edited.realigned = true;
        }
    }
    let retired = candidate.junctions.remove(&retire).unwrap();
    candidate
        .junctions
        .get_mut(&keep)
        .unwrap()
        .members
        .extend(retired.members);
    candidate.record(
        if roundabout {
            "roundabout throat consolidated to priority junction"
        } else {
            "short same-layer connector contracted"
        },
        &[keep, retire, bridge.key],
    );
    *graph = candidate;
    true
}

fn collapse_roundabouts(graph: &mut Graph) {
    let mut pending: Vec<_> = graph
        .links
        .values()
        .filter(|link| link.roundabout)
        .map(|link| link.key)
        .collect();
    loop {
        let size = pending.len();
        pending.retain(|key| {
            let Some(link) = graph.links.get(key) else {
                return false;
            };
            let [a, b] = link.endpoints;
            if a == b {
                graph.links.remove(key);
                graph.record(
                    "residual roundabout loop removed after throat consolidation",
                    &[*key],
                );
                return false;
            }
            !contract(graph, a.min(b), a.max(b), 175.0, true)
        });
        if pending.len() == size {
            break;
        }
    }
    // Any still-separated throats are subject to the same geometry and storage checks as other roads.
}

fn inset_cuts(graph: &mut Graph, config: TownConfig) {
    for link in graph.links.values_mut() {
        let last = link.points.len() - 1;
        for (key, index, next) in [
            (link.endpoints[0], 0, 1),
            (link.endpoints[1], last, last - 1),
        ] {
            if !matches!(key, SourceKey::Cut { .. }) {
                continue;
            }
            let point = link.points[index];
            let margin = vec2(32.0, 32.0);
            if point
                == point
                    .max(config.owned_min + margin)
                    .min(config.owned_max - margin)
            {
                continue;
            }
            let inward = (link.points[next] - point).normalize();
            let inset = project_on_segment(point + inward * 32.0, point, link.points[next]);
            let safe = inset
                .max(config.owned_min + margin)
                .min(config.owned_max - margin);
            graph.junctions.get_mut(&key).unwrap().position = safe.z(10.0);
            link.points[index] = safe;
            link.realigned |= safe.distance(point) > 32.1;
        }
    }
}

fn deviation(a: &[Vec2], b: &[Vec2]) -> f32 {
    let directed = |from: &[Vec2], to: &[Vec2]| {
        from.windows(2)
            .flat_map(|edge| {
                let count = (edge[0].distance(edge[1]) / 8.0).ceil().max(1.0) as usize;
                (0..=count)
                    .map(move |step| edge[0] + (edge[1] - edge[0]) * (step as f32 / count as f32))
            })
            .map(|p| {
                to.windows(2)
                    .map(|edge| p.distance(project_on_segment(p, edge[0], edge[1])))
                    .fold(f32::INFINITY, f32::min)
            })
            .fold(0.0, f32::max)
    };
    directed(a, b).max(directed(b, a))
}

fn curve(from: Vec2, to: Vec2, elbow: Vec2) -> Spline {
    let RoadSegmentKind::Curved((from_derivative, to_derivative)) =
        RoadSegmentKind::from_elbow(from, to, elbow)
    else {
        unreachable!()
    };
    Spline {
        from,
        to,
        from_derivative,
        to_derivative,
    }
}

fn fit(points: &[Vec2], envelope: f32) -> (Option<Vec2>, Vec<Vec2>, f32) {
    let a = points[0];
    let b = *points.last().unwrap();
    let mut best = (None, vec![a, b], deviation(points, &[a, b]));
    if best.2 <= envelope.min(2.0) {
        return best;
    }
    let ab = b - a;
    if ab.mag2() < 1.0 {
        return best;
    }
    // Least squares for the single elbow of the engine's cubic. Refine against
    // geometric deviation because the source is sampled by distance, not cubic t.
    let mut sum = Vec2::ZERO;
    let mut weights = 0.0;
    for &p in &points[1..points.len() - 1] {
        let t = ((p - a).dot(ab) / ab.mag2()).clamp(0.01, 0.99);
        let base = curve(a, b, Vec2::ZERO).get(t);
        let weight = (curve(a, b, Vec2::X).get(t) - base).x;
        sum += (p - base) * weight;
        weights += weight * weight;
    }
    if weights <= 0.0 {
        return best;
    }
    let midpoint = (a + b) * 0.5;
    let mut center = sum / weights;
    for radius in [0.0, ab.mag() * 0.25, ab.mag() * 0.08, ab.mag() * 0.025] {
        for offset in [Vec2::ZERO, Vec2::X, -Vec2::X, Vec2::Y, -Vec2::Y] {
            let elbow = center + offset * radius;
            if elbow.distance(midpoint) > ab.mag() * 1.3
                || (elbow - a).dot(ab) < 0.0
                || (b - elbow).dot(ab) < 0.0
            {
                continue;
            }
            let spline = curve(a, b, elbow);
            let trial: Vec<_> = spline.points(25).collect();
            let error = deviation(points, &trial);
            if error < best.2 {
                best = (Some(elbow), trial, error);
            }
        }
        center = best.0.unwrap_or(center);
    }
    best
}

fn join_seams(graph: &mut Graph, envelope: f32) {
    loop {
        let adjacency = graph.adjacency();
        let joined = adjacency.iter().find_map(|(&via, keys)| {
            let &[a, b] = keys.as_slice() else {
                return None;
            };
            if graph.junctions[&via].required {
                return None;
            }
            let (mut a, mut b) = (graph.links[&a].clone(), graph.links[&b].clone());
            if a.class != b.class
                || a.layer != b.layer
                || a.one_way != b.one_way
                || a.roundabout
                || b.roundabout
            {
                return None;
            }
            if a.one_way && a.endpoints[0] == via && b.endpoints[1] == via {
                std::mem::swap(&mut a, &mut b);
            }
            if a.one_way && (a.endpoints[1] != via || b.endpoints[0] != via) {
                return None;
            }
            if a.endpoints[0] == via {
                a = a.reversed();
            }
            if b.endpoints[1] == via {
                b = b.reversed();
            }
            if a.endpoints[0] == b.endpoints[1]
                || graph
                    .neighbours(a.endpoints[0])
                    .any(|link| link.endpoints.contains(&b.endpoints[1]))
            {
                return None;
            }
            let points: Vec<_> = a
                .points
                .iter()
                .copied()
                .chain(b.points.iter().copied().skip(1))
                .collect();
            let fits = fit(&points, envelope).2 <= envelope;
            fits.then_some((via, a, b, points))
        });
        let Some((via, mut a, b, points)) = joined else {
            break;
        };
        graph.links.remove(&b.key);
        graph.junctions.remove(&via);
        a.endpoints[1] = b.endpoints[1];
        a.points = points;
        a.samples.extend(b.samples.into_iter().skip(1));
        a.inherited.push(b.key);
        a.inherited.extend(b.inherited);
        a.lineage.extend(b.lineage);
        a.realigned |= b.realigned;
        a.named &= b.named;
        graph.links.insert(a.key, a);
    }
}

/// Trim isolated optional components while retaining the rail and motorway and the main connected town.
fn main_components(graph: &mut Graph) {
    let adjacency = graph.adjacency();
    let mut visited = BTreeSet::new();
    let mut components = Vec::<BTreeSet<SourceKey>>::new();
    for &key in graph.junctions.keys() {
        if !visited.insert(key) {
            continue;
        }
        let mut component = BTreeSet::new();
        let mut pending = vec![key];
        while let Some(endpoint) = pending.pop() {
            for &link in adjacency.get(&endpoint).into_iter().flatten() {
                component.insert(link);
                pending.extend(
                    graph.links[&link]
                        .endpoints
                        .into_iter()
                        .filter(|&end| visited.insert(end)),
                );
            }
        }
        components.push(component);
    }
    let main = components
        .iter()
        .max_by_key(|component| {
            component
                .iter()
                .filter(|key| {
                    let class = graph.links[key].class;
                    class.driving() && class < RoadClass::Motorway
                })
                .count()
        })
        .cloned()
        .unwrap_or_default();
    let removed: Vec<_> = components
        .into_iter()
        .filter(|component| {
            !component
                .iter()
                .any(|key| main.contains(key) || graph.links[key].class >= RoadClass::Motorway)
        })
        .flatten()
        .collect();
    if !removed.is_empty() {
        graph.record(
            "disconnected source fragment without approved approach",
            &removed,
        );
    }
    for key in removed {
        graph.links.remove(&key);
    }
    graph.remove_isolated();
}

fn remove_link(graph: &mut Graph, key: SourceKey, reason: &'static str) {
    graph.links.remove(&key);
    graph.remove_isolated();
    graph.record(reason, &[key]);
}

fn trim_short_links(graph: &mut Graph, limits: SpacingLimits) {
    loop {
        let mut short: Vec<_> = graph
            .links
            .values()
            .filter_map(|link| {
                if link.class >= RoadClass::Ramp {
                    return None;
                }
                let reserve = match link.class {
                    RoadClass::Walk => 12.0,
                    RoadClass::Local => 38.0,
                    RoadClass::Collector => 36.0,
                    _ => 43.0,
                };
                let margin = arc(&link.points) - limits.lane(link.class) - reserve;
                (margin < 0.0).then_some((margin, link.key))
            })
            .collect();
        short.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let Some(&(_, key)) = short.first() else {
            break;
        };
        let link = &graph.links[&key];
        let [a, b] = link.endpoints;
        let (keep, retire) = if graph.priority(a) >= graph.priority(b) {
            (a, b)
        } else {
            (b, a)
        };
        let contraction = graph.junctions[&a]
            .position
            .distance(graph.junctions[&b].position)
            < limits.max_cluster
            && contract(graph, keep, retire, limits.max_cluster, false);
        if !contraction {
            remove_link(
                graph,
                key,
                "short approach removed for CM0 storage; component and corridors rechecked",
            );
        }
        join_seams(graph, limits.alignment);
    }
}

fn clear_close_nodes(graph: &mut Graph, limits: SpacingLimits) -> Result<(), String> {
    loop {
        let pairs = close_pairs(
            graph.junctions.values().map(|j| (j.key, j.position.xy())),
            limits.junction,
        );
        let Some(&(dist, a, b)) = pairs.first() else {
            return Ok(());
        };
        let (keep, retire) = if (graph.junctions[&a].required, graph.priority(a))
            >= (graph.junctions[&b].required, graph.priority(b))
        {
            (a, b)
        } else {
            (b, a)
        };
        if contract(graph, keep, retire, limits.max_cluster, false) {
            join_seams(graph, limits.alignment);
            continue;
        }
        if graph.junctions[&retire].required || graph.priority(retire).0 >= RoadClass::Ramp {
            return Err(format!(
                "unresolved junction pair {keep:?}/{retire:?}: {dist:.2} m"
            ));
        }
        let incident: Vec<_> = graph.neighbours(retire).map(|link| link.key).collect();
        for key in incident {
            remove_link(
                graph,
                key,
                "optional close approach retired without a mode, grade or direction merge",
            );
        }
        join_seams(graph, limits.alignment);
    }
}

fn fit_links(graph: &mut Graph, limits: SpacingLimits) {
    let mut realigned = Vec::new();
    for link in graph.links.values_mut() {
        if let Some(elbow) = link.elbow {
            link.points = curve(link.points[0], *link.points.last().unwrap(), elbow)
                .points(25)
                .collect();
            continue;
        }
        let (elbow, points, error) = fit(&link.points, limits.alignment);
        if error > limits.alignment {
            realigned.push(link.key);
            link.realigned = true;
        }
        link.elbow = elbow;
        link.points = points;
    }
    if !realigned.is_empty() {
        graph.record(
            "source reach realigned to reproducible engine spline",
            &realigned,
        );
    }
}

fn segment_parameters(a: Vec2, b: Vec2, c: Vec2, d: Vec2) -> Option<(f32, f32)> {
    let cross = |a: Vec2, b: Vec2| a.x * b.y - a.y * b.x;
    let determinant = cross(b - a, d - c);
    if determinant.abs() <= 1e-5 {
        return None;
    }
    let t = cross(c - a, d - c) / determinant;
    let u = cross(c - a, b - a) / determinant;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)).then_some((t, u))
}

fn overlap(a: &[Vec3], b: &[Vec3], radius: f32) -> bool {
    for x in a.windows(2) {
        for y in b.windows(2) {
            if let Some((t, u)) = segment_parameters(x[0].xy(), x[1].xy(), y[0].xy(), y[1].xy()) {
                let z1 = x[0].z + t * (x[1].z - x[0].z);
                let z2 = y[0].z + u * (y[1].z - y[0].z);
                if (z1 - z2).abs() < 5.0 {
                    return true;
                }
            }
            for ([p, q], line) in [([x[0], x[1]], y), ([y[0], y[1]], x)] {
                for point in [p, q] {
                    let nearest = project_on_segment(point.xy(), line[0].xy(), line[1].xy());
                    if nearest.distance(point.xy()) >= radius {
                        continue;
                    }
                    let length = line[0].xy().distance(line[1].xy()).max(0.001);
                    let z = line[0].z
                        + (line[1].z - line[0].z) * line[0].xy().distance(nearest) / length;
                    if (point.z - z).abs() < 5.0 {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn too_acute(a: Vec2, b: Vec2) -> bool {
    a.dot(b) > 0.866
}

fn add_rim_access(graph: &mut Graph, config: TownConfig) -> Result<(), String> {
    let target = super::layout::design_point(vec2(2820.0, 2520.0), config);
    let rim = graph
        .links
        .values()
        .filter(|link| link.class == RoadClass::Motorway)
        .find(|link| link.lineage.contains(&61233481))
        .ok_or("missing A2 southeast approach")?;
    let rim_key = rim.key;
    let rim_end = rim.endpoints[1];
    let (index, aim) = rim
        .points
        .windows(2)
        .enumerate()
        .map(|(index, pair)| (index, project_on_segment(target, pair[0], pair[1])))
        .min_by(|(_, a), (_, b)| a.distance2(target).total_cmp(&b.distance2(target)))
        .unwrap();
    let mut first = rim.points[..=index].to_vec();
    first.push(aim);
    let mut second = vec![aim];
    second.extend_from_slice(&rim.points[index + 1..]);
    first.dedup();
    second.dedup();
    if arc(&first) < 160.0 || arc(&second) < 160.0 {
        return Err(format!(
            "A2 ramp split has insufficient approach length: {:.1}/{:.1} m",
            arc(&first),
            arc(&second)
        ));
    }
    let h = &graph.junctions[&SourceKey::Node {
        id: 11950373997,
        rail: false,
    }];
    if h.position.xy().distance(aim) < 150.0 {
        return Err("Hraška approach is too close to the A2 ramp".into());
    }
    let h_key = h.key;
    let start = h.position.xy();
    let access = SourceKey::Authored("a2-town-access");
    let new_rim = SourceKey::Authored("a2-southeast-span");
    let ramp = SourceKey::Authored("a2-hraska-ramp");
    graph
        .junctions
        .insert(access, JunctionSpec::new(access, aim));
    let previous = graph.links.get_mut(&rim_key).unwrap();
    let mut next = previous.clone();
    previous.endpoints[1] = access;
    previous.samples = first.clone();
    previous.points = first;
    next.key = new_rim;
    next.endpoints = [access, rim_end];
    next.samples = second.clone();
    next.points = second;
    graph.links.insert(new_rim, next);
    let bend = (start + aim) * 0.5 + vec2(0.0, 43.0);
    graph.links.insert(
        ramp,
        LinkSpec {
            key: ramp,
            endpoints: [h_key, access],
            class: RoadClass::Ramp,
            layer: 0,
            one_way: false,
            roundabout: false,
            named: false,
            lineage: BTreeSet::new(),
            samples: Vec::new(),
            points: vec![start, aim],
            elbow: Some(bend),
            realigned: false,
            inherited: Vec::new(),
        },
    );
    graph.junctions.get_mut(&h_key).unwrap().required = true;
    graph.record(
        "two-way ramp replaces access within the clipped A2 interchange",
        &[rim_key, new_rim, ramp, h_key],
    );
    Ok(())
}

fn grade_and_controls(graph: &mut Graph) {
    // The dry rail reservation uses a continuous deck through the road corridor.
    // A later freight profile must grade back down across the market approaches.
    for link in graph
        .links
        .values()
        .filter(|link| matches!(link.class, RoadClass::Rail | RoadClass::Motorway))
    {
        for key in link.endpoints {
            graph.junctions.get_mut(&key).unwrap().position.z = 18.0;
        }
    }
    if let Some(junction) = graph.junctions.get_mut(&SourceKey::Node {
        id: 11950373997,
        rail: false,
    }) {
        // The normal preview uses end-to-end deck grading when its low end
        // clears flat terrain by 0.5 m.
        junction.position.z = 10.6;
    }
    let adjacency = graph.adjacency();
    for (key, links) in adjacency {
        let driving: Vec<_> = links
            .iter()
            .map(|id| &graph.links[id])
            .filter(|link| link.class.driving())
            .collect();
        let node = graph.junctions.get_mut(&key).unwrap();
        node.turn.back_turns = driving.len() == 1;
        node.turn.crosswalks = !driving.iter().any(|link| link.class == RoadClass::Motorway);
        let mut angles: Vec<_> = driving
            .iter()
            .map(|link| {
                let d = link.direction_at(key);
                d.y.atan2(d.x)
            })
            .collect();
        angles.sort_by(f32::total_cmp);
        let opposite = |a: f32, b: f32| (a - b).cos() < -0.9;
        node.light = if driving.len() == 4
            && driving.iter().any(|link| link.class == RoadClass::Avenue)
            && driving
                .iter()
                .all(|link| !link.one_way && link.class <= RoadClass::Avenue)
            && opposite(angles[0], angles[2])
            && opposite(angles[1], angles[3])
        {
            // Suppress permissive left turns across the opposing phase.
            node.turn.left_turns = false;
            LightPolicy::Lights
        } else {
            LightPolicy::NoLights
        };
    }
}

fn prune_geometry_conflicts(graph: &mut Graph) -> Result<(), String> {
    loop {
        let mut conflict = None;
        'scan: for (index, a) in graph.links.values().enumerate() {
            for b in graph.links.values().skip(index + 1) {
                let shared = a
                    .endpoints
                    .into_iter()
                    .find(|key| b.endpoints.contains(key));
                let invalid = if let Some(shared) = shared {
                    ordered(a.endpoints) == ordered(b.endpoints)
                        || too_acute(a.direction_at(shared), b.direction_at(shared))
                } else {
                    let as_profile = |link: &LinkSpec| {
                        let za = graph.junctions[&link.endpoints[0]].position.z;
                        let zb = graph.junctions[&link.endpoints[1]].position.z;
                        let length = arc(&link.points);
                        let mut distance = 0.0;
                        link.points
                            .iter()
                            .enumerate()
                            .map(|(i, &point)| {
                                if i > 0 {
                                    distance += link.points[i - 1].distance(point);
                                }
                                point.z(za + (zb - za) * distance / length)
                            })
                            .collect::<Vec<_>>()
                    };
                    let width = (a.class.pattern(a.one_way, 0.0).width()
                        + b.class.pattern(b.one_way, 0.0).width())
                        * 0.5;
                    overlap(&as_profile(a), &as_profile(b), width)
                };
                if invalid {
                    let rank = |link: &LinkSpec| {
                        (
                            matches!(link.key, SourceKey::Authored(_))
                                || link.class >= RoadClass::Ramp,
                            link.class,
                            link.named,
                            arc(&link.points),
                        )
                    };
                    let (keep, drop) = if rank(a) >= rank(b) { (a, b) } else { (b, a) };
                    if matches!(drop.key, SourceKey::Authored(_)) || drop.class >= RoadClass::Ramp {
                        return Err(format!(
                            "protected geometry conflict {:?}/{:?} at {shared:?}: {:?}",
                            keep.key,
                            drop.key,
                            shared.map(|key| keep.direction_at(key).dot(drop.direction_at(key)))
                        ));
                    }
                    conflict = Some(drop.key);
                    break 'scan;
                }
            }
        }
        let Some(key) = conflict else { return Ok(()) };
        remove_link(
            graph,
            key,
            "approach retired for swept clearance, duplicate alignment or acute turning angle",
        );
    }
}

fn restore_avenues(graph: &mut Graph, source: &Graph) -> Result<(), String> {
    let node = |id| SourceKey::Node { id, rail: false };
    let west = node(289194013);
    let middle = node(11950373997);
    let east_cut = SourceKey::Cut {
        way: 61233484,
        segment: 1,
        side: 0,
    };
    let north_local = node(4193639635);
    let specs = [
        ("hraska-west", west, middle, &[663442009, 26398038][..]),
        (
            "hraska-east",
            middle,
            east_cut,
            &[1288727380, 27690361, 61233484][..],
        ),
        ("hraska-neighbourhood-access", middle, north_local, &[][..]),
        (
            "alpska-rail-underpass",
            node(862116385),
            node(330169744),
            &[1230075924][..],
        ),
    ];
    let alignment: BTreeSet<_> = specs
        .iter()
        .flat_map(|spec| spec.3.iter().copied())
        .collect();
    let superseded: Vec<_> = graph
        .links
        .values()
        .filter(|link| link.lineage.iter().any(|id| alignment.contains(id)))
        .map(|link| link.key)
        .collect();
    for key in superseded {
        remove_link(graph, key, "merged into the keyed Hraška / 452 alignment");
    }
    for key in specs.iter().flat_map(|spec| [spec.1, spec.2]) {
        let junction = source
            .junctions
            .get(&key)
            .ok_or_else(|| format!("missing avenue alignment anchor {key:?}"))?
            .clone();
        graph.junctions.entry(key).or_insert(junction).required = true;
    }
    for (name, start, end, ways) in specs {
        let key = SourceKey::Authored(name);
        let from = graph.junctions[&start].position.xy();
        let to = graph.junctions[&end].position.xy();
        let mut samples = vec![from, to];
        for link in source
            .links
            .values()
            .filter(|link| link.lineage.iter().any(|id| ways.contains(id)))
        {
            samples.extend(link.samples.iter().copied());
        }
        samples.sort_by(|a, b| {
            (*a - from)
                .dot(to - from)
                .total_cmp(&(*b - from).dot(to - from))
        });
        samples.dedup_by(|a, b| a.distance(*b) < 2.0);
        let mut points = samples.clone();
        points[0] = from;
        *points.last_mut().unwrap() = to;
        graph.links.insert(
            key,
            LinkSpec {
                key,
                endpoints: [start, end],
                class: if ways.is_empty() {
                    RoadClass::Collector
                } else {
                    RoadClass::Avenue
                },
                layer: 0,
                one_way: false,
                roundabout: false,
                named: !ways.is_empty(),
                lineage: ways.iter().copied().collect(),
                samples,
                points,
                elbow: None,
                realigned: true,
                inherited: Vec::new(),
            },
        );
        let reason = match name {
            "alpska-rail-underpass" => "joined Alpska source spans through the rail corridor; no road junction below the deck",
            "hraska-neighbourhood-access" => "collector restores north neighbourhood access to Hraška",
            _ => "Hraška single carriageway replaces roundabout and splitters",
        };
        graph.record(reason, &[key, start, end]);
    }
    Ok(())
}

pub(crate) fn clean_network(
    mut graph: Graph,
    limits: SpacingLimits,
    config: TownConfig,
) -> Result<Graph, String> {
    let source = graph.clone();
    // The dry A2 is one game corridor. The mapped deck seam remains in the source view.
    for link in graph
        .links
        .values_mut()
        .filter(|link| link.class == RoadClass::Motorway)
    {
        link.layer = 1;
    }
    inset_cuts(&mut graph, config);
    collapse_roundabouts(&mut graph);
    join_seams(&mut graph, limits.alignment);
    trim_short_links(&mut graph, limits);
    clear_close_nodes(&mut graph, limits)?;
    restore_avenues(&mut graph, &source)?;
    inset_cuts(&mut graph, config);
    main_components(&mut graph);
    add_rim_access(&mut graph, config)?;
    clear_close_nodes(&mut graph, limits)?;
    fit_links(&mut graph, limits);
    grade_and_controls(&mut graph);
    prune_geometry_conflicts(&mut graph)?;
    main_components(&mut graph);
    grade_and_controls(&mut graph);
    validate_plan(&graph, limits)?;
    Ok(graph)
}

pub fn validate_plan(graph: &Graph, limits: SpacingLimits) -> Result<(), String> {
    for junction in graph.junctions.values() {
        if !junction.position.is_finite() {
            return Err(format!("non-finite junction {:?}", junction.key));
        }
    }
    if let Some((length, a, b)) = close_pairs(
        graph
            .junctions
            .values()
            .map(|node| (node.key, node.position.xy())),
        limits.junction,
    )
    .first()
    {
        return Err(format!("junctions {a:?}/{b:?} are {length:.3} m apart"));
    }
    let mut pairs = BTreeSet::new();
    for link in graph.links.values() {
        let [a, b] = link.endpoints;
        if a == b
            || !pairs.insert(ordered([a, b]))
            || !graph.junctions.contains_key(&a)
            || !graph.junctions.contains_key(&b)
            || link.points.len() < 2
            || link.points.iter().any(|point| !point.is_finite())
        {
            return Err(format!("degenerate or ambiguous link {:?}", link.key));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct BuiltNetwork {
    pub junctions: BTreeMap<SourceKey, IntersectionID>,
    pub links: BTreeMap<SourceKey, DirectedSpan>,
}

#[derive(Debug)]
pub struct DirectedSpan {
    pub road: RoadID,
    pub endpoints: [IntersectionID; 2],
    pub lanes: Vec<LaneID>,
    pub length: f32,
}

/// Apply a single normal road command batch. A rejection leaves a partial
/// candidate, so callers build in a disposable world and discard it on error.
pub fn build_network(
    sim: &mut Simulation,
    graph: &Graph,
    limits: SpacingLimits,
) -> Result<BuiltNetwork, String> {
    validate_plan(graph, limits)?;
    let projects: Vec<_> = graph
        .junctions
        .values()
        .map(|junction| MapProject::ground(junction.position))
        .collect();
    let indices: BTreeMap<_, _> = graph
        .junctions
        .keys()
        .enumerate()
        .map(|(index, &key)| (key, index))
        .collect();
    let links: Vec<_> = graph
        .links
        .values()
        .enumerate()
        .map(|(index, link)| {
            let [a, b] = link.endpoints;
            (
                indices[&a],
                indices[&b],
                link.elbow,
                pattern(graph, link, index),
            )
        })
        .collect();
    sim.apply_commands([&WorldCommand::MapMakeMultipleConnections(projects, links)]);
    {
        let outcomes = sim.read::<CommandOutcomes>();
        if !outcomes.last.is_empty() || outcomes.accepted != [0] {
            return Err(format!(
                "CITYMAP connection batch rejected {:?}; ordered links {:?}",
                outcomes.last,
                graph.links.keys().collect::<Vec<_>>()
            ));
        }
    }
    let built = resolve_network(&sim.map(), graph)?;
    let policies: Vec<_> = graph
        .junctions
        .values()
        .map(|junction| WorldCommand::MapUpdateIntersectionPolicy {
            inter: built.junctions[&junction.key],
            turn: junction.turn,
            light: junction.light,
        })
        .collect();
    sim.apply_commands(&policies);
    {
        let outcomes = sim.read::<CommandOutcomes>();
        if !outcomes.last.is_empty() || outcomes.accepted.len() != policies.len() {
            return Err(format!(
                "CITYMAP intersection policies rejected {:?}",
                outcomes.last
            ));
        }
    }
    validate_live(&sim.map(), graph, &built, limits)?;
    Ok(built)
}

fn pattern(graph: &Graph, link: &LinkSpec, index: usize) -> LanePattern {
    // A compatible shallow degree-two junction is merged by the engine.
    // Seams left in the graph by curve fitting need a distinct signed speed.
    let control = link.endpoints.into_iter().any(|key| {
        let mut incident = graph.neighbours(key);
        incident.next().is_some() && incident.next().is_some() && incident.next().is_none()
    });
    let adjustment = if control {
        (index + 1) as f32 * 0.001
    } else {
        0.0
    };
    link.class.pattern(link.one_way, adjustment)
}

pub fn resolve_network(map: &Map, graph: &Graph) -> Result<BuiltNetwork, String> {
    let mut junctions = BTreeMap::new();
    for junction in graph.junctions.values() {
        let mut candidates = map
            .intersections()
            .values()
            .filter(|live| live.pos.distance2(junction.position) < 0.01);
        let first = candidates
            .next()
            .filter(|_| candidates.next().is_none())
            .ok_or_else(|| format!("missing or ambiguous live junction {:?}", junction.key))?;
        junctions.insert(junction.key, first.id);
    }
    let mut links = BTreeMap::new();
    for (index, link) in graph.links.values().enumerate() {
        let ids = link.endpoints.map(|key| junctions[&key]);
        let mut candidates = map
            .roads()
            .values()
            .filter(|road| [road.src, road.dst] == ids);
        let road = candidates
            .next()
            .filter(|_| candidates.next().is_none())
            .ok_or_else(|| format!("missing or ambiguous live link {:?}", link.key))?;
        if road.pattern(map.lanes()) != pattern(graph, link, index) {
            return Err(format!(
                "direction or lane pattern changed for {:?}",
                link.key
            ));
        }
        links.insert(
            link.key,
            DirectedSpan {
                road: road.id,
                endpoints: ids,
                lanes: road.lanes_iter().map(|(id, _)| id).collect(),
                length: road.length(),
            },
        );
    }
    Ok(BuiltNetwork { junctions, links })
}

pub fn validate_live(
    map: &Map,
    graph: &Graph,
    built: &BuiltNetwork,
    limits: SpacingLimits,
) -> Result<(), String> {
    if let Some((length, a, b)) = close_pairs(
        map.intersections()
            .values()
            .map(|junction| (junction.id, junction.pos.xy())),
        limits.junction,
    )
    .first()
    {
        return Err(format!(
            "live junctions {a:?}/{b:?} are {length:.3} m apart"
        ));
    }
    let mut chords = Vec::new();
    let mut lane_errors = Vec::new();
    for (&key, span) in &built.links {
        let link = &graph.links[&key];
        let road = map
            .get(span.road)
            .ok_or_else(|| format!("missing live road {key:?}"))?;
        let from = graph.junctions[&link.endpoints[0]].position;
        let to = graph.junctions[&link.endpoints[1]].position;
        let segment = link.elbow.map_or(RoadSegmentKind::Straight, |elbow| {
            RoadSegmentKind::from_elbow(from.xy(), to.xy(), elbow)
        });
        let (_, error) = Road::generate_points(from, to, segment, false, &map.environment);
        if error.is_some() {
            return Err(format!("invalid live profile {key:?}"));
        }
        if link.class == RoadClass::Ramp
            && road
                .points
                .iter()
                .zip(road.points.iter().skip(1))
                .any(|(a, b)| (a.z - b.z).abs() > 0.08 * a.xy().distance(b.xy()))
        {
            return Err(format!("ramp {key:?} exceeds 8% actual grade"));
        }
        if link.class.driving() {
            chords.push(from.xy().distance(to.xy()));
        }
        for (id, kind) in road
            .lanes_iter()
            .filter(|&(_, kind)| matches!(kind, LaneKind::Driving | LaneKind::Rail))
        {
            let length = map.get(id).unwrap().points.length();
            if !length.is_finite() || length < limits.lane(link.class) {
                lane_errors.push(format!(
                    "{key:?} {kind:?} lane {id:?} is {length:.3} m; requires {:.1} m",
                    limits.lane(link.class)
                ));
            }
        }
    }
    if !lane_errors.is_empty() {
        return Err(lane_errors.join("\n"));
    }
    chords.sort_by(f32::total_cmp);
    let median = match chords.len() {
        0 => 0.0,
        count => (chords[count / 2] + chords[(count - 1) / 2]) * 0.5,
    };
    if median < limits.median_driving {
        return Err(format!(
            "median adjacent driving junction spacing {median:.3} m below {}",
            limits.median_driving
        ));
    }
    for (index, (&key, span)) in built.links.iter().enumerate() {
        for (&other, second) in built.links.iter().skip(index + 1) {
            if span
                .endpoints
                .iter()
                .any(|end| second.endpoints.contains(end))
            {
                continue;
            }
            let a = map.get(span.road).unwrap();
            let b = map.get(second.road).unwrap();
            let a_points: Vec<_> = a.points.iter().copied().collect();
            let b_points: Vec<_> = b.points.iter().copied().collect();
            if overlap(&a_points, &b_points, (a.width + b.width) * 0.5) {
                return Err(format!("live swept overlap {key:?}/{other:?}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citymap_spacing_checks_unlinked_pairs_in_stable_order() {
        let nodes = [(3, vec2(50.0, 0.0)), (2, vec2(40.0, 0.0)), (1, Vec2::ZERO)];
        assert_eq!(close_pairs(nodes, 40.0), vec![(10.0, 2, 3)]);
        assert_eq!(
            close_pairs(nodes.into_iter().rev(), 40.0),
            vec![(10.0, 2, 3)]
        );
    }

    #[test]
    fn citymap_swept_overlap_requires_vertical_clearance() {
        let east = [vec2(-10.0, 0.0).z(10.0), vec2(10.0, 0.0).z(10.0)];
        let north = [vec2(0.0, -10.0).z(10.0), vec2(0.0, 10.0).z(10.0)];
        assert!(overlap(&east, &north, 3.0));
        assert!(!overlap(
            &east,
            &north.map(|point| point + Vec3::Z * 8.0),
            3.0
        ));
    }

    #[test]
    fn citymap_fits_curvature_without_an_extra_junction() {
        let a = Vec2::ZERO;
        let b = vec2(200.0, 0.0);
        let points: Vec<_> = curve(a, b, vec2(115.0, 100.0)).points(12).collect();
        let (elbow, fitted, error) = fit(&points, 8.0);
        assert!(elbow.is_some());
        assert!(error <= 8.0, "{error}");
        assert_eq!(fitted[0], a);
        assert_eq!(*fitted.last().unwrap(), b);
    }
}

/// Probe connectivity through actual directed lanes and generated turn movements.
pub fn route_checks(
    map: &Map,
    graph: &Graph,
    built: &BuiltNetwork,
) -> BTreeMap<&'static str, bool> {
    let route = |from: IntersectionID,
                 to: IntersectionID,
                 kind: LaneKind,
                 allowed: &dyn Fn(RoadID) -> bool| {
        let mut visited = BTreeSet::new();
        let mut pending: Vec<_> = map
            .lanes()
            .values()
            .filter(|lane| lane.src == from && lane.kind == kind && allowed(lane.parent))
            .map(|lane| lane.id)
            .collect();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            let lane = map.get(id).unwrap();
            if lane.dst == to {
                return true;
            }
            let end = map.get(lane.dst).unwrap();
            pending.extend(
                end.turns()
                    .filter(|turn| turn.id.src == id)
                    .filter_map(|turn| {
                        let next = map.get(turn.id.dst)?;
                        (next.kind == kind && allowed(next.parent)).then_some(next.id)
                    }),
            );
        }
        false
    };
    let boundary = |class| {
        let endpoints: BTreeSet<_> = graph
            .links
            .values()
            .filter(|link| link.class == class)
            .flat_map(|link| link.endpoints)
            .filter(|key| matches!(key, SourceKey::Cut { .. }))
            .map(|key| built.junctions[&key])
            .collect();
        endpoints.into_iter().collect::<Vec<_>>()
    };
    let rim = boundary(RoadClass::Motorway);
    let rail = boundary(RoadClass::Rail);
    let motorway: BTreeSet<_> = graph
        .links
        .values()
        .filter(|link| link.class == RoadClass::Motorway)
        .map(|link| built.links[&link.key].road)
        .collect();
    let h = built.junctions.get(&SourceKey::Node {
        id: 11950373997,
        rail: false,
    });
    let core = built.junctions.get(&SourceKey::Node {
        id: 772450604,
        rail: false,
    });
    let rim_through = match rim.as_slice() {
        &[a, b] => {
            route(a, b, LaneKind::Driving, &|road| motorway.contains(&road))
                && route(b, a, LaneKind::Driving, &|road| motorway.contains(&road))
        }
        _ => false,
    };
    let rail_through = match rail.as_slice() {
        &[a, b] => route(a, b, LaneKind::Rail, &|_| true) && route(b, a, LaneKind::Rail, &|_| true),
        _ => false,
    };
    let h_access = h.is_some_and(|&h| {
        rim.len() == 2
            && rim.iter().all(|&gate| {
                route(gate, h, LaneKind::Driving, &|_| true)
                    && route(h, gate, LaneKind::Driving, &|_| true)
            })
    });
    let core_access = h.zip(core).is_some_and(|(&h, &core)| {
        route(h, core, LaneKind::Driving, &|_| true) && route(core, h, LaneKind::Driving, &|_| true)
    });
    BTreeMap::from([
        ("rim_through_both_directions", rim_through),
        ("mainline_rail_both_directions", rail_through),
        ("two_way_ramp_from_both_gateways", h_access),
        ("hraska_to_old_centre_both_directions", core_access),
    ])
}
