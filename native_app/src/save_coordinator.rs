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

#[derive(Clone)]
pub(crate) struct SavePermit {
    pub id: u64,
    generation: u64,
    cancelled: Arc<AtomicBool>,
}

impl SavePermit {
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl SaveCoordinator {
    pub(crate) fn request(&self, generation: u64, slot: &SlotId) -> Option<SavePermit> {
        let mut state = self.0.try_lock().ok()?;
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

    /// The caller completes capture and encoding before publishing.
    /// Hold the gate through the snapshot rename and the sidecar updates.
    pub(crate) fn publish<T>(&self, permit: &SavePermit, publish: impl FnOnce() -> T) -> Option<T> {
        let state = self.0.lock().unwrap();
        state.allows(permit).then(publish)
    }

    pub(crate) fn delete(&self, generation: u64, root: &Path, id: &SlotId) -> std::io::Result<()> {
        let state = self.0.lock().unwrap();
        if state.generation != generation {
            return Err(std::io::Error::other(
                "The city changed before the deletion.",
            ));
        }
        // A save still encoding must not recreate a slot the user just deleted.
        if let Some((slot, request)) = &state.request {
            if slot == id {
                cancel(request);
            }
        }
        saveload::delete_slot(root, id)
    }

    pub(crate) fn try_swap(&self, generation: u64) -> bool {
        let Ok(mut state) = self.0.try_lock() else {
            return false;
        };
        state.swap(generation);
        true
    }

    #[cfg(test)]
    pub(crate) fn swap(&self, generation: u64, install: impl FnOnce()) {
        let mut state = self.0.lock().unwrap();
        state.swap(generation);
        install();
    }
}

impl Publication {
    fn swap(&mut self, generation: u64) {
        // A cancelled worker may still own a capture. Keep its permit reserved
        // until it releases those buffers, including across a generation swap.
        if let Some((_, request)) = &self.request {
            cancel(request);
        }
        self.generation = generation;
    }

    fn allows(&self, permit: &SavePermit) -> bool {
        self.generation == permit.generation && !permit.cancelled()
    }
}

fn cancel(request: &Weak<AtomicBool>) {
    if let Some(request) = request.upgrade() {
        request.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_save_reserves_one_capture_across_generation_swap() {
        let coordinator = SaveCoordinator::default();
        let slot = SlotId::legacy();
        let permit = coordinator.request(0, &slot).unwrap();
        assert!(coordinator.try_swap(1));
        assert!(permit.cancelled());
        assert!(coordinator.publish(&permit, || ()).is_none());
        assert!(coordinator.request(1, &slot).is_none());
        drop(permit);
        assert!(coordinator.request(1, &slot).is_some());
    }
}
