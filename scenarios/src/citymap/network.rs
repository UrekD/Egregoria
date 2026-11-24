use super::clearance::{bounds_overlap, live_conflict, segment_bounds};
use super::source::{serialize_optional_point, serialize_points, TownConfig};
use geom::{vec2, PolyLine3, Spline, Vec2, Vec3};
use serde::Serialize;
use simulation::map::{
    Environment, Intersection, IntersectionID, Intersections, LaneID, LaneKind, LanePattern,
    LanePatternBuilder, Lanes, LightPolicy, Map, MapProject, ParkingSpots, PathKind, Pathfinder,
    Road, RoadID, RoadSegmentKind, Roads, SpatialMap, Traversable, TraverseDirection, TraverseKind,
    TurnKind, TurnPolicy, MAX_SLOPE, ROAD_Z_OFFSET,
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
    Infill { number: u8, part: u8 },
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

    fn lane_floor(&self, limits: SpacingLimits) -> f32 {
        if self.layer > 0 && self.class.driving() {
            limits.lane(self.class).max(limits.arterial_lane)
        } else {
            limits.lane(self.class)
        }
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

    pub(crate) fn remove_isolated(&mut self) {
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

fn consolidate_sobceva(graph: &mut Graph) {
    // The mapped roundabout and its splitter forks lie inside the minimum
    // junction spacing. Consolidate the four throats, including the short
    // walking connection to Na Vrtači, into a local priority crossing.
    let way_ids = [
        123625633, 1288843577, 1288843578, 1288843579, 1288843580, 1288856224, 1288856225,
        1453606426,
    ];
    let retired: Vec<_> = graph
        .links
        .values()
        .filter(|link| link.lineage.iter().any(|id| way_ids.contains(id)))
        .map(|link| link.key)
        .collect();
    let node = |id| SourceKey::Node { id, rail: false };
    let centre = node(1377878734);
    let Some(junction) = graph.junctions.get_mut(&centre) else {
        return;
    };
    let position = junction.position.xy();
    let members: BTreeSet<_> = retired
        .iter()
        .flat_map(|id| graph.links[id].endpoints)
        .collect();
    for id in &retired {
        graph.links.remove(id);
    }
    for link in graph.links.values_mut() {
        let last = link.points.len() - 1;
        for (end, i) in link.endpoints.iter_mut().zip([0, last]) {
            if members.contains(end) {
                *end = centre;
                link.points[i] = position;
                link.realigned = true;
            }
        }
    }
    graph.remove_isolated();
    graph.record(
        "Šobčeva splitter and Na Vrtači connection consolidated into a priority junction",
        &retired,
    );
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

type SeamCache = BTreeMap<SourceKey, [(SourceKey, Vec<Vec2>); 2]>;

fn join_seams(graph: &mut Graph, envelope: f32, rejected: &mut SeamCache) {
    loop {
        let adjacency = graph.adjacency();
        let joined = adjacency.iter().find_map(|(&via, keys)| {
            let &[a, b] = keys.as_slice() else {
                return None;
            };
            if graph.junctions[&via].required
                || rejected.get(&via).is_some_and(|entry| {
                    entry[0].0 == a
                        && entry[1].0 == b
                        && entry[0].1 == graph.links[&a].points
                        && entry[1].1 == graph.links[&b].points
                })
            {
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
            // Keep source junctions usable as access points instead of joining
            // every collinear town span into a single block-long road.
            let joins_town = matches!(a.class, RoadClass::Local | RoadClass::Collector);
            // Fit the consolidated A2 after choosing a ramp station. An
            // intermediate OSM way seam would otherwise constrain the split
            // to a short piece of the same through corridor after reframing.
            let fits = a.class == RoadClass::Motorway
                || ((!joins_town || arc(&points) <= 170.0) && fit(&points, envelope).2 <= envelope);
            if !fits {
                rejected.insert(
                    via,
                    [keys[0], keys[1]].map(|key| (key, graph.links[&key].points.clone())),
                );
            }
            fits.then_some((via, a, b, points))
        });
        let Some((via, mut a, b, points)) = joined else {
            break;
        };
        rejected.remove(&via);
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
pub(crate) fn main_components(graph: &mut Graph) {
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

pub(crate) fn retire_outer_road_fragments(
    graph: &mut Graph,
    districts: &[&[Vec2]],
) -> Result<(), String> {
    let core = SourceKey::Node {
        id: 772450604,
        rail: false,
    };
    if !graph.junctions.contains_key(&core) {
        return Err("missing old-centre road anchor".into());
    }
    let mut visited = BTreeSet::from([core]);
    let mut pending = vec![core];
    while let Some(key) = pending.pop() {
        for link in graph.neighbours(key).filter(|link| link.class.driving()) {
            pending.extend(
                link.endpoints
                    .into_iter()
                    .filter(|&end| visited.insert(end)),
            );
        }
    }
    let mut retired = Vec::new();
    for link in graph
        .links
        .values()
        .filter(|link| link.class.driving() && !visited.contains(&link.endpoints[0]))
    {
        if link.class >= RoadClass::Ramp
            || link
                .points
                .iter()
                .any(|&point| districts.iter().any(|area| super::contains(area, point)))
        {
            return Err(format!(
                "district or gateway road {:?} has no driving route to the old centre",
                link.key
            ));
        }
        retired.push(link.key);
    }
    if !retired.is_empty() {
        graph.record(
            "disconnected fringe roads outside the town districts remain source overlays",
            &retired,
        );
        graph.links.retain(|key, _| !retired.contains(key));
        graph.remove_isolated();
        main_components(graph);
    }
    Ok(())
}

fn remove_link(graph: &mut Graph, key: SourceKey, reason: &'static str) {
    graph.links.remove(&key);
    graph.remove_isolated();
    graph.record(reason, &[key]);
}

fn trim_estimate(graph: &Graph, link: &LinkSpec, key: SourceKey) -> f32 {
    let width = link.class.pattern(link.one_way, 0.0).width();
    let others: Vec<_> = graph
        .neighbours(key)
        .filter(|other| other.key != link.key)
        .collect();
    let direction = link.direction_at(key);
    let straight = |other: &LinkSpec| direction.dot(other.direction_at(key));
    let other_width = |other: &LinkSpec| other.class.pattern(other.one_way, 0.0).width();
    match others.as_slice() {
        [] => Intersection::empty_interface(width),
        [other] => {
            let dot = straight(other).clamp(-1.0, 1.0);
            if dot < -0.999 {
                1.0
            } else if dot >= 0.985 {
                50.0
            } else {
                ((other_width(other) + width * dot) * 0.5 / (1.0 - dot * dot).sqrt()).max(1.0)
            }
        }
        _ => others
            .iter()
            .fold(Intersection::empty_interface(width), |trim, other| {
                let dot = straight(other).clamp(0.0, 1.0);
                let radius = (width * 0.5).hypot(other_width(other) * 0.5);
                trim.max((radius * 1.1 / (1.0 - dot * dot).sqrt()).min(50.0))
            }),
    }
}

fn storage_margin(graph: &Graph, link: &LinkSpec, limits: SpacingLimits) -> f32 {
    let trim = link
        .endpoints
        .into_iter()
        .map(|key| trim_estimate(graph, link, key))
        .sum::<f32>();
    let floor = link
        .lane_floor(limits)
        .max(if link.class == RoadClass::Walk {
            18.0
        } else {
            0.0
        });
    arc(&link.points) - trim - floor - 2.5
}

fn move_junction(graph: &mut Graph, key: SourceKey, at: Vec2) {
    graph.junctions.get_mut(&key).unwrap().position = at.z(graph.junctions[&key].position.z);
    for link in graph
        .links
        .values_mut()
        .filter(|link| link.endpoints.contains(&key))
    {
        let i = if link.endpoints[0] == key {
            0
        } else {
            link.points.len() - 1
        };
        link.points[i] = at;
        link.realigned = true;
    }
}

fn spread_approach(
    graph: &mut Graph,
    a: SourceKey,
    b: SourceKey,
    limits: SpacingLimits,
    for_spacing: bool,
) -> bool {
    let mut best = None;
    for (key, other) in [(a, b), (b, a)] {
        let node = &graph.junctions[&key];
        if node.required
            || matches!(
                key,
                SourceKey::Cut { .. } | SourceKey::Node { rail: true, .. }
            )
            || graph.priority(key).0 >= RoadClass::Ramp
        {
            continue;
        }
        let from = node.position.xy();
        let away = (from - graph.junctions[&other].position.xy()).normalize();
        let incident: Vec<_> = graph.neighbours(key).map(|link| link.key).collect();
        let mut local = BTreeSet::new();
        for id in &incident {
            for end in graph.links[id].endpoints {
                local.extend(graph.neighbours(end).map(|link| link.key));
            }
        }
        let worst = |g: &Graph| {
            local
                .iter()
                .map(|id| storage_margin(g, &g.links[id], limits))
                .fold(0.0, f32::min)
        };
        let before = worst(graph);
        let target = incident
            .iter()
            .copied()
            .find(|id| graph.links[id].endpoints.contains(&other));
        let mut directions = vec![
            away,
            (away + away.perpendicular()).normalize(),
            (away - away.perpendicular()).normalize(),
            away.perpendicular(),
            -away.perpendicular(),
        ];
        directions.extend(incident.iter().flat_map(|id| {
            let dir = graph.links[id].direction_at(key);
            [dir, -dir]
        }));
        for distance in [8.0, 16.0, 24.0, 32.0, 44.0, 56.0] {
            for &dir in &directions {
                let at = from + distance * dir;
                if !at.is_finite()
                    || node
                        .members
                        .iter()
                        .any(|&p| p.distance(at) > limits.max_cluster)
                    || graph.junctions.iter().any(|(&k, n)| {
                        k != key && at.distance(n.position.xy()) < limits.junction + 0.15
                    })
                {
                    continue;
                }
                let mut candidate = graph.clone();
                move_junction(&mut candidate, key, at);
                if target.is_some_and(|id| {
                    !for_spacing && storage_margin(&candidate, &candidate.links[&id], limits) < 0.0
                }) {
                    continue;
                }
                let after = worst(&candidate);
                if after < before.min(-1.0) - 0.1 {
                    continue;
                }
                let outward = |g: &Graph, id: SourceKey| g.links[&id].direction_at(key);
                if incident.iter().enumerate().any(|(i, &l)| {
                    incident[i + 1..].iter().any(|&r| {
                        outward(&candidate, l).dot(outward(&candidate, r))
                            > outward(graph, l).dot(outward(graph, r)).max(0.85) + 0.025
                    })
                }) {
                    continue;
                }
                let weight = if graph.priority(key).0 == RoadClass::Avenue {
                    1.6
                } else {
                    1.0
                };
                let score = distance * weight - (after - before) * 0.3;
                if best.is_none_or(|(cost, _, _)| score < cost) {
                    best = Some((score, key, at));
                }
            }
        }
    }
    if let Some((_, key, at)) = best {
        move_junction(graph, key, at);
        graph.record(
            "local junction repositioned to keep adjacent source streets and lane storage",
            &[key],
        );
        true
    } else {
        false
    }
}

fn trim_short_links(graph: &mut Graph, limits: SpacingLimits, seams: &mut SeamCache) {
    // A later neighbouring move can shorten a link that was already
    // repaired. Try one relocation per endpoint pair before contracting
    // or retiring it, so competing approaches cannot oscillate.
    let mut relocated = BTreeSet::new();
    loop {
        let mut short: Vec<_> = graph
            .links
            .values()
            .filter_map(|link| {
                if link.class >= RoadClass::Ramp || matches!(link.key, SourceKey::Authored(_)) {
                    return None;
                }
                let margin = storage_margin(graph, link, limits);
                (margin < 0.0).then_some((margin, link.key))
            })
            .collect();
        short.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let Some(&(_, key)) = short.first() else {
            break;
        };
        let [a, b] = graph.links[&key].endpoints;
        let (keep, retire) = if (graph.junctions[&a].required, graph.priority(a))
            >= (graph.junctions[&b].required, graph.priority(b))
        {
            (a, b)
        } else {
            (b, a)
        };
        let distance = graph.junctions[&a]
            .position
            .distance(graph.junctions[&b].position);
        let contraction = |g: &mut Graph| {
            distance < limits.max_cluster && contract(g, keep, retire, limits.max_cluster, false)
        };
        if !(distance < 52.0 && contraction(graph)
            || (graph.links[&key].class != RoadClass::Walk
                && relocated.insert((key, ordered([a, b])))
                && spread_approach(graph, a, b, limits, false))
            || contraction(graph))
        {
            remove_link(graph, key, "short approach retired after merge and bounded relocation could not preserve lane storage");
        }
        join_seams(graph, limits.alignment, seams);
    }
}

fn clear_close_nodes(
    graph: &mut Graph,
    limits: SpacingLimits,
    seams: &mut SeamCache,
) -> Result<(), String> {
    // A clipped roundabout can lose its last arc during consolidation.
    // Its orphaned cut must not participate in the junction spacing scan.
    let mut last_count = usize::MAX;
    loop {
        graph.remove_isolated();
        let pairs = close_pairs(
            graph.junctions.values().map(|j| (j.key, j.position.xy())),
            limits.junction,
        );
        let Some(&(dist, a, b)) = pairs.first() else {
            return Ok(());
        };
        if pairs.len() >= last_count {
            return Err(format!(
                "junction spacing stalled at {a:?}/{b:?} ({dist:.2} m)"
            ));
        }
        last_count = pairs.len();
        let (keep, retire) = if (graph.junctions[&a].required, graph.priority(a))
            >= (graph.junctions[&b].required, graph.priority(b))
        {
            (a, b)
        } else {
            (b, a)
        };
        if contract(graph, keep, retire, limits.max_cluster, false) {
            join_seams(graph, limits.alignment, seams);
            continue;
        }
        if spread_approach(graph, keep, retire, limits, true) {
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
        join_seams(graph, limits.alignment, seams);
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

pub(crate) fn segment_parameters(a: Vec2, b: Vec2, c: Vec2, d: Vec2) -> Option<(f32, f32)> {
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
    let conflict = |p: Vec3, q: Vec3| (p.z - q.z).abs() < 5.0;
    for x in a.windows(2) {
        for y in b.windows(2) {
            if !bounds_overlap(
                segment_bounds(x[0], x[1]),
                segment_bounds(y[0], y[1]),
                radius,
            ) {
                continue;
            }
            if let Some((t, u)) = segment_parameters(x[0].xy(), x[1].xy(), y[0].xy(), y[1].xy()) {
                if conflict(x[0] + t * (x[1] - x[0]), y[0] + u * (y[1] - y[0])) {
                    return true;
                }
            }
            for (endpoints, line) in [(x, y), (y, x)] {
                for &point in endpoints {
                    let nearest = project_on_segment(point.xy(), line[0].xy(), line[1].xy());
                    if nearest.distance(point.xy()) >= radius {
                        continue;
                    }
                    let length = line[0].xy().distance(line[1].xy()).max(0.001);
                    let z = line[0].z
                        + (line[1].z - line[0].z) * line[0].xy().distance(nearest) / length;
                    if conflict(point, nearest.z(z)) {
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
    let length = arc(&rim.points);
    let approach = 156.0;
    if length < 2.0 * approach {
        return Err(format!(
            "A2 corridor is {length:.1} m after the owned cut and cannot fit both ramp approaches"
        ));
    }
    let mut along = 0.0;
    let desired = rim
        .points
        .windows(2)
        .map(|pair| {
            let point = project_on_segment(target, pair[0], pair[1]);
            let station = along + pair[0].distance(point);
            along += pair[0].distance(pair[1]);
            (point, station)
        })
        .min_by(|(a, _), (b, _)| a.distance2(target).total_cmp(&b.distance2(target)))
        .unwrap()
        .1
        .clamp(approach, length - approach);
    along = 0.0;
    let (index, aim) = rim
        .points
        .windows(2)
        .enumerate()
        .find_map(|(index, pair)| {
            let length = pair[0].distance(pair[1]);
            let before = along;
            along += length;
            (along >= desired && length > 0.0).then(|| {
                (
                    index,
                    pair[0] + (pair[1] - pair[0]) * ((desired - before) / length),
                )
            })
        })
        .unwrap();
    let mut first = rim.points[..=index].to_vec();
    first.push(aim);
    let mut second = vec![aim];
    second.extend_from_slice(&rim.points[index + 1..]);
    first.dedup();
    second.dedup();
    if arc(&first) < 150.0 || arc(&second) < 150.0 {
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
        node.light = junction_control(key, &driving);
        // The engine runs opposing legs in the same phase. Eliminate turns
        // across that phase and restore ordinary turns if the layout changes.
        node.turn.left_turns = node.light != LightPolicy::Lights;
    }
}

fn junction_control(key: SourceKey, driving: &[&LinkSpec]) -> LightPolicy {
    let avenues = driving.iter().any(|link| link.class == RoadClass::Avenue);
    let collectors = driving
        .iter()
        .filter(|link| link.class == RoadClass::Collector)
        .count();
    if driving.iter().any(|link| link.class >= RoadClass::Ramp) {
        return LightPolicy::NoLights;
    }
    if let [a, b, c, d] = driving {
        if avenues && driving.iter().all(|link| !link.one_way && !link.roundabout) {
            let mut directions = [a, b, c, d].map(|link| link.direction_at(key));
            directions.sort_by(|a, b| a.y.atan2(a.x).total_cmp(&b.y.atan2(b.x)));
            if opposing_phases(directions) {
                return LightPolicy::Lights;
            }
        }
    }
    if (driving.len() >= 4 && avenues) || (driving.len() >= 3 && collectors >= 2) {
        LightPolicy::StopSigns
    } else if driving.len() >= 3 && (avenues || collectors > 0) {
        LightPolicy::NoLights
    } else {
        LightPolicy::Auto
    }
}

fn opposing_phases(directions: [Vec2; 4]) -> bool {
    // Opposite legs must be at least 120° apart. Keep adjacent legs
    // between 35° and 145° to avoid bunching two approaches into one sector.
    directions[0].dot(directions[2]) <= -0.5
        && directions[1].dot(directions[3]) <= -0.5
        && directions.iter().enumerate().all(|(i, &direction)| {
            direction.cross(directions[(i + 1) % 4]) >= 35.0_f32.to_radians().sin()
        })
}

fn validate_signal_turns(map: &Map, inter: &Intersection, key: SourceKey) -> Result<(), String> {
    let approaches: Vec<_> = inter
        .roads
        .iter()
        .filter_map(|&id| map.get(id))
        .filter(|road| {
            road.incoming_lanes_to(inter.id)
                .iter()
                .any(|(_, kind)| kind.needs_light())
        })
        .collect();
    let [a, b, c, d] = approaches.as_slice() else {
        return Err(format!("signal {key:?} needs four live approaches"));
    };
    if !opposing_phases([a, b, c, d].map(|road| road.dir_from(inter.id))) {
        return Err(format!(
            "signal {key:?} lacks two opposed live approach pairs"
        ));
    }
    for road in &approaches {
        if road
            .incoming_lanes_to(inter.id)
            .iter()
            .filter(|(_, kind)| kind.needs_light())
            .any(|&(lane, _)| !map.get(lane).unwrap().control.is_light())
        {
            return Err(format!("signal {key:?} has an uncontrolled approach"));
        }
    }

    let approach_index = |lane: LaneID| {
        let road = map.get(lane)?.parent;
        approaches.iter().position(|approach| approach.id == road)
    };
    let mut movements = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for turn in inter.turns().filter(|turn| turn.kind == TurnKind::Driving) {
        let (Some(src), Some(dst)) = (approach_index(turn.id.src), approach_index(turn.id.dst))
        else {
            return Err(format!(
                "signal {key:?} has a turn outside its four approaches"
            ));
        };
        if dst != (src + 1) % 4 && dst != (src + 2) % 4 {
            return Err(format!(
                "signal {key:?} allows a turn across its opposing phase"
            ));
        }
        movements[src].push(turn);
    }
    if movements.iter().any(Vec::is_empty) {
        return Err(format!(
            "signal {key:?} has an approach with no driving turns"
        ));
    }
    for (first, opposite) in [(0, 2), (1, 3)] {
        for a in &movements[first] {
            for b in &movements[opposite] {
                if a.points.segments().any(|x| {
                    b.points.segments().any(|y| {
                        segment_parameters(x.src.xy(), x.dst.xy(), y.src.xy(), y.dst.xy()).is_some()
                    })
                }) {
                    return Err(format!("signal {key:?} has crossing turns in one phase"));
                }
            }
        }
    }
    Ok(())
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
    let east_ways = &[1288727380, 27690361, 61233484];
    let east_cut = source
        .links
        .values()
        .filter(|link| east_ways.iter().any(|id| link.lineage.contains(id)))
        .flat_map(|link| link.endpoints)
        .max_by(|a, b| {
            source.junctions[a]
                .position
                .x
                .total_cmp(&source.junctions[b].position.x)
        })
        .ok_or("missing Hraška eastern continuation")?;
    let north_local = node(4193639635);
    let specs = [
        ("hraska-west", west, middle, &[663442009, 26398038][..]),
        ("hraska-east", middle, east_cut, &east_ways[..]),
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
                elbow: (name == "hraska-east").then(|| {
                    // Preserve the departure split from the ramp and the
                    // neighbourhood collector as the east terminal moves.
                    from + from.distance(to) * vec2(0.46, -0.21)
                }),
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
    let mut seams = SeamCache::new();
    // Both dry through corridors run on a continuous deck; mapped layer
    // changes remain in the source graph for the wet/outer-tile profile.
    for link in graph
        .links
        .values_mut()
        .filter(|link| matches!(link.class, RoadClass::Motorway | RoadClass::Rail))
    {
        link.layer = 1;
    }
    inset_cuts(&mut graph, config);
    consolidate_sobceva(&mut graph);
    collapse_roundabouts(&mut graph);
    join_seams(&mut graph, limits.alignment, &mut seams);
    restore_avenues(&mut graph, &source)?;
    inset_cuts(&mut graph, config);
    clear_close_nodes(&mut graph, limits, &mut seams)?;
    trim_short_links(&mut graph, limits, &mut seams);
    main_components(&mut graph);
    add_rim_access(&mut graph, config)?;
    clear_close_nodes(&mut graph, limits, &mut seams)?;
    fit_links(&mut graph, limits);
    grade_and_controls(&mut graph);
    prune_geometry_conflicts(&mut graph)?;
    let degree = graph.adjacency();
    let orphaned_ring: Vec<_> = graph
        .links
        .values()
        .filter(|link| link.roundabout && link.endpoints.iter().any(|key| degree[key].len() == 1))
        .map(|link| link.key)
        .collect();
    for key in orphaned_ring {
        remove_link(
            &mut graph,
            key,
            "dead-end roundabout arc superseded by the connected priority approach",
        );
    }
    main_components(&mut graph);
    grade_and_controls(&mut graph);
    validate_plan(&graph, limits)?;
    Ok(graph)
}

fn measured_shortfalls(
    graph: &Graph,
    limits: SpacingLimits,
    environment: &Environment,
) -> Vec<(f32, SourceKey)> {
    let mut intersections = Intersections::default();
    let mut roads = Roads::default();
    let mut lanes = Lanes::default();
    let mut parking = ParkingSpots::default();
    let mut spatial = SpatialMap::default();
    let nodes: BTreeMap<_, _> = graph
        .junctions
        .values()
        .map(|node| {
            (
                node.key,
                Intersection::make(&mut intersections, &mut spatial, node.position),
            )
        })
        .collect();
    let spans: Vec<_> = graph
        .links
        .values()
        .map(|link| {
            let [a, b] = link.endpoints.map(|key| nodes[&key]);
            let [from, to] = [intersections[a].pos, intersections[b].pos];
            let segment = link.elbow.map_or(RoadSegmentKind::Straight, |elbow| {
                RoadSegmentKind::from_elbow(from.xy(), to.xy(), elbow)
            });
            let road = Road::make(
                &intersections[a],
                &intersections[b],
                segment,
                &link.class.pattern(link.one_way, 0.0),
                environment,
                &mut roads,
                &mut lanes,
                &mut parking,
                &mut spatial,
            );
            intersections[a].add_road(&roads, &roads[road]);
            intersections[b].add_road(&roads, &roads[road]);
            (link, road)
        })
        .collect();
    for inter in intersections.values_mut() {
        inter.update_interface_radius(&mut roads);
    }
    let mut failures = Vec::new();
    for (link, id) in spans {
        let road = &mut roads[id];
        road.update_lanes(&mut lanes, &mut parking, environment);
        let minimum = road
            .lanes_iter()
            .filter(|(_, kind)| matches!(kind, LaneKind::Driving | LaneKind::Rail))
            .map(|(id, _)| lanes[id].points.length())
            .fold(f32::INFINITY, f32::min);
        if minimum < link.lane_floor(limits) {
            failures.push((minimum - link.lane_floor(limits), link.key));
        }
    }
    failures.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    failures
}

pub(crate) fn calibrate_storage(
    graph: &mut Graph,
    config: TownConfig,
    limits: SpacingLimits,
    environment: &Environment,
) -> Result<(), String> {
    let mut attempts = BTreeSet::new();
    while let Some(&(shortfall, key)) = measured_shortfalls(graph, limits, environment).first() {
        let link = &graph.links[&key];
        if matches!(key, SourceKey::Authored(_)) || link.class >= RoadClass::Ramp {
            return Err(format!(
                "protected road {key:?} is {:.1} m short after actual trimming",
                -shortfall
            ));
        }
        let [a, b] = link.endpoints;
        let rank = |key| (graph.junctions[&key].required, graph.priority(key));
        let (keep, retire) = if rank(a) >= rank(b) { (a, b) } else { (b, a) };
        let mut candidate = graph.clone();
        if attempts.insert(key)
            && (contract(&mut candidate, keep, retire, limits.max_cluster, false)
                || spread_approach(&mut candidate, keep, retire, limits, false))
        {
            fit_links(&mut candidate, limits);
            if validate_plan(&candidate, limits).is_ok()
                && prune_geometry_conflicts(&mut candidate).is_ok()
                && !measured_shortfalls(&candidate, limits, environment)
                    .iter()
                    .any(|&(deficit, candidate)| candidate == key && deficit <= shortfall)
            {
                *graph = candidate;
                continue;
            }
        }
        remove_link(
            graph,
            key,
            "approach retired after native intersection and offset-lane measurement",
        );
        // Removing short dead-end roads can leave their remaining neighbour
        // in a degree-two seam. Keep the surviving geometry and recalculate
        // its actual engine trims on the next pass.
        inset_cuts(graph, config);
    }
    grade_and_controls(graph);
    Ok(())
}

#[derive(Clone, Copy)]
enum AccessSite {
    Junction(SourceKey),
    Middle {
        link: SourceKey,
        segment: usize,
        point: Vec2,
    },
}

impl AccessSite {
    fn point(self, graph: &Graph) -> Vec2 {
        match self {
            Self::Junction(key) => graph.junctions[&key].position.xy(),
            Self::Middle { point, .. } => point,
        }
    }

    fn on(self, link: &LinkSpec) -> bool {
        match self {
            Self::Junction(key) => link.endpoints.contains(&key),
            Self::Middle { link: key, .. } => link.key == key,
        }
    }
}

fn spacing_median(graph: &Graph) -> f32 {
    let mut chords: Vec<_> = graph
        .links
        .values()
        .filter(|link| link.class.driving())
        .map(|link| link.points[0].distance(*link.points.last().unwrap()))
        .collect();
    chords.sort_by(f32::total_cmp);
    let n = chords.len();
    if n == 0 {
        0.0
    } else {
        (chords[n / 2] + chords[(n - 1) / 2]) * 0.5
    }
}

/// Densify around retained curved streets. Split a live alignment only when
/// it gains a connecting residential street, which creates actual access
/// and blocks. Both new intersections and every affected span are checked
/// before committing the candidate graph.
pub(crate) fn connect_infill(
    graph: &mut Graph,
    limits: SpacingLimits,
    areas: &[&[Vec2]],
    river: &[Vec2],
    environment: &Environment,
) {
    use super::contains;
    for number in 0..38u8 {
        if spacing_median(graph) <= 139.5 {
            break;
        }
        let mut sites = Vec::new();
        let road_access = |link: &LinkSpec| link.frontage() && !link.one_way;
        for (&key, node) in &graph.junctions {
            let links: Vec<_> = graph.neighbours(key).collect();
            if node.position.z == 10.0
                && !node.required
                && links.len() <= 3
                && links.iter().any(|link| road_access(link))
                && links.iter().all(|link| link.class <= RoadClass::Avenue)
            {
                sites.push(AccessSite::Junction(key));
            }
        }
        for link in graph.links.values().filter(|link| road_access(link)) {
            if link
                .endpoints
                .iter()
                .any(|key| graph.junctions[key].position.z != 10.0)
            {
                continue;
            }
            let minimum = link.lane_floor(limits) + 33.0;
            let length = arc(&link.points);
            let mut target = minimum;
            let mut along = 0.0;
            for (segment, edge) in link.points.windows(2).enumerate() {
                let size = edge[0].distance(edge[1]);
                while target <= (along + size).min(length - minimum) && size > 0.0 {
                    let point = edge[0] + ((target - along) / size) * (edge[1] - edge[0]);
                    if graph
                        .junctions
                        .values()
                        .all(|node| node.position.xy().distance(point) > limits.junction + 1.0)
                    {
                        sites.push(AccessSite::Middle {
                            link: link.key,
                            point,
                            segment,
                        });
                    }
                    target += 25.0;
                }
                along += size;
            }
        }
        let mut candidates = Vec::new();
        for (i, &a) in sites.iter().enumerate() {
            let from = a.point(graph);
            for &b in &sites[i + 1..] {
                let to = b.point(graph);
                let chord = from.distance(to);
                if !(93.0..=138.0).contains(&chord) {
                    continue;
                }
                let mid = (from + to) * 0.5;
                let Some(district) = areas.iter().position(|area| contains(area, mid)) else {
                    continue;
                };
                let split_cost = [a, b]
                    .iter()
                    .filter(|site| matches!(site, AccessSite::Middle { .. }))
                    .count();
                let score = (chord - 116.0).abs() - 4.0 * split_cost as f32 + 5.0 * district as f32;
                candidates.push((score, a, b));
            }
        }
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut accepted = false;
        for (_, a, b) in candidates {
            if let Some(candidate) = try_infill(graph, limits, [a, b], number, river) {
                if !measured_shortfalls(&candidate, limits, environment).is_empty() {
                    continue;
                }
                *graph = candidate;
                accepted = true;
                break;
            }
        }
        if !accepted {
            break;
        }
    }
}

fn try_infill(
    graph: &Graph,
    limits: SpacingLimits,
    [a, b]: [AccessSite; 2],
    number: u8,
    river: &[Vec2],
) -> Option<Graph> {
    let [from, to] = [a, b].map(|site| site.point(graph));
    let tangent = (to - from).normalize();
    let bend = if number % 2 == 0 { 13.0 } else { -10.0 };
    let elbow = (from + to) * 0.5 + bend * tangent.perpendicular();
    let points: Vec<_> = curve(from, to, elbow).points(25).collect();
    let profile: Vec<_> = points.iter().map(|point| point.z(10.0)).collect();
    let water: Vec<_> = river.iter().map(|point| point.z(10.0)).collect();
    if overlap(&profile, &water, 88.0) {
        return None;
    }
    let width = RoadClass::Local.pattern(false, 0.0).width();
    for link in graph.links.values() {
        if a.on(link) && b.on(link) {
            return None;
        }
        if let Some((site, heading)) = [
            (a, (elbow - from).normalize()),
            (b, (elbow - to).normalize()),
        ]
        .into_iter()
        .find(|(site, _)| site.on(link))
        {
            match site {
                AccessSite::Junction(key) if link.direction_at(key).dot(heading) > 0.64 => {
                    return None
                }
                AccessSite::Middle { segment, .. } => {
                    let axis = (link.points[segment + 1] - link.points[segment]).normalize();
                    if axis.dot(heading).abs() > 0.58 {
                        return None;
                    }
                }
                _ => {}
            }
            continue;
        }
        let radius = (link.class.pattern(link.one_way, 0.0).width() + width) * 0.5 + 1.0;
        let lower = from.min(to) - Vec2::splat(radius + bend.abs());
        let upper = from.max(to) + Vec2::splat(radius + bend.abs());
        if link.points.windows(2).any(|pair| {
            let [p, q] = [pair[0], pair[1]];
            let lo = p.min(q);
            let hi = p.max(q);
            hi.x >= lower.x
                && lo.x <= upper.x
                && hi.y >= lower.y
                && lo.y <= upper.y
                && overlap(&profile, &[p.z(10.0), q.z(10.0)], radius)
        }) {
            return None;
        }
    }
    let mut candidate = graph.clone();
    let endpoints = [
        split_for_access(&mut candidate, a, number, 0, limits)?,
        split_for_access(&mut candidate, b, number, 1, limits)?,
    ];
    let key = SourceKey::Infill { number, part: 4 };
    candidate.links.insert(
        key,
        LinkSpec {
            key,
            endpoints,
            class: RoadClass::Local,
            layer: 0,
            one_way: false,
            roundabout: false,
            named: false,
            lineage: BTreeSet::new(),
            samples: Vec::new(),
            points,
            elbow: Some(elbow),
            realigned: false,
            inherited: Vec::new(),
        },
    );
    if candidate
        .links
        .values()
        .filter(|link| link.endpoints.iter().any(|key| endpoints.contains(key)))
        .any(|link| storage_margin(&candidate, link, limits) < 2.0)
    {
        return None;
    }
    candidate.record(
        "short curved street gives an irregular block two connected frontages",
        &[key, endpoints[0], endpoints[1]],
    );
    Some(candidate)
}

fn split_for_access(
    graph: &mut Graph,
    site: AccessSite,
    number: u8,
    side: u8,
    limits: SpacingLimits,
) -> Option<SourceKey> {
    let AccessSite::Middle {
        link,
        segment,
        point,
    } = site
    else {
        let AccessSite::Junction(key) = site else {
            unreachable!()
        };
        return Some(key);
    };
    let junction = SourceKey::Infill { number, part: side };
    let second = SourceKey::Infill {
        number,
        part: side + 2,
    };
    let mut left = graph.links.get(&link)?.clone();
    let mut right = left.clone();
    left.endpoints[1] = junction;
    right.endpoints[0] = junction;
    right.key = second;
    left.points.truncate(segment + 1);
    left.points.push(point);
    right.points.drain(..=segment);
    right.points.insert(0, point);
    for link in [&mut left, &mut right] {
        let (elbow, fitted, error) = fit(&link.points, limits.alignment);
        if error > 3.0 {
            return None;
        }
        link.elbow = elbow;
        link.points = fitted;
        link.realigned |= error > 1.0;
    }
    graph
        .junctions
        .insert(junction, JunctionSpec::new(junction, point));
    graph.links.insert(left.key, left);
    graph.links.insert(right.key, right);
    Some(junction)
}

pub(crate) fn finish_network(graph: &mut Graph, limits: SpacingLimits) -> Result<(), String> {
    fit_links(graph, limits);
    grade_and_controls(graph);
    validate_plan(graph, limits)
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
        validate_profile(map, graph, link, road)?;
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

fn validate_profile(map: &Map, graph: &Graph, link: &LinkSpec, road: &Road) -> Result<(), String> {
    let [from, to] = link.endpoints.map(|key| graph.junctions[&key].position);
    let segment = link.elbow.map_or(RoadSegmentKind::Straight, |elbow| {
        RoadSegmentKind::from_elbow(from.xy(), to.xy(), elbow)
    });
    let (expected, error) = Road::generate_points(
        from,
        to,
        segment,
        link.class == RoadClass::Rail,
        map.environment(),
    );
    if error.is_some() || !same_profile(&expected, &road.points) {
        return Err(format!(
            "live centreline differs from the declared 3D profile for {:?}",
            link.key
        ));
    }

    let clipped = expected.cut(road.interface_from(road.src), road.interface_from(road.dst));
    if !same_profile(&clipped, &road.interfaced_points) {
        // Terrain-following roads are sampled a second time after trimming.
        let (terrain_trim, error) = Road::heightfinder(
            &clipped.flatten(),
            expected.first().z - ROAD_Z_OFFSET,
            expected.last().z - ROAD_Z_OFFSET,
            MAX_SLOPE,
            map.environment(),
        );
        if error.is_some() || !same_profile(&terrain_trim, &road.interfaced_points) {
            return Err(format!("live trimmed centreline drift for {:?}", link.key));
        }
    }
    for (id, _) in road.lanes_iter() {
        let lane = map
            .get(id)
            .ok_or_else(|| format!("missing live lane {id:?}"))?;
        let mut expected_lane = lane.clone();
        expected_lane.gen_pos(road);
        if !same_profile(&expected_lane.points, &lane.points) {
            return Err(format!(
                "live lane geometry drift for {:?} lane {id:?}",
                link.key
            ));
        }
    }
    Ok(())
}

fn same_profile(a: &PolyLine3, b: &PolyLine3) -> bool {
    // Public construction and reload preserve these polylines. Comparing in
    // both directions permits equivalent resampling without hiding a detour.
    a.first().distance2(b.first()) <= 0.01
        && a.last().distance2(b.last()) <= 0.01
        && [(a, b), (b, a)].into_iter().all(|(from, to)| {
            from.iter()
                .all(|&point| point.is_finite() && to.project(point).distance2(point) <= 0.01)
        })
}

fn live_sweeps(map: &Map) -> Result<(), String> {
    let mut roads: Vec<_> = map.roads().values().collect();
    roads.sort_by_key(|road| road.id);
    let profiles: Vec<_> = roads
        .iter()
        .map(|road| road.points.iter().copied().collect::<Vec<_>>())
        .collect();
    let bounds: Vec<_> = profiles
        .iter()
        .map(|points| {
            points
                .iter()
                .fold((points[0].xy(), points[0].xy()), |(lo, hi), point| {
                    (lo.min(point.xy()), hi.max(point.xy()))
                })
        })
        .collect();
    let has_rail = |road: &Road| road.lanes_iter().any(|(_, kind)| kind == LaneKind::Rail);
    for (i, &a) in roads.iter().enumerate() {
        for (j, &b) in roads.iter().enumerate().skip(i + 1) {
            let width = (a.width + b.width) * 0.5;
            if !bounds_overlap(bounds[i], bounds[j], width) {
                continue;
            }
            let junction = [a.src, a.dst]
                .into_iter()
                .find(|end| [b.src, b.dst].contains(end))
                .map(|end| {
                    // Extend the longer interface by the half-width of each
                    // approach to enclose the junction's swept pavement.
                    let radius = a.interface_from(end).max(b.interface_from(end)) + width;
                    (map.get(end).unwrap().pos.xy(), radius)
                });
            let (rail_a, rail_b) = (has_rail(a), has_rail(b));
            // The upper deck occupies 1 m below its running surface. Reserve
            // 5 m for rolling stock or 4.5 m for road vehicles, plus 0.5 m
            // operating margin above the lower corridor.
            let clearance = |rail| if rail { 6.5 } else { 6.0 };
            if live_conflict(
                &profiles[i],
                &profiles[j],
                width,
                junction,
                [-clearance(rail_a), clearance(rail_b)],
            ) {
                let problem = if rail_a != rail_b {
                    "rail clearance"
                } else {
                    "live swept overlap"
                };
                return Err(format!(
                    "{problem} {:?}/{:?} at {:?}/{:?}",
                    a.id,
                    b.id,
                    a.points.first(),
                    b.points.first()
                ));
            }
        }
    }
    Ok(())
}

fn validate_lane_components(map: &Map) -> Result<(), String> {
    for kind in [LaneKind::Driving, LaneKind::Walking, LaneKind::Rail] {
        let lanes: BTreeSet<_> = map
            .lanes()
            .values()
            .filter(|lane| lane.kind == kind)
            .map(|lane| lane.id)
            .collect();
        let Some(&start) = lanes.first() else {
            continue;
        };
        let mut forward = BTreeMap::<_, Vec<_>>::new();
        let mut backward = BTreeMap::<_, Vec<_>>::new();
        for junction in map.intersections().values() {
            for turn in junction
                .turns()
                .filter(|turn| lanes.contains(&turn.id.src) && lanes.contains(&turn.id.dst))
            {
                let (src, dst) = (turn.id.src, turn.id.dst);
                forward.entry(src).or_default().push(dst);
                backward.entry(dst).or_default().push(src);
                if turn.id.bidirectional {
                    forward.entry(dst).or_default().push(src);
                    backward.entry(src).or_default().push(dst);
                }
            }
        }
        for edges in [&forward, &backward] {
            let mut visited = BTreeSet::from([start]);
            let mut remaining = vec![start];
            while let Some(lane) = remaining.pop() {
                remaining.extend(
                    edges
                        .get(&lane)
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|&next| visited.insert(next)),
                );
            }
            if visited != lanes {
                return Err(format!(
                    "{kind:?} connectivity reaches only {}/{} lanes (first missing {:?})",
                    visited.len(),
                    lanes.len(),
                    lanes.difference(&visited).next()
                ));
            }
        }
    }
    Ok(())
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
        validate_profile(map, graph, link, road)?;
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
            if !length.is_finite() || length < link.lane_floor(limits) {
                lane_errors.push(format!(
                    "{key:?} {kind:?} lane {id:?} is {length:.3} m; requires {:.1} m",
                    link.lane_floor(limits)
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
    for node in graph
        .junctions
        .values()
        .filter(|node| node.light == LightPolicy::Lights)
    {
        let inter = built
            .junctions
            .get(&node.key)
            .and_then(|&id| map.get(id))
            .ok_or_else(|| format!("missing live signal {:?}", node.key))?;
        validate_signal_turns(map, inter, node.key)?;
    }
    live_sweeps(map)?;
    validate_lane_components(map)
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

/// Route over the live, directed turn graph, optionally omitting a bridge or excluding through roads.
pub(crate) fn can_route(
    map: &Map,
    from: IntersectionID,
    to: IntersectionID,
    kind: LaneKind,
    allowed: impl Fn(RoadID) -> bool,
) -> bool {
    let walking = kind == LaneKind::Walking;
    let mut visited = BTreeSet::new();
    let mut pending = Vec::new();
    for lane in map
        .lanes()
        .values()
        .filter(|lane| lane.kind == kind && allowed(lane.parent))
    {
        if lane.src == from {
            pending.push((lane.id, lane.dst));
        }
        if walking && lane.dst == from {
            pending.push((lane.id, lane.src));
        }
    }
    while let Some((id, end)) = pending.pop() {
        if !visited.insert((id, end)) {
            continue;
        }
        if end == to {
            return true;
        }
        pending.extend(
            map.get(end)
                .unwrap()
                .turns_from(id)
                .filter_map(|(turn, dir)| {
                    let next = match dir {
                        TraverseDirection::Forward => turn.dst,
                        TraverseDirection::Backward => turn.src,
                    };
                    let lane = map.get(next)?;
                    if lane.kind != kind || !allowed(lane.parent) {
                        return None;
                    }
                    if lane.src == end {
                        Some((next, lane.dst))
                    } else if walking && lane.dst == end {
                        Some((next, lane.src))
                    } else {
                        None
                    }
                }),
        );
    }
    false
}

fn routed_rim_trip(
    map: &Map,
    from: IntersectionID,
    to: IntersectionID,
    rim: &BTreeSet<RoadID>,
) -> bool {
    let incoming = |lane: &&simulation::map::Lane| lane.src == from;
    let outgoing = |lane: &&simulation::map::Lane| lane.dst == to;
    let lanes: Vec<_> = map
        .lanes()
        .values()
        .filter(|lane| lane.kind == LaneKind::Driving && rim.contains(&lane.parent))
        .collect();
    for start in lanes.iter().copied().filter(incoming) {
        for end in lanes.iter().copied().filter(outgoing) {
            let start = Traversable::new(TraverseKind::Lane(start.id), TraverseDirection::Forward);
            if PathKind::Vehicle
                .path(map, prototypes::Tick(0), start, end.id)
                .is_none_or(|path| {
                    path.iter().any(|step| {
                        !rim.contains(&map.get(step.destination_lane()).unwrap().parent)
                    })
                })
            {
                return false;
            }
        }
    }
    lanes.iter().any(incoming) && lanes.iter().any(outgoing)
}

/// Probe connectivity through actual directed lanes and generated turn movements.
pub fn route_checks(
    map: &Map,
    graph: &Graph,
    built: &BuiltNetwork,
) -> BTreeMap<&'static str, bool> {
    let boundary = |class| {
        let endpoints: BTreeSet<_> = graph
            .links
            .values()
            .filter(|link| link.class == class)
            .flat_map(|link| link.endpoints)
            .filter(|key| matches!(key, SourceKey::Cut { .. }))
            .map(|key| built.junctions[&key])
            .collect();
        let mut endpoints = endpoints.into_iter().collect::<Vec<_>>();
        endpoints.sort_by(|&a, &b| {
            map.get(b)
                .unwrap()
                .pos
                .y
                .total_cmp(&map.get(a).unwrap().pos.y)
        });
        endpoints
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
        &[a, b] => routed_rim_trip(map, a, b, &motorway) && routed_rim_trip(map, b, a, &motorway),
        _ => false,
    };
    let rail_through = match rail.as_slice() {
        &[a, b] => {
            can_route(map, a, b, LaneKind::Rail, |_| true)
                && can_route(map, b, a, LaneKind::Rail, |_| true)
        }
        _ => false,
    };
    let ramp = built
        .links
        .get(&SourceKey::Authored("a2-hraska-ramp"))
        .map(|span| span.road);
    let access = |index, inbound| {
        h.zip(rim.get(index)).is_some_and(|(&town, &gate)| {
            let (from, to) = if inbound { (gate, town) } else { (town, gate) };
            can_route(map, from, to, LaneKind::Driving, |road| {
                Some(road) == ramp || motorway.contains(&road)
            })
        })
    };
    let core_access = h.zip(core).is_some_and(|(&h, &core)| {
        can_route(map, h, core, LaneKind::Driving, |_| true)
            && can_route(map, core, h, LaneKind::Driving, |_| true)
    });
    let through_only = |via: u64, source_way: u64, destination: SourceKey| {
        let key = SourceKey::Node {
            id: via,
            rail: false,
        };
        let Some(&junction) = built.junctions.get(&key) else {
            return false;
        };
        let inbound: BTreeSet<_> = graph
            .neighbours(key)
            .filter(|link| link.lineage.contains(&source_way))
            .map(|link| built.links[&link.key].road)
            .collect();
        // A westward crop can retire the source inlet before this node.
        // With the inlet absent the restricted movement cannot be made.
        if inbound.is_empty() {
            return true;
        }
        let Some(destination) = built.links.get(&destination).map(|span| span.road) else {
            return false;
        };
        let mut driving = map
            .get(junction)
            .unwrap()
            .turns()
            .filter(|turn| {
                let lane = map.get(turn.id.src).unwrap();
                lane.kind == LaneKind::Driving && inbound.contains(&lane.parent)
            })
            .peekable();
        driving.peek().is_some()
            && driving.all(|turn| map.get(turn.id.dst).unwrap().parent == destination)
    };
    let consolidated_ramp_no_u_turn = built
        .links
        .get(&SourceKey::Authored("a2-hraska-ramp"))
        .zip(h)
        .is_some_and(|(ramp, &town)| {
            map.get(town).unwrap().turns().all(|turn| {
                let from = map.get(turn.id.src).unwrap();
                let to = map.get(turn.id.dst).unwrap();
                from.kind != LaneKind::Driving || from.parent != ramp.road || to.parent != ramp.road
            })
        });
    BTreeMap::from([
        ("rim_through_both_directions", rim_through),
        ("mainline_rail_both_directions", rail_through),
        ("1231337592_north_gateway_to_hraska", access(0, true)),
        ("1205190017_hraska_to_north_gateway", access(0, false)),
        ("1205190016_south_gateway_to_hraska", access(1, true)),
        (
            "37854835_26261250_hraska_to_south_gateway",
            access(1, false),
        ),
        ("hraska_to_old_centre_both_directions", core_access),
        (
            "restriction_2465892_straight_on_to_hraska",
            through_only(289194013, 26270881, SourceKey::Authored("hraska-west")),
        ),
        (
            "restriction_18225139_no_consolidated_ramp_u_turn",
            consolidated_ramp_no_u_turn,
        ),
    ])
}
