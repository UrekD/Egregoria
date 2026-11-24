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
            let Some(v) = E::remove(&mut sim.world, entity) else {
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
    use crate::world::{HumanEnt, HumanID, VehicleEnt, VehicleID};
    use slotmapd::HopSlotMap;

    #[test]
    fn scale_t0_serial_callback_order() {
        let mut sim = TestCtx::new().g;
        sim.insert(Vec::<usize>::new());

        let mut ids = HopSlotMap::<VehicleID, ()>::with_key();
        let first = ids.insert(());
        let second = ids.insert(());
        let third = ids.insert(());
        ids.remove(first).unwrap();
        let reused = ids.insert(());
        let keys = [
            first,
            second,
            third,
            reused,
            ids.insert(()),
            ids.insert(()),
            ids.insert(()),
        ];

        {
            let buffer = sim.read::<ParCommandBuffer<VehicleEnt>>();
            for ordinal in 0..64 {
                let key = keys[(ordinal * 5) % keys.len()];
                buffer.exec_on(key, move |trace: &mut Vec<usize>| trace.push(ordinal));
            }
        }
        sim.apply_commands([]);
        // Captured from the pre-T0 serial drain with Rust 1.95. Some repeated keys
        // run out of enqueue order when the queue is this large.
        assert_eq!(
            sim.read::<Vec<usize>>().as_slice(),
            [
                0, 63, 7, 14, 21, 28, 35, 42, 49, 56, 30, 16, 23, 9, 2, 37, 44, 51, 58, 3, 17, 31,
                10, 38, 24, 45, 52, 59, 34, 41, 48, 13, 55, 6, 27, 62, 20, 5, 33, 19, 40, 12, 47,
                26, 54, 61, 29, 36, 50, 15, 1, 22, 57, 8, 43, 25, 11, 4, 53, 39, 60, 32, 18, 46,
            ],
        );

        sim.write::<Vec<usize>>().clear();
        sim.read::<ParCommandBuffer<VehicleEnt>>()
            .exec_ent(first, move |sim| {
                sim.write::<Vec<usize>>().push(1);
                sim.read::<ParCommandBuffer<HumanEnt>>()
                    .exec_ent(HumanID::default(), move |sim| {
                        sim.write::<Vec<usize>>().push(2);
                        sim.read::<ParCommandBuffer<VehicleEnt>>()
                            .exec_on(reused, |trace: &mut Vec<usize>| trace.push(3));
                    });
            });
        sim.apply_commands([]);
        assert_eq!(sim.read::<Vec<usize>>().as_slice(), [1, 2, 3]);
    }
}
