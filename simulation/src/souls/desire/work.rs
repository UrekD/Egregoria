use crate::economy::cargo::LoadId;
use crate::map::BuildingID;
use crate::map_dynamic::{Destination, Router};
use crate::souls::human::HumanDecisionKind;
use crate::transportation::Location;
use crate::world::{CompanyID, VehicleID};
use egui_inspect::Inspect;
use prototypes::{DayTime, GameTime, RecTimeInterval, MINUTES_PER_HOUR, SECONDS_PER_MINUTE};
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum WorkKind {
    Driver {
        deliver_order: Option<BuildingID>,
        truck: VehicleID,
        #[serde(default)]
        load: Option<LoadId>,
    },
    Worker,
}
debug_inspect_impl!(WorkKind);

#[derive(Inspect, Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub workplace: BuildingID,
    pub work_inter: RecTimeInterval,
    pub kind: WorkKind,
    pub last_score: f32,
    #[inspect(skip)]
    #[serde(default)]
    pub(crate) employer: Option<CompanyID>,
    #[inspect(skip)]
    #[serde(default)]
    attendance: Option<(CompanyID, i32)>,
}

impl Work {
    pub fn new(workplace: BuildingID, kind: WorkKind, offset: f32) -> Self {
        Work {
            workplace,
            work_inter: RecTimeInterval::new(
                (8, (offset * MINUTES_PER_HOUR as f32) as i32),
                (18, (offset * MINUTES_PER_HOUR as f32) as i32),
            ),
            kind,
            last_score: 0.0,
            employer: None,
            attendance: None,
        }
    }

    pub fn apply(&mut self, loc: &Location, router: &Router) -> HumanDecisionKind {
        use HumanDecisionKind::*;
        match self.kind {
            WorkKind::Worker => GoTo(Destination::Building(self.workplace)),
            WorkKind::Driver {
                deliver_order,
                truck,
                load,
            } => {
                if &Location::Building(self.workplace) != loc {
                    MultiStack(vec![
                        GoTo(Destination::Building(self.workplace)),
                        SetVehicle(router.personal_car),
                    ])
                } else if let Some(b) = deliver_order {
                    MultiStack(vec![
                        SetVehicle(router.personal_car),
                        GoTo(Destination::Building(self.workplace)),
                        load.map_or(DeliverAtBuilding(b), |load| DeliverLoad {
                            destination: b,
                            load,
                        }),
                        GoTo(Destination::Building(b)),
                        SetVehicle(Some(truck)),
                    ])
                } else {
                    Yield
                }
            }
        }
    }

    pub(crate) fn record_attendance(&mut self, time: &GameTime, employer: CompanyID) {
        if self.employer == Some(employer) && self.work_inter.is_active(&time.daytime) {
            self.attendance = Some((employer, time.daytime.day));
        }
    }

    pub(crate) fn attended_today(&self, time: &GameTime, live_employer: Option<CompanyID>) -> bool {
        live_employer.is_some_and(|employer| {
            self.employer == Some(employer) && self.attendance == Some((employer, time.daytime.day))
        })
    }

    pub(crate) fn shift_ending(&self, time: &GameTime, minutes: i32) -> bool {
        let later = DayTime::new(
            time.daytime
                .gamesec()
                .saturating_add(minutes * SECONDS_PER_MINUTE),
        );
        self.work_inter.is_active(&time.daytime) && !self.work_inter.is_active(&later)
    }

    pub(crate) fn shift_score(
        &self,
        time: &GameTime,
        loc: &Location,
        attended: Option<(BuildingID, i32)>,
    ) -> f32 {
        if self.work_inter.dist_start(&time.daytime) != 0 {
            return 0.0;
        }
        let at_work = *loc == Location::Building(self.workplace);
        if at_work || attended != Some((self.workplace, time.daytime.day)) {
            1.0
        } else {
            0.0
        }
    }

    pub fn score(&self, time: &GameTime) -> f32 {
        if self.work_inter.dist_start(&time.daytime) == 0 {
            0.5
        } else {
            0.0
        }
    }
}
