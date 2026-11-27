use goryak::{mincolumn, minrow, outline, padxy};
use prototypes::{
    prototypes_iter, BuildingGen, RenderAsset, RollingStockID, RollingStockPrototype, Size2D,
};
use simulation::map::BuildingKind;
use simulation::world_command::WorldCommand;
use yakui::widgets::List;
use yakui::{button, divider, label, CrossAxisAlignment, MainAxisAlignment};

use crate::gui::addtrain::TrainSpawnResource;
use crate::gui::specialbuilding::{SpecialBuildKind, SpecialBuildingResource};
use crate::gui::Tool;
use crate::uiworld::UiWorld;

pub fn train_properties(uiw: &UiWorld) {
    let mut state = uiw.write::<TrainSpawnResource>();

    padxy(12.0, 8.0, || {
        let mut l = List::row();
        l.main_axis_alignment = MainAxisAlignment::Start;
        l.cross_axis_alignment = CrossAxisAlignment::Center;
        l.item_spacing = 10.0;
        l.show(|| {
            mincolumn(0.1, || {
                if uiw.can_author() && button("Passenger Station").clicked {
                    *uiw.write::<Tool>() = Tool::SpecialBuilding;
                    uiw.write::<SpecialBuildingResource>().opt = Some(SpecialBuildKind {
                        company: None,
                        make: Box::new(|args| {
                            vec![WorldCommand::MapBuildSpecialBuilding {
                                pos: args.obb,
                                connected_road: args.connected_road,
                                kind: BuildingKind::TrainStation,
                                gen: BuildingGen::House,
                                zone: None,
                            }]
                        }),
                        size: Size2D::new(12.0, 20.0),
                        asset: RenderAsset::Mesh {
                            path: "rail_freight_station.glb".into(),
                        },
                        road_snap: true,
                    });
                }
                if button("remove train").clicked {
                    state.wagons.clear();
                    state.set_zero();
                }
                label(format!("Acceleration: {:.1} m/s^2", state.acceleration));
                label(format!("Deceleration: {:.1} m/s^2", state.deceleration));
                label(format!("Total Lenght: {} m", state.total_lenght.ceil()));
            });

            mincolumn(0.5, || {
                // Rows of a few buttons keep the shelf inside a 1024 px window.
                let mut remove: Option<usize> = None;
                let wagons: Vec<_> = state.wagons.iter().copied().enumerate().collect();
                for row in wagons.chunks(6) {
                    minrow(0.0, || {
                        for &(i, id) in row {
                            if button(RollingStockID::prototype(id).label.clone()).clicked {
                                remove = Some(i);
                            }
                        }
                    });
                }
                if let Some(i) = remove {
                    state.wagons.remove(i);
                    state.calculate();
                }

                divider(outline(), 10.0, 1.0);

                let stock: Vec<_> = prototypes_iter::<RollingStockPrototype>().collect();
                for row in stock.chunks(3) {
                    minrow(0.0, || {
                        for rolling_stock in row {
                            let resp = button(rolling_stock.label.clone());
                            if resp.clicked {
                                state.wagons.push(rolling_stock.id);
                                state.calculate();
                            }
                        }
                    });
                }
            });
        });
    });
}

/*
if ui.button(freightstation).clicked() {
   *uiworld.write::<Tool>() = Tool::SpecialBuilding;

   uiworld.write::<SpecialBuildingResource>().opt = Some(SpecialBuildKind {
       make: Box::new(move |args| {
           let obb = args.obb;
           let c = obb.center().z(args.mpos.z + 0.3);

           let [offx, offy] = obb.axis().map(|x| x.normalize().z(0.0));

           let pat =
               LanePatternBuilder::new().rail(true).one_way(true).build();

           let mut commands = Vec::with_capacity(5);

           commands.push(WorldCommand::MapMakeConnection {
               from: MapProject::ground(c - offx * 45.0 - offy * 100.0),
               to: MapProject::ground(c - offx * 45.0 + offy * 100.0),
               inter: None,
               pat,
           });

           commands.push(WorldCommand::MapBuildSpecialBuilding {
               pos: args.obb,
               kind: BuildingKind::RailFreightStation(proto.id),
               gen: BuildingGen::NoWalkway {
                   door_pos: Vec2::ZERO,
               },
               zone: None,
               connected_road: args.connected_road,
           });
           commands
       }),
       size: proto.size,
       asset: proto.asset.clone(),
       road_snap: false,
   });
}
*/
