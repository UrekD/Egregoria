// Find template in _template.rs
crate::gen_prototypes!(
    mod item:          ItemID              = ItemPrototype,
    mod building:      BuildingPrototypeID = BuildingPrototype,
    mod residence:     ResidencePrototypeID = ResidencePrototype => BuildingPrototypeID,
    mod goods_company: GoodsCompanyID      = GoodsCompanyPrototype => BuildingPrototypeID,
    mod growable_company: GrowableCompanyID = GrowableCompanyPrototype => GoodsCompanyID,
    mod leisure:       LeisurePrototypeID  = LeisurePrototype => BuildingPrototypeID,
    mod solar:         SolarPanelID        = SolarPanelPrototype => GoodsCompanyID,
    mod service:       ServiceID           = ServicePrototype => GoodsCompanyID,

    mod vehicle:       VehiclePrototypeID = VehiclePrototype,
    mod road_vehicle:  RoadVehicleID      = RoadVehiclePrototype => VehiclePrototypeID,
    mod rolling_stock: RollingStockID     = RollingStockPrototype => VehiclePrototypeID,

    mod colors:         ColorsPrototypeID   = ColorsPrototype,
    mod freightstation: FreightStationPrototypeID = FreightStationPrototype,
);

mod base;
mod growable_massing;
pub use base::*;
pub use growable_massing::*;
