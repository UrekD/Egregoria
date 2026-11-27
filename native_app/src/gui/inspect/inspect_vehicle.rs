use crate::gui::inspect::{entity_link, focus_entity, follow_button};
use crate::gui::traffic_repair::hold_label;
use crate::gui::{GuiState, InspectedEntity};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use common::FastMap;
use geom::{Color, Vec3};
use goryak::{
    button_secondary, mincolumn, minrow, on_secondary_container, primary_link, textc,
    VertScrollSize, Window,
};
use prototypes::{try_prototype, ItemID};
use simulation::economy::cargo::{CargoBook, FreightFragment, FreightTrace, ItemQuantity};
use simulation::economy::TripPurpose;
use simulation::map::{BuildingID, BuildingKind, LaneKind, Map, RoadID};
use simulation::map_dynamic::transit::{StopID, Transit, BUS_CAPACITY};
use simulation::map_dynamic::{Destination, Itinerary, ItineraryTarget, RouterError};
use simulation::souls::desire::WorkKind;
use simulation::souls::freight_station::FreightTrainState;
use simulation::transportation::queue_diagnostics::QueueHoldReason;
use simulation::transportation::road::vehicle_hold_reason;
use simulation::transportation::{Location, VehicleKind, VehicleState};
use simulation::{AnyEntity, HumanID, Simulation, TrainID, VehicleID};
use slotmapd::Key;
use std::collections::{BTreeMap, BTreeSet};
use yakui::widgets::Pad;

#[derive(Default)]
pub(crate) struct ShowVehicleRoute(pub Option<AnyEntity>);

struct TripView {
    kind: &'static str,
    operator: Option<String>,
    operator_link: Option<HumanID>,
    purpose: Option<String>,
    destination: Option<String>,
    state: String,
    remaining: Option<f32>,
    origin: Option<String>,
    cargo: Option<String>,
    reserved_cargo: bool,
    buyer_line: Option<String>,
    passengers: Option<(String, Vec<HumanID>)>,
}

impl TripView {
    fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(5);
        if let Some(operator) = &self.operator {
            lines.push(operator.clone());
        }
        if let Some(purpose) = &self.purpose {
            lines.push(format!("Trip: {purpose}"));
        }
        lines.push(format!(
            "Destination: {}",
            self.destination.as_deref().unwrap_or("None assigned")
        ));
        lines.push(self.state.clone());
        if let Some(meters) = self.remaining {
            let distance = if meters >= 1000.0 {
                format!("{:.1} km", meters / 1000.0)
            } else {
                format!("{meters:.0} m")
            };
            lines.push(format!("{distance} remaining"));
        }
        lines
    }

    fn detail_lines(&self) -> impl Iterator<Item = String> + '_ {
        let origin = self.origin.as_deref().unwrap_or("Unavailable");
        std::iter::once(format!("Origin: {origin}"))
            .chain(self.shipment_lines())
            .chain(
                self.passengers
                    .as_ref()
                    .map(|(count, _)| format!("Passengers: {count}")),
            )
    }

    fn shipment_lines(&self) -> impl Iterator<Item = String> + '_ {
        self.cargo
            .as_ref()
            .map(|cargo| {
                let label = if self.reserved_cargo {
                    "Reserved cargo"
                } else {
                    "Cargo"
                };
                format!("{label}: {cargo}")
            })
            .into_iter()
            .chain(self.buyer_line.iter().cloned())
    }
}

#[derive(Default)]
pub(crate) struct VehicleRouteFrame(Vec<(AnyEntity, Option<VehicleRoute>)>);

struct VehicleRoute {
    points: Vec<Vec3>,
    meters: Option<f32>,
}

impl VehicleRouteFrame {
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    fn get(&mut self, sim: &Simulation, id: AnyEntity) -> Option<&VehicleRoute> {
        if let Some(index) = self.0.iter().position(|(entity, _)| *entity == id) {
            return self.0[index].1.as_ref();
        }
        let route = vehicle_path(sim, &sim.map(), id).map(|points| {
            let meters: f32 = points
                .windows(2)
                .map(|pair| pair[0].distance(pair[1]))
                .sum();
            VehicleRoute {
                points,
                meters: (meters.is_finite() && meters >= 1.0).then_some(meters),
            }
        });
        self.0.push((id, route));
        self.0.last().and_then(|(_, route)| route.as_ref())
    }
}

#[derive(Default)]
pub(crate) struct VehicleOperators {
    snapshot: Option<(u64, usize)>,
    by_vehicle: FastMap<VehicleID, VehicleOperator>,
}

#[derive(Default, Clone, Copy)]
struct VehicleOperator {
    driver: Option<HumanID>,
    owner: Option<HumanID>,
}

impl VehicleOperators {
    fn get(&mut self, sim: &Simulation, id: VehicleID) -> VehicleOperator {
        let humans = &sim.world().humans;
        let snapshot = (sim.get_tick(), humans.len());
        let cached = self.by_vehicle.get(&id).copied().unwrap_or_default();
        if self.snapshot == Some(snapshot)
            || cached.driver.is_some_and(|driver| {
                humans
                    .get(driver)
                    .is_some_and(|human| human.location == Location::Vehicle(id))
            })
        {
            return cached;
        }
        if cached.driver.is_none()
            && cached.owner.is_some_and(|owner| {
                humans.get(owner).is_some_and(|human| {
                    human.router.personal_car == Some(id) && human.location != Location::Vehicle(id)
                })
            })
            && sim
                .get(id)
                .is_some_and(|entity| entity.vehicle.state.is_parked())
        {
            return cached;
        }

        self.by_vehicle.clear();
        for (human_id, human) in humans {
            if let Location::Vehicle(vehicle) = human.location {
                let driver = &mut self.by_vehicle.entry(vehicle).or_default().driver;
                driver.get_or_insert(human_id);
            }
            if let Some(car) = human.router.personal_car {
                self.by_vehicle.entry(car).or_default().owner = Some(human_id);
            }
        }
        self.snapshot = Some(snapshot);
        self.by_vehicle.get(&id).copied().unwrap_or_default()
    }
}

pub(crate) fn vehicle_tooltip(
    uiworld: &UiWorld,
    sim: &Simulation,
    id: AnyEntity,
) -> Option<String> {
    let view = vehicle_trip_view(uiworld, sim, id, false)?;
    let mut lines = view.summary_lines();
    lines.insert(0, view.kind.into());
    lines.extend(view.shipment_lines());
    Some(lines.join("\n"))
}

fn vehicle_trip_view(
    uiworld: &UiWorld,
    sim: &Simulation,
    id: AnyEntity,
    details: bool,
) -> Option<TripView> {
    let id = match id {
        AnyEntity::WagonID(wagon) => sim.get(wagon)?.itfollower.leader.into(),
        _ => id,
    };
    let mut view = trip_view(sim, id, details, &mut uiworld.write::<VehicleOperators>())?;
    view.remaining = uiworld
        .write::<VehicleRouteFrame>()
        .get(sim, id)
        .and_then(|route| route.meters);
    Some(view)
}

pub(crate) fn inspect_vehicle(uiworld: &UiWorld, sim: &Simulation, id: AnyEntity) -> bool {
    let Some(view) = vehicle_trip_view(uiworld, sim, id, true) else {
        return false;
    };
    let mut is_open = true;
    Window {
        title: view.kind.into(),
        pad: Pad::all(10.0),
        radius: 10.0,
        opened: &mut is_open,
        child_spacing: 5.0,
    }
    .show(|| {
        if cfg!(debug_assertions) {
            textc(on_secondary_container(), format!("{id:?}"));
        }
        for (index, line) in view
            .summary_lines()
            .into_iter()
            .chain(view.detail_lines())
            .enumerate()
        {
            match view.operator_link.filter(|_| index == 0) {
                Some(human) => {
                    if primary_link(line) {
                        focus_entity(uiworld, sim, human.into());
                    }
                }
                None => textc(on_secondary_container(), line),
            }
        }
        if let Some((_, passengers)) = &view.passengers {
            if !passengers.is_empty() {
                VertScrollSize::Fixed(210.0).show(|| {
                    mincolumn(4.0, || {
                        for &human in passengers {
                            entity_link(uiworld, sim, human);
                        }
                    });
                });
            }
        }

        minrow(8.0, || {
            follow_button(uiworld, id);
            let mut route = uiworld.write::<ShowVehicleRoute>();
            let showing = route.0 == Some(id);
            if (showing || view.remaining.is_some())
                && button_secondary(if showing { "Hide route" } else { "Show route" })
                    .show()
                    .clicked
            {
                route.0 = (!showing).then_some(id);
            }
        });
    });
    if !is_open {
        uiworld.write::<ShowVehicleRoute>().0 = None;
    }
    is_open
}

pub(crate) fn draw_vehicle_route(sim: &Simulation, uiworld: &UiWorld) {
    let Some(id) = uiworld.read::<ShowVehicleRoute>().0 else {
        return;
    };
    if uiworld.read::<InspectedEntity>().e != Some(id) || uiworld.read::<GuiState>().hidden {
        return;
    }
    let mut frame = uiworld.write::<VehicleRouteFrame>();
    let Some(route) = frame.get(sim, id) else {
        return;
    };
    let mut draw = uiworld.write::<ImmediateDraw>();
    let end = route.points.last().unwrap().up(0.65);
    let points: Vec<_> = route.points.iter().map(|point| point.up(0.65)).collect();
    let color = Color::from_hex(0x56e5ff);
    draw.polyline(points, 1.6, false).color(color);
    draw.stroke_circle(end, 2.2, 0.6).color(color);
}

#[cfg(test)]
thread_local! {
    static ROUTE_BUILD_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn vehicle_path(sim: &Simulation, map: &Map, id: AnyEntity) -> Option<Vec<Vec3>> {
    #[cfg(test)]
    ROUTE_BUILD_COUNT.with(|count| count.set(count.get() + 1));
    match id {
        AnyEntity::VehicleID(id) => {
            let vehicle = sim.get(id)?;
            if let VehicleState::RoadToPark(spline, progress, _) = &vehicle.vehicle.state {
                let start = progress.clamp(0.0, 1.0);
                let mut points = vec![vehicle.trans.pos];
                points.extend(
                    (1..=8).map(|step| spline.get(start + (1.0 - start) * step as f32 / 8.0)),
                );
                return Some(points);
            }
            vehicle.it.remaining_path(map, vehicle.trans.pos)
        }
        AnyEntity::TrainID(id) => {
            let train = sim.get(id)?;
            train.it.remaining_path(map, train.trans.pos)
        }
        _ => None,
    }
}

fn trip_view(
    sim: &Simulation,
    id: AnyEntity,
    details: bool,
    operators: &mut VehicleOperators,
) -> Option<TripView> {
    let map = sim.map();
    match id {
        AnyEntity::VehicleID(id) => road_trip(sim, &map, id, details, operators),
        AnyEntity::TrainID(id) => rail_trip(sim, &map, id, details),
        _ => None,
    }
}

fn road_trip(
    sim: &Simulation,
    map: &Map,
    id: VehicleID,
    details: bool,
    operators: &mut VehicleOperators,
) -> Option<TripView> {
    let vehicle = sim.get(id)?;
    let mut view = TripView {
        kind: match vehicle.vehicle.kind {
            VehicleKind::Car => "Car",
            VehicleKind::Truck => "Truck",
            VehicleKind::Bus => "Bus",
        },
        operator: None,
        operator_link: None,
        purpose: None,
        destination: None,
        state: road_state(sim, map, id),
        remaining: None,
        origin: None,
        cargo: None,
        reserved_cargo: false,
        buyer_line: None,
        passengers: None,
    };
    match vehicle.vehicle.kind {
        VehicleKind::Car => car_trip(sim, map, operators.get(sim, id), &mut view),
        VehicleKind::Truck => truck_trip(sim, map, id, details, &mut view, operators),
        VehicleKind::Bus => bus_trip(sim, map, id, details, &mut view),
    }
    if !vehicle.vehicle.state.is_parked() && view.destination.is_none() {
        view.destination = itinerary_destination(map, &vehicle.it, LaneKind::Driving);
    }
    Some(view)
}

fn car_trip(sim: &Simulation, map: &Map, operator: VehicleOperator, view: &mut TripView) {
    if let Some((driver, human)) = operator.driver.and_then(|id| Some((id, sim.get(id)?))) {
        view.operator = Some(format!("Driver: {}", human.personal_info.name));
        view.operator_link = Some(driver);
        view.purpose = purpose_label(human.router.trip_purpose).map(str::to_owned);
        view.destination = human
            .router
            .target_dest
            .and_then(|place| destination_label(map, place));
        view.origin = human
            .router
            .trip_origin()
            .and_then(|place| destination_label(map, place));
        apply_router_error(view, human.router.last_error);
    } else if let Some((owner, human)) = operator.owner.and_then(|id| Some((id, sim.get(id)?))) {
        view.operator = Some(format!("Owner: {}", human.personal_info.name));
        view.operator_link = Some(owner);
    }
}

fn truck_trip(
    sim: &Simulation,
    map: &Map,
    id: VehicleID,
    details: bool,
    view: &mut TripView,
    operators: &mut VehicleOperators,
) {
    let cargo = sim.read::<CargoBook>();
    if let Some(leg) = cargo.truck_leg(id) {
        set_freight_purpose(view, leg.returning);
        view.operator = building_label(map, leg.depot).map(|name| format!("Operator: {name}"));
        if let Some(driver) = leg.driver.and_then(|id| sim.get(id)) {
            view.operator_link = leg.driver;
            let name = &driver.personal_info.name;
            view.operator = Some(view.operator.take().map_or_else(
                || format!("Driver: {name}"),
                |operator| format!("Driver: {name} · {operator}"),
            ));
            if driver.location == Location::Vehicle(id) {
                apply_router_error(view, driver.router.last_error);
            }
        }
        view.destination = building_place(map, leg.destination);
        if details {
            view.origin = leg.origin.and_then(|origin| building_place(map, origin));
            if view.origin.is_none() && leg.returning {
                view.origin = leg
                    .driver
                    .and_then(|driver| sim.get(driver))
                    .filter(|driver| {
                        driver.router.target_dest == Some(Destination::Building(leg.destination))
                    })
                    .and_then(|driver| driver.router.trip_origin())
                    .and_then(|origin| destination_label(map, origin));
            }
        }
        let trace = sim.read::<FreightTrace>();
        if let Some((buyer, fragments)) = trace.truck_shipment(&cargo, id) {
            view.cargo = Some(traced_cargo_label(fragments));
            view.buyer_line = buyer_line(map, [buyer.building]);
        } else if !leg.returning && !leg.items.is_empty() {
            view.cargo = Some(cargo_label(leg.items.iter().copied(), 0));
            view.buyer_line = buyer_line(map, [leg.destination]);
        } else if details {
            view.cargo = Some("Empty".into());
        }
        return;
    }

    let vehicle = sim.get(id).unwrap();
    let mut returning = matches!(vehicle.vehicle.state, VehicleState::ReturningToDepot { .. });
    let mut depot = cargo.station_depot(id);
    let mut destination = None;
    if depot.is_none() {
        for (_, company) in &sim.world().companies {
            if !company.comp.trucks.contains(&id) {
                continue;
            }
            depot = Some(company.comp.building);
            for &worker in &company.workers.0 {
                let Some(human) = sim.get(worker) else {
                    continue;
                };
                if human.location == Location::Vehicle(id) {
                    view.operator = Some(format!("Driver: {}", human.personal_info.name));
                    view.operator_link = Some(worker);
                    returning |= human.router.trip_purpose == TripPurpose::FreightReturn;
                    destination = human
                        .router
                        .target_dest
                        .and_then(|place| destination_label(map, place));
                    apply_router_error(view, human.router.last_error);
                    break;
                }
            }
            break;
        }
    }
    // Older vehicles can have a driver and a delivery order without a manifest.
    if view.operator.is_none() && depot.is_none() {
        let driver = operators.get(sim, id).driver;
        if let Some((human_id, human)) = driver.and_then(|id| Some((id, sim.get(id)?))) {
            if let Some(work) = human
                .work
                .as_ref()
                .filter(|work| matches!(work.kind, WorkKind::Driver { truck, .. } if truck == id))
            {
                depot = Some(work.workplace);
            }
            view.operator = Some(format!("Driver: {}", human.personal_info.name));
            view.operator_link = Some(human_id);
            returning |= human.router.trip_purpose == TripPurpose::FreightReturn;
            destination = human
                .router
                .target_dest
                .and_then(|place| destination_label(map, place));
            apply_router_error(view, human.router.last_error);
        }
    }
    if view.operator.is_none() {
        view.operator = depot
            .and_then(|depot| building_label(map, depot))
            .map(|name| format!("Operator: {name}"));
    }
    if returning || destination.is_some() {
        set_freight_purpose(view, returning);
        view.destination =
            destination.or_else(|| depot.and_then(|depot| building_place(map, depot)));
        if details && !returning {
            view.origin = depot.and_then(|depot| building_place(map, depot));
        }
    }
    if details {
        view.cargo = Some(
            if returning || vehicle.vehicle.state.is_parked() {
                "Empty"
            } else {
                "Unavailable"
            }
            .into(),
        );
    }
}

fn set_freight_purpose(view: &mut TripView, returning: bool) {
    view.purpose = Some(
        if returning {
            "Freight return"
        } else {
            "Freight delivery"
        }
        .into(),
    );
}

fn cargo_label(items: impl IntoIterator<Item = ItemQuantity>, unspecified_units: u32) -> String {
    let mut lines: Vec<_> = items
        .into_iter()
        .map(|line| {
            let name = try_prototype(line.item).map_or("Unknown item", |item| &item.label);
            format!("{name} × {}", line.units)
        })
        .collect();
    if unspecified_units != 0 {
        lines.push(format!("Unspecified goods × {unspecified_units}"));
    }
    if lines.is_empty() {
        "Empty".into()
    } else {
        lines.join(", ")
    }
}

fn traced_cargo_label(fragments: &[FreightFragment]) -> String {
    let mut units: BTreeMap<ItemID, u32> = BTreeMap::new();
    let mut unspecified_units = 0;
    for fragment in fragments {
        if let Some(item) = fragment.item {
            *units.entry(item).or_default() += fragment.units;
        } else {
            unspecified_units += fragment.units;
        }
    }
    cargo_label(
        units
            .into_iter()
            .map(|(item, units)| ItemQuantity { item, units }),
        unspecified_units,
    )
}

fn buyer_line(map: &Map, buyers: impl IntoIterator<Item = BuildingID>) -> Option<String> {
    let buyers: BTreeSet<_> = buyers.into_iter().collect();
    let mut names = buyers
        .into_iter()
        .filter_map(|buyer| building_place(map, buyer));
    let first = names.next()?;
    let Some(second) = names.next() else {
        return Some(format!("Buyer: {first}"));
    };
    let mut labels = vec![first, second];
    labels.extend(names.by_ref().take(1));
    let more = names.count();
    if more > 0 {
        labels.push(format!("+{more} more"));
    }
    Some(format!("Buyers: {}", labels.join(", ")))
}

fn bus_trip(sim: &Simulation, map: &Map, id: VehicleID, details: bool, view: &mut TripView) {
    let transit = sim.read::<Transit>();
    if let Some(run) = transit.runs.get(&id) {
        view.purpose = Some(format!("Bus line {}", run.line.data().as_ffi() as u32));
        view.operator = Some("Operator: City transit".into());
        if let Some(line) = transit
            .lines
            .get(run.line)
            .filter(|line| !line.stops.is_empty())
        {
            let index = run.next_stop_idx;
            let label = |stop| stop_label(map, &transit, stop);
            if run.dwell_until.is_some() {
                view.state = if line.suspended || line.target_vehicles == 0 {
                    "Service paused".into()
                } else {
                    "Boarding at stop".into()
                };
                view.origin = line.stops.get(index).and_then(|&stop| label(stop));
                if !line.suspended && line.target_vehicles != 0 {
                    view.destination = label(line.stops[(index + 1) % line.stops.len()]);
                }
            } else {
                view.destination = line.stops.get(index).and_then(|&stop| label(stop));
                view.origin = transit.bus_leg_origin(id).and_then(label);
            }
        }
    } else {
        view.purpose = Some("Out of service".into());
    }
    if details {
        let passengers: Vec<_> = sim
            .world()
            .humans
            .iter()
            .filter_map(|(human_id, human)| {
                (human.location == Location::Transit(id)).then_some(human_id)
            })
            .collect();
        let scale = sim.population_scale();
        view.passengers = Some((
            format!(
                "{} / {}",
                scale.persons(passengers.len()),
                scale.persons(BUS_CAPACITY)
            ),
            passengers,
        ));
    }
}

fn rail_trip(sim: &Simulation, map: &Map, id: TrainID, details: bool) -> Option<TripView> {
    let train = sim.get(id)?;
    let assignment = sim.world().freight_stations.values().find_map(|station| {
        station
            .f
            .trains
            .iter()
            .find_map(|&(train, state)| (train == id).then_some((&station.f, state)))
    });
    let trace = sim.read::<FreightTrace>();
    let shipment = trace.rail_shipment(id);
    let external = map.external_train_station();
    let (operator, origin, destination) = match assignment {
        Some((station, FreightTrainState::Arriving)) => {
            (Some(station.building), None, Some(station.building))
        }
        Some((station, FreightTrainState::Loading | FreightTrainState::Moving)) => {
            (Some(station.building), Some(station.building), external)
        }
        None => (None, None, None),
    };
    let state = if train.it.is_wait_for_reroute().is_some() {
        "Stuck · Route unavailable".into()
    } else if matches!(assignment, Some((_, FreightTrainState::Loading))) {
        "Loading at station".into()
    } else if train.speed.0 > 0.2 {
        speed_label(train.speed.0)
    } else if !train.it.is_none_or_wait() && train.res.waited_for > 0.5 {
        "Waiting for rail clearance".into()
    } else if train.it.is_none_or_wait() {
        "Idle".into()
    } else {
        "Stopped".into()
    };

    let rail_trip = assignment.and_then(|(station, _)| station.rail_trip(id));
    let fragments = shipment.as_ref().map(|shipment| shipment.fragments);
    let cargo = if let Some(fragments) = fragments {
        (details || !fragments.is_empty()).then(|| traced_cargo_label(fragments))
    } else if let Some(load) = rail_trip.and_then(|trip| trip.exports.as_ref()) {
        Some(cargo_label(
            load.items.iter().copied(),
            load.unspecified_units,
        ))
    } else {
        details.then(|| "Manifest unavailable".into())
    };
    Some(TripView {
        kind: "Train",
        operator: operator
            .and_then(|id| building_label(map, id))
            .map(|name| format!("Operator: {name}")),
        operator_link: None,
        purpose: Some("Rail freight".into()),
        destination: shipment
            .as_ref()
            .map(|shipment| shipment.destination)
            .or(destination)
            .and_then(|id| building_place(map, id))
            .or_else(|| itinerary_destination(map, &train.it, LaneKind::Rail)),
        state,
        remaining: None,
        origin: origin
            .and_then(|id| building_place(map, id))
            .or_else(|| {
                rail_trip.map(|trip| position_label(map, trip.dispatch_position, LaneKind::Rail))
            })
            .or_else(|| {
                shipment
                    .as_ref()
                    .filter(|shipment| shipment.destination != shipment.outside_gate)
                    .and_then(|shipment| building_place(map, shipment.outside_gate))
            }),
        cargo,
        reserved_cargo: shipment.is_some_and(|shipment| shipment.reserved),
        buyer_line: fragments.and_then(|fragments| {
            buyer_line(
                map,
                fragments
                    .iter()
                    .filter_map(|fragment| fragment.ordered_for.map(|buyer| buyer.building)),
            )
        }),
        passengers: None,
    })
}

fn road_state(sim: &Simulation, map: &Map, id: VehicleID) -> String {
    let vehicle = sim.get(id).unwrap();
    match vehicle.vehicle.state {
        VehicleState::Parked(_) | VehicleState::ParkedAtDepot => return "Parked".into(),
        VehicleState::Panicking(_) => return "Stuck · Gridlock recovery".into(),
        VehicleState::RoadToPark(_, _, _) => return "Parking".into(),
        VehicleState::Driving | VehicleState::ReturningToDepot { .. } => {}
    }
    if vehicle.it.is_wait_for_reroute().is_some() {
        return "Stuck · Route unavailable".into();
    }
    if vehicle.speed.0 > 0.2 {
        return speed_label(vehicle.speed.0);
    }
    if let Some(reason) = vehicle_hold_reason(sim, map, id) {
        let state = match reason {
            QueueHoldReason::Unknown => return "Waiting at junction".into(),
            QueueHoldReason::Signal | QueueHoldReason::StopDwell | QueueHoldReason::ExitFull => {
                "Waiting at junction"
            }
            QueueHoldReason::InvalidTraversal | QueueHoldReason::NoRoute => "Stuck",
            _ => "Waiting",
        };
        return format!("{state} · {}", hold_label(reason));
    }
    if matches!(vehicle.vehicle.state, VehicleState::ReturningToDepot { .. }) {
        "Returning to depot".into()
    } else if vehicle.it.get_point().is_some() {
        "Stopped".into()
    } else {
        "Idle".into()
    }
}

fn apply_router_error(view: &mut TripView, error: Option<RouterError>) {
    if matches!(view.state.as_str(), "Stopped" | "Idle") {
        if let Some(error) = error {
            let reason = match error {
                RouterError::ReservingParkingSpot(_) => "Parking unavailable",
                RouterError::TranslatingParkingSpotToDrivePos => "Parking access unavailable",
                RouterError::LocatingVehicle => "Vehicle location unavailable",
                RouterError::PropertyParkingBlocked => "Property parking entrance unavailable",
                RouterError::PropertyParkingUnavailable(_) => "Property parking claim unavailable",
                RouterError::PropertyParkingBudgetExhausted => "Parking search limit reached",
            };
            view.state = format!("Stuck · {reason}");
        }
    }
}

fn speed_label(speed: f32) -> String {
    format!("Driving at {:.0} km/h", speed * 3.6)
}

fn purpose_label(purpose: TripPurpose) -> Option<&'static str> {
    Some(match purpose {
        TripPurpose::Home => "Home",
        TripPurpose::Work => "Work",
        TripPurpose::Shopping => "Shopping",
        TripPurpose::Leisure => "Leisure",
        TripPurpose::School => "School",
        TripPurpose::FreightOutbound => "Freight delivery",
        TripPurpose::FreightReturn => "Freight return",
        TripPurpose::Other => return None,
    })
}

fn destination_label(map: &Map, place: Destination) -> Option<String> {
    match place {
        Destination::Building(id) => building_place(map, id),
        Destination::Outside(pos) => Some(position_label(map, pos, LaneKind::Driving)),
    }
}

fn building_label(map: &Map, id: BuildingID) -> Option<String> {
    Some(
        match map.get(id)?.kind {
            BuildingKind::House => "House",
            BuildingKind::GoodsCompany(id) => {
                try_prototype(id).map_or("Company", |proto| &proto.label)
            }
            BuildingKind::RailFreightStation(id) => {
                try_prototype(id).map_or("Freight station", |proto| &proto.label)
            }
            BuildingKind::TrainStation => "Train station",
            BuildingKind::ExternalTrading => "External trading",
            BuildingKind::ParkingGarage(kind) => kind.label(),
        }
        .into(),
    )
}

fn building_place(map: &Map, id: BuildingID) -> Option<String> {
    let name = building_label(map, id)?;
    let building = map.get(id)?;
    Some(
        match building.connected_road.filter(|&id| map.get(id).is_some()) {
            Some(road) => format!("{name} · {}", road_label(road, LaneKind::Driving)),
            None => format!(
                "{name} · {:.0}, {:.0}",
                building.door_pos.x, building.door_pos.y
            ),
        },
    )
}

fn stop_label(map: &Map, transit: &Transit, id: StopID) -> Option<String> {
    let stop = transit.stops.get(id)?;
    Some(format!(
        "Stop {} · {}",
        id.data().as_ffi() as u32,
        position_label(map, stop.berth, LaneKind::Driving)
    ))
}

fn road_label(id: RoadID, kind: LaneKind) -> String {
    let name = if kind == LaneKind::Rail {
        "Track"
    } else {
        "Road"
    };
    format!("{name} {:?}", id.data())
}

fn position_label(map: &Map, pos: Vec3, kind: LaneKind) -> String {
    map.nearest_lane(pos, kind, Some(40.0))
        .and_then(|lane| map.get(lane))
        .map_or_else(
            || format!("{:.0}, {:.0}", pos.x, pos.y),
            |lane| road_label(lane.parent, kind),
        )
}

fn itinerary_destination(map: &Map, itinerary: &Itinerary, kind: LaneKind) -> Option<String> {
    match itinerary.target()? {
        ItineraryTarget::Position(pos) => Some(position_label(map, pos, kind)),
        ItineraryTarget::DirectedLane(id, _) => Some(road_label(map.get(id)?.parent, kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec2, vec3, Transform};
    use prototypes::{Money, Tick};
    use scenarios::builders::{fill_houses, grid_city};
    use scenarios::ScenarioCtx;
    use simulation::map_dynamic::transit::{BusRun, TransitLine, TransitStop};
    use simulation::map_dynamic::Router;
    use simulation::souls::desire::Work;
    use simulation::souls::human::spawn_human;
    use simulation::transportation::spawn_driving_vehicle;
    use simulation::SimulationOptions;

    fn vehicle_destination_fixture() -> (ScenarioCtx, VehicleID, VehicleID, VehicleID) {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut ctx, vec2(80.0, 80.0), 3, 3, 80.0);
        let houses = fill_houses(&mut ctx, 12);
        let home = houses[0];
        let workplace = houses[1];
        let driver = spawn_human(&mut ctx.sim, home).unwrap();
        let truck_driver = spawn_human(&mut ctx.sim, workplace).unwrap();
        let mut spawn = |kind, x| {
            spawn_driving_vehicle(&mut ctx.sim, kind, Transform::new(vec3(x, 80.0, 0.0))).unwrap()
        };
        let car = spawn(VehicleKind::Car, 110.0);
        let truck = spawn(VehicleKind::Truck, 125.0);
        let bus = spawn(VehicleKind::Bus, 140.0);
        let world = ctx.sim.world_mut_unchecked();
        world.edit_human(driver, |human| {
            human.personal_info.name = "Avery Driver".into();
            human.location = Location::Vehicle(car);
            human.router = Router::new(Some(car));
            human.router.target_dest = Some(Destination::Building(workplace));
            human.router.trip_purpose = TripPurpose::Work;
        });
        world.edit_human(truck_driver, |human| {
            human.personal_info.name = "Morgan Freight".into();
            human.location = Location::Vehicle(truck);
            human.work = Some(Work::new(
                workplace,
                WorkKind::Driver {
                    truck,
                    deliver_order: Some(home),
                    load: None,
                },
                0.0,
            ));
            human.router.target_dest = Some(Destination::Building(home));
            human.router.trip_purpose = TripPurpose::FreightOutbound;
        });
        world.vehicles[car].it = Itinerary::simple(vec![vec3(150.0, 80.0, 0.0)]);
        world.vehicles[car].speed.0 = 10.0;
        let mut transit = ctx.sim.write::<Transit>();
        let stops = [150.0, 200.0].map(|x| {
            let pos = vec3(x, 80.0, 0.0);
            transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos,
                dir: geom::Vec2::X,
                walk_pos: pos,
                berth: pos,
            })
        });
        let line = transit.lines.insert_with_key(|id| TransitLine {
            id,
            stops: stops.to_vec(),
            target_vehicles: 1,
            fare: Money::ZERO,
            color: geom::Color::WHITE,
            suspended: false,
        });
        transit.runs.insert(
            bus,
            BusRun {
                line,
                next_stop_idx: 1,
                dwell_until: None,
                leg_started: Tick(0),
                failures: 0,
            },
        );
        drop(transit);
        (ctx, car, truck, bus)
    }

    #[test]
    fn vehicle_destination_inspector_and_overlay_share_one_route() {
        let (ctx, car, _, _) = vehicle_destination_fixture();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        *ui.write::<InspectedEntity>() = InspectedEntity {
            e: Some(car.into()),
            ..Default::default()
        };
        ui.write::<ShowVehicleRoute>().0 = Some(car.into());
        ROUTE_BUILD_COUNT.with(|count| count.set(0));
        assert_eq!(
            vehicle_trip_view(&ui, &ctx.sim, car.into(), true)
                .unwrap()
                .remaining,
            Some(40.0)
        );
        draw_vehicle_route(&ctx.sim, &ui);
        ROUTE_BUILD_COUNT.with(|count| assert_eq!(count.get(), 1));
    }

    #[test]
    fn vehicle_destination_car_commute() {
        let (ctx, car, _, _) = vehicle_destination_fixture();
        let (tooltip, inspector) = presentation(&ctx.sim, car);
        assert!(tooltip.contains("Avery Driver"), "{tooltip}");
        assert!(tooltip.contains("Trip: Work"), "{tooltip}");
        assert!(tooltip.contains("Destination: House"), "{tooltip}");
        assert!(tooltip.contains("Driving at 36 km/h"), "{tooltip}");
        assert!(tooltip.contains("40 m remaining"), "{tooltip}");
        assert!(inspector.contains("Origin: Unavailable"), "{inspector}");
    }

    #[test]
    fn vehicle_destination_freight_truck() {
        let (ctx, _, truck, _) = vehicle_destination_fixture();
        let (tooltip, inspector) = presentation(&ctx.sim, truck);
        assert!(tooltip.contains("Morgan Freight"), "{tooltip}");
        assert!(tooltip.contains("Trip: Freight delivery"), "{tooltip}");
        assert!(tooltip.contains("Destination: House"), "{tooltip}");
        assert!(inspector.contains("Origin: House"), "{inspector}");
        assert!(inspector.contains("Cargo: Unavailable"), "{inspector}");
    }

    #[test]
    fn vehicle_destination_shipment_follows_import_from_train_to_truck() {
        use geom::{Vec2, OBB};
        use prototypes::{BuildingGen, FreightStationPrototypeID, GoodsCompanyID, RollingStockID};
        use simulation::economy::cargo::{Endpoint, FreightStage};
        use simulation::economy::Market;
        use simulation::map::{LanePatternBuilder, MapProject, RailMarketSite};
        use simulation::map_dynamic::BuildingInfos;
        use simulation::souls::freight_station::{freight_station_soul, train_stop};
        use simulation::souls::goods_company::company_soul;
        use simulation::world_command::{CommandOutcomes, WorldCommand};
        use simulation::{AuthoredTownOptions, SoulID};

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let mut sim = Simulation::new_authored_town(AuthoredTownOptions::default()).unwrap();
        let connection = |y, rail| WorldCommand::MapMakeConnection {
            from: MapProject::ground(vec3(100.0, y, 10.0)),
            to: MapProject::ground(vec3(1000.0, y, 10.0)),
            inter: None,
            pat: LanePatternBuilder::new().rail(rail).build(),
        };
        sim.apply_commands([&connection(625.0, false), &connection(700.0, true)]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0, 1]);
        let (road, lane, gate, platform, direction) = {
            let map = sim.map();
            let road = map
                .lanes()
                .values()
                .find(|lane| lane.kind == LaneKind::Driving)
                .unwrap()
                .parent;
            let lane = map
                .lanes()
                .values()
                .find(|lane| {
                    lane.kind == LaneKind::Rail && lane.points.first().x < lane.points.last().x
                })
                .unwrap();
            let (gate, direction) = lane.points.point_dir_along(300.0);
            (
                road,
                lane.id,
                gate,
                lane.points.point_along(600.0),
                direction,
            )
        };
        let mut build = |centre, direction, kind| {
            let building = sim
                .write::<Map>()
                .build_special_building(
                    &OBB::new(centre, direction, 12.0, 12.0),
                    kind,
                    BuildingGen::CenteredDoor {
                        vertical_factor: 1.0,
                    },
                    None,
                    Some(road),
                )
                .unwrap();
            sim.write::<BuildingInfos>().insert(building);
            building
        };
        let station_proto = FreightStationPrototypeID::new("freight-station");
        let station_building = build(
            (platform - direction * 75.0 + direction.perp_up() * 40.0).xy(),
            direction.xy(),
            BuildingKind::RailFreightStation(station_proto),
        );
        let buyer_proto = GoodsCompanyID::new("commercial-l1");
        let buyer_pos = vec2(350.0, 660.0);
        let buyer_building = build(buyer_pos, Vec2::X, BuildingKind::GoodsCompany(buyer_proto));
        let station = freight_station_soul(&mut sim, station_building, station_proto).unwrap();
        let buyer = company_soul(&mut sim, buyer_building, buyer_proto).unwrap();
        let platform_stop = train_stop(&sim.get(station).unwrap().trans);
        sim.place_authored_rail_market(RailMarketSite {
            footprint: OBB::new(gate.xy(), direction.xy(), 100.0, 24.0),
            service_lane: lane,
            service_offset: 300.0,
            platform_stop,
            consist_length_m: 100.0,
        })
        .unwrap();
        sim.apply_commands([&WorldCommand::SpawnTrain {
            wagons: vec![RollingStockID::new("locomotive")],
            lane,
            dist: 300.0,
        }]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
        let train = sim.world().trains.keys().next().unwrap();
        let goods = ItemID::new("goods");
        sim.write::<Market>().buy(buyer, buyer_pos, goods, 2);
        let terminal = Endpoint {
            soul: SoulID::FreightStation(station),
            building: station_building,
        };
        let mut schedule = Simulation::schedule();
        let ui = UiWorld::with_defaults();
        let mut train_tooltip = None;
        let mut truck_tooltip = None;
        let mut checked_return_route = false;
        for _ in 0..6000 {
            sim.tick(&mut schedule, &[]);
            let returning = sim
                .get(station)
                .unwrap()
                .f
                .trains
                .iter()
                .any(|&(id, state)| id == train && matches!(state, FreightTrainState::Moving));
            if returning && !checked_return_route {
                let itinerary = &sim.get(train).unwrap().it;
                assert!(!itinerary.get_route().unwrap().reversed_route.is_empty());
                let path = vehicle_path(&sim, &sim.map(), train.into())
                    .expect("the cross-track freight return should have a visible route");
                assert_eq!(path.last().copied(), itinerary.end_pos());
                checked_return_route = true;
            }
            let trace = sim.read::<FreightTrace>();
            let page = trace.terminal_view(terminal);
            let Some(record) = page.rows.first() else {
                continue;
            };
            match record.stage {
                FreightStage::InboundRail if train_tooltip.is_none() => {
                    train_tooltip = vehicle_tooltip(&ui, &sim, train.into())
                        .map(|text| (text, record.fragment.units));
                }
                FreightStage::OutboundTruck if truck_tooltip.is_none() => {
                    assert_eq!(record.receiver.unwrap().building, buyer_building);
                    truck_tooltip = vehicle_tooltip(&ui, &sim, record.truck.unwrap().into())
                        .map(|text| (text, record.fragment.units));
                }
                _ => {}
            }
            if checked_return_route && truck_tooltip.is_some() {
                break;
            }
        }
        assert!(
            checked_return_route,
            "fixture should dispatch a returning train"
        );
        let buyer = format!("Buyer: {}", buyer_proto.prototype().label);
        let tooltips = [
            train_tooltip.expect("fixture should dispatch an inbound train"),
            truck_tooltip.expect("fixture should dispatch a station truck"),
        ];
        for (text, units) in tooltips {
            let cargo = format!("Cargo: {} × {units}", goods.prototype().label);
            assert!(text.lines().any(|line| line == cargo), "{text}");
            assert!(text.contains(&buyer), "{text}");
        }
    }

    #[test]
    fn vehicle_destination_bus_line() {
        let (ctx, _, _, bus) = vehicle_destination_fixture();
        let (tooltip, inspector) = presentation(&ctx.sim, bus);
        assert!(tooltip.contains("Trip: Bus line "), "{tooltip}");
        assert!(tooltip.contains("Destination: Stop "), "{tooltip}");
        assert!(inspector.contains("Origin: Unavailable"), "{inspector}");
        assert!(inspector.contains("Passengers: 0 / "), "{inspector}");

        ctx.sim
            .write::<Transit>()
            .runs
            .get_mut(&bus)
            .unwrap()
            .dwell_until = Some(Tick(1));
        let (boarding, inspector) = presentation(&ctx.sim, bus);
        assert!(boarding.contains("Boarding at stop"), "{boarding}");
        assert!(inspector.contains("Origin: Stop "), "{inspector}");
        let destination = |text: &str| {
            text.lines()
                .find(|line| line.starts_with("Destination:"))
                .unwrap()
                .to_owned()
        };
        assert_ne!(destination(&tooltip), destination(&boarding));
    }

    #[test]
    fn vehicle_destination_route_ends_at_the_berth_and_hides_during_reroute() {
        let (ctx, _, _, _) = vehicle_destination_fixture();
        let map = ctx.sim.map();
        let lanes: Vec<_> = map
            .lanes()
            .values()
            .filter(|lane| lane.kind == LaneKind::Driving)
            .collect();
        let (start, itinerary) = lanes
            .iter()
            .find_map(|from| {
                lanes
                    .iter()
                    .filter(|to| to.parent != from.parent)
                    .find_map(|to| {
                        let start = from.points.middle();
                        let itinerary =
                            Itinerary::route_to_lane(Tick(0), start, &map, to.id, 10.0)?;
                        (!itinerary.get_route()?.reversed_route.is_empty())
                            .then_some((start, itinerary))
                    })
            })
            .expect("fixture should have a route through at least one junction");
        let path = itinerary.remaining_path(&map, start).unwrap();
        assert_eq!(path.first().copied(), Some(start));
        assert_eq!(path.last().copied(), itinerary.end_pos());
        assert!(path.len() > itinerary.remaining_points() + 1);
        let ItineraryTarget::DirectedLane(lane, offset) = itinerary.target().unwrap() else {
            panic!("expected a directed lane target");
        };
        assert!(Itinerary::wait_for_reroute_to_lane(lane, offset)
            .remaining_path(&map, start)
            .is_none());
    }

    fn presentation(sim: &Simulation, id: VehicleID) -> (String, String) {
        crate::init::init();
        let ui = UiWorld::with_defaults();
        let tooltip = vehicle_tooltip(&ui, sim, id.into()).unwrap();
        let view = vehicle_trip_view(&ui, sim, id.into(), true).unwrap();
        let inspector = view
            .summary_lines()
            .into_iter()
            .chain(view.detail_lines())
            .collect::<Vec<_>>()
            .join("\n");
        (tooltip, inspector)
    }
}
