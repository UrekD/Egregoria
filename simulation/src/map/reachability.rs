use super::pathfinding::lane_turn_successors;
use super::{LaneID, Map};
use slotmapd::SecondaryMap;

pub(super) struct DrivingReachability {
    lanes: SecondaryMap<LaneID, LaneReachability>,
    components: Vec<Component>,
}

struct LaneReachability {
    component: usize,
    has_incoming_turn: bool,
}

#[derive(Default)]
struct Component {
    successors: Vec<usize>,
    cyclic: bool,
}

impl DrivingReachability {
    pub(super) fn build(map: &Map) -> Self {
        let lane_ids: Vec<_> = map.lanes.keys().collect();
        let mut index = SecondaryMap::with_capacity(lane_ids.len());
        for (i, &lane) in lane_ids.iter().enumerate() {
            index.insert(lane, i);
        }

        let mut forward = vec![Vec::new(); lane_ids.len()];
        let mut reverse = vec![Vec::new(); lane_ids.len()];
        for (src, &lane) in lane_ids.iter().enumerate() {
            for dst in lane_turn_successors(map, lane).filter_map(|dst| index.get(dst).copied()) {
                forward[src].push(dst);
                reverse[dst].push(src);
            }
        }

        let (component_of, mut components) = strong_components(&forward, &reverse);
        let mut lanes = SecondaryMap::with_capacity(lane_ids.len());
        for (src, lane) in lane_ids.into_iter().enumerate() {
            let component = component_of[src];
            lanes.insert(
                lane,
                LaneReachability {
                    component,
                    has_incoming_turn: !reverse[src].is_empty(),
                },
            );
            components[component].successors.extend(
                forward[src]
                    .iter()
                    .map(|&dst| component_of[dst])
                    .filter(|&dst| dst != component),
            );
        }
        for component in &mut components {
            component.successors.sort_unstable();
            component.successors.dedup();
        }

        Self { lanes, components }
    }

    /// Reachability via one or more turns, after the caller's local-route check.
    pub(super) fn can_reach(&self, start: LaneID, end: LaneID) -> bool {
        let Some((from, to)) = self.lanes.get(start).zip(self.lanes.get(end)) else {
            return false;
        };
        if start != end && !to.has_incoming_turn {
            return false;
        }

        let (start, end) = (from.component, to.component);
        if start == end {
            return self.components[start].cyclic;
        }
        // Component IDs are topological, so every turn between components advances the ID.
        if start > end {
            return false;
        }
        match self.components[start].successors.binary_search(&end) {
            Ok(_) => return true,
            Err(0) => return false,
            Err(_) => {}
        }

        let mut visited = vec![false; end - start];
        let mut pending = vec![start];
        while let Some(component) = pending.pop() {
            for &next in &self.components[component].successors {
                if next == end {
                    return true;
                }
                if next > end {
                    break;
                }
                if !std::mem::replace(&mut visited[next - start], true) {
                    pending.push(next);
                }
            }
        }
        false
    }
}

// Iterative Kosaraju: visit lanes in slot order and successors in turn order.
fn strong_components(
    forward: &[Vec<usize>],
    reverse: &[Vec<usize>],
) -> (Vec<usize>, Vec<Component>) {
    let mut visited = vec![false; forward.len()];
    let mut finished = Vec::with_capacity(forward.len());
    let mut dfs = Vec::new();
    for (root, successors) in forward.iter().enumerate() {
        if std::mem::replace(&mut visited[root], true) {
            continue;
        }
        dfs.push((root, successors.iter()));
        while let Some((lane, successors)) = dfs.last_mut() {
            if let Some(&next) = successors.next() {
                if !std::mem::replace(&mut visited[next], true) {
                    dfs.push((next, forward[next].iter()));
                }
            } else {
                finished.push(*lane);
                dfs.pop();
            }
        }
    }

    let mut component_of = vec![usize::MAX; forward.len()];
    let mut components = Vec::new();
    let mut pending = Vec::new();
    for root in finished.into_iter().rev() {
        if component_of[root] != usize::MAX {
            continue;
        }
        let id = components.len();
        let mut component = Component::default();
        component_of[root] = id;
        pending.push(root);
        while let Some(lane) = pending.pop() {
            for &previous in &reverse[lane] {
                if previous == lane {
                    component.cyclic = true;
                }
                if component_of[previous] == usize::MAX {
                    component.cyclic = true;
                    component_of[previous] = id;
                    pending.push(previous);
                }
            }
        }
        components.push(component);
    }
    (component_of, components)
}
