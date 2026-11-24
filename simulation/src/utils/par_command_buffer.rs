use crate::world::Entity;
use crate::Simulation;
use std::sync::Mutex;

pub trait SimDrop: Entity {
    fn sim_drop(self, id: Self::ID, sim: &mut Simulation);
}

type ExecType = Box<dyn for<'a> FnOnce(&'a mut Simulation) + Send>;

pub struct ParCommandBuffer<E: SimDrop> {
    to_kill: Mutex<Vec<E::ID>>,
    exec_ent: Mutex<Vec<(E::ID, ExecType)>>,
}

impl<E: SimDrop> Default for ParCommandBuffer<E> {
    fn default() -> Self {
        Self {
            to_kill: Default::default(),
            exec_ent: Default::default(),
        }
    }
}

#[allow(clippy::unwrap_used)]
impl<E: SimDrop> ParCommandBuffer<E> {
    pub fn kill(&self, e: E::ID) {
        self.to_kill.lock().unwrap().push(e);
    }

    pub fn kill_all(&self, e: &[E::ID]) {
        self.to_kill.lock().unwrap().extend_from_slice(e);
    }

    pub fn exec_ent(&self, e: E::ID, f: impl for<'a> FnOnce(&'a mut Simulation) + 'static + Send) {
        self.exec_ent.lock().unwrap().push((e, Box::new(f)));
    }

    pub fn exec_on<T: Send + Sync + 'static>(
        &self,
        e: E::ID,
        f: impl for<'a> FnOnce(&'a mut T) + 'static + Send,
    ) {
        self.exec_ent(e, move |sim| {
            f(&mut *sim.write::<T>());
        })
    }

    /// Applies queued deletions and callbacks. Returns whether any work was queued.
    pub fn apply(sim: &mut Simulation) -> bool {
        profiling::scope!("par_command_buffer::apply");
        let mut deleted: Vec<E::ID> = std::mem::take(
            &mut *sim
                .write::<ParCommandBuffer<E>>()
                .to_kill
                .get_mut()
                .unwrap(),
        );

        let had_kills = !deleted.is_empty();
        deleted.sort_unstable();

        for entity in deleted {
            let Some(v) = E::storage_mut(&mut sim.world).remove(entity) else {
                continue;
            };

            E::sim_drop(v, entity, sim);
        }

        let mut exec_ent = std::mem::take(
            &mut *sim
                .write::<ParCommandBuffer<E>>()
                .exec_ent
                .get_mut()
                .unwrap(),
        );

        let had_callbacks = !exec_ent.is_empty();
        exec_ent.sort_unstable_by_key(|(id, _)| *id);

        for (_, exec) in exec_ent {
            exec(sim);
        }
        had_kills || had_callbacks
    }
}

#[cfg(test)]
mod tests {
    use super::ParCommandBuffer;
    use crate::tests::TestCtx;
    use crate::world::{VehicleEnt, VehicleID};
    use slotmapd::HopSlotMap;

    #[test]
    fn scale_t0_serial_callback_order() {
        let mut sim = TestCtx::new().g;
        sim.insert(Vec::<&str>::new());

        let mut ids = HopSlotMap::<VehicleID, ()>::with_key();
        let first = ids.insert(());
        let second = ids.insert(());
        let third = ids.insert(());
        ids.remove(first).unwrap();
        let reused = ids.insert(());

        let buffer = sim.read::<ParCommandBuffer<VehicleEnt>>();
        for (id, label) in [
            (third, "third a"),
            (reused, "reused a"),
            (second, "second a"),
            (first, "original generation"),
            (third, "third b"),
            (second, "second b"),
            (reused, "reused b"),
        ] {
            buffer.exec_on(id, move |trace: &mut Vec<&str>| trace.push(label));
        }
        drop(buffer);

        assert!(ParCommandBuffer::<VehicleEnt>::apply(&mut sim));
        assert_eq!(
            sim.read::<Vec<&str>>().as_slice(),
            [
                "original generation",
                "reused a",
                "reused b",
                "second a",
                "second b",
                "third a",
                "third b",
            ],
        );
        assert!(!ParCommandBuffer::<VehicleEnt>::apply(&mut sim));
    }
}
