use ordered_float::OrderedFloat;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

thread_local! {
    static SEARCH: RefCell<Search> = RefCell::default();
}

// Keep the priority and tie behavior of pathfinding 4.10's A* so equally
// ranked routes still follow the same successor insertion order.
struct Pending {
    node: usize,
    cost: OrderedFloat<f32>,
    estimate: OrderedFloat<f32>,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.estimate == other.estimate && self.cost == other.cost
    }
}

impl Eq for Pending {}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pending {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .estimate
            .cmp(&self.estimate)
            .then_with(|| self.cost.cmp(&other.cost))
    }
}

#[derive(Clone, Default)]
struct Visit {
    generation: u32,
    cost: OrderedFloat<f32>,
    parent: usize,
}

#[derive(Default)]
struct Search {
    generation: u32,
    visited: Vec<Visit>,
    pending: BinaryHeap<Pending>,
}

pub(super) fn astar<T>(
    node_count: usize,
    start: usize,
    mut successors: impl FnMut(usize) -> T,
    mut heuristic: impl FnMut(usize) -> f32,
    mut arrived: impl FnMut(usize) -> bool,
) -> Option<Vec<usize>>
where
    T: IntoIterator<Item = (usize, f32)>,
{
    SEARCH.with_borrow_mut(|search| {
        search.pending.clear();
        if search.visited.len() < node_count {
            search.visited.resize(node_count, Visit::default());
        }
        // A new generation makes previous visits invisible without clearing
        // every lane on each query.
        if search.generation == u32::MAX {
            search.visited.fill(Visit::default());
            search.generation = 0;
        }
        search.generation += 1;
        let generation = search.generation;
        search.visited[start] = Visit {
            generation,
            cost: OrderedFloat(0.0),
            parent: usize::MAX,
        };
        search.pending.push(Pending {
            node: start,
            cost: OrderedFloat(0.0),
            estimate: OrderedFloat(0.0),
        });

        while let Some(Pending { node, cost, .. }) = search.pending.pop() {
            if arrived(node) {
                let mut path = vec![node];
                let mut parent = search.visited[node].parent;
                while parent != usize::MAX {
                    path.push(parent);
                    parent = search.visited[parent].parent;
                }
                path.reverse();
                return Some(path);
            }
            if cost > search.visited[node].cost {
                continue;
            }
            for (next, edge_cost) in successors(node) {
                let next_cost = OrderedFloat(cost.0 + edge_cost);
                let visit = &mut search.visited[next];
                if visit.generation == generation && visit.cost <= next_cost {
                    continue;
                }
                *visit = Visit {
                    generation,
                    cost: next_cost,
                    parent: node,
                };
                search.pending.push(Pending {
                    node: next,
                    cost: next_cost,
                    estimate: OrderedFloat(next_cost.0 + heuristic(next)),
                });
            }
        }
        None
    })
}
