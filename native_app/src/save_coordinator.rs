use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use common::saveload::{self, SlotId};

#[derive(Clone, Default)]
pub(crate) struct SaveCoordinator(Arc<Mutex<Publication>>);

#[derive(Default)]
struct Publication {
    generation: u64,
    next_id: u64,
    request: Option<(SlotId, Weak<AtomicBool>)>,
}

pub(crate) struct SavePermit {
    pub id: u64,
    generation: u64,
    cancelled: Arc<AtomicBool>,
}

impl SaveCoordinator {
    pub(crate) fn request(&self, generation: u64, slot: &SlotId) -> Option<SavePermit> {
        let mut state = self.0.lock().unwrap();
        if state.generation != generation
            || state
                .request
                .as_ref()
                .is_some_and(|(_, request)| request.strong_count() != 0)
        {
            return None;
        }
        state.next_id += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        state.request = Some((slot.clone(), Arc::downgrade(&cancelled)));
        Some(SavePermit {
            id: state.next_id,
            generation,
            cancelled,
        })
    }

    pub(crate) fn is_current(&self, permit: &SavePermit) -> bool {
        self.0.lock().unwrap().allows(permit)
    }

    /// The caller releases the simulation read lock before publishing.
    /// Hold the gate through the snapshot rename and the sidecar updates.
    pub(crate) fn publish<T>(&self, permit: &SavePermit, publish: impl FnOnce() -> T) -> Option<T> {
        let state = self.0.lock().unwrap();
        state.allows(permit).then(publish)
    }

    pub(crate) fn delete(&self, root: &Path, id: &SlotId) -> std::io::Result<()> {
        let state = self.0.lock().unwrap();
        // A save still encoding must not recreate a slot the user just deleted.
        if let Some((slot, request)) = &state.request {
            if slot == id {
                cancel(request);
            }
        }
        saveload::delete_slot(root, id)
    }

    /// Call while holding the simulation write lock. Publishers never take that lock.
    pub(crate) fn swap(&self, generation: u64, install: impl FnOnce()) {
        let mut state = self.0.lock().unwrap();
        if let Some((_, request)) = state.request.take() {
            cancel(&request);
        }
        state.generation = generation;
        install();
    }
}

impl Publication {
    fn allows(&self, permit: &SavePermit) -> bool {
        self.generation == permit.generation && !permit.cancelled.load(Ordering::Relaxed)
    }
}

fn cancel(request: &Weak<AtomicBool>) {
    if let Some(request) = request.upgrade() {
        request.store(true, Ordering::Relaxed);
    }
}
