mod binfos;
mod coverage;
mod dispatch;
mod electricity;
mod environment;
mod itinerary;
mod parking;
mod router;
pub mod transit;
mod utilities;

pub use binfos::*;
pub use coverage::*;
pub use dispatch::*;
pub use electricity::*;
pub use environment::*;
pub use itinerary::*;
pub use parking::*;
pub use router::*;
pub use transit::{BusRun, LineID, StopID, Transit, TransitLine, TransitStop};
pub use utilities::*;

use crate::map::{Building, GrowableBusinessState};
use crate::{CompanyID, SoulID, World};
use prototypes::{try_prototype, GoodsCompanyPrototype};

/// The company role attached to the live address and owned by the matching entity.
#[derive(Clone, Copy, Debug)]
pub struct ActiveBusiness {
    pub company: CompanyID,
    pub prototype: &'static GoodsCompanyPrototype,
    pub slots: u32,
    pub filled: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct BusinessRates {
    pub staffed_fraction: f64,
    pub full_speed_batch_ticks: Option<f64>,
    pub operating_factor: f64,
}

impl ActiveBusiness {
    pub fn rates(self) -> BusinessRates {
        BusinessRates {
            staffed_fraction: if self.slots == 0 {
                0.0
            } else {
                (self.filled as f64 / f64::from(self.slots)).min(1.0)
            },
            full_speed_batch_ticks: self
                .prototype
                .recipe
                .as_ref()
                .map(|recipe| recipe.batch_ticks()),
            operating_factor: 1.0,
        }
    }
}

pub fn active_business(
    building: &Building,
    infos: &BuildingInfos,
    world: &World,
) -> Option<ActiveBusiness> {
    if building.development().is_some_and(|site| {
        building.is_abandoned_growable() || site.business != Some(GrowableBusinessState::Open)
    }) {
        return None;
    }
    let proto = building.kind.as_goods_company()?;
    let SoulID::GoodsCompany(company) = infos.owner(building.id)? else {
        return None;
    };
    let entity = world.companies.get(company)?;
    if entity.comp.building != building.id || entity.comp.proto != proto {
        return None;
    }
    Some(ActiveBusiness {
        company,
        prototype: try_prototype(proto)?,
        slots: building
            .development()
            .map_or(entity.comp.max_workers, |site| site.planned_jobs),
        filled: entity.workers.0.len(),
    })
}
