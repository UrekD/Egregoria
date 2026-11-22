use std::cell::RefCell;
use std::time::{Duration, Instant};

use goryak::{
    mincolumn, on_primary_container, on_secondary_container, textc, VertScrollSize, Window,
};
use prototypes::{try_prototype, TICKS_PER_MINUTE};
use simulation::map::{BuildingKind, LaneKind, Map, Road, RoadID};
use simulation::transportation::road_throughput::ROAD_HISTORY_TICKS;
use simulation::transportation::traffic_metrics::{
    road_network_revision, sample_road_users, RoadSampler, RoadTraffic, RoadUsers, TruckCargo,
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

pub(crate) fn inspect_road(_uiw: &UiWorld, sim: &Simulation, road: RoadID) -> bool {
    let map = sim.map();
    let Some(segment) = map.get(road) else {
        return false;
    };
    let (entries, duration) = sim.read::<RoadThroughput>().current_window(road);

    let mut opened = true;
    SAMPLE.with_borrow_mut(|cached| {
        let now = Instant::now();
        let revision = road_network_revision(&map);
        if cached.as_ref().is_none_or(|sample| {
            sample.road != road || sample.revision != revision || now - sample.at >= SAMPLE_INTERVAL
        }) {
            let mut sampler = RoadSampler::new(&map, std::iter::once(road));
            *cached = Some(RoadSample {
                road,
                revision,
                at: now,
                users: sample_road_users(sim, road),
                traffic: sampler.sample(sim).remove(&road),
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
                    road_details(&map, segment);
                    mincolumn(3.0, || {
                        textc(on_primary_container(), "On this road now");
                        traffic_counts([users.cars, users.trucks, users.buses, users.pedestrians]);
                    });
                    mincolumn(3.0, || {
                        textc(on_primary_container(), "Entered in last 10 game minutes");
                        if duration < ROAD_HISTORY_TICKS {
                            let minutes = duration as f64 / TICKS_PER_MINUTE as f64;
                            textc(
                                on_secondary_container(),
                                format!("Collecting history · {minutes:.1} of 10 min"),
                            );
                        }
                        traffic_counts(entries);
                    });
                    let congestion = traffic.map_or_else(
                        || "Congestion · no speed samples".to_owned(),
                        |sample| {
                            format!(
                                "Congestion · {:.0}% of free-flow speed",
                                100.0 * sample.rel_speed
                            )
                        },
                    );
                    textc(on_primary_container(), congestion);
                    inspect_utility::render(sim, road);
                    truck_cargo(&map, users);
                });
            });
        });
    });
    opened
}

fn road_details(map: &Map, road: &Road) {
    let mut driving = 0;
    let mut rail = 0;
    let mut walking = 0;
    let mut limit: f32 = 0.0;
    for (id, kind) in road.lanes_iter() {
        match kind {
            LaneKind::Driving | LaneKind::Bus | LaneKind::Biking => driving += 1,
            LaneKind::Rail => rail += 1,
            LaneKind::Walking => walking += 1,
            LaneKind::Parking => continue,
        }
        if kind != LaneKind::Walking {
            limit = limit.max(map.get(id).map_or(0.0, |lane| lane.speed_limit));
        }
    }
    let elevated = Road::pylons_positions(road.interfaced_points(), &map.environment)
        .next()
        .is_some();
    let kind = match (driving > 0, rail > 0, elevated) {
        (true, _, false) => "Street",
        (true, _, true) => "Road bridge",
        (false, true, false) => "Railway",
        (false, true, true) => "Rail bridge",
        (false, false, false) => "Footpath",
        (false, false, true) => "Footbridge",
    };
    let lanes = [(driving, "traffic"), (rail, "rail"), (walking, "walking")]
        .into_iter()
        .filter(|&(count, _)| count > 0)
        .map(|(count, lane)| format!("{count} {lane}"))
        .collect::<Vec<_>>()
        .join(" · ");
    mincolumn(3.0, || {
        textc(on_primary_container(), kind);
        textc(on_secondary_container(), format!("Lanes · {lanes}"));
        if limit > 0.0 {
            textc(
                on_secondary_container(),
                format!("Speed limit · {:.0} km/h", 3.6 * limit),
            );
        }
    });
}

fn traffic_counts([cars, trucks, buses, pedestrians]: [u32; 4]) {
    textc(
        on_secondary_container(),
        format!("Cars {cars}  ·  Trucks {trucks}  ·  Buses {buses}"),
    );
    textc(
        on_secondary_container(),
        format!("Pedestrians {pedestrians}"),
    );
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
