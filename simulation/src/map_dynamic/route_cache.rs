use crate::map::{LaneID, Traversable, TraverseDirection, TraverseKind};
use std::collections::BTreeMap;
use std::mem::size_of;

const MAX_WALK_ROUTES: usize = 8192;
const MAX_CACHED_WALK_LENGTH: usize = 256;
const MAX_WALK_ROUTE_BYTES: usize = 4 * 1024 * 1024;

pub(crate) type WalkState = (TraverseKind, bool);
type WalkRouteKey = (WalkState, LaneID);
type WalkRoute = Option<Box<[Traversable]>>;

pub(crate) fn walk_state(t: Traversable) -> WalkState {
    (t.kind, t.dir == TraverseDirection::Forward)
}

#[derive(Default)]
pub(crate) struct WalkRouteCache {
    routes: BTreeMap<WalkRouteKey, WalkRoute>,
    bytes: usize,
    // Keep the lowest ordered prefix of all eligible keys seen this epoch.
    // Remember the first discarded key when unequal payload sizes leave slack.
    discard_from: Option<WalkRouteKey>,
}

fn entry_bytes(path: Option<&[Traversable]>) -> usize {
    size_of::<(WalkRouteKey, WalkRoute)>() + path.map_or(0, std::mem::size_of_val)
}

impl WalkRouteCache {
    pub(crate) fn get(&self, key: WalkRouteKey) -> Option<Option<Vec<Traversable>>> {
        self.routes
            .get(&key)
            .map(|path| path.as_deref().map(<[_]>::to_vec))
    }

    pub(crate) fn insert(&mut self, key: WalkRouteKey, path: Option<&[Traversable]>) {
        if path.is_some_and(|path| path.len() > MAX_CACHED_WALK_LENGTH)
            || self.discard_from.is_some_and(|discarded| key >= discarded)
            || self.routes.contains_key(&key)
        {
            return;
        }
        self.bytes += entry_bytes(path);
        self.routes.insert(key, path.map(Box::from));
        while self.bytes > MAX_WALK_ROUTE_BYTES || self.routes.len() > MAX_WALK_ROUTES {
            let (key, path) = self.routes.pop_last().unwrap();
            self.bytes -= entry_bytes(path.as_deref());
            self.discard_from = Some(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_perf_walk_cache_keeps_same_bounded_prefix_in_any_order() {
        let mut ids = slotmapd::HopSlotMap::<LaneID, ()>::with_key();
        let target = ids.insert(());
        let keys: Vec<_> = (0..1500)
            .map(|_| ((TraverseKind::Lane(ids.insert(())), true), target))
            .collect();
        let step = Traversable::new(TraverseKind::Lane(target), TraverseDirection::Forward);
        let route = vec![step; MAX_CACHED_WALK_LENGTH];
        let path = |i: usize| (i % 8 != 0).then(|| &route[..MAX_CACHED_WALK_LENGTH - i % 23]);
        let build = |order: &mut dyn Iterator<Item = usize>| {
            let mut cache = WalkRouteCache::default();
            for i in order {
                cache.insert(keys[i], path(i));
                assert!(cache.bytes <= MAX_WALK_ROUTE_BYTES);
            }
            cache
        };
        let forward = build(&mut (0..keys.len()));
        let reverse = build(&mut (0..keys.len()).rev());
        assert_eq!(forward.routes, reverse.routes);
        assert_eq!(forward.discard_from, reverse.discard_from);
        assert_eq!(forward.get(keys[0]), Some(None));
        assert_eq!(forward.get(keys[1]), Some(path(1).map(<[_]>::to_vec)));
        assert_eq!(forward.get(*keys.last().unwrap()), None);
    }
}
