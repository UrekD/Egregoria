use std::borrow::Cow;
use std::cell::RefCell;
use std::time::{Duration, Instant};

use goryak::{
    mincolumn, on_primary_container, on_secondary_container, textc, VertScrollSize, Window,
};
use prototypes::{try_prototype, TICKS_PER_MINUTE};
use simulation::map::{BuildingKind, LaneKind, Map, Road, RoadID};
use simulation::transportation::road_throughput::ROAD_HISTORY_TICKS;
use simulation::transportation::traffic_metrics::{
    road_is_elevated, road_network_revision, sample_road_users, RoadSampler, RoadTraffic,
    RoadUsers, TruckCargo,
};
use simulation::transportation::RoadThroughput;
use simulation::Simulation;
use slotmapd::Key;
use yakui::widgets::Pad;

use super::inspect_utility;
use crate::uiworld::UiWorld;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

struct RoadSample {
    road: RoadID,
    revision: (u64, u64),
    at: Instant,
    users: RoadUsers,
    traffic: Option<RoadTraffic>,
}

thread_local! {
    static SAMPLE: RefCell<Option<RoadSample>> = const { RefCell::new(None) };
}

pub(crate) fn inspect_road(uiw: &UiWorld, sim: &Simulation, road: RoadID) -> bool {
    let map = sim.map();
    let Some(segment) = map.get(road) else {
        return false;
    };
    let details = RoadDetails::new(&map, segment);
    let has_vehicles = details.driving > 0;
    let has_pedestrians = details.walking > 0;
    let has_traffic = has_vehicles || has_pedestrians;

    let mut opened = true;
    SAMPLE.with_borrow_mut(|cached| {
        let now = Instant::now();
        let revision = road_network_revision(&map);
        if cached.as_ref().is_none_or(|sample| {
            sample.road != road || sample.revision != revision || now - sample.at >= SAMPLE_INTERVAL
        }) {
            *cached = Some(RoadSample {
                road,
                revision,
                at: now,
                users: if has_traffic {
                    sample_road_users(sim, road)
                } else {
                    RoadUsers::default()
                },
                traffic: has_vehicles
                    .then(|| {
                        RoadSampler::new(&map, std::iter::once(road))
                            .sample(sim)
                            .remove(&road)
                    })
                    .flatten(),
            });
        }
        let RoadSample { users, traffic, .. } = cached.as_ref().unwrap();

        Window {
            title: format!("Road {:?}", road.data()).into(),
            pad: Pad::all(14.0),
            radius: 10.0,
            opened: &mut opened,
            child_spacing: 0.0,
        }
        .show(|| {
            VertScrollSize::Fixed(465.0).show(|| {
                mincolumn(12.0, || {
                    details.render();
                    crate::gui::traffic_repair::render_road(uiw, sim, &map, segment);
                    if has_traffic {
                        let (entries, duration) = sim.read::<RoadThroughput>().current_window(road);
                        mincolumn(3.0, || {
                            textc(on_primary_container(), "On this road now");
                            traffic_counts(
                                [users.cars, users.trucks, users.buses, users.pedestrians],
                                has_vehicles,
                                has_pedestrians,
                            );
                        });
                        mincolumn(3.0, || {
                            textc(on_primary_container(), history_label(duration));
                            traffic_counts(entries, has_vehicles, has_pedestrians);
                        });
                    }
                    if has_vehicles {
                        mincolumn(3.0, || {
                            textc(on_primary_container(), speed_label(*traffic));
                            for note in speed_snapshot_notes() {
                                textc(on_secondary_container(), note);
                            }
                        });
                    }
                    inspect_utility::render(sim, road);
                    truck_cargo(&map, users);
                });
            });
        });
    });
    opened
}

fn history_label(duration: u64) -> String {
    if duration >= ROAD_HISTORY_TICKS {
        "Entered in last 10 game minutes".to_owned()
    } else {
        let tenths = duration * 10 / TICKS_PER_MINUTE;
        format!("Entered in last {}.{} game min", tenths / 10, tenths % 10)
    }
}

fn speed_label(traffic: Option<RoadTraffic>) -> String {
    let Some(sample) = traffic else {
        return "Speed snapshot · no usable samples".to_owned();
    };
    let percent = (100.0 * sample.rel_limit_speed).round();
    let band = if percent >= 80.0 {
        "Free"
    } else if percent >= 50.0 {
        "Busy"
    } else {
        "Jammed"
    };
    let unit = if sample.samples == 1 {
        "vehicle"
    } else {
        "vehicles"
    };
    format!(
        "Speed snapshot · {percent:.0}% of lane limit · {band} ({} {unit})",
        sample.samples
    )
}

fn speed_snapshot_notes() -> [Cow<'static, str>; 3] {
    [
        format!(
            "Mean of lane-limit percentages · refreshes every {} ms of real time",
            SAMPLE_INTERVAL.as_millis()
        )
        .into(),
        "Both snapshots exclude parked vehicles".into(),
        "Bus dwell counts toward occupancy only".into(),
    ]
}

struct RoadDetails {
    kind: &'static str,
    driving: u32,
    biking: u32,
    rail: u32,
    walking: u32,
    limit: f32,
}

impl RoadDetails {
    fn new(map: &Map, road: &Road) -> Self {
        let mut driving = 0;
        let mut biking = 0;
        let mut rail = 0;
        let mut walking = 0;
        let mut driving_limit: f32 = 0.0;
        let mut other_limit: f32 = 0.0;
        for (id, kind) in road.lanes_iter() {
            let limit = map.get(id).map_or(0.0, |lane| lane.speed_limit);
            match kind {
                LaneKind::Driving | LaneKind::Bus => {
                    driving += 1;
                    driving_limit = driving_limit.max(limit);
                }
                LaneKind::Biking => {
                    biking += 1;
                    other_limit = other_limit.max(limit);
                }
                LaneKind::Rail => {
                    rail += 1;
                    other_limit = other_limit.max(limit);
                }
                LaneKind::Walking => walking += 1,
                LaneKind::Parking => {}
            }
        }
        let kind = match (driving + biking > 0, rail > 0, road_is_elevated(road)) {
            (true, _, false) => "Street",
            (true, _, true) => "Road bridge",
            (false, true, false) => "Railway",
            (false, true, true) => "Rail bridge",
            (false, false, false) => "Footpath",
            (false, false, true) => "Footbridge",
        };
        let limit = if driving > 0 {
            driving_limit
        } else {
            other_limit
        };
        Self {
            kind,
            driving,
            biking,
            rail,
            walking,
            limit,
        }
    }

    fn render(&self) {
        let lanes = [
            (self.driving, "traffic"),
            (self.biking, "biking"),
            (self.rail, "rail"),
            (self.walking, "walking"),
        ]
        .into_iter()
        .filter(|&(count, _)| count > 0)
        .map(|(count, lane)| format!("{count} {lane}"))
        .collect::<Vec<_>>()
        .join(" · ");
        mincolumn(3.0, || {
            textc(on_primary_container(), self.kind);
            textc(on_secondary_container(), format!("Lanes · {lanes}"));
            if self.limit > 0.0 {
                textc(
                    on_secondary_container(),
                    format!("Speed limit · {:.0} km/h", 3.6 * self.limit),
                );
            }
        });
    }
}

fn traffic_counts(
    [cars, trucks, buses, pedestrians]: [u32; 4],
    has_vehicles: bool,
    has_pedestrians: bool,
) {
    if has_vehicles {
        textc(
            on_secondary_container(),
            format!("Cars {cars}  ·  Trucks {trucks}  ·  Buses {buses}"),
        );
    }
    if has_pedestrians {
        textc(
            on_secondary_container(),
            format!("Pedestrians {pedestrians}"),
        );
    }
}

fn truck_cargo(map: &Map, users: &RoadUsers) {
    if users.cargo.is_empty() {
        return;
    }
    mincolumn(4.0, || {
        textc(on_primary_container(), "Truck cargo");
        for row in &users.cargo {
            let destination =
                row.destination
                    .and_then(|id| map.get(id))
                    .map(|building| match building.kind {
                        BuildingKind::GoodsCompany(id) => id.prototype().label.as_str(),
                        BuildingKind::RailFreightStation(id) => id.prototype().label.as_str(),
                        BuildingKind::House => "House",
                        BuildingKind::TrainStation => "Train station",
                        BuildingKind::ExternalTrading => "External trading",
                        BuildingKind::ParkingGarage(kind) => kind.label(),
                    });
            let truck = format!("Truck {:?}", row.truck.data());
            match &row.cargo {
                TruckCargo::Manifest(items) => {
                    textc(on_secondary_container(), truck);
                    for item in items {
                        let name =
                            try_prototype(item.item).map_or("Unknown item", |item| &item.label);
                        let destination = destination.unwrap_or("destination unknown");
                        textc(
                            on_secondary_container(),
                            format!("{name} × {} to {destination}", item.units),
                        );
                    }
                }
                TruckCargo::EmptyReturn => {
                    textc(on_secondary_container(), format!("{truck} · Empty return"))
                }
                TruckCargo::Unknown => {
                    let text = destination.map_or_else(
                        || format!("{truck} · Cargo unknown"),
                        |destination| format!("{truck} · Cargo unknown · To {destination}"),
                    );
                    textc(on_secondary_container(), text);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec3;
    use scenarios::ScenarioCtx;
    use simulation::map::{LanePatternBuilder, MapProject, ProjectKind};
    use simulation::SimulationOptions;

    #[test]
    fn u5_qw1_road_inspector_labels_speed_and_window() {
        assert_eq!(history_label(0), "Entered in last 0.0 game min");
        assert_eq!(
            history_label(ROAD_HISTORY_TICKS - 1),
            "Entered in last 9.9 game min"
        );
        assert_eq!(
            history_label(ROAD_HISTORY_TICKS),
            "Entered in last 10 game minutes"
        );
        assert_eq!(speed_label(None), "Speed snapshot · no usable samples");
        assert_eq!(
            speed_snapshot_notes().each_ref().map(|note| note.as_ref()),
            [
                "Mean of lane-limit percentages · refreshes every 250 ms of real time",
                "Both snapshots exclude parked vehicles",
                "Bus dwell counts toward occupancy only",
            ]
        );
        for (ratio, label) in [(0.0, "Jammed"), (0.5, "Busy"), (0.8, "Free")] {
            let sample = RoadTraffic {
                vehicles: 1,
                samples: 1,
                rel_speed: 1.0,
                rel_limit_speed: ratio,
            };
            assert_eq!(
                speed_label(Some(sample)),
                format!(
                    "Speed snapshot · {:.0}% of lane limit · {label} (1 vehicle)",
                    ratio * 100.0
                )
            );
        }
    }

    #[test]
    fn u5_road_inspector_classifies_short_decks_without_pylons() {
        let ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            ..SimulationOptions::LEGACY
        });
        let mut map = ctx.sim.write::<Map>();
        for (index, pattern, kind, traffic_lanes) in [
            (0, LanePatternBuilder::default(), "Road bridge", (2, 2)),
            (
                1,
                LanePatternBuilder::default().rail(true),
                "Rail bridge",
                (0, 0),
            ),
            (
                2,
                LanePatternBuilder::default().n_lanes(0),
                "Footbridge",
                (0, 2),
            ),
        ] {
            let y = 1400.0 + 100.0 * index as f32;
            let ground = [1400.0, 1460.0]
                .map(|x| map.environment().height(vec3(x, y, 0.0).xy()).unwrap())
                .into_iter()
                .reduce(f32::max)
                .unwrap();
            let endpoint = |x| MapProject {
                pos: vec3(x, y, ground + 15.0),
                kind: ProjectKind::Ground,
            };
            let (_, id) = map
                .make_connection(endpoint(1400.0), endpoint(1460.0), None, &pattern.build())
                .unwrap();
            let road = map.get(id).unwrap();
            assert!(
                Road::pylons_positions(road.interfaced_points(), map.environment())
                    .next()
                    .is_none()
            );
            let details = RoadDetails::new(&map, road);
            assert_eq!(details.kind, kind);
            assert_eq!((details.driving, details.walking), traffic_lanes);
        }
    }
}
