use common::FastMap;
use engine::meshload::load_scaled_mesh;
use engine::{FrameContext, GfxContext, InstancedMeshBuilder, SpriteBatchBuilder};
use geom::{LinearColor, Vec3, V3};
use prototypes::{RenderAsset, RollingStockID, RollingStockPrototype};
use std::time::Instant;

use super::world_snapshot::{ActorKind, SelectionView, WorldView};

pub struct InstancedRender {
    pub path_not_found: SpriteBatchBuilder<true>,
    pub rolling_stock: FastMap<RollingStockID, InstancedMeshBuilder<true>>,
    pub cars: InstancedMeshBuilder<true>,
    // pub locomotives: InstancedMeshBuilder<true>,
    // pub wagons_passenger: InstancedMeshBuilder<true>,
    // pub wagons_freight: InstancedMeshBuilder<true>,
    pub trucks: InstancedMeshBuilder<true>,
    pub buses: InstancedMeshBuilder<true>,
    pub pedestrians: InstancedMeshBuilder<true>,
}

impl InstancedRender {
    pub fn new(gfx: &mut GfxContext) -> Self {
        defer!(log::info!("finished init of instanced render"));

        let mut rolling_stock = FastMap::default();
        RollingStockPrototype::iter()
            .map(|rail_wagon_proto| (&rail_wagon_proto.asset, rail_wagon_proto.id))
            .filter_map(|(asset, id)| {
                let RenderAsset::Mesh { path } = asset else {
                    None?
                };
                match gfx.mesh(path) {
                    Err(e) => {
                        log::error!("Failed to load mesh {}: {:?}", asset, e);
                        None
                    }
                    Ok(m) => Some((id, m)),
                }
            })
            .for_each(|(id, mesh)| {
                rolling_stock.insert(id, InstancedMeshBuilder::new_ref(&mesh));
            });

        let car = gfx.mesh("simple_car.glb".as_ref()).unwrap();
        InstancedRender {
            path_not_found: SpriteBatchBuilder::new(
                &gfx.texture("assets/sprites/path_not_found.png", "path_not_found"),
                gfx,
            ),

            rolling_stock,

            cars: InstancedMeshBuilder::new_ref(&car),
            // locomotives: InstancedMeshBuilder::new_ref(&gfx.mesh("train.glb".as_ref()).unwrap()),
            // wagons_freight: InstancedMeshBuilder::new_ref(&gfx.mesh("wagon_freight.glb".as_ref()).unwrap()),
            // wagons_passenger: InstancedMeshBuilder::new_ref(&gfx.mesh("wagon.glb".as_ref()).unwrap()),
            trucks: InstancedMeshBuilder::new_ref(&gfx.mesh("truck.glb".as_ref()).unwrap()),
            buses: InstancedMeshBuilder::new(
                load_scaled_mesh(gfx, "truck.glb".as_ref(), 1.5).unwrap(),
            ),
            pedestrians: InstancedMeshBuilder::new_ref(
                &gfx.mesh("pedestrian.glb".as_ref()).unwrap(),
            ),
        }
    }

    pub fn render(
        &mut self,
        view: &WorldView,
        previous: Option<&WorldView>,
        selection: SelectionView,
        fctx: &mut FrameContext<'_>,
    ) {
        profiling::scope!("entity_render::render");
        self.cars.instances.clear();
        self.trucks.instances.clear();
        self.buses.instances.clear();
        self.pedestrians.instances.clear();
        self.rolling_stock
            .values_mut()
            .for_each(|builder| builder.instances.clear());
        let pair = previous.and_then(|previous| {
            let fraction = view.stamp.interpolation_from(
                previous.stamp,
                Instant::now(),
                view.paused || previous.paused,
            );
            (fraction < 1.0).then_some((previous, fraction))
        });
        let previous_actors: FastMap<_, _> = pair
            .into_iter()
            .flat_map(|(previous, _)| {
                previous
                    .entities
                    .actors
                    .iter()
                    .map(|actor| (actor.id, actor))
            })
            .collect();
        let (elapsed_steps, fraction) = pair.map_or((0, 1.0), |(previous, fraction)| {
            (
                view.stamp.completed_step - previous.stamp.completed_step,
                fraction,
            )
        });

        for actor in &view.entities.actors {
            let instance = if selection.contains(actor.id) {
                actor.pose
            } else {
                previous_actors
                    .get(&actor.id)
                    .map_or(actor.pose, |previous| {
                        actor.pose_from(previous, elapsed_steps, fraction)
                    })
            };
            match actor.kind {
                ActorKind::Car => self.cars.instances.push(instance),
                ActorKind::Truck => self.trucks.instances.push(instance),
                ActorKind::Bus => self.buses.instances.push(instance),
                ActorKind::Pedestrian => self.pedestrians.instances.push(instance),
                ActorKind::RollingStock(id) => {
                    if let Some(builder) = self.rolling_stock.get_mut(&id) {
                        builder.instances.push(instance);
                    }
                }
            }
        }

        self.path_not_found.clear();
        for &(_, pos, wait) in &view.entities.reroutes {
            let r = wait as f32 / 200.0;
            let off = 1.0 - r;

            let s = 7.0;
            self.path_not_found.push(
                pos + off * 3.0 * V3::Y + 3.0 * V3::Z,
                Vec3::X,
                LinearColor::RED.a(r),
                (s, s),
            );
        }

        if let Some(x) = self.path_not_found.build(fctx.gfx) {
            fctx.objs.push(Box::new(x));
        }
        if let Some(x) = self.cars.build(fctx.gfx) {
            fctx.objs.push(Box::new(x));
        }
        if let Some(x) = self.trucks.build(fctx.gfx) {
            fctx.objs.push(Box::new(x));
        }
        if let Some(x) = self.buses.build(fctx.gfx) {
            fctx.objs.push(Box::new(x));
        }
        if let Some(x) = self.pedestrians.build(fctx.gfx) {
            fctx.objs.push(Box::new(x));
        }

        self.rolling_stock.iter_mut().for_each(|(_, imb)| {
            if let Some(x) = imb.build(fctx.gfx) {
                fctx.objs.push(Box::new(x));
            }
        });
    }
}
