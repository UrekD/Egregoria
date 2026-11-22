use geom::{Vec2, AABB};

/// A static bounding-volume tree. Leaves retain their individual bounds so
/// large queries do not emit candidates from the empty parts of a branch.
pub(super) struct BoundsIndex<T>(Option<Box<Node<T>>>);

enum Node<T> {
    Leaf(AABB, Vec<(AABB, T)>),
    Branch(AABB, Box<Node<T>>, Box<Node<T>>),
}

impl<T> BoundsIndex<T> {
    pub(super) fn new(entries: impl IntoIterator<Item = (AABB, T)>) -> Self {
        let entries: Vec<_> = entries.into_iter().collect();
        Self((!entries.is_empty()).then(|| Node::build(entries)))
    }

    pub(super) fn query(&self, area: AABB) -> impl Iterator<Item = &T> {
        let mut pending: Vec<_> = self.0.as_deref().into_iter().collect();
        let mut leaf = [].iter();
        std::iter::from_fn(move || loop {
            for (bounds, value) in leaf.by_ref() {
                if overlaps(*bounds, area) {
                    return Some(value);
                }
            }
            match pending.pop()? {
                Node::Branch(bounds, a, b) if overlaps(*bounds, area) => {
                    pending.push(b);
                    pending.push(a);
                }
                Node::Leaf(bounds, entries) if overlaps(*bounds, area) => leaf = entries.iter(),
                _ => {}
            }
        })
    }
}

impl<T> Node<T> {
    fn build(mut entries: Vec<(AABB, T)>) -> Box<Self> {
        let bounds = entries
            .iter()
            .map(|&(bounds, _)| bounds)
            .reduce(|a, b| AABB {
                ll: a.ll.min(b.ll),
                ur: a.ur.max(b.ur),
            })
            .unwrap();
        if entries.len() <= 8 {
            return Box::new(Self::Leaf(bounds, entries));
        }
        let size = bounds.ur - bounds.ll;
        let center = |bounds: AABB| {
            let p = bounds.ll + bounds.ur;
            if size.x >= size.y {
                p.x
            } else {
                p.y
            }
        };
        let mid = entries.len() / 2;
        entries.select_nth_unstable_by(mid, |a, b| center(a.0).total_cmp(&center(b.0)));
        let right = entries.split_off(mid);
        Box::new(Self::Branch(
            bounds,
            Self::build(entries),
            Self::build(right),
        ))
    }
}

pub(super) fn point_bounds(point: Vec2) -> AABB {
    AABB {
        ll: point,
        ur: point,
    }
}

pub(super) fn points_bounds(points: impl Iterator<Item = Vec2>) -> AABB {
    points.fold(
        AABB {
            ll: Vec2::splat(f32::INFINITY),
            ur: Vec2::splat(f32::NEG_INFINITY),
        },
        |bounds, point| AABB {
            ll: bounds.ll.min(point),
            ur: bounds.ur.max(point),
        },
    )
}

fn overlaps(a: AABB, b: AABB) -> bool {
    a.ll.x <= b.ur.x && b.ll.x <= a.ur.x && a.ll.y <= b.ur.y && b.ll.y <= a.ur.y
}
