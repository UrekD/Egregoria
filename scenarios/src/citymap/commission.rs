use super::adapters::{
    apply, crosses_reserved_bank, downtown_cells, frontage_roads, place_company,
    place_scored_company, place_special, site_clear, water_site,
};
use super::network::close_pairs;
use super::{blocks, contains, BuiltNetwork, RoadClass, SourceKey, TownPlan};
use geom::{vec2, Intersect, Polygon, Transform, Vec2, Vec3, OBB};
use prototypes::{
    try_prototype, BuildingGen, FreightStationPrototypeID, GameTime, ItemID, Money, ResidencePrototype,
    RollingStockID, ServiceID, ServiceKind, Tick, TICKS_PER_HOUR, TICKS_PER_MINUTE,
};
use serde::Serialize;
use simulation::economy::cargo::{CargoBook, WarehousePolicy};
use simulation::economy::{
    Budget, BudgetCategory, DailyEconomyMetrics, Government, Market, TransitStats, TripPurpose,
    TripStats, TICKS_PER_BUDGET_DAY,
};
use simulation::map::{
    water_provider_site, BuildingID, BuildingKind, Density, LaneID, LaneKind, LanePatternBuilder, LotID, Map, MapProject, PathKind, ProjectKind,
    LightPolicy, RoadID, TrafficBehavior, TraverseKind, TurnID, Zone, ZoneCell, ZoneGrid, ZoneKind,
};
use simulation::map::{RailMarketError, RailMarketSite};
use simulation::map_dynamic::service_reach_sources;
use simulation::diagnostics::elapsed_tick;
use simulation::map_dynamic::transit::{LineID, StopID};
use simulation::map_dynamic::{
    BuildingInfos, CoverageGrid, Destination, ElectricityFlow, Itinerary, ItineraryTarget, ParkingManagement, RoutingStep,
    Transit, UtilityFlow, UtilityStatus,
};
use simulation::souls::freight_station::{train_stop, FreightTrainState};
use simulation::souls::{
    growth::check_growable_lot,
    human::HumanDecisionKind,
    residence::{is_retail_store, Residences},
};
use simulation::transportation::{
    road::calc_decision, Location, TransportGrid, Vehicle, VehicleKind, VehicleState,
};
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::WorldCommand;
use simulation::{FreightStationID, HumanID, TrainID, VehicleEnt, VehicleID};
use simulation::{Simulation, SoulID};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const SERVICES: [ServiceKind; 6] = [
    ServiceKind::Fire,
    ServiceKind::Police,
    ServiceKind::Health,
    ServiceKind::School,
    ServiceKind::Garbage,
    ServiceKind::Park,
];
const SAMPLE_TICKS: u64 = TICKS_PER_MINUTE / 2;
const SOAK_DAYS: u64 = 3;
// CM2 final supersedes the original opening size and basket targets.
const MIN_OPENING_POPULATION: u64 = 2500;
const MIN_BASKETS_FILLED_PERCENT: u128 = 94;
const MIN_EMPLOYED_PERCENT: u128 = 75;
const MAX_SCHOOLS: usize = 22;
const MAX_STANDING_MINUTES: u64 = 120;

#[derive(Serialize)]
pub struct OpeningManifest {
    pub opening_tick: u64,
    sites: BTreeMap<String, BuildingID>,
    parcels: Vec<Parcel>,
    reserves: Vec<Parcel>,
    lines: BTreeMap<&'static str, BusLine>,
    freight: Freight,
    #[serde(skip_serializing_if = "Option::is_none")]
    cross_bank: Option<CrossBank>,
}

#[derive(Serialize)]
struct CrossBank {
    park: BuildingID,
    stop: StopID,
}

#[derive(Clone, Serialize)]
struct Parcel {
    lot: LotID,
    road: RoadID,
    district: &'static str,
    block: Option<usize>,
    footprint: OBB,
    zoning: ZoneCell,
    capacity: u32,
    workers: u32,
    building: Option<BuildingID>,
}

#[derive(Serialize)]
struct BusLine {
    id: LineID,
    stops: Vec<StopID>,
    lanes: Vec<LaneID>,
    vehicles: u8,
}

#[derive(Serialize)]
struct Freight {
    terminal: BuildingID,
    station: FreightStationID,
    market: BuildingID,
    train: TrainID,
    market_lane: LaneID,
    market_offset: f32,
    bakery: BuildingID,
}

struct Cohort {
    district: &'static str,
    anchor: Vec2,
    houses: usize,
    infill: usize,
    retail: usize,
    industry: usize,
}

const SOUTHEAST_STUB: SourceKey = SourceKey::Way {
    id: 168886779,
    span: 0,
};
const SOUTHEAST_DEAD_END: SourceKey = SourceKey::Node {
    id: 11951438144,
    rail: false,
};
const WET_BAKERY_WALK_M: f32 = 180.0;

pub(super) fn configure_commissioned_network(plan: &mut TownPlan) {
    let graph = &mut plan.graph;
    if graph
        .links
        .values()
        .filter(|link| link.endpoints.contains(&SOUTHEAST_DEAD_END))
        .count()
        == 1
    {
        if let Some(stub) = graph
            .links
            .get_mut(&SOUTHEAST_STUB)
            .filter(|link| link.endpoints.contains(&SOUTHEAST_DEAD_END))
        {
            // Commuters were using the stub as a U-turn, spilling back into
            // its junction and trapping cars that parked on the short road.
            stub.class = RoadClass::Walk;
            graph.record(
                "keep the southeast turnaround as a walking path",
                &[SOUTHEAST_STUB],
            );
        }
    }
    let west_south_shortcut = SourceKey::Infill { number: 7, part: 4 };
    if let Some(link) = graph.links.get_mut(&west_south_shortcut) {
        // Opposing turns queued through both junctions and blocked this short
        // link. Keep the direct walk between neighbourhoods.
        link.class = RoadClass::Walk;
        for end in link.endpoints {
            let driving = graph.links.values().filter(|link| {
                link.class.driving() && link.endpoints.contains(&end)
            }).count();
            if driving == 1 {
                graph.junctions.get_mut(&end).unwrap().turn.back_turns = true;
            }
        }
        graph.record(
            "keep the west–south infill shortcut for walking",
            &[west_south_shortcut],
        );
    }
    let west_local_junction = SourceKey::Node {
        id: 3567122171,
        rail: false,
    };
    if let Some(junction) = graph.junctions.get_mut(&west_local_junction) {
        // The automatic signal backed traffic through the short approach from the north.
        junction.light = LightPolicy::StopSigns;
        graph.record(
            "use an all-way stop to keep the short West approach flowing",
            &[west_local_junction],
        );
    }
    let fuel_frontage = SourceKey::Way {
        id: 72625754,
        span: 0,
    };
    if let Some(link) = graph.links.get(&fuel_frontage) {
        for endpoint in link.endpoints {
            if let Some(junction) = graph.junctions.get_mut(&endpoint) {
                // Plant and mine loading sides share this road in opposite directions.
                junction.turn.back_turns = true;
            }
        }
        graph.record(
            "provide turnarounds at both ends of the coal delivery road",
            &[fuel_frontage],
        );
    }
}

pub fn commission(
    sim: &mut Simulation,
    plan: &TownPlan,
    built: &BuiltNetwork,
) -> Result<OpeningManifest, String> {
    commission_opening(sim, plan, built, None)
}

struct OpeningLayout {
    cohorts: [Cohort; 5],
    parcels: Vec<Parcel>,
    reserves: Vec<Parcel>,
    intake_homes: BTreeSet<LotID>,
    sites: BTreeMap<String, BuildingID>,
    depots: Vec<WorldCommand>,
    freight: FreightSetup,
    bakery: BuildingID,
}

fn prepare_opening(
    sim: &mut Simulation,
    plan: &TownPlan,
    built: &BuiltNetwork,
    freight: Option<FreightSetup>,
) -> Result<OpeningLayout, String> {
    let roads = frontage_roads(plan, built);
    let wet = sim.map().environment().has_water_configuration();
    let profiled_wet = wet && ResidencePrototype::iter().any(|home| home.massing.is_some());
    let at = |x, y| super::layout::design_point(vec2(x, y), plan.config);
    let cohorts = [
        Cohort {
            district: "station",
            anchor: at(2100.0, 1635.0),
            houses: 47,
            infill: 8,
            retail: 10,
            industry: 4,
        },
        Cohort {
            district: "hraska",
            anchor: at(1950.0, 2200.0),
            houses: 85,
            infill: if wet { 21 } else { 10 },
            retail: 14,
            industry: 6,
        },
        Cohort {
            district: "south",
            anchor: at(2060.0, 1180.0),
            // Keep the profiled opening near the developed town centre. Its
            // denser fringe had the largest clusters of empty pantries.
            houses: if profiled_wet { 94 } else if wet { 110 } else { 98 },
            infill: if wet { 14 } else { 10 },
            retail: if wet { 24 } else { 16 },
            industry: if wet { 12 } else { 8 },
        },
        Cohort {
            district: "west",
            anchor: at(1415.0, 1690.0),
            houses: if profiled_wet { 76 } else { 92 },
            infill: if wet { 20 } else { 10 },
            retail: 14,
            industry: 8,
        },
        Cohort {
            district: "employment",
            anchor: at(2530.0, 2375.0),
            houses: 0,
            infill: 5,
            retail: 8,
            industry: 30,
        },
    ];
    let mut sites = BTreeMap::new();
    let freight = match freight {
        Some(freight) => freight,
        None => prepare_freight(sim, plan, built)?,
    };
    let terminal = freight.terminal;
    let bakery_anchor = sim.map().buildings()[terminal].door_pos.xy();
    let buyer = place_company(sim, plan, &roads, "bakery", bakery_anchor)?.0;
    sites.insert("rail-bakery".into(), buyer);
    verify_parking_pair(&sim.map(), terminal, buyer, Tick(sim.get_tick()))?;

    // Give the utilities access to the same labour pool as the shops.
    // A sixth pair covers the utility margin after the reserves are built.
    let wet_intake = at(760.0, 940.0);
    // This bank frontage fits the outfall foundations clear of the road profile.
    let wet_outlet = at(1630.0, 510.0);
    let mut utilities: Vec<_> = cohorts
        .iter()
        .map(|cohort| cohort.anchor)
        .chain([at(1735.0, 1050.0)])
        .enumerate()
        .flat_map(|(index, anchor)| {
            [("water-pump", wet_intake), ("sewage-outlet", wet_outlet)]
                .map(|(name, shore)| (name, index + 1, if wet { shore } else { anchor }))
        })
        .collect();
    if wet {
        // Install all upstream intakes before choosing downstream outlets.
        utilities.sort_by_key(|&(name, _, _)| name != "water-pump");
    }
    for (name, index, anchor) in utilities {
        let building = water_site(sim, plan, &roads, name, anchor)?;
        sites.insert(format!("{name}-{index}"), building);
    }
    if wet {
        let parkway = plan
            .riverside_park
            .as_ref()
            .ok_or("missing Sava park reservation")?;
        let road = built.links[&parkway.far_bank_access].road;
        let anchor = at(1056.0, 650.0);
        let (park, _) = place_scored_company(
            sim,
            plan,
            &BTreeSet::from([road]),
            "park",
            |_, _, footprint| {
                let center = footprint.center();
                (center.y < anchor.y).then(|| center.distance2(anchor))
            },
        )?
        .ok_or("no dry park site on the far-bank parkway")?;
        sites.insert("sava/far-bank-park".into(), park);

        // Reserve the staffed, town-facing employment entrance and the
        // southern shopping pocket before filling their residential frontage.
        for (name, key, anchor) in [
            ("employment/supermarket", "employment", at(2350.0, 1960.0)),
            ("south/supermarket", "south", at(2514.0, 1033.0)),
            ("hraska/ridge-supermarket", "hraska", at(1290.0, 2630.0)),
        ] {
            let district = plan.districts.iter().find(|district| district.key == key).unwrap();
            let (store, _) = place_scored_company(sim, plan, &roads, "supermarket", |map, road, footprint| {
                let center = footprint.center();
                (contains(&district.polygon, center)
                    && map.roads()[road].lanes_iter().any(|(_, kind)| kind == LaneKind::Parking))
                    .then(|| center.distance2(anchor))
            })?
            .ok_or_else(|| format!("no supermarket frontage in the {key} district"))?;
            sites.insert(name.into(), store);
        }
    }
    // Use the frontage between South and West so mine drivers can reach work
    // and deliver fuel without competing for the terminal's delivery fleet.
    let (plant, plant_road) =
        place_company(sim, plan, &roads, "coal-power-plant", at(1740.0, 1430.0))?;
    sites.insert("coal-power-plant".into(), plant);
    let plant_door = sim.map().buildings()[plant].door_pos.xy();
    let mine_frontage = BTreeSet::from([plant_road]);
    let mut mine_offsets = [vec2(-70.0, 0.0), vec2(70.0, 0.0), vec2(0.0, 60.0)];
    if profiled_wet {
        // Freight considers producers in commissioning order. Put the mine
        // upstream of the plant's loading curb first so loaded trucks can
        // reach it without a turnaround.
        mine_offsets.swap(0, 1);
    }
    for (index, offset) in mine_offsets.into_iter().enumerate() {
        let (mine, _) = place_company(sim, plan, &mine_frontage, "coal-mine", plant_door + offset)?;
        verify_parking_pair(&sim.map(), mine, plant, Tick(sim.get_tick()))?;
        sites.insert(format!("coal-mine-{}", index + 1), mine);
    }
    let intake_parcels = if profiled_wet {
        commission_intake_frontage(sim, plan, &roads)?
    } else {
        Vec::new()
    };
    let mut depots = Vec::new();
    if profiled_wet {
        // A separate local fleet can deliver staged coal while the mine
        // trucks are out collecting or returning to their depots.
        let (buffer, _) = place_company(sim, plan, &roads, "large-warehouse", at(1688.0, 1248.0))?;
        verify_parking_pair(&sim.map(), buffer, plant, Tick(sim.get_tick()))?;
        depots.push(WorldCommand::SetWarehousePolicy {
            building: buffer,
            policy: WarehousePolicy::ReserveLocally,
        });
        sites.insert("coal-buffer".into(), buffer);
    }
    // Use parking frontage near both the field clusters and the shops.
    for (i, (x, y)) in [
        (1110.0, 1550.0),
        (1320.0, 1310.0),
        (1490.0, 1010.0),
        (1800.0, 960.0),
        (2200.0, 1080.0),
        (2500.0, 1370.0),
        (1710.0, 2460.0),
        (1940.0, 2410.0),
        (2250.0, 2450.0),
        (2560.0, 2100.0),
    ]
    .into_iter()
    .enumerate()
    {
        let (building, _) = place_company(sim, plan, &roads, "flour-factory", at(x, y))?;
        sites.insert(format!("flour-mill-{}", i + 1), building);
    }
    let mut farms = Vec::new();
    for (i, (x, y)) in [
        (1030.0, 1460.0),
        (1330.0, 1190.0),
        (1730.0, 800.0),
        (2180.0, 2760.0),
        (1800.0, 2790.0),
        (2560.0, 2700.0),
        (2680.0, 2150.0),
        (2110.0, 700.0),
    ]
    .into_iter()
    .enumerate()
    {
        let (building, _) = place_company(sim, plan, &roads, "cereal-farm", at(x, y))?;
        farms.push(building);
        sites.insert(format!("cereal-farm-{}", i + 1), building);
    }
    for cohort in &cohorts {
        for (i, (x, y)) in [
            (-180., -180.),
            (-165., 110.),
            (-35., -95.),
            (-30., 180.),
            (100., -210.),
            (155., -60.),
            (160., 130.),
            (35., 50.),
            (55., -155.),
        ]
        .into_iter()
        .enumerate()
        {
            let anchor = cohort.anchor + vec2(x, y);
            let (building, _) = place_company(sim, plan, &roads, "bakery", anchor)?;
            sites.insert(format!("{}/bakery-{}", cohort.district, i + 1), building);
        }
    }
    // The north frontage reaches the village centre by a long detour. Put
    // groceries on both ends of the occupied roadside before assigning lots.
    for (name, x, y) in [
        ("hraska/ridge-bakery", 2014.0, 2776.0),
        ("hraska/east-frontage-bakery", 2260.0, 2647.0),
    ] {
        let (building, _) = place_company(sim, plan, &roads, "bakery", at(x, y))?;
        sites.insert(name.into(), building);
    }
    if profiled_wet {
        // Reserve the eastern market bakery before the South housing lots use
        // all of its immediate frontage.
        let (building, _) = place_company(sim, plan, &roads, "bakery", at(2580.0, 1098.0))?;
        sites.insert("south/market-bakery".into(), building);
    }
    // Leave a park site within the dense blocks before assigning their frontage
    // to housing, in places where fitting a park afterwards would displace a lot.
    for cohort in cohorts.iter().filter(|cohort| cohort.houses > 0) {
        let polygon = &plan
            .districts
            .iter()
            .find(|d| d.key == cohort.district)
            .unwrap()
            .polygon;
        let mut parks: Vec<Vec2> = Vec::new();
        for index in 1..=6 {
            let Some((building, _)) =
                place_scored_company(sim, plan, &roads, "park", |_, _, footprint| {
                    let center = footprint.center();
                    if !contains(polygon, center) {
                        return None;
                    }
                    let dist = center.distance(cohort.anchor);
                    let separation = parks
                        .iter()
                        .map(|&park| park.distance(center))
                        .fold(200.0, f32::min);
                    Some(dist - 4.0 * separation)
                })?
            else {
                break;
            };
            parks.push(sim.map().buildings()[building].obb.center());
            sites.insert(format!("{}/pocket-park-{index}", cohort.district), building);
        }
    }
    // These outlying streets had residents short of park coverage after
    // reload. Give their service sites priority over nearby buildable lots.
    let [ridge, north_frontage] = if wet {
        [(1250.0, 2600.0), (1330.0, 2700.0)]
    } else {
        [(1940.0, 2647.0), (2040.0, 2755.0)]
    };
    let [meadow, outer] = if wet {
        [
            ("south/east-frontage-park", 2598.0, 1118.0),
            ("south/east-junction-park", 2628.0, 1238.0),
        ]
    } else {
        [
            ("south/meadow-park", 1800.0, 685.0),
            ("south/outer-park", 2055.0, 730.0),
        ]
    };
    for (name, x, y) in [
        ("hraska/ridge-park", ridge.0, ridge.1),
        (
            "hraska/north-frontage-park",
            north_frontage.0,
            north_frontage.1,
        ),
        ("south/southwest-park", 1695.0, 885.0),
        meadow,
        outer,
        ("west/walkway-park", 1220.0, 1945.0),
        ("west/frontage-park", 1370.0, 1560.0),
    ] {
        let anchor = at(x, y);
        let (building, _) = place_scored_company(sim, plan, &roads, "park", |_, _, footprint| {
            let distance = footprint.center().distance2(anchor);
            (distance <= 150.0_f32.powi(2)).then_some(distance)
        })?
        .ok_or_else(|| format!("no clear civic site for {name}"))?;
        sites.insert(name.into(), building);
    }

    let mut zoning_roads = roads.clone();
    if let Some(avenue) = built.links.get(&SourceKey::Authored("hraska-west")) {
        // This through avenue has no loading frontage. Its nearest stocked
        // shop required a walk of almost 2.5 km from the approved housing.
        zoning_roads.remove(&avenue.road);
    }
    let intake_homes = intake_parcels.iter().map(|parcel| parcel.lot).collect();
    let (parcels, reserves) = plan_parcels(sim, plan, &zoning_roads, &cohorts, &intake_parcels)?;
    place_civic_sites(sim, plan, &roads, &cohorts, &parcels, &reserves, &mut sites)?;

    // Keep the food facilities clear of approved housing and future infill.
    // Opening them before housing still gives them the first hiring pool.
    let place_company_around_parcels = |sim: &mut Simulation, name: &str, anchor: Vec2, district: Option<&str>| {
        let polygon = district.and_then(|key| plan.districts.iter().find(|d| d.key == key));
        place_scored_company(sim, plan, &roads, name, |map, road, footprint| {
            if !map.roads()[road]
                .lanes_iter()
                .any(|(_, kind)| kind == LaneKind::Parking)
                || parcels
                    .iter()
                    .chain(&reserves)
                    .any(|parcel| parcel.footprint.intersects(&footprint))
            {
                return None;
            }
            let center = footprint.center();
            let outside = polygon.is_some_and(|district| !contains(&district.polygon, center));
            Some(center.distance2(anchor) + if outside { 450.0_f32.powi(2) } else { 0.0 })
        })?
        .map(|(building, _)| building)
        .ok_or_else(|| format!("no clear fitting site for {name} outside approved frontage"))
    };
    for cohort in cohorts.iter().filter(|cohort| cohort.houses > 0) {
        let depot = place_company_around_parcels(sim, "small-depot", cohort.anchor, None)?;
        sites.insert(format!("{}/depot", cohort.district), depot);
        depots.push(WorldCommand::SetWarehousePolicy {
            building: depot,
            policy: WarehousePolicy::ReserveLocally,
        });
    }
    // Leave retail an alternative to goods from local workshops while limiting
    // the number of simultaneous supermarket orders for meat, produce and cereal.
    for (district, x, y) in [
        ("station", 2210.0, 1800.0),
        ("hraska", 1960.0, 2440.0),
        ("south", 2050.0, 1100.0),
        ("west", 1350.0, 1840.0),
        ("employment", 2530.0, 2400.0),
    ] {
        if wet && matches!(district, "employment" | "south" | "station") {
            continue;
        }
        let store = place_company_around_parcels(sim, "supermarket", at(x, y), Some(district))?;
        sites.insert(format!("{district}/supermarket"), store);
    }
    if wet {
        let store =
            place_company_around_parcels(sim, "supermarket", at(2660.0, 1325.0), Some("south"))?;
        sites.insert("south/east-supermarket".into(), store);
    }
    if profiled_wet {
        // Add stocked frontage on the streets where the denser opening still
        // sends shoppers on the longest walks home.
        for (name, district, x, y) in [
            ("west/rim-bakery", "west", 570.0, 1788.0),
            ("west/northwest-bakery", "west", 718.0, 1948.0),
            ("south/alpska-bakery", "south", 2613.0, 1288.0),
            ("south/east-rim-bakery", "south", 2670.0, 1270.0),
            ("station/east-bakery", "station", 2070.0, 1775.0),
            ("station/north-bakery", "station", 2110.0, 1903.0),
        ] {
            let bakery = place_company_around_parcels(sim, "bakery", at(x, y), Some(district))?;
            sites.insert(name.into(), bakery);
        }
        // Use the stock meat and vegetable recipes to supply local shelves
        // when the freight terminal is also handling industrial imports.
        for (name, district, x, y) in [
            ("animal-farm", "south", 2100.0, 885.0),
            ("slaughterhouse", "south", 1900.0, 1350.0),
            ("meat-facility", "south", 2250.0, 1370.0),
            ("vegetable-farm", "west", 1180.0, 1500.0),
        ] {
            let supplier = place_company_around_parcels(sim, name, at(x, y), Some(district))?;
            sites.insert(name.into(), supplier);
        }
        let map = sim.map();
        for (from, to) in [
            ("animal-farm", "slaughterhouse"),
            ("slaughterhouse", "meat-facility"),
            ("meat-facility", "employment/supermarket"),
            ("vegetable-farm", "employment/supermarket"),
        ] {
            verify_parking_pair(&map, sites[from], sites[to], Tick(sim.get_tick()))?;
        }
        drop(map);
        // Keep the regular iron-ore buyer near the West workforce and verify
        // its loading route from the rail terminal.
        let foundry = place_company_around_parcels(
            sim,
            "foundry",
            at(1114.0, 2186.0),
            Some("west"),
        )?;
        verify_parking_pair(&sim.map(), terminal, foundry, Tick(sim.get_tick()))?;
        sites.insert("rail-foundry".into(), foundry);
    }
    place_walkable_bakeries(sim, plan, &roads, &parcels, &reserves, &mut sites)?;
    expand_cereal_fields(sim, plan, &farms, &parcels, &reserves)?;
    // Fit backup fuel supply around the approved opening and infill lots.
    for (name, offset) in [
        ("coal-mine-4", vec2(0.0, -65.0)),
        ("coal-mine-5", vec2(-85.0, -40.0)),
    ]
    .into_iter()
    .take(if profiled_wet { 2 } else { 1 })
    {
        let mine = place_company_around_parcels(sim, "coal-mine", plant_door + offset, None)?;
        verify_parking_pair(&sim.map(), mine, plant, Tick(sim.get_tick()))?;
        sites.insert(name.into(), mine);
    }
    if wet {
        // The existing stations serve the eastern blocks to capacity before
        // reaching this South frontage. Give it local fire service.
        let station = place_company_around_parcels(
            sim,
            "fire-station",
            at(2438.0, 938.0),
            Some("south"),
        )?;
        sites.insert("south/east-fire-station".into(), station);
    }
    if profiled_wet {
        let depot = place_company_around_parcels(
            sim,
            "garbage-depot",
            at(2390.0, 1125.0),
            Some("south"),
        )?;
        sites.insert("south/east-garbage-depot".into(), depot);
    }

    Ok(OpeningLayout {
        cohorts,
        parcels,
        reserves,
        intake_homes,
        sites,
        depots,
        freight,
        bakery: buyer,
    })
}

pub(super) fn commission_opening(
    sim: &mut Simulation,
    plan: &TownPlan,
    built: &BuiltNetwork,
    freight: Option<FreightSetup>,
) -> Result<OpeningManifest, String> {
    let OpeningLayout {
        cohorts,
        mut parcels,
        reserves,
        intake_homes,
        sites,
        depots,
        freight:
            FreightSetup {
                terminal,
                market,
                train,
                market_lane,
                market_offset,
                platform,
            },
        bakery,
    } = prepare_opening(sim, plan, built, freight)?;

    let wet = sim.map().environment().has_water_configuration();
    let near_outfalls = |parcel: &Parcel| outfall_housing(parcel, plan.config.owned_min);
    let near_east_market = |parcel: &Parcel| {
        let local = parcel.footprint.center() - plan.config.owned_min;
        parcel.zoning.kind == ZoneKind::Residential
            && parcel.district == "south"
            && local.x >= 2700.0
            && local.y >= 950.0
    };
    let early_retail = |parcel: &Parcel| {
        parcel.zoning.kind == ZoneKind::Commercial
            && matches!(parcel.district, "hraska" | "south")
    };
    let mut schedule = Simulation::schedule();
    if wet {
        // Settle the remaining waterfront housing by the downstream outlets.
        paint(sim, parcels.iter().filter(|parcel| near_outfalls(parcel)))?;
        advance(sim, &mut schedule, 3 * TICKS_PER_HOUR);
        // The eastern supermarket also needs a nearby hiring pool before
        // the town's other workplaces open.
        paint(
            sim,
            parcels.iter().filter(|parcel| near_east_market(parcel)),
        )?;
        advance(sim, &mut schedule, 2 * TICKS_PER_HOUR);
        record_development(&sim.map(), &mut parcels);
    }
    // Ordinary housing opens first so civic and utility sites can hire while
    // the first households arrive.
    let housing = parcels.iter().filter(|parcel| {
        parcel.zoning.kind == ZoneKind::Residential
            && !(wet && (near_outfalls(parcel) || near_east_market(parcel)
                || intake_homes.contains(&parcel.lot)))
    });
    paint(sim, housing)?;
    verify_votes(
        sim,
        parcels
            .iter()
            .filter(|parcel| parcel.zoning.kind == ZoneKind::Residential),
    )?;
    if wet {
        // Open local retail while these districts are settling so shops can
        // hire from the neighbourhood before the industrial parcels open.
        paint(sim, parcels.iter().filter(|parcel| early_retail(parcel)))?;
    }
    advance(sim, &mut schedule, 3 * TICKS_PER_HOUR);
    apply(sim, "opening depot policies", &depots)?;
    let (station, ent) = sim
        .world()
        .freight_stations
        .iter()
        .find(|(_, ent)| ent.f.building == terminal)
        .ok_or("missing terminal soul after settling")?;
    if train_stop(&ent.trans).distance2(platform) > 0.01 {
        return Err(format!(
            "freight terminal {terminal:?} changed its validated platform"
        ));
    }
    paint(
        sim,
        parcels
            .iter()
            .filter(|parcel| parcel.zoning.kind == ZoneKind::Industrial),
    )?;
    advance(sim, &mut schedule, 4 * TICKS_PER_HOUR);
    paint(
        sim,
        parcels
            .iter()
            .filter(|parcel| {
                parcel.zoning.kind == ZoneKind::Commercial && !(wet && early_retail(parcel))
            }),
    )?;
    record_development(&sim.map(), &mut parcels);
    verify_votes(sim, parcels.iter())?;

    let park = sites.get("sava/far-bank-park").copied();
    let (lines, far_stop) = install_bus_loops(sim, built, plan, &cohorts, &parcels, park)?;
    if park.is_some() && far_stop.is_none() {
        return Err("Sava park is missing its bus stop".into());
    }
    let cross_bank = park
        .zip(far_stop)
        .map(|(park, stop)| CrossBank { park, stop });
    let freight = Freight {
        terminal,
        station,
        market,
        train,
        market_lane,
        market_offset,
        bakery,
    };
    // Use elapsed time for settling periods. The authored clock starts in the
    // morning, so a whole elapsed day returns to the same commute phase.
    let warmup_days = if intake_homes.is_empty() { 4 } else { 5 };
    for day in 1..=warmup_days {
        let target = day * TICKS_PER_BUDGET_DAY;
        let now = elapsed_tick(sim).0;
        advance(sim, &mut schedule, target - now);
        eprintln!(
            "CITYMAP warm-up day {day}: {} residents, {} completed transit trips, treasury {}",
            sim.world().humans.len(),
            sim.read::<TripStats>().transit.completed,
            sim.read::<Government>().money
        );
    }
    let transit = sim.read::<Transit>();
    for (&name, line) in &lines {
        let actual = &transit.lines[line.id];
        let berths: Vec<_> = line.stops.iter().map(|&id| transit.stops[id].berth).collect();
        eprintln!(
            "CITYMAP {name} bus: {} buses, {:?} berths, {:?} directed leg times, suspensions {:?}",
            line.vehicles,
            berths,
            transit.directed_leg_times(line.id),
            transit.suspensions_since(0).filter(|event| event.line == line.id).collect::<Vec<_>>()
        );
        if actual.suspended {
            eprintln!("CITYMAP {name} bus suspended at handoff");
        }
    }
    let opening_tick = sim.get_tick();
    record_development(&sim.map(), &mut parcels);
    Ok(OpeningManifest {
        opening_tick,
        sites,
        parcels,
        reserves,
        lines,
        freight,
        cross_bank,
    })
}

fn advance(sim: &mut Simulation, schedule: &mut SeqSchedule, ticks: u64) {
    for _ in 0..ticks {
        sim.tick(schedule, []);
    }
}

fn outfall_housing(parcel: &Parcel, owned_min: Vec2) -> bool {
    let local = parcel.footprint.center() - owned_min;
    parcel.zoning.kind == ZoneKind::Residential
        && parcel.district == "south"
        && local.x < 1820.0
        && local.y < 760.0
}

fn expand_cereal_fields(
    sim: &mut Simulation,
    plan: &TownPlan,
    farms: &[BuildingID],
    parcels: &[Parcel],
    reserves: &[Parcel],
) -> Result<(), String> {
    for &farm in farms {
        let field = {
            let map = sim.map();
            let footprint = map.buildings()[farm].obb;
            let [along, outward] = footprint.axis();
            let fill = outward.normalize();
            // Extend away from the road so the established truck access and
            // reserved frontage stay usable.
            [80.0, 40.0].into_iter().find_map(|extra| {
                let field = OBB::new(
                    footprint.center() + fill * (extra * 0.5),
                    fill,
                    outward.mag() + extra,
                    along.mag(),
                );
                let command = WorldCommand::UpdateZone {
                    building: farm,
                    zone: Zone::new(Polygon(field.corners.to_vec()), along),
                };
                (site_clear(&map, plan, field, Some(farm))
                    && !parcels
                        .iter()
                        .chain(reserves)
                        .any(|parcel| parcel.footprint.intersects(&field))
                    && (map.land_ownership().extent().is_none() || command.preview(sim).is_ok()))
                .then_some(command)
            })
        };
        if let Some(command) = field {
            apply(sim, "enlarge cereal field", &[command])?;
        }
    }
    Ok(())
}

fn verify_parking_pair(map: &Map, a: BuildingID, b: BuildingID, tick: Tick) -> Result<(), String> {
    let access = |building: BuildingID| {
        let site = map.get(building)?;
        let spot = ParkingManagement::default()
            .reserve_near(site.door_pos, map)
            .ok()?;
        let pos = spot.park_pos(map)?;
        let lane = map.nearest_lane(pos, LaneKind::Driving, None)?;
        let road = map.get(site.connected_road?)?;
        road.lanes_iter()
            .filter(|(_, kind)| *kind == LaneKind::Driving)
            .any(|(frontage, _)| {
                let points = &map.get(frontage).unwrap().points;
                let midpoint = points.length() * 0.5;
                let parking = &map.get(lane).unwrap().points;
                let offset = parking.length_at_proj(parking.project(pos));
                Itinerary::route_to_lane(tick, pos, map, frontage, midpoint).is_some()
                    && Itinerary::route_to_lane(
                        tick,
                        points.point_along(midpoint),
                        map,
                        lane,
                        offset,
                    )
                    .is_some()
            })
            .then_some(pos)
    };
    if let Some((a_pos, b_pos)) = access(a).zip(access(b)) {
        if Itinerary::route(tick, a_pos, b_pos, map, PathKind::Vehicle).is_some()
            && Itinerary::route(tick, b_pos, a_pos, map, PathKind::Vehicle).is_some()
        {
            return Ok(());
        }
    }
    Err(format!(
        "freight sites {a:?} and {b:?} lack bidirectional parking access"
    ))
}

pub(super) struct FreightSetup {
    pub terminal: BuildingID,
    pub market: BuildingID,
    train: TrainID,
    market_lane: LaneID,
    market_offset: f32,
    platform: Vec3,
}

pub(super) fn prepare_freight(
    sim: &mut Simulation,
    plan: &TownPlan,
    built: &BuiltNetwork,
) -> Result<FreightSetup, String> {
    let roads = frontage_roads(plan, built);
    let wagons = vec![
        RollingStockID::new("locomotive"),
        RollingStockID::new("freight-wagon"),
    ];
    let consist_length_m = wagons
        .iter()
        .map(|id| try_prototype(*id).unwrap().length)
        .sum::<f32>()
        + 20.0;
    let proto_id = FreightStationPrototypeID::new("freight-station");
    let size = try_prototype(proto_id)
        .ok_or("missing freight station prototype")?
        .size;
    let (platforms, markets) = {
        let map = sim.map();
        let access: Vec<_> = roads
            .iter()
            .copied()
            .filter(|road| {
                let road = &map.roads()[*road];
                road.lanes_iter().any(|(_, kind)| kind == LaneKind::Parking)
            })
            .collect();
        let mut platforms = Vec::new();
        let mut markets = Vec::new();
        for (key, lane) in built
            .links
            .iter()
            .flat_map(|(&key, span)| span.lanes.iter().map(move |&lane| (key, lane)))
        {
            let live = &map.lanes()[lane];
            if live.kind != LaneKind::Rail {
                continue;
            }
            let points = &live.points;
            let margin = consist_length_m.ceil() as usize + 10;
            let limit = (points.length() as usize).saturating_sub(margin);
            for offset in (margin..limit).step_by(20) {
                let (on_track, tangent) = points.point_dir_along(offset as f32);
                let Some(dir) = tangent.xy().try_normalize() else {
                    continue;
                };
                let market = OBB::new(on_track.xy(), dir, 56.0, 32.0);
                markets.push((
                    on_track.xy().distance2(plan.station),
                    market,
                    lane,
                    offset as f32,
                ));
                if matches!(
                    key,
                    SourceKey::Authored("rail-outer-north" | "rail-outer-south")
                ) {
                    continue;
                }
                for dir in [dir, -dir] {
                    let across = dir.perpendicular();
                    let center = on_track.xy() - dir * 75.0 + across * 40.0;
                    // The stock freight frontage is long along the mainline. Keep
                    // its edge clear of the deck while using the real platform offset.
                    let width = size.h.min(56.0);
                    let footprint = OBB::new(center, dir, size.w, width);
                    if !site_clear(&map, plan, footprint, None) {
                        continue;
                    }
                    let Some(height) = map.environment().height(center) else {
                        continue;
                    };
                    let platform = train_stop(&Transform::new_dir(center.z(height), dir.z0()));
                    for &road in &access {
                        let near = map.roads()[road].points().project_2d(center).xy();
                        let to_road = near - center;
                        let door = vec2(
                            to_road.dot(across).clamp(-width * 0.5, width * 0.5),
                            to_road.dot(dir).clamp(-size.w * 0.5, size.w * 0.5),
                        );
                        let door_world = center + across * door.x + dir * door.y;
                        let distance = door_world.distance(near);
                        if distance > 40.0
                            || ParkingManagement::default()
                                .reserve_near(door_world.z(height), &map)
                                .is_err()
                        {
                            continue;
                        }
                        platforms.push((
                            center.distance(plan.station) + distance * 3.0,
                            footprint,
                            platform,
                            road,
                            door,
                        ));
                    }
                }
            }
        }
        (platforms, markets)
    };
    let mut platforms = platforms;
    let mut markets = markets;
    platforms.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Prefer the outer side of the mainline for the market marker.
    markets.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut rejected = BTreeMap::new();
    for (_, footprint, platform_stop, road, door) in &platforms {
        for &(_, market_obb, lane, offset) in &markets {
            if footprint.center().distance2(market_obb.center()) < 230.0_f32.powi(2) {
                continue;
            }
            let market = match sim.place_authored_rail_market(RailMarketSite {
                footprint: market_obb,
                service_lane: lane,
                service_offset: offset,
                platform_stop: *platform_stop,
                consist_length_m,
            }) {
                Ok(id) => id,
                Err(error) => {
                    *rejected.entry(error.to_string()).or_insert(0) += 1;
                    if error == RailMarketError::Duplicate {
                        return Err(error.to_string());
                    }
                    continue;
                }
            };
            let terminal = place_special(
                sim,
                "freight terminal",
                BuildingKind::RailFreightStation(proto_id),
                *footprint,
                BuildingGen::NoWalkway { door_pos: *door },
                *road,
            )?;
            apply(
                sim,
                "freight consist",
                &[WorldCommand::SpawnTrain {
                    wagons,
                    lane,
                    dist: offset,
                }],
            )?;
            let train = sim
                .world()
                .trains
                .keys()
                .next()
                .ok_or("missing freight train")?;
            return Ok(FreightSetup {
                terminal,
                market,
                train,
                market_lane: lane,
                market_offset: offset,
                platform: *platform_stop,
            });
        }
    }
    Err(format!(
        "no dispatchable CITYMAP freight pair ({} platforms, {} market anchors): {rejected:?}",
        platforms.len(),
        markets.len()
    ))
}

struct ReachRoad {
    ends: [usize; 2],
    directions: [bool; 2],
    points: geom::PolyLine3,
}

#[derive(Clone, Copy)]
struct ReachPosition {
    road: RoadID,
    along: f32,
    offset: f32,
}

struct ServiceReach {
    distances: Vec<Vec<f32>>,
    roads: BTreeMap<RoadID, ReachRoad>,
}

impl ServiceReach {
    fn new(map: &Map, walking: bool) -> Self {
        let mut intersections: Vec<_> = map.intersections().keys().collect();
        intersections.sort_unstable();
        let indices: BTreeMap<_, _> = intersections
            .into_iter()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();
        let mut distances = vec![vec![f32::INFINITY; indices.len()]; indices.len()];
        for (i, row) in distances.iter_mut().enumerate() {
            row[i] = 0.0;
        }
        let mut roads = BTreeMap::new();
        for (id, road) in map.roads().iter() {
            let ends = [indices[&road.src], indices[&road.dst]];
            let directions = if walking {
                [road.lanes_iter().any(|(_, kind)| kind == LaneKind::Walking); 2]
            } else {
                [road.src, road.dst].map(|src| {
                    road.outgoing_lanes_from(src)
                        .iter()
                        .any(|(_, kind)| *kind == LaneKind::Driving)
                })
            };
            if directions == [false; 2] {
                continue;
            }
            for (i, usable) in directions.into_iter().enumerate() {
                if usable {
                    let distance = &mut distances[ends[i]][ends[1 - i]];
                    *distance = distance.min(road.length());
                }
            }
            roads.insert(
                id,
                ReachRoad {
                    ends,
                    directions,
                    points: road.points().clone(),
                },
            );
        }
        // The compact town has fewer than 200 junctions. Distances are reused
        // across all provider candidate sites and never touch simulation state.
        for i in 0..distances.len() {
            let onward = distances[i].clone();
            for row in &mut distances {
                let approach = row[i];
                if approach.is_finite() {
                    for (distance, next) in row.iter_mut().zip(&onward) {
                        *distance = distance.min(approach + next);
                    }
                }
            }
        }
        Self { distances, roads }
    }

    fn position(&self, road: RoadID, point: Vec2) -> Option<ReachPosition> {
        let points = &self.roads.get(&road)?.points;
        let projected = points.project_2d(point);
        let along = points.length_at_proj(projected);
        Some(ReachPosition {
            road,
            along,
            offset: projected.xy().distance(point),
        })
    }

    fn distance(&self, from: ReachPosition, to: ReachPosition) -> f32 {
        let a = &self.roads[&from.road];
        let b = &self.roads[&to.road];
        let delta = to.along - from.along;
        let mut best = if from.road == to.road && a.directions[usize::from(delta < 0.0)] {
            delta.abs()
        } else {
            f32::INFINITY
        };
        let out = [a.points.length() - from.along, from.along];
        let into = [to.along, b.points.length() - to.along];
        for (i, travel) in out
            .into_iter()
            .enumerate()
            .filter(|(i, _)| a.directions[*i])
        {
            for (j, arrive) in into
                .into_iter()
                .enumerate()
                .filter(|(j, _)| b.directions[*j])
            {
                best = best.min(travel + self.distances[a.ends[1 - i]][b.ends[j]] + arrive);
            }
        }
        best + from.offset + to.offset
    }
}

fn place_civic_sites(
    sim: &mut Simulation,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
    cohorts: &[Cohort],
    parcels: &[Parcel],
    reserves: &[Parcel],
    sites: &mut BTreeMap<String, BuildingID>,
) -> Result<(), String> {
    let (street, foot, wet) = {
        let map = sim.map();
        (
            ServiceReach::new(&map, false),
            ServiceReach::new(&map, true),
            map.environment().has_water_configuration(),
        )
    };
    let protected: Vec<_> = parcels
        .iter()
        .chain(reserves)
        .map(|parcel| parcel.footprint)
        .collect();
    for (name, reach) in [
        ("school", &foot),
        ("fire-station", &street),
        ("police-station", &street),
        ("clinic", &street),
        ("garbage-depot", &street),
        ("park", &foot),
    ] {
        let service =
            try_prototype(ServiceID::new(name)).ok_or_else(|| format!("missing service {name}"))?;
        let radius = service.radius_m - 15.0;
        let mut receivers: Vec<_> = parcels
            .iter()
            .chain(reserves)
            .filter_map(|parcel| {
                let mut position = reach.position(parcel.road, parcel.footprint.center())?;
                // The generated door sits closer to the road than the lot center.
                position.offset = (position.offset - 8.0).max(0.0);
                let residents = if parcel.zoning.kind == ZoneKind::Residential {
                    ResidencePrototype::iter()
                        .filter(|home| {
                            home.zone_kind
                                == prototypes::GrowableZoneKind::Pure(ZoneKind::Residential)
                                && home.density == parcel.zoning.density
                        })
                        .map(|home| home.capacity)
                        .max()
                        .unwrap_or(parcel.capacity)
                } else {
                    0
                };
                Some((
                    position,
                    residents,
                    residents + parcel.workers,
                    parcel.district,
                ))
            })
            .collect();
        let front = |footprint: OBB| (footprint.corners[0] + footprint.corners[1]) * 0.5;
        let allocation = |source, receivers: &[(ReachPosition, u32, u32, &str)]| {
            let mut reachable: Vec<_> = receivers
                .iter()
                .enumerate()
                .filter_map(|(i, receiver)| {
                    let distance = reach.distance(source, receiver.0);
                    (distance <= radius && receiver.2 > 0).then_some((distance, i))
                })
                .collect();
            reachable.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let mut available = service.capacity * 9 / 10;
            let mut serviced = Vec::new();
            for (_, i) in reachable {
                if available == 0 {
                    break;
                }
                let served = available.min(receivers[i].2);
                available -= served;
                serviced.push((i, served));
            }
            serviced
        };
        let mut serial = 0;
        for cohort in cohorts.iter().filter(|cohort| cohort.houses > 0) {
            let in_district =
                |receiver: &(ReachPosition, u32, u32, &str)| receiver.3 == cohort.district;
            let total: u32 = receivers
                .iter()
                .filter(|r| in_district(r))
                .map(|r| r.1)
                .sum();
            let remaining = |receivers: &[(ReachPosition, u32, u32, &str)]| {
                receivers
                    .iter()
                    .filter(|r| in_district(r))
                    .map(|r| r.1.min(r.2))
                    .sum::<u32>()
            };
            while remaining(&receivers) * 100 > total * 3
                && (service.service_kind != ServiceKind::School || serial < MAX_SCHOOLS)
            {
                let Some((building, road)) =
                    place_scored_company(sim, plan, roads, name, |_, road, footprint| {
                        let distance = footprint.center().distance(cohort.anchor);
                        if protected.iter().any(|parcel| {
                            parcel.center().distance2(footprint.center()) < 45.0_f32.powi(2)
                                && parcel.intersects(&footprint)
                        }) {
                            return None;
                        }
                        let source = reach.position(road, front(footprint))?;
                        let covered: u32 = allocation(source, &receivers)
                            .into_iter()
                            .map(|(i, served)| {
                                let receiver = &receivers[i];
                                if in_district(receiver) {
                                    served.min(receiver.1)
                                } else {
                                    0
                                }
                            })
                            .sum();
                        (covered > 0).then_some(distance / 1000.0 - covered as f32)
                    })?
                else {
                    break;
                };
                let footprint = sim.map().buildings()[building].obb;
                let source = reach.position(road, front(footprint)).unwrap();
                for (i, served) in allocation(source, &receivers) {
                    receivers[i].2 -= served;
                }
                serial += 1;
                sites.insert(format!("{name}-{serial}"), building);
            }
        }
        if wet && service.service_kind == ServiceKind::School {
            // The settled South infill strains the schools around its eastern
            // streets. Use any remaining school allowance near that frontage.
            for point in [vec2(2363.0, 1168.0), vec2(2488.0, 818.0)]
                .into_iter()
                .take(MAX_SCHOOLS - serial)
            {
                let anchor = super::layout::design_point(point, plan.config);
                let school = place_scored_company(sim, plan, roads, name, |_, _, footprint| {
                    let distance = footprint.center().distance2(anchor);
                    (distance <= 160.0_f32.powi(2)
                        && protected
                            .iter()
                            .all(|parcel| !parcel.intersects(&footprint)))
                    .then_some(distance)
                })?;
                if let Some((building, _)) = school {
                    serial += 1;
                    sites.insert(format!("{name}-{serial}"), building);
                }
            }
        }
    }
    Ok(())
}

fn place_walkable_bakeries(
    sim: &mut Simulation,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
    parcels: &[Parcel],
    reserves: &[Parcel],
    sites: &mut BTreeMap<String, BuildingID>,
) -> Result<(), String> {
    let (reach, existing, wet) = {
        let map = sim.map();
        let reach = ServiceReach::new(&map, true);
        let bakery = prototypes::GoodsCompanyID::new("bakery");
        let existing: Vec<_> = map.buildings().values()
            .filter(|building| building.kind.as_goods_company() == Some(bakery))
            .filter_map(|building| reach.position(building.connected_road?, building.door_pos.xy()))
            .collect();
        (reach, existing, map.environment().has_water_configuration())
    };
    let (distance_limit, site_limit) = if wet { (WET_BAKERY_WALK_M, 48) } else { (240.0, 4) };
    let mut underserved: Vec<_> = parcels.iter()
        .filter(|parcel| parcel.zoning.kind == ZoneKind::Residential)
        .filter_map(|parcel| {
            let home = reach.position(parcel.road, parcel.footprint.center())?;
            existing
                .iter()
                .all(|&shop| reach.distance(shop, home) > distance_limit)
                .then_some((home, parcel.capacity))
        })
        .collect();
    for serial in 1..=site_limit {
        let Some((building, road)) = place_scored_company(sim, plan, roads, "bakery", |map, road, footprint| {
            let front = (footprint.corners[0] + footprint.corners[1]) * 0.5;
            let shop = reach.position(road, front)?;
            let score: f32 = underserved.iter().map(|&(home, residents)| {
                residents as f32 * (distance_limit - reach.distance(shop, home)).max(0.0)
            }).sum();
            if score <= 0.0 || parcels.iter().chain(reserves)
                .any(|parcel| parcel.footprint.intersects(&footprint)) {
                return None;
            }
            let curb_parking = map.roads()[road].lanes_iter().any(|(_, kind)| kind == LaneKind::Parking);
            (curb_parking || wet && ParkingManagement::default()
                .reserve_near_radius(front.z(10.0), map, 100.0).is_ok())
                .then_some(-score)
        })? else {
            break;
        };
        let door = sim.map().buildings()[building].door_pos.xy();
        let shop = reach.position(road, door).unwrap();
        underserved.retain(|&(home, _)| reach.distance(shop, home) > distance_limit);
        sites.insert(format!("walkable-bakery-{serial}"), building);
    }
    Ok(())
}

fn opening_frontage(
    map: &Map,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
    polygon: &[Vec2],
    lot: &simulation::map::Lot,
) -> bool {
    roads.contains(&lot.parent)
        && map.electricity.net_id(lot.parent).is_some()
        && lot.shape.corners.iter().all(|&corner| contains(polygon, corner))
        && !crosses_reserved_bank(plan, lot.shape)
        && ParkingManagement::default()
            .reserve_near(lot.shape.center().z(lot.height), map)
            .is_ok()
}

fn commission_intake_frontage(
    sim: &mut Simulation,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
) -> Result<Vec<Parcel>, String> {
    // Settle the intake crews before the mills, farms and shops start hiring.
    let mut housing = {
        let map = sim.map();
        let district = plan.districts.iter().find(|area| area.key == "west").unwrap();
        let anchor = super::layout::design_point(vec2(1156.0, 1245.0), plan.config);
        let rings = blocks(&plan.graph);
        let mut candidates: Vec<_> = map.lots().values()
            .filter(|lot| opening_frontage(&map, plan, roads, &district.polygon, lot))
            .map(|lot| (lot, rings.iter().position(|ring| contains(ring, lot.shape.center()))))
            .collect();
        candidates.sort_by(|(a, _), (b, _)| {
            a.shape.center().distance2(anchor)
                .total_cmp(&b.shape.center().distance2(anchor))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut housing = Vec::new();
        take_parcels(&map, "west", &mut candidates, downtown_cells().0, 12, &mut housing)?;
        housing
    };
    paint(sim, housing.iter())?;
    advance(sim, &mut Simulation::schedule(), 4 * TICKS_PER_HOUR);
    record_development(&sim.map(), &mut housing);
    verify_votes(sim, housing.iter())?;
    Ok(housing)
}

fn plan_parcels(
    sim: &Simulation,
    plan: &TownPlan,
    roads: &BTreeSet<RoadID>,
    cohorts: &[Cohort],
    commissioned: &[Parcel],
) -> Result<(Vec<Parcel>, Vec<Parcel>), String> {
    use Density::{High, Low, Medium};
    use ZoneKind::{Commercial, Industrial, Residential};
    let map = sim.map();
    let wet = map.environment().has_water_configuration();
    let profiled_housing = wet && ResidencePrototype::iter().any(|home| home.massing.is_some());
    let rings = blocks(&plan.graph);
    let (high_house, high_shop) = downtown_cells();
    let medium_house = ZoneCell {
        kind: Residential,
        density: Medium,
    };
    let mut painted = Vec::new();
    let mut reserves = Vec::new();
    for cohort in cohorts {
        // The profiled High residences have substantially more capacity.
        // Use Medium for ordinary wet neighbourhood frontage, with High
        // at the centre and the existing blocks that initiate each district.
        let medium_neighbourhood = profiled_housing && cohort.district != "station";
        let neighbourhood_house = if medium_neighbourhood { medium_house } else { high_house };
        let district = plan
            .districts
            .iter()
            .find(|district| district.key == cohort.district)
            .unwrap();
        let mut candidates: Vec<_> = map
            .lots()
            .values()
            .filter(|lot| {
                !commissioned.iter().any(|parcel| parcel.lot == lot.id)
                    && opening_frontage(&map, plan, roads, &district.polygon, lot)
            })
            .map(|lot| {
                (
                    lot,
                    rings
                        .iter()
                        .position(|ring| contains(ring, lot.shape.center())),
                )
            })
            .collect();
        let reserve_anchor = district
            .reservations
            .iter()
            .find(|site| site.role == "infill")
            .map_or(cohort.anchor, |site| site.position);
        candidates.sort_by(|(a, _), (b, _)| {
            a.shape
                .center()
                .distance2(reserve_anchor)
                .total_cmp(&b.shape.center().distance2(reserve_anchor))
                .then_with(|| a.id.cmp(&b.id))
        });
        let reserve_count = cohort.infill;
        let reserve_cell = if cohort.houses > 0 {
            neighbourhood_house
        } else {
            ZoneCell {
                kind: Industrial,
                density: Medium,
            }
        };
        take_parcels(
            &map,
            district.key,
            &mut candidates,
            reserve_cell,
            reserve_count,
            &mut reserves,
        )?;
        let retail_reserve = ZoneCell {
            kind: Commercial,
            density: Medium,
        };
        if wet && cohort.houses > 0 {
            // Reserve neighbouring shops before housing uses the remaining
            // frontage on these narrower wet neighbourhoods.
            take_parcels(
                &map,
                district.key,
                &mut candidates,
                retail_reserve,
                2,
                &mut reserves,
            )?;
        }
        candidates.sort_by(|(a, _), (b, _)| {
            a.shape
                .center()
                .distance2(cohort.anchor)
                .total_cmp(&b.shape.center().distance2(cohort.anchor))
                .then_with(|| a.id.cmp(&b.id))
        });
        let base = painted.len();
        painted.extend(commissioned.iter().filter(|parcel| parcel.district == district.key).cloned());
        let already_open = painted.len() - base;
        let starter_zone = if cohort.houses > 0 {
            high_house
        } else {
            ZoneCell {
                kind: Industrial,
                density: Medium,
            }
        };
        let mut block_count = BTreeMap::new();
        for &(lot, block) in &candidates {
            if let Some(block) =
                block.filter(|_| check_growable_lot(&map, lot, starter_zone).is_ok())
            {
                *block_count.entry(block).or_insert(0usize) += 1;
            }
        }
        let mut chosen_blocks = BTreeSet::new();
        // The employment district has a spare starter block in case one
        // of the first two opens with an inactive lot.
        let starter_blocks = if cohort.district == "employment" { 3 } else { 2 };
        for block in candidates.iter().filter_map(|(_, block)| *block) {
            if block_count.get(&block).copied().unwrap_or(0) >= 2 {
                chosen_blocks.insert(block);
                if chosen_blocks.len() == starter_blocks {
                    break;
                }
            }
        }
        if chosen_blocks.len() < 2 {
            return Err(format!("{}: fewer than two fitting blocks", district.key));
        }
        let mut starting_industry = 0;
        for block in chosen_blocks {
            candidates.sort_by_key(|(_, candidate)| *candidate != Some(block));
            let count = if cohort.district == "employment" {
                block_count[&block].min(3)
            } else {
                2
            };
            take_parcels(
                &map,
                district.key,
                &mut candidates,
                starter_zone,
                count,
                &mut painted,
            )?;
            starting_industry += count;
        }
        candidates.sort_by(|(a, _), (b, _)| {
            a.shape
                .center()
                .distance2(cohort.anchor)
                .total_cmp(&b.shape.center().distance2(cohort.anchor))
                .then_with(|| a.id.cmp(&b.id))
        });
        let shop_cell = if cohort.district == "station" {
            high_shop
        } else {
            ZoneCell { kind: Commercial, density: Medium }
        };
        let shops_start = painted.len();
        for _ in 0..cohort.retail {
            // Spread retail across walkable frontage rather than filling the
            // nearest row of lots before choosing the residential parcels.
            let score = |lot: &simulation::map::Lot| {
                let center = lot.shape.center();
                let distance = center.distance(cohort.anchor);
                let separation = painted[shops_start..].iter()
                    .map(|shop| shop.footprint.center().distance(center))
                    .fold(135.0, f32::min);
                distance * 0.6 - separation * 1.6
            };
            candidates.sort_by(|(a, _), (b, _)| {
                score(a).total_cmp(&score(b)).then_with(|| a.id.cmp(&b.id))
            });
            take_parcels(&map, district.key, &mut candidates, shop_cell, 1, &mut painted)?;
        }
        candidates.sort_by(|(a, _), (b, _)| {
            a.shape.center().distance2(cohort.anchor)
                .total_cmp(&b.shape.center().distance2(cohort.anchor))
                .then_with(|| a.id.cmp(&b.id))
        });
        let fitting = |candidates: &[(&simulation::map::Lot, Option<usize>)], zoning| {
            candidates
                .iter()
                .filter(|(lot, _)| check_growable_lot(&map, lot, zoning).is_ok())
                .count()
        };
        if cohort.district == "employment" {
            let high = ZoneCell {
                kind: Industrial,
                density: High,
            };
            let remaining = cohort.industry - starting_industry;
            let works = remaining.min(fitting(&candidates, high));
            take_parcels(
                &map,
                district.key,
                &mut candidates,
                high,
                works,
                &mut painted,
            )?;
            take_parcels(
                &map,
                district.key,
                &mut candidates,
                starter_zone,
                remaining - works,
                &mut painted,
            )?;
        }
        let houses = if cohort.houses > 0 {
            cohort.houses.saturating_sub(already_open)
                .min(fitting(&candidates, neighbourhood_house) + 4)
        } else {
            0
        };
        let low = if cohort.district == "station" {
            0
        } else {
            houses / 15
        };
        let medium = if cohort.district == "station" || medium_neighbourhood {
            0
        } else {
            houses / 8
        };
        take_parcels(
            &map,
            district.key,
            &mut candidates,
            neighbourhood_house,
            houses.saturating_sub(low + medium + 4),
            &mut painted,
        )?;

        take_parcels(
            &map,
            district.key,
            &mut candidates,
            medium_house,
            medium,
            &mut painted,
        )?;
        take_parcels(
            &map,
            district.key,
            &mut candidates,
            ZoneCell {
                kind: Residential,
                density: Low,
            },
            low,
            &mut painted,
        )?;
        if cohort.houses > 0 {
            let workshop = ZoneCell {
                kind: Industrial,
                density: Medium,
            };
            let stores: Vec<_> = painted[base..]
                .iter()
                .filter(|parcel| parcel.zoning.kind == Commercial)
                .map(|parcel| (parcel.road, parcel.footprint.center()))
                .collect();
            let distance_to_store = |lot: &simulation::map::Lot| {
                stores
                    .iter()
                    .map(|&(road, center)| {
                        let distance = center.distance(lot.shape.center());
                        // Prefer shared frontage when it keeps the leg short.
                        distance + if road == lot.parent { 0.0 } else { 80.0 }
                    })
                    .fold(f32::INFINITY, f32::min)
            };
            candidates.sort_by(|(a, _), (b, _)| {
                distance_to_store(a)
                    .total_cmp(&distance_to_store(b))
                    .then_with(|| a.id.cmp(&b.id))
            });
            let count = cohort.industry.min(fitting(&candidates, workshop));
            take_parcels(&map, district.key, &mut candidates, workshop, count, &mut painted)?;
            // Keep surplus fitting lots as additional capacity without moving
            // any of the opening housing, stores or workshops.
            candidates.sort_by(|(a, _), (b, _)| {
                a.shape.center().distance2(reserve_anchor)
                    .total_cmp(&b.shape.center().distance2(reserve_anchor))
                    .then_with(|| a.id.cmp(&b.id))
            });
            let additional = 22.min(fitting(&candidates, neighbourhood_house));
            take_parcels(
                &map,
                district.key,
                &mut candidates,
                neighbourhood_house,
                additional,
                &mut reserves,
            )?;
            // Put future shops beside the new homes so they can hire from the
            // same neighbourhood during the infill trial.
            if !wet {
                let shops = 2.min(fitting(&candidates, retail_reserve));
                take_parcels(
                    &map,
                    district.key,
                    &mut candidates,
                    retail_reserve,
                    shops,
                    &mut reserves,
                )?;
            }
        }
        let mut counts = BTreeMap::new();
        for parcel in &painted[base..] {
            if let Some(block) = parcel.block {
                *counts.entry(block).or_insert(0) += 1;
            }
        }
        if counts.values().filter(|&&count| count >= 2).count() < 2 {
            return Err(format!(
                "{}: fewer than two blocks with two approved parcels ({counts:?})",
                district.key
            ));
        }
    }
    Ok((painted, reserves))
}

fn take_parcels(
    map: &Map,
    district: &'static str,
    candidates: &mut Vec<(&simulation::map::Lot, Option<usize>)>,
    zoning: ZoneCell,
    count: usize,
    selected: &mut Vec<Parcel>,
) -> Result<(), String> {
    let first = selected.len();
    candidates.retain(|&(lot, block)| {
        if selected.len() - first == count {
            return true;
        }
        let Ok(site) = check_growable_lot(map, lot, zoning) else {
            return true;
        };
        selected.push(Parcel {
            lot: lot.id,
            road: lot.parent,
            district,
            block,
            footprint: lot.shape,
            zoning,
            capacity: site.home.map_or(0, |home| home.capacity),
            workers: site.company.map_or(0, |company| company.base.n_workers),
            building: None,
        });
        false
    });
    if selected.len() - first < count {
        return Err(format!(
            "{district}: only {} of {count} parcels fit {zoning:?}",
            selected.len() - first
        ));
    }
    Ok(())
}

fn paint<'a>(
    sim: &mut Simulation,
    parcels: impl Iterator<Item = &'a Parcel>,
) -> Result<(), String> {
    let commands: Vec<_> = parcels
        .map(|parcel| WorldCommand::ZonePaint {
            area: parcel.footprint,
            kind: Some(parcel.zoning.kind),
            density: parcel.zoning.density,
        })
        .collect();
    for batch in commands.chunks(sim.command_batch_limit()) {
        apply(sim, "parcel paint", batch)?;
    }
    Ok(())
}

fn verify_votes<'a>(
    sim: &Simulation,
    parcels: impl Iterator<Item = &'a Parcel>,
) -> Result<(), String> {
    let map = sim.map();
    let grid = sim.read::<ZoneGrid>();
    let mut expected = BTreeMap::new();
    for parcel in parcels {
        verify_parcel_frontage(&map, &grid, parcel)?;
        if parcel.building.is_none() {
            expected.insert(parcel.lot, parcel.zoning);
        }
    }
    audit_votes(
        map.lots()
            .values()
            .map(|lot| (lot.id, grid.lot_zone(&lot.shape))),
        &expected,
    )
}

fn audit_votes(
    votes: impl Iterator<Item = (LotID, Option<ZoneCell>)>,
    expected: &BTreeMap<LotID, ZoneCell>,
) -> Result<(), String> {
    let mut surviving_votes = 0;
    for (lot, actual) in votes {
        let intended = expected.get(&lot).copied();
        if actual != intended {
            return Err(format!(
                "parcel {lot:?}: zone vote {actual:?} differs from intended {intended:?}"
            ));
        }
        surviving_votes += usize::from(intended.is_some());
    }
    if surviving_votes != expected.len() {
        return Err(format!(
            "only {surviving_votes} of {} planned vacant lots survive",
            expected.len()
        ));
    }
    Ok(())
}

fn same_footprint(actual: &OBB, expected: &OBB) -> bool {
    actual
        .corners
        .iter()
        .zip(expected.corners)
        .all(|(actual, expected)| actual.is_close(expected, 0.001))
}

fn verify_parcel_frontage(map: &Map, zones: &ZoneGrid, parcel: &Parcel) -> Result<(), String> {
    let (shape, road) = if let Some(id) = parcel.building {
        let origin = map.lot_origin(id).ok_or_else(|| format!("missing parcel origin for {id:?}"))?;
        if map.get(id).and_then(|building| building.connected_road) != Some(parcel.road) {
            return Err(format!("developed parcel {id:?} lost its road connection"));
        }
        (&origin.shape, origin.parent_road)
    } else {
        let lot = map.get(parcel.lot).ok_or_else(|| format!("missing opening lot {:?}", parcel.lot))?;
        (&lot.shape, lot.parent)
    };
    if road != parcel.road || !same_footprint(shape, &parcel.footprint)
        || zones.lot_zone(shape) != Some(parcel.zoning)
    {
        return Err(format!("parcel {:?} changed footprint, frontage or zoning", parcel.lot));
    }
    Ok(())
}

fn record_development(map: &Map, parcels: &mut [Parcel]) {
    let developed: Vec<_> = map
        .buildings()
        .values()
        .filter_map(|building| Some((building.id, map.lot_origin(building.id)?)))
        .collect();
    for parcel in parcels {
        parcel.building = developed.iter().find_map(|&(id, origin)| {
            same_footprint(&origin.shape, &parcel.footprint).then_some(id)
        });
        if parcel.building.is_some() || map.get(parcel.lot).is_some() {
            continue;
        }
        // A collapsed growable can restore the same parcel under a new lot ID.
        if let Some(restored) = map.lots().values().find(|lot| {
            lot.parent == parcel.road && same_footprint(&lot.shape, &parcel.footprint)
        }) {
            parcel.lot = restored.id;
        }
    }
}

#[derive(Clone, Copy)]
struct CurbBerth {
    lane: LaneID,
    offset: f32,
    pos: Vec3,
    dir: Vec2,
}

fn loop_distance(map: &Map, stops: &[CurbBerth], tick: Tick) -> Option<f32> {
    let mut distance = 0.0;
    for (from, to) in stops.iter().zip(stops.iter().cycle().skip(1)) {
        let route = Itinerary::route_to_lane(tick, from.pos, map, to.lane, to.offset)?;
        let mut previous = from.pos;
        for &point in route.local_path().iter().rev() {
            distance += previous.distance(point);
            previous = point;
        }
        if let Some(route) = route.get_route() {
            distance += route
                .reversed_route
                .iter()
                .skip(1)
                .filter_map(|span| span.raw_points(map))
                .map(|points| points.length())
                .sum::<f32>();
            if !route.reversed_route.is_empty() {
                distance += to.offset;
            }
        }
    }
    Some(distance)
}

fn install_bus_loops(
    sim: &mut Simulation,
    built: &BuiltNetwork,
    plan: &TownPlan,
    cohorts: &[Cohort],
    parcels: &[Parcel],
    far_bank_park: Option<BuildingID>,
) -> Result<(BTreeMap<&'static str, BusLine>, Option<StopID>), String> {
    let mut anchors = vec![
        cohorts[0].anchor, // Shared civic hub.
        cohorts[2].anchor, // South.
        cohorts[3].anchor, // West.
        super::layout::design_point(vec2(1920.0, 2400.0), plan.config),
        super::layout::design_point(vec2(2390.0, 2540.0), plan.config), // Employment frontage.
    ];
    let parkway = far_bank_park.map(|park| {
        let map = sim.map();
        let park = &map.buildings()[park];
        anchors.push(park.door_pos.xy());
        park.connected_road.unwrap()
    });
    let park_index = 5;
    let neighbourhood: &[usize] = if parkway.is_some() {
        &[0, 1, park_index, 2]
    } else {
        &[0, 1, 2]
    };
    let routes = [
        // South to West has a long detour around the walking-only link.
        (
            "neighbourhood",
            neighbourhood,
            if parkway.is_some() { 8 } else { 7 },
        ),
        ("employment", &[0, 3, 4][..], 6),
    ];
    let berths = {
        let map = sim.map();
        let mut options = Vec::new();
        let mut used = BTreeSet::new();
        let walk = ServiceReach::new(&map, true);
        for (index, &anchor) in anchors.iter().enumerate() {
            let district = [Some(0), Some(2), Some(3), Some(1), Some(4), None][index];
            let destinations: Vec<_> = parcels
                .iter()
                .filter(|p| {
                    district.is_some_and(|district| p.district == cohorts[district].district)
                })
                .filter_map(|p| {
                    Some((
                        walk.position(p.road, p.footprint.center())?,
                        p.capacity.max(p.workers),
                    ))
                })
                .collect();
            let score = |road, berth: CurbBerth| {
                let distance = berth.pos.xy().distance(anchor);
                let nearby = walk.position(road, berth.pos.xy()).map_or(0.0, |source| {
                    destinations
                        .iter()
                        .map(|&(target, people)| {
                            f64::from((1.0 - walk.distance(source, target) / 250.0).max(0.0))
                                * f64::from(people)
                        })
                        .sum::<f64>()
                });
                nearby - f64::from(distance) * 0.2
            };
            let (road, berths) = built
                .links
                .iter()
                .filter(|(key, span)| {
                    if index == park_index {
                        return Some(span.road) == parkway;
                    }
                    Some(span.road) != parkway
                        && !used.contains(&span.road)
                        && matches!(
                            plan.graph.links[key].class,
                            RoadClass::Local | RoadClass::Collector | RoadClass::Avenue
                        )
                        && plan.graph.links[key].layer == 0
                        && !plan.crossings.iter().any(|crossing| {
                            crossing.approaches_links.contains(key) || crossing.bridge == **key
                        })
                })
                .filter_map(|(_, span)| {
                    let road = &map.roads()[span.road];
                    // Keep a bus and a queue clear of either junction and leave
                    // storage on the berth lane for through traffic.
                    let berths: Vec<_> = [road.src, road.dst]
                        .into_iter()
                        .filter_map(|src| {
                            road.sidewalks(src).outgoing?;
                            let &(lane, _) = road
                                .outgoing_lanes_from(src)
                                .iter()
                                .rfind(|(_, kind)| *kind == LaneKind::Driving)?;
                            let points = &map.lanes()[lane].points;
                            let offset = points.length() * 0.5;
                            let (pos, dir) = points.point_dir_along(offset);
                            (offset >= 45.0 && (pos.z - 10.0).abs() < 1.0).then_some(CurbBerth {
                                lane,
                                offset,
                                pos,
                                dir: dir.xy(),
                            })
                        })
                        .collect();
                    let berths: [CurbBerth; 2] = berths.try_into().ok()?;
                    Some((span.road, berths))
                })
                .filter(|(_, berths)| {
                    let radius = if district == Some(4) { 350.0 } else { 430.0 };
                    berths[0].pos.xy().distance(anchor) < radius
                })
                .map(|(road, berths)| {
                    let lanes = map.roads()[road]
                        .outgoing_lanes_from(map.roads()[road].src)
                        .iter()
                        .filter(|(_, kind)| *kind == LaneKind::Driving)
                        .count();
                    let storage = berths[0].offset.min(120.0) + berths[1].offset.min(120.0);
                    let berth_score = score(road, berths[0]) + f64::from(storage) * 0.4
                        + (lanes.saturating_sub(1) as f64) * 40.0;
                    (berth_score, road, berths)
                })
                .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
                .map(|(_, road, berths)| (road, berths))
                .ok_or_else(|| format!("no two-way curb berth near {anchor:?}"))?;
            used.insert(road);
            options.push(berths);
        }

        let tick = Tick(sim.get_tick());
        let mut best = None;
        for mask in 0..(1 << anchors.len()) {
            let candidate: Vec<_> = options
                .iter()
                .enumerate()
                .map(|(i, berths)| berths[(mask >> i) & 1])
                .collect();
            let legs = routes.map(|(_, indices, _)| {
                let a: Vec<_> = indices.iter().map(|&i| candidate[i]).collect();
                let b = std::iter::once(a[0])
                    .chain(a[1..].iter().rev().copied())
                    .collect();
                [a, b]
                    .into_iter()
                    .filter_map(|stops| Some((loop_distance(&map, &stops, tick)?, stops)))
                    .min_by(|a, b| a.0.total_cmp(&b.0))
            });
            let [Some((a, first)), Some((b, second))] = legs else {
                continue;
            };
            if best.as_ref().is_none_or(|(distance, _)| a + b < *distance) {
                best = Some((a + b, [first, second]));
            }
        }
        best.ok_or("no connected directed berth loops through the civic stop")?
            .1
    };

    let mut lines = BTreeMap::new();
    let mut stops_by_lane = BTreeMap::new();
    let mut far_stop = None;
    for ((key, _, vehicles), berths) in routes.into_iter().zip(berths) {
        let mut stops = Vec::new();
        let mut lanes = Vec::new();
        for berth in berths {
            let stop = if let Some(&stop) = stops_by_lane.get(&berth.lane) {
                stop
            } else {
                let before: BTreeSet<_> = sim.read::<Transit>().stops.keys().collect();
                apply(
                    sim,
                    key,
                    &[WorldCommand::TransitAddStop {
                        pos: berth.pos,
                        dir: berth.dir,
                    }],
                )?;
                let stop = sim
                    .read::<Transit>()
                    .stops
                    .keys()
                    .find(|id| !before.contains(id))
                    .ok_or("missing new bus stop")?;
                stops_by_lane.insert(berth.lane, stop);
                stop
            };
            stops.push(stop);
            lanes.push(berth.lane);
            if Some(sim.map().lanes()[berth.lane].parent) == parkway {
                far_stop = Some(stop);
            }
        }
        let before: BTreeSet<_> = sim.read::<Transit>().lines.keys().collect();
        apply(
            sim,
            key,
            &[WorldCommand::TransitCreateLine {
                stops: stops.clone(),
            }],
        )?;
        let id = sim
            .read::<Transit>()
            .lines
            .keys()
            .find(|id| !before.contains(id))
            .ok_or("missing bus line")?;
        apply(
            sim,
            key,
            &[
                WorldCommand::TransitSetVehicles {
                    line: id,
                    n: vehicles,
                },
                WorldCommand::TransitSetFare {
                    line: id,
                    fare: Money::ZERO,
                },
            ],
        )?;
        lines.insert(
            key,
            BusLine {
                id,
                stops,
                lanes,
                vehicles,
            },
        );
    }
    Ok((lines, far_stop))
}

fn population_change_ok(earlier: u64, later: u64) -> bool {
    earlier > 0 && u128::from(earlier.abs_diff(later)) * 10 <= u128::from(earlier)
}

fn stable_population(opening: u64, housed: u64, employed: u64, earlier: u64) -> bool {
    opening >= MIN_OPENING_POPULATION
        && housed > 0
        && employed <= housed
        && u128::from(employed) * 100 >= u128::from(housed) * MIN_EMPLOYED_PERCENT
        && population_change_ok(earlier, housed)
}

fn needs_met(
    due: u64,
    filled: u64,
    stockout: u64,
    resident_ticks: u64,
    work: u64,
    park: u64,
) -> bool {
    due >= 100
        && filled <= due
        && u128::from(filled) * 100 >= u128::from(due) * MIN_BASKETS_FILLED_PERCENT
        && resident_ticks > 0
        && u128::from(stockout) * 24 <= u128::from(resident_ticks)
        && work > 0
        && park > 0
}

fn service_covered(served: u64, residents: u64) -> bool {
    residents > 0 && served <= residents && u128::from(served) * 10 >= u128::from(residents) * 9
}

fn spare_capacity(supply: f64, demand: f64) -> bool {
    supply.is_finite() && demand.is_finite() && demand > 0.0 && supply >= demand * 1.25
}

fn line_operating(line: &str, boardings: u64, loops: u64, completed: u64) -> bool {
    let boarding_floor = if line == "employment" { 5 } else { 10 };
    boardings >= boarding_floor && loops > 0 && completed > 0
}

fn traffic_clear(car_arrivals: u64, standing: u64, blocking: u64, stranded: u64) -> bool {
    car_arrivals > 0
        && standing <= MAX_STANDING_MINUTES * TICKS_PER_MINUTE
        && blocking < 20 * TICKS_PER_MINUTE
        && stranded == 0
}

fn recurring_balance(day: &BTreeMap<BudgetCategory, i128>) -> i128 {
    day.iter()
        .filter_map(|(category, amount)| category.is_operating().then_some(amount))
        .sum()
}

fn closed_window(load: u64, finish: u64) -> std::ops::Range<u64> {
    load.div_ceil(TICKS_PER_BUDGET_DAY)..finish / TICKS_PER_BUDGET_DAY
}

#[derive(Default, Serialize)]
struct DistrictState {
    residents: u64,
    employed_residents: u64,
    jobs: u64,
    occupied_homes: BTreeMap<String, u64>,
    stocked_shops: u64,
    developed_blocks: BTreeMap<usize, u64>,
    operating_blocks: BTreeMap<usize, u64>,
    coverage: BTreeMap<String, u64>,
}

impl DistrictState {
    fn opening_uses_met(&self, district: &str) -> bool {
        self.stocked_shops > 0
            && if district == "employment" {
                self.jobs > 0
            } else {
                self.residents > 0
                    && self.operating_blocks.values().filter(|&&active| active >= 2).count() >= 2
            }
    }
}

#[derive(Serialize)]
struct TownState {
    residents: u64,
    physical_agents: u64,
    employed_residents: u64,
    districts: BTreeMap<&'static str, DistrictState>,
    provider_capacity: BTreeMap<String, Vec<(BuildingID, f32, f32)>>,
    utilities: Vec<NetworkState>,
    unserved_buildings: Vec<BuildingID>,
    unfuelled_plant: bool,
}

#[derive(Serialize)]
struct NetworkState {
    network: String,
    power_produced_w: i64,
    power_demand_w: i64,
    water_supply: f64,
    water_demand: f64,
    sewage_capacity: f64,
    sewage_demand: f64,
    adequate: bool,
}

fn has_retail_stock(market: &Market, soul: SoulID) -> bool {
    ["groceries", "retail-goods"]
        .into_iter()
        .filter_map(|item| market.inner().get(&ItemID::new(item)))
        .any(|shelf| {
            shelf.sell_order(soul).is_some_and(|order| order.qty > 0)
                && shelf.capital(soul).is_some_and(|stock| stock > 0)
        })
}

fn snapshot(sim: &Simulation, plan: &TownPlan, manifest: &OpeningManifest) -> TownState {
    let map = sim.map();
    let world = sim.world();
    let infos = sim.read::<BuildingInfos>();
    let market = sim.read::<Market>();
    let mut districts = BTreeMap::<_, DistrictState>::new();
    let mut occupants = BTreeMap::<_, (u64, u64)>::new();
    let mut employed_residents = 0;
    for (id, human) in world.humans.iter() {
        let house = human.home.house;
        if map.get(house).is_none_or(|site| site.is_orphaned()) {
            continue;
        }
        let employed = human.work.as_ref().is_some_and(|work| {
            let Some(SoulID::GoodsCompany(id_company)) = infos.owner(work.workplace) else {
                return false;
            };
            world
                .companies
                .get(id_company)
                .is_some_and(|company| company.workers.0.contains(&id))
                && map
                    .get(work.workplace)
                    .is_some_and(|site| !site.is_orphaned())
        });
        let (population, working) = occupants.entry(house).or_default();
        *population += 1;
        *working += u64::from(employed);
        employed_residents += u64::from(employed);
    }
    let covered = sim.read::<CoverageGrid>();
    let electricity = sim.read::<ElectricityFlow>();
    let utility = sim.read::<UtilityFlow>();
    let residences = sim.read::<Residences>();
    let totals: BTreeMap<_, _> = utility.networks().collect();
    let mut networks = BTreeSet::new();
    let mut unserved_buildings = Vec::new();
    let mut fuelled_plant = false;
    let parcel_at: BTreeMap<_, _> = manifest
        .parcels
        .iter()
        .filter_map(|parcel| Some((parcel.building?, parcel)))
        .collect();
    for (id, building) in map.buildings().iter() {
        let workers = match infos.owner(id) {
            Some(SoulID::GoodsCompany(company)) => world
                .companies
                .get(company)
                .map_or(0, |company| company.workers.0.len() as u64),
            _ => 0,
        };
        let (people, employed) = occupants.get(&id).copied().unwrap_or_default();
        let staffed = workers > 0 && !building.is_orphaned();
        if people > 0
            || staffed
            || manifest.sites.values().any(|&site| site == id)
            || id == manifest.freight.terminal
        {
            let power = map.electricity.net_id(id).is_some_and(|net| {
                networks.insert(net);
                let flow = electricity.network_stats(net);
                flow.produced_power.0 > 0 && !flow.blackout
            });
            let water = utility.status(&map, &residences, id);
            let water_served = match water {
                Some(status) => {
                    status.water == UtilityStatus::Served && status.sewage == UtilityStatus::Served
                }
                None => people == 0,
            };
            if !power || !water_served {
                unserved_buildings.push(id);
            }
        }
        if building
            .kind
            .as_goods_company()
            .is_some_and(|proto| proto == prototypes::GoodsCompanyID::new("coal-power-plant"))
        {
            fuelled_plant |= staffed
                && infos
                    .owner(id)
                    .is_some_and(|soul| market.capital(soul, ItemID::new("coal")) > 0);
        }
        let parcel = parcel_at.get(&id).copied();
        let district = parcel.map(|p| p.district).or_else(|| {
            plan.districts
                .iter()
                .find(|district| contains(&district.polygon, building.obb.center()))
                .map(|d| d.key)
        });
        let Some(district) = district else { continue };
        let state = districts.entry(district).or_default();
        state.residents += people;
        state.employed_residents += employed;
        state.jobs += workers;
        let stocked = staffed
            && is_retail_store(building.kind)
            && infos.owner(id).is_some_and(|soul| has_retail_stock(&market, soul));
        state.stocked_shops += u64::from(stocked);
        if let Some(parcel) = parcel {
            if people > 0 {
                *state
                    .occupied_homes
                    .entry(format!("{:?}", parcel.zoning.density))
                    .or_default() += 1;
            }
            if let Some(block) = parcel.block {
                *state.developed_blocks.entry(block).or_default() += 1;
                let active = if parcel.zoning.kind == ZoneKind::Commercial {
                    stocked
                } else {
                    people > 0 || staffed
                };
                if active {
                    *state.operating_blocks.entry(block).or_default() += 1;
                }
            }
        }
        for service in SERVICES {
            if people > 0 && covered.coverage(id, service) >= 0.5 {
                *state.coverage.entry(format!("{service:?}")).or_default() += people;
            }
        }
    }
    let utilities = networks
        .into_iter()
        .map(|network| {
            let power = electricity.network_stats(network);
            let water = totals.get(&network).copied().unwrap_or_default();
            NetworkState {
                network: format!("{network:?}"),
                power_produced_w: power.produced_power.0,
                power_demand_w: power.consumed_power.0,
                water_supply: water.water_production,
                water_demand: water.water_demand,
                sewage_capacity: water.sewage_capacity,
                sewage_demand: water.sewage_demand,
                adequate: !power.blackout
                    && spare_capacity(power.produced_power.0 as f64, power.consumed_power.0 as f64)
                    && spare_capacity(water.water_production, water.water_demand)
                    && spare_capacity(water.sewage_capacity, water.sewage_demand),
            }
        })
        .collect();
    let provider_capacity = SERVICES
        .into_iter()
        .map(|kind| {
            let providers = service_reach_sources(sim, kind)
                .into_iter()
                .map(|p| (p.building, p.effective_capacity, p.served_distance_m))
                .collect();
            (format!("{kind:?}"), providers)
        })
        .collect();
    TownState {
        residents: occupants.values().map(|&(people, _)| people).sum(),
        physical_agents: world.humans.len() as u64,
        employed_residents,
        districts,
        provider_capacity,
        utilities,
        unserved_buildings,
        unfuelled_plant: !fuelled_plant,
    }
}

fn coverage_failures(state: &TownState) -> Vec<String> {
    let mut failures = Vec::new();
    let seats: f32 = state.provider_capacity.get("School").into_iter().flatten()
        .map(|&(_, capacity, _)| capacity)
        .sum();
    if seats * 10.0 < state.residents as f32 * 11.0 {
        failures.push(format!("staffed school seats {seats:.0} are below 110% of {} residents", state.residents));
    }
    for service in SERVICES {
        let name = format!("{service:?}");
        let served: u64 = state
            .districts
            .values()
            .filter_map(|d| d.coverage.get(&name))
            .sum();
        if !service_covered(served, state.residents) {
            failures.push(format!(
                "{name} coverage {served}/{} townwide",
                state.residents
            ));
        }
        for (district, data) in &state.districts {
            let district_served = data.coverage.get(&name).copied().unwrap_or(0);
            if data.residents > 0 && !service_covered(district_served, data.residents) {
                failures.push(format!(
                    "{district}: {name} covers {district_served}/{}",
                    data.residents
                ));
            }
        }
    }
    failures
}

fn verify_opening_ids(sim: &Simulation, manifest: &OpeningManifest) -> Result<u32, String> {
    let map = sim.map();
    let zones = sim.read::<ZoneGrid>();
    let transit = sim.read::<Transit>();
    if sim.get_tick() != manifest.opening_tick {
        return Err("loaded opening tick differs from commissioning handoff".into());
    }
    for (key, &site) in &manifest.sites {
        let building = map
            .get(site)
            .ok_or_else(|| format!("missing opening site {key}"))?;
        if building.is_orphaned() {
            return Err(format!("opening site {key} lost its road"));
        }
        if let Some(Err(error)) = water_provider_site(&map, site) {
            return Err(format!("opening water site {key}: {error}"));
        }
    }
    for parcel in &manifest.parcels {
        verify_parcel_frontage(&map, &zones, parcel)?;
    }
    let mut capacity = 0;
    for parcel in &manifest.reserves {
        let lot = map.get(parcel.lot).ok_or("infill reserve was consumed")?;
        if zones.lot_zone(&lot.shape).is_some()
            || check_growable_lot(&map, lot, parcel.zoning).is_err()
            || lot.parent != parcel.road
            || !same_footprint(&lot.shape, &parcel.footprint)
        {
            return Err(format!("invalid unpainted infill reserve {:?}", parcel.lot));
        }
        capacity += parcel.capacity;
    }
    for (key, line) in &manifest.lines {
        if !transit.lines.get(line.id).is_some_and(|actual| {
            actual.stops == line.stops
                && !actual.suspended
                && actual.target_vehicles == line.vehicles
        }) || transit.directed_leg_times(line.id).is_none()
        {
            return Err(format!("line {key} suspended or missing after reload"));
        }
        for (&stop, &lane) in line.stops.iter().zip(&line.lanes) {
            let actual = transit
                .stops
                .get(stop)
                .ok_or("missing bus stop after reload")?;
            let berth = map.get(lane).ok_or("missing bus berth lane after reload")?;
            if berth.points.project(actual.berth).distance2(actual.berth) > 0.01 {
                return Err(format!("stop {stop:?} changed directed berth"));
            }
        }
    }
    let freight = &manifest.freight;
    if sim.world().freight_stations.len() != 1 || !sim
        .world()
        .freight_stations
        .get(freight.station)
        .is_some_and(|station| station.f.building == freight.terminal)
        || sim.world().trains.len() != 1
        || !sim.world().trains.contains_key(freight.train)
        || !map
            .get(freight.market)
            .is_some_and(|site| site.kind == BuildingKind::ExternalTrading)
        || !map
            .get(freight.market_lane)
            .is_some_and(|lane| lane.kind == LaneKind::Rail)
    {
        return Err("freight IDs did not survive opening reload".into());
    }
    verify_parking_pair(&map, freight.terminal, freight.bakery, Tick(sim.get_tick()))?;
    Ok(capacity)
}

fn verify_infill_spacing(map: &Map, floor: f32) -> Result<(), String> {
    let junctions = map.intersections().values().map(|junction| (junction.id, junction.pos.xy()));
    if let Some((gap, a, b)) = close_pairs(junctions, floor).first() {
        return Err(format!("infill junctions {a:?}/{b:?} are {gap:.3} m apart, require {floor:.1} m"));
    }
    Ok(())
}

// Exercise an ordinary network and zoning edit after the untouched soak, on
// the simulation that will be discarded by the publisher.
fn verify_infill(
    sim: &mut Simulation,
    plan: &TownPlan,
    manifest: &OpeningManifest,
) -> Result<(), String> {
    let startup_state = snapshot(sim, plan, manifest);
    let mut opening_parcels = manifest.parcels.clone();
    record_development(&sim.map(), &mut opening_parcels);
    verify_votes(sim, opening_parcels.iter())?;
    let road = {
        let map = sim.map();
        let mut painted = sim.read::<ZoneGrid>().clone();
        for parcel in &manifest.reserves {
            painted.paint_on_map(
                &map,
                &parcel.footprint,
                Some(parcel.zoning.kind),
                parcel.zoning.density,
            )
            .map_err(|error| format!("infill paint on {:?}: {error:?}", parcel.lot))?;
        }
        let planned_votes = opening_parcels
            .iter()
            .chain(&manifest.reserves)
            .filter(|parcel| parcel.building.is_none())
            .map(|parcel| (parcel.lot, parcel.zoning))
            .collect();
        let mut junctions = BTreeMap::new();
        for parcel in &manifest.reserves {
            let road = &map.roads()[parcel.road];
            for (id, other) in [(road.src, road.dst), (road.dst, road.src)] {
                junctions.entry(id).or_insert((parcel, other));
            }
        }
        let pattern = LanePatternBuilder::new().build();
        junctions.into_iter().find_map(|(id, (parcel, other))| {
            let junction = &map.intersections()[id];
            let pos = junction.pos;
            let along = (pos - map.intersections()[other].pos).xy().try_normalize()?;
            [along, -along.perpendicular(), along.perpendicular()]
                .into_iter()
                .find_map(|direction| {
                    let clearance = (junction.radius + 8.0).max(20.0);
                    let length = 80.0;
                    if clearance >= length {
                        return None;
                    }
                    let center = pos.xy() + direction * ((length + clearance) * 0.5);
                    let corridor = OBB::new(center, direction, length - clearance, pattern.width());
                    if !site_clear(&map, plan, corridor, None)
                        || manifest.reserves.iter().chain(&manifest.parcels)
                            .any(|parcel| parcel.footprint.intersects(&corridor))
                    {
                        return None;
                    }
                    let from = MapProject { pos, kind: ProjectKind::Intersection(id) };
                    let to = MapProject::ground(pos + (direction * length).z(0.0));
                    // Extending a junction can create frontage outside the road corridor.
                    // Test the resulting lot votes before committing to this extension.
                    let mut extended = (*map).clone();
                    extended.make_connection(from, to, None, &pattern)?;
                    verify_infill_spacing(&extended, plan.limits.junction).ok()?;
                    for parcel in opening_parcels.iter().chain(&manifest.reserves) {
                        verify_parcel_frontage(&extended, &painted, parcel).ok()?;
                    }
                    for parcel in &manifest.reserves {
                        check_growable_lot(&extended, extended.get(parcel.lot)?, parcel.zoning).ok()?;
                    }
                    audit_votes(
                        extended.lots().values().map(|lot| (lot.id, painted.lot_zone(&lot.shape))),
                        &planned_votes,
                    ).ok()?;
                    Some((parcel.district, WorldCommand::MapMakeConnection {
                        from, to, inter: None, pat: pattern.clone(),
                    }))
                })
        })
    };
    let (district, road) = road.ok_or("no adjacent extension preserving opening parcels, reserves and junction spacing")?;
    apply(sim, &format!("{district} infill road"), &[road])?;
    verify_votes(sim, opening_parcels.iter())?;
    {
        let map = sim.map();
        verify_infill_spacing(&map, plan.limits.junction)?;
        let transit = sim.read::<Transit>();
        for (&name, line) in &manifest.lines {
            let berths: Vec<_> = line.stops.iter().zip(&line.lanes)
                .map(|(&stop, &lane)| {
                    let stop = transit.stops.get(stop)?;
                    let points = &map.get(lane)?.points;
                    let projected = points.project(stop.berth);
                    (projected.distance2(stop.berth) < 0.01).then_some(CurbBerth {
                        lane,
                        offset: points.length_at_proj(projected),
                        pos: stop.berth,
                        dir: stop.dir,
                    })
                })
                .collect::<Option<_>>()
                .ok_or_else(|| format!("{name} bus lost its berth after the infill road edit"))?;
            if loop_distance(&map, &berths, Tick(sim.get_tick())).is_none() {
                return Err(format!("{name} bus lost its directed loop after the infill road edit"));
            }
        }
    }
    {
        let map = sim.map();
        let electricity = sim.read::<ElectricityFlow>();
        let utility = sim.read::<UtilityFlow>();
        let totals: BTreeMap<_, _> = utility.networks().collect();
        let mut added = BTreeMap::<_, (i64, f64)>::new();
        for parcel in &manifest.reserves {
            let lot = map
                .get(parcel.lot)
                .ok_or("reserved lot missing after soak")?;
            let network = map
                .electricity
                .net_id(lot.parent)
                .ok_or("reserved lot has no utility network")?;
            let site = check_growable_lot(&map, lot, parcel.zoning)
                .map_err(|error| format!("infill {:?}: {error:?}", parcel.lot))?;
            let (power, water) = if let Some(home) = site.home {
                (
                    home.power_consumption.map_or(100, |power| power.0),
                    home.water_consumption,
                )
            } else {
                let company = site.company.ok_or("missing infill prototype")?;
                let company = &company.base;
                (
                    company.power_consumption.map_or(0, |power| power.0),
                    company.water_consumption,
                )
            };
            let (watts, flow) = added.entry(network).or_default();
            *watts += power;
            *flow += water.map_or(0.0, f64::from);
        }
        for (network, (watts, flow)) in added {
            let power = electricity.network_stats(network);
            let water = totals.get(&network).ok_or("infill utility flow missing")?;
            if !spare_capacity(
                power.produced_power.0 as f64,
                (power.consumed_power.0 + watts) as f64,
            ) || !spare_capacity(water.water_production, water.water_demand + flow)
                || !spare_capacity(water.sewage_capacity, water.sewage_demand + flow)
            {
                return Err(format!("network {network:?} lacks the required spare capacity with the reserved infill"));
            }
        }
    }

    paint(sim, manifest.reserves.iter())?;
    verify_votes(sim, opening_parcels.iter().chain(&manifest.reserves))?;
    advance(sim, &mut Simulation::schedule(), TICKS_PER_BUDGET_DAY);

    let mut developed = manifest.reserves.clone();
    let mut occupied_districts = BTreeSet::new();
    let mut workplaces = 0;
    let mut residents = 0;
    {
        let map = sim.map();
        let infos = sim.read::<BuildingInfos>();
        record_development(&map, &mut developed);
        let occupied_homes: BTreeMap<_, usize> = sim.world().humans.values().fold(
            BTreeMap::new(),
            |mut homes, human| {
                *homes.entry(human.home.house).or_default() += 1;
                homes
            },
        );
        for parcel in &developed {
            let Some(id) = parcel.building else { continue };
            if map.get(id).and_then(|building| building.connected_road) != Some(parcel.road) {
                return Err(format!("infill building {id:?} lost its frontage"));
            }
            if let Some(&people) = occupied_homes.get(&id) {
                residents += people;
                occupied_districts.insert(parcel.district);
            }
            if let Some(SoulID::GoodsCompany(company)) = infos.owner(id) {
                workplaces += usize::from(sim.world().companies.get(company)
                    .is_some_and(|company| !company.workers.0.is_empty()));
            }
        }
    }
    let state = snapshot(sim, plan, manifest);
    let mut failures = coverage_failures(&state);
    if occupied_districts.len() < 3 || workplaces == 0 {
        failures.push(format!(
            "infill settled {residents} residents in {occupied_districts:?} and {workplaces} staffed workplaces"
        ));
    }
    if state.utilities.iter().any(|net| !net.adequate) || !state.unserved_buildings.is_empty() {
        failures.push("settled infill lacked utility coverage or spare capacity".into());
    }
    eprintln!(
        "CITYMAP infill: road extended beside {district}, {residents} reserve residents, {workplaces} reserve workplaces, town population {} → {}",
        startup_state.residents, state.residents,
    );
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    Ok(())
}

#[derive(Default, Serialize)]
struct Traffic {
    max_standing_ticks: u64,
    longest_standing: Option<(VehicleID, VehicleKind, [f32; 3], Option<TraverseKind>)>,
    longest_standing_elapsed_window: Option<[u64; 2]>,
    longest_standing_decision: Option<(VehicleID, u64, StandingDecision)>,
    max_blocking_ticks: u64,
    longest_blocked_egress: Option<LaneID>,
    stranded_drivers: u64,
    panicking: u64,
    car_arrivals: u64,
    directed_lanes_entered: usize,
    #[serde(skip)]
    observed: BTreeMap<VehicleID, VehicleProgress>,
    #[serde(skip)]
    blocked_egresses: BTreeMap<LaneID, BlockedEgress>,
    #[serde(skip)]
    directional_entries: BTreeMap<LaneID, u64>,
    #[serde(skip)]
    egress_probes: Vec<EgressProbe>,
    #[serde(skip)]
    recent_crossings: BTreeSet<LaneID>,
}

struct EgressProbe {
    vehicle: VehicleID,
    turn: TurnID,
    previous: TraverseKind,
    entrance: Vec3,
    radius_squared: f32,
}

struct VehicleProgress {
    position: Vec3,
    moved_at: u64,
    standing_ticks: u64,
    path: Option<TraverseKind>,
    sample: u64,
    panicking: bool,
    incoming_turn: Option<TurnID>,
}

struct BlockedEgress {
    start: u64,
    permissive_seen: bool,
}

impl Traffic {
    fn observe_egresses(
        &mut self,
        tick: u64,
        conflicts: BTreeMap<LaneID, bool>,
        crossed: &BTreeSet<LaneID>,
    ) {
        self.blocked_egresses
            .retain(|lane, _| conflicts.contains_key(lane) && !crossed.contains(lane));
        for (lane, permissive) in conflicts {
            let episode = self.blocked_egresses.entry(lane).or_insert(BlockedEgress {
                start: tick,
                permissive_seen: false,
            });
            episode.permissive_seen |= permissive;
            let duration = tick - episode.start;
            if episode.permissive_seen && duration > self.max_blocking_ticks {
                self.max_blocking_ticks = duration;
                self.longest_blocked_egress = Some(lane);
            }
        }
    }
}

#[derive(Serialize)]
struct StandingDecision {
    state: String,
    speed: f32,
    objective: Option<[f32; 3]>,
    speed_with_neighbours: Option<f32>,
    speed_without_neighbours: Option<f32>,
    blocking_colliders: Vec<(String, [f32; 2])>,
}

impl StandingDecision {
    fn sample(sim: &Simulation, id: VehicleID, car: &VehicleEnt) -> Self {
        let mut decision = Self {
            state: format!("{:?}", car.vehicle.state),
            speed: car.speed.0,
            objective: car.it.get_point().map(Into::into),
            speed_with_neighbours: None,
            speed_without_neighbours: None,
            blocking_colliders: Vec::new(),
        };
        let grid = sim.read::<TransportGrid>();
        let Some((_, own)) = car.collider.and_then(|collider| grid.get(collider.0)) else {
            return decision;
        };
        if !matches!(car.vehicle.state, VehicleState::Driving) {
            return decision;
        }
        let map = sim.map();
        let time = sim.read::<GameTime>();
        let query_radius = 12.0
            + (own.speed.powi(2) / (2.0 * car.vehicle.kind.deceleration())).min(100.0);
        let neighbours: Vec<_> = grid
            .query_around(car.trans.pos.xy(), query_radius)
            .filter_map(|(handle, pos)| Some((pos, grid.get(handle)?.1)))
            .collect();
        let evaluate = |nearby: &[_]| {
            // Evaluate a detached decision at the same tick without its
            // short retry delay. The live world and itinerary are read-only.
            let mut vehicle = Vehicle {
                ang_velocity: car.vehicle.ang_velocity,
                wait_time: 0.0,
                max_speed_multiplier: car.vehicle.max_speed_multiplier,
                state: VehicleState::Driving,
                kind: car.vehicle.kind,
                tint: car.vehicle.tint,
                flag: car.vehicle.flag,
            };
            calc_decision(id, &mut vehicle, &map, &time, &car.trans, own, &car.it, nearby.iter().copied()).0
        };
        decision.speed_with_neighbours = Some(evaluate(&neighbours));
        let isolated = evaluate(&[]);
        decision.speed_without_neighbours = Some(isolated);
        if isolated > 0.0 {
            decision.blocking_colliders = neighbours
                .iter()
                .filter(|neighbour| evaluate(std::slice::from_ref(*neighbour)) == 0.0)
                .map(|(pos, collider)| (format!("{:?}", collider.group), (*pos).into()))
                .collect();
        }
        decision
    }
}

impl Traffic {
    // Between samples, inspect only vehicles whose current or next turn
    // leads into an egress that the occupancy scan found blocked.
    fn probe_egresses(&mut self, sim: &Simulation) {
        self.egress_probes.retain_mut(|probe| {
            let Some(car) = sim.world().vehicles.get(probe.vehicle) else {
                return false;
            };
            let Some(path) = car.it.get_travers().map(|path| path.kind) else {
                return false;
            };
            let destination = TraverseKind::Lane(probe.turn.dst);
            let was_entering = match probe.previous {
                TraverseKind::Turn(turn) => turn == probe.turn,
                TraverseKind::Lane(lane) => lane == probe.turn.src || lane == probe.turn.dst,
            };
            if was_entering && path == destination
                && !car.vehicle.state.is_parked()
                && probe.entrance.distance2(car.trans.pos) > probe.radius_squared
            {
                self.recent_crossings.insert(probe.turn.dst);
                return false;
            }
            probe.previous = path;
            true
        });
    }

    fn observe(&mut self, sim: &Simulation, tick: u64) {
        let transit = sim.read::<Transit>();
        let world = sim.world();
        let map = sim.map();
        let seconds = sim.read::<GameTime>().seconds;
        let mut upstream = Vec::new();
        let mut stopped_egresses = BTreeMap::new();
        let mut crossed = std::mem::take(&mut self.recent_crossings);
        let mut turn_candidates = Vec::new();
        for (id, car) in world.vehicles.iter() {
            if car.vehicle.state.is_parked()
                || transit
                    .runs
                    .get(&id)
                    .is_some_and(|bus| bus.dwell_until.is_some())
            {
                self.observed.remove(&id);
                continue;
            }
            let path = car.it.get_travers().map(|path| path.kind).or_else(|| {
                let lane = map.nearest_lane(car.trans.pos, LaneKind::Driving, Some(3.0))?;
                Some(TraverseKind::Lane(lane))
            });
            if path.is_none() {
                self.observed.remove(&id);
                continue;
            }
            let panicking = matches!(car.vehicle.state, VehicleState::Panicking(_));
            let progress = self.observed.entry(id).or_insert(VehicleProgress {
                position: car.trans.pos,
                moved_at: tick,
                standing_ticks: 0,
                path: None,
                sample: tick,
                panicking: false,
                incoming_turn: None,
            });
            if panicking && !progress.panicking {
                self.panicking += 1;
            }
            progress.panicking = panicking;
            let sample_ticks = tick - progress.sample;
            progress.sample = tick;
            let waiting_at_signal = path.is_some_and(|path| {
                let TraverseKind::Lane(id) = path else { return false };
                map.get(id).is_some_and(|lane| {
                    matches!(lane.control.get_behavior(seconds), TrafficBehavior::RED | TrafficBehavior::ORANGE)
                        && lane.points.last().distance2(car.trans.pos) <= 8.0_f32.powi(2)
                })
            });
            if progress.position.distance2(car.trans.pos) > 1.0 {
                progress.moved_at = tick;
                progress.standing_ticks = 0;
                progress.position = car.trans.pos;
                if let Some(TraverseKind::Lane(lane)) =
                    path.filter(|path| Some(*path) != progress.path)
                {
                    *self.directional_entries.entry(lane).or_default() += 1;
                }
            } else if !waiting_at_signal {
                if progress.standing_ticks == 0 {
                    progress.moved_at = tick - sample_ticks;
                }
                progress.standing_ticks += sample_ticks;
            }
            if let Some(TraverseKind::Turn(turn)) = path {
                progress.incoming_turn = Some(turn);
            }
            let radius = car.vehicle.kind.width() * 0.5;
            if let Some(TraverseKind::Lane(lane)) = path {
                if let Some(turn) = progress.incoming_turn {
                    if lane != turn.dst {
                        progress.incoming_turn = None;
                    } else if map.lanes()[lane].points.first().distance2(car.trans.pos) > radius.powi(2) {
                        crossed.insert(lane);
                        progress.incoming_turn = None;
                    }
                }
            }
            if let Some(current) = car.it.get_travers().map(|path| path.kind) {
                let next_turn = progress.incoming_turn.or_else(|| {
                    car.it.get_route()?.reversed_route.last().and_then(|step| match step.kind {
                        TraverseKind::Turn(turn) => Some(turn),
                        TraverseKind::Lane(_) => None,
                    })
                });
                if let Some(turn) = next_turn {
                    turn_candidates.push((id, turn, current, radius));
                }
            }
            progress.path = path;
            let duration = progress.standing_ticks;
            if duration > self.max_standing_ticks {
                self.max_standing_ticks = duration;
                self.longest_standing = Some((id, car.vehicle.kind, car.trans.pos.into(), path));
                self.longest_standing_elapsed_window = Some([progress.moved_at, tick]);
                if duration >= 30 * TICKS_PER_MINUTE
                    && self.longest_standing_decision.as_ref().is_none_or(|(vehicle, start, _)| {
                        *vehicle != id || *start != progress.moved_at
                    })
                {
                    self.longest_standing_decision = Some((
                        id, progress.moved_at, StandingDecision::sample(sim, id, car),
                    ));
                }
            }
            if car.speed.0.abs() <= 0.5 {
                match path {
                    Some(TraverseKind::Turn(turn)) => upstream.push((turn, id)),
                    Some(TraverseKind::Lane(lane)) => {
                        let points = &map.lanes()[lane].points;
                        if points.first().distance2(car.trans.pos) < (24.0 + radius).powi(2) {
                            if let Some(turn) = progress.incoming_turn {
                                upstream.push((turn, id));
                            } else {
                                stopped_egresses.entry(lane).or_insert(id);
                            }
                        }
                        if points.last().distance2(car.trans.pos) < (5.0 + radius).powi(2) {
                            if let Some(TraverseKind::Turn(turn)) = car.it.get_route()
                                .and_then(|route| route.reversed_route.last())
                                .map(|traverse| traverse.kind)
                            {
                                upstream.push((turn, id));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.observed.retain(|_, progress| progress.sample == tick);
        let mut conflicts = BTreeMap::new();
        for (turn, id) in upstream {
            if stopped_egresses.get(&turn.dst).is_some_and(|&blocking| blocking != id) {
                let permissive = matches!(
                    map.lanes()[turn.src].control.get_behavior(seconds),
                    TrafficBehavior::GREEN | TrafficBehavior::STOP
                );
                *conflicts.entry(turn.dst).or_default() |= permissive;
            }
        }
        self.egress_probes.clear();
        self.egress_probes.extend(turn_candidates.into_iter().filter_map(
            |(vehicle, turn, previous, radius)| {
                if !conflicts.contains_key(&turn.dst) {
                    return None;
                }
                Some(EgressProbe {
                    vehicle,
                    turn,
                    previous,
                    entrance: map.get(turn.dst)?.points.first(),
                    radius_squared: radius.powi(2),
                })
            },
        ));
        self.observe_egresses(tick, conflicts, &crossed);
        self.directed_lanes_entered = self.directional_entries.len();
        let stranded = world
            .humans
            .values()
            .filter(|human| {
                let Location::Vehicle(car) = human.location else {
                    return false;
                };
                matches!(human.router.cur_step(), Some(RoutingStep::DriveTo(_, _)))
                    && world
                        .vehicles
                        .get(car)
                        .is_some_and(|car| car.vehicle.state.is_parked())
            })
            .count() as u64;
        self.stranded_drivers = self.stranded_drivers.max(stranded);
    }
}

#[derive(Default)]
struct LoopEvidence {
    runs: BTreeMap<VehicleID, (usize, Option<Tick>, usize)>,
    loops: BTreeMap<LineID, u64>,
}

impl LoopEvidence {
    fn observe(&mut self, transit: &Transit) {
        self.runs.retain(|id, _| transit.runs.contains_key(id));
        for (&id, run) in &transit.runs {
            let Some(line) = transit.lines.get(run.line).filter(|line| !line.suspended) else {
                continue;
            };
            let state = self.runs.entry(id).or_default();
            track_arrival(
                state,
                run.next_stop_idx,
                run.dwell_until,
                run.failures > 0,
                line.stops.len(),
                self.loops.entry(run.line).or_default(),
            );
        }
    }
}

fn track_arrival(
    state: &mut (usize, Option<Tick>, usize),
    stop: usize,
    dwell: Option<Tick>,
    failed: bool,
    len: usize,
    loops: &mut u64,
) {
    if failed {
        state.2 = 0;
    }
    if dwell.is_none() || state.1 == dwell {
        return;
    }
    let consecutive = state.2 > 0 && (state.0 + 1) % len == stop;
    let count = if consecutive { state.2 + 1 } else { 1 };
    *loops += u64::from(count > len);
    *state = (stop, dwell, if count > len { 1 } else { count });
}

#[derive(Default)]
struct PassengerEvidence {
    total_boardings: u64,
    active: BTreeMap<HumanID, PassengerJourney>,
    aboard_at_start: BTreeSet<HumanID>,
    completed: BTreeMap<LineID, u64>,
    examples: BTreeMap<LineID, PassengerArrival>,
    cross_bank_arrival: Option<PassengerArrival>,
}

struct PassengerJourney {
    bus: VehicleID,
    line: LineID,
    board: StopID,
    alight: StopID,
    destination: BuildingID,
    purpose: TripPurpose,
    started: Tick,
    alighted: bool,
}

#[derive(Clone, Copy, Serialize)]
struct PassengerArrival {
    passenger: HumanID,
    board: StopID,
    alight: StopID,
    destination: BuildingID,
    purpose: TripPurpose,
    elapsed_tick: u64,
}

impl PassengerEvidence {
    fn new(sim: &Simulation) -> Self {
        Self {
            total_boardings: sim.read::<TransitStats>().boardings.values().sum(),
            aboard_at_start: sim.world().humans.iter()
                .filter_map(|(id, human)| matches!(human.location, Location::Transit(_)).then_some(id))
                .collect(),
            ..Self::default()
        }
    }

    fn observe(
        &mut self,
        sim: &Simulation,
        transit: &Transit,
        tick: u64,
        controlled: Option<(&CrossBank, LineID, &BTreeSet<HumanID>)>,
    ) {
        let world = sim.world();
        self.aboard_at_start.retain(|&id| world.humans.get(id)
            .is_some_and(|human| matches!(human.location, Location::Transit(_))));
        self.active.retain(|&id, journey| {
            let Some(human) = world.humans.get(id) else {
                return false;
            };
            let router = &human.router;
            if router.target_dest != Some(Destination::Building(journey.destination)) {
                return false;
            }
            if human.location == Location::Transit(journey.bus) {
                return router.trip_start == Some(journey.started);
            }
            if !journey.alighted {
                // A forced exit after a skip or line cancellation is not a
                // completed ride. The bus must be serving the intended berth.
                let served = transit.runs.get(&journey.bus).is_some_and(|run| {
                    run.line == journey.line
                        && run.dwell_until.is_some()
                        && transit.lines[run.line].stops[run.next_stop_idx] == journey.alight
                });
                let at_stop = transit.stops.get(journey.alight).is_some_and(|stop| {
                    human.trans.pos.distance2(stop.walk_pos) <= 2.0_f32.powi(2)
                });
                if !served || !at_stop || !transit.allows_line_for_trip(id, router, journey.line) {
                    return false;
                }
                journey.alighted = true;
            }
            if human.location == Location::Building(journey.destination) && router.trip_start.is_none() {
                let arrival = PassengerArrival {
                    passenger: id,
                    board: journey.board,
                    alight: journey.alight,
                    destination: journey.destination,
                    purpose: journey.purpose,
                    elapsed_tick: tick,
                };
                if controlled.is_some_and(|(bank, line, riders)| {
                    journey.destination == bank.park
                        && journey.alight == bank.stop
                        && journey.line == line
                        && riders.contains(&id)
                        && router.personal_car.is_none()
                }) {
                    self.cross_bank_arrival = Some(arrival);
                }
                *self.completed.entry(journey.line).or_default() += 1;
                self.examples.entry(journey.line).or_insert(arrival);
                return false;
            }
            router.trip_start == Some(journey.started)
                && !matches!(human.location, Location::Transit(_) | Location::Vehicle(_))
        });

        let boardings = sim.read::<TransitStats>().boardings.values().sum();
        if self.total_boardings == boardings {
            return;
        }
        self.total_boardings = boardings;
        for (id, human) in world.humans.iter() {
            let (
                Location::Transit(bus),
                Some(RoutingStep::RideTransit { line, alight }),
                Some(Destination::Building(destination)),
                Some(started),
            ) = (
                &human.location,
                human.router.cur_step(),
                human.router.target_dest,
                human.router.trip_start,
            ) else {
                continue;
            };
            if self.aboard_at_start.contains(&id) || self.active.contains_key(&id) {
                continue;
            }
            let Some(run) = transit.runs.get(bus)
                .filter(|run| run.line == *line && run.dwell_until.is_some())
            else {
                continue;
            };
            let board = transit.lines[run.line].stops[run.next_stop_idx];
            if board != *alight
                && human.trans.pos.distance2(transit.stops[board].walk_pos) <= 8.0_f32.powi(2)
            {
                self.active.insert(id, PassengerJourney {
                    bus: *bus,
                    line: *line,
                    board,
                    alight: *alight,
                    destination,
                    purpose: human.router.trip_purpose,
                    started,
                    alighted: false,
                });
            }
        }
    }
}

pub(super) fn verify_cross_bank_trip(
    mut sim: Simulation,
    manifest: &OpeningManifest,
) -> Result<serde_json::Value, String> {
    let bank = manifest
        .cross_bank
        .as_ref()
        .ok_or("wet opening has no far-bank destination")?;
    let line = manifest.lines["neighbourhood"].id;
    let (riders, trip_limit) = {
        let map = sim.map();
        let transit = sim.read::<Transit>();
        let (legs, dwell) = transit
            .directed_leg_times(line)
            .ok_or("neighbourhood bus has no directed trip times")?;
        // Give a rider one scheduled circuit, plus time to walk to and from it.
        let limit = legs.iter().sum::<u64>() + dwell * legs.len() as u64 + 2 * TICKS_PER_HOUR;
        let candidates: Vec<_> = sim
            .world()
            .humans
            .iter()
            .filter_map(|(id, human)| {
                if human.router.personal_car.is_some()
                    || human.location != Location::Building(human.home.house)
                    || human.router.trip_start.is_some()
                {
                    return None;
                }
                Some((id, human.home.house, map.get(human.home.house)?.door_pos))
            })
            .collect();
        let mut riders = BTreeSet::new();
        for &stop in transit.lines[line]
            .stops
            .iter()
            .filter(|&&stop| stop != bank.stop)
        {
            let walk = transit.stops[stop].walk_pos;
            let mut nearby: Vec<_> = candidates
                .iter()
                .map(|&(id, home, door)| (walk.distance2(door), id, home))
                .collect();
            nearby.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let mut homes = BTreeSet::new();
            riders.extend(
                nearby
                    .into_iter()
                    .filter(|&(_, _, home)| homes.insert(home))
                    .take(8)
                    .map(|(_, id, _)| id),
            );
        }
        (riders, limit)
    };
    if riders.is_empty() {
        return Err("no carless residents at home for the cross-Sava bus fixture".into());
    }
    // Give existing carless residents an ordinary destination in this disposable reload.
    // The router chooses the mode and the scheduled service must carry them there.
    {
        let humans = &mut sim.world_mut_unchecked().humans;
        for &id in &riders {
            let human = &mut humans[id];
            human.router.use_vehicle(None);
            human.decision.kind = HumanDecisionKind::GoTo(Destination::Building(bank.park));
            human.decision.wait = 0;
        }
    }
    let mut schedule = Simulation::schedule();
    let mut passengers = PassengerEvidence::new(&sim);
    for _ in 0..trip_limit {
        sim.tick(&mut schedule, []);
        passengers.observe(
            &sim,
            &sim.read::<Transit>(),
            elapsed_tick(&sim).0,
            Some((bank, line, &riders)),
        );
        if let Some(arrival) = passengers.cross_bank_arrival {
            return serde_json::to_value(arrival).map_err(|error| error.to_string());
        }
    }
    Err("no carless passenger completed a one-seat trip to the far-bank park".into())
}

#[derive(Default, Serialize)]
struct FreightEvidence {
    station_receipts: BTreeMap<String, u64>,
    observed_unloads: BTreeMap<String, u64>,
    train_returns_after_unload: u64,
    tracked_dispatches: u64,
    proven_delivery: Option<FreightDelivery>,
}

impl FreightEvidence {
    fn record_day(&mut self, sim: &Simulation, day: u64) {
        let book = sim.read::<CargoBook>();
        let Some(day) = book.freight_day(day) else {
            return;
        };
        let market = sim.read::<Market>();
        for (&item, _) in market.iter() {
            let units = sim.world().companies.keys()
                .map(|id| day.receipt(SoulID::GoodsCompany(id), item).station_units)
                .sum::<u64>();
            if units > 0 {
                *self.station_receipts.entry(item.prototype().name.clone()).or_default() += units;
            }
        }
    }

    fn has_delivery(&self) -> bool {
        self.proven_delivery.as_ref().is_some_and(|delivery| {
            delivery.units_from_train > 0
                && delivery.unload_tick < delivery.dispatch_tick
                && delivery.dispatch_tick < delivery.receipt_tick.unwrap_or(0)
                && delivery.unload_tick < delivery.return_tick.unwrap_or(0)
        })
    }
}

#[derive(Clone, Serialize)]
struct FreightDelivery {
    train: TrainID,
    station: BuildingID,
    buyer: BuildingID,
    truck: VehicleID,
    item: String,
    unload_tick: u64,
    dispatch_tick: u64,
    receipt_tick: Option<u64>,
    return_tick: Option<u64>,
    observed_unload_units: u64,
    loaded_units: u64,
    units_from_train: u64,
}

#[derive(Clone, Copy)]
struct RailStock {
    unload_tick: u64,
    unloaded: u64,
    remaining: u64,
    return_tick: Option<u64>,
}

#[derive(Default)]
struct StationInventory {
    unproved: u64,
    batches: VecDeque<RailStock>,
}

impl StationInventory {
    fn withdraw(&mut self, units: u64) -> Option<(RailStock, u64)> {
        // Station stock is fungible. Charge withdrawals against unproved
        // stock first, then against observed calls in unload order.
        let unproved = units.min(self.unproved);
        self.unproved -= unproved;
        self.withdraw_batches(units - unproved)
    }

    fn withdraw_batches(&mut self, mut units: u64) -> Option<(RailStock, u64)> {
        let mut first = None;
        while units > 0 {
            let batch = self.batches.front_mut()?;
            let used = units.min(batch.remaining);
            first.get_or_insert((*batch, used));
            batch.remaining -= used;
            units -= used;
            if batch.remaining == 0 {
                self.batches.pop_front();
            }
        }
        first
    }

    fn account_for_possible_restock(&mut self, units: u64) {
        let credited = self.batches.iter().map(|batch| batch.remaining).sum::<u64>();
        let uncertain = units.min(credited);
        self.withdraw_batches(uncertain);
        self.unproved += uncertain;
    }
}

struct StationDraw {
    item: ItemID,
    loaded: u64,
    batch: RailStock,
    attributed: u64,
}

struct TrackedDelivery {
    delivery: FreightDelivery,
    buyer: SoulID,
    item: ItemID,
    target: Option<ItineraryTarget>,
    receipt_day: u64,
    previous_receipts: u64,
}

#[derive(Clone, Copy, PartialEq)]
struct StationTruck {
    parked: bool,
    target: Option<ItineraryTarget>,
}

struct FreightMonitor {
    market_position: Vec2,
    train_state: Option<FreightTrainState>,
    latest_unload: Option<u64>,
    stock: BTreeMap<ItemID, u64>,
    inventory: BTreeMap<ItemID, StationInventory>,
    trucks: BTreeMap<VehicleID, StationTruck>,
    deliveries: BTreeMap<VehicleID, TrackedDelivery>,
    awaiting_return: Option<FreightDelivery>,
    evidence: FreightEvidence,
}

impl FreightMonitor {
    fn new(sim: &Simulation, freight: &Freight) -> Self {
        let map = sim.map();
        let mut monitor = Self {
            market_position: map.buildings()[freight.market].obb.center(),
            train_state: Self::train_state(sim, freight),
            latest_unload: None,
            stock: sim.read::<CargoBook>().import_stock(freight.terminal),
            inventory: BTreeMap::new(),
            trucks: BTreeMap::new(),
            deliveries: BTreeMap::new(),
            awaiting_return: None,
            evidence: FreightEvidence::default(),
        };
        monitor.observe_fleet(sim, freight.terminal);
        monitor
    }

    fn observe_fleet(&mut self, sim: &Simulation, terminal: BuildingID) {
        for id in sim.read::<CargoBook>().station_fleet(terminal) {
            if let Some(vehicle) = sim.world().vehicles.get(id) {
                self.trucks.entry(id).or_insert(StationTruck {
                    parked: matches!(vehicle.vehicle.state, VehicleState::ParkedAtDepot),
                    target: vehicle.it.target(),
                });
            }
        }
    }

    fn train_state(sim: &Simulation, freight: &Freight) -> Option<FreightTrainState> {
        sim.get(freight.station)?
            .f.trains.iter()
            .find_map(|&(id, state)| (id == freight.train).then_some(state))
    }

    fn observe(&mut self, sim: &Simulation, freight: &Freight, tick: u64) {
        if self.evidence.has_delivery() {
            return;
        }

        let state = Self::train_state(sim, freight);
        let unloading = matches!(self.train_state, Some(FreightTrainState::Arriving))
            && matches!(state, Some(FreightTrainState::Loading));
        let returned = matches!(self.train_state, Some(FreightTrainState::Moving))
            && matches!(state, None | Some(FreightTrainState::Arriving))
            && sim.get(freight.train).is_some_and(|train| {
                train.trans.pos.xy().distance2(self.market_position) < 10.0_f32.powi(2)
            });
        self.train_state = state;
        if returned {
            self.record_train_return(tick);
        }

        // The terminal can have every truck out on deliveries at handoff.
        // Use the cargo book's assignments to observe the entire fleet.
        const FLEET_SIZE: usize = 6;
        let vehicles = &sim.world().vehicles;
        if self.trucks.len() < FLEET_SIZE && tick % SAMPLE_TICKS == 0 {
            self.observe_fleet(sim, freight.terminal);
        }

        let full_fleet = self.trucks.len() == FLEET_SIZE
            && self.trucks.keys().all(|&id| vehicles.contains_key(id));
        let mut departure = None;
        let mut departures = 0;
        let mut route_changes = 0;
        for (&id, previous) in &mut self.trucks {
            let Some(truck) = vehicles.get(id) else { continue };
            let current = StationTruck {
                parked: matches!(truck.vehicle.state, VehicleState::ParkedAtDepot),
                target: truck.it.target(),
            };
            if previous.parked && !current.parked {
                departures += 1;
                if let Some(ItineraryTarget::Position(destination)) = current.target {
                    departure = Some((id, destination));
                }
            } else if !previous.parked && *previous != current {
                route_changes += 1;
            }
            *previous = current;
        }

        if !full_fleet || unloading || departures > 0 || route_changes > 0 {
            let book = sim.read::<CargoBook>();
            let stock = book.import_stock(freight.terminal);
            // A station truck can return at most one truckload. When a
            // return might overlap a draw or train call, reserve that
            // potential turnover from the observed batch amounts.
            const STATION_TRUCK_CAPACITY: u64 = 48;
            let possible_restock = if unloading || departures > 0 {
                route_changes as u64 * STATION_TRUCK_CAPACITY
            } else {
                0
            };
            let draw = self.update_stock(stock, tick, unloading, full_fleet, possible_restock);
            if full_fleet && !unloading && departures == 1 && route_changes == 0 {
                if let (Some((truck, destination)), Some(draw)) = (departure, draw) {
                    self.record_dispatch(sim, &book, freight, tick, draw, truck, destination);
                }
            }
        }

        if !self.deliveries.is_empty() {
            self.record_receipts(sim, tick, route_changes);
        }
    }

    fn update_stock(
        &mut self,
        next: BTreeMap<ItemID, u64>,
        tick: u64,
        train_unload: bool,
        fleet_complete: bool,
        possible_restock: u64,
    ) -> Option<StationDraw> {
        if !fleet_complete {
            self.inventory.clear();
            self.stock = next;
            return None;
        }

        let mut draw = None;
        for item in self.stock.keys().chain(next.keys().filter(|item| !self.stock.contains_key(*item))) {
            let item = *item;
            let previous = self.stock.get(&item).copied().unwrap_or(0);
            let units = next.get(&item).copied().unwrap_or(0);
            let inventory = self.inventory.entry(item).or_insert_with(|| StationInventory {
                unproved: previous,
                ..Default::default()
            });
            if possible_restock > 0 {
                inventory.account_for_possible_restock(possible_restock);
            }
            if units < previous {
                let loaded = previous - units;
                if let Some((batch, attributed)) = inventory.withdraw(loaded) {
                    draw.get_or_insert(StationDraw { item, loaded, batch, attributed });
                }
            } else if units > previous {
                let added = units - previous;
                if train_unload && possible_restock == 0 {
                    // Dispatch runs before the train call. A positive net
                    // increase is a lower bound on this call's actual unload.
                    self.latest_unload = Some(tick);
                    *self.evidence.observed_unloads.entry(item.prototype().name.clone()).or_default() += added;
                    inventory.batches.push_back(RailStock {
                        unload_tick: tick,
                        unloaded: added,
                        remaining: added,
                        return_tick: None,
                    });
                } else {
                    inventory.unproved += added;
                }
            }
        }
        self.inventory.retain(|item, _| next.contains_key(item));
        self.stock = next;
        draw
    }

    fn record_dispatch(
        &mut self,
        sim: &Simulation,
        book: &CargoBook,
        freight: &Freight,
        tick: u64,
        draw: StationDraw,
        truck: VehicleID,
        destination: Vec3,
    ) {
        let StationDraw { item, batch, loaded, attributed } = draw;
        let map = sim.map();
        let mut matching_buyers = sim.world().companies.iter().filter_map(|(id, company)| {
            let building = company.comp.building;
            let berth = ParkingManagement::default()
                .reserve_near(map.get(building)?.door_pos, &map).ok()?.park_pos(&map)?;
            (berth == destination).then_some((SoulID::GoodsCompany(id), building))
        });
        let Some((buyer, building)) = matching_buyers.next() else { return };
        if matching_buyers.next().is_some() {
            return;
        }

        let day = tick / TICKS_PER_BUDGET_DAY;
        self.evidence.tracked_dispatches += 1;
        self.deliveries.insert(truck, TrackedDelivery {
            delivery: FreightDelivery {
                train: freight.train,
                station: freight.terminal,
                buyer: building,
                truck,
                item: item.prototype().name.clone(),
                unload_tick: batch.unload_tick,
                dispatch_tick: tick,
                loaded_units: loaded,
                units_from_train: attributed,
                observed_unload_units: batch.unloaded,
                receipt_tick: None,
                return_tick: batch.return_tick,
            },
            buyer,
            item,
            target: Some(ItineraryTarget::Position(destination)),
            receipt_day: day,
            previous_receipts: book.freight_day(day).map_or(0, |record| {
                record.receipt(buyer, item).station_units
            }),
        });
    }

    fn record_receipts(&mut self, sim: &Simulation, tick: u64, route_changes: usize) {
        let book = sim.read::<CargoBook>();
        let day = tick / TICKS_PER_BUDGET_DAY;
        let mut arrived = None;
        self.deliveries.retain(|&id, tracked| {
            let Some(truck) = sim.get(id) else { return false };
            let receipts = book.freight_day(day)
                .map_or(0, |record| record.receipt(tracked.buyer, tracked.item).station_units);
            let delta = receipts.saturating_sub(if day == tracked.receipt_day {
                tracked.previous_receipts
            } else {
                0
            });
            tracked.receipt_day = day;
            tracked.previous_receipts = receipts;
            // The truck leaves its buyer itinerary on the same tick the ledger
            // credits its cargo. A reroute or abort alone supplies no proof.
            let target = self.trucks[&id].target;
            if target == tracked.target && !truck.vehicle.state.is_parked() {
                return true;
            }
            if route_changes == 1 && delta == tracked.delivery.loaded_units {
                let mut delivery = tracked.delivery.clone();
                delivery.receipt_tick = Some(tick);
                arrived = Some(delivery);
            }
            false
        });
        if let Some(delivery) = arrived {
            if delivery.return_tick.is_some() {
                self.evidence.proven_delivery = Some(delivery);
            } else if self.latest_unload == Some(delivery.unload_tick) {
                self.awaiting_return = Some(delivery);
            }
        }
    }

    fn record_train_return(&mut self, tick: u64) {
        let Some(unload_tick) = self.latest_unload.take() else { return };
        self.evidence.train_returns_after_unload += 1;
        for inventory in self.inventory.values_mut() {
            for batch in &mut inventory.batches {
                if batch.unload_tick == unload_tick {
                    batch.return_tick = Some(tick);
                }
            }
        }
        for tracked in self.deliveries.values_mut().filter(|trip| trip.delivery.unload_tick == unload_tick) {
            tracked.delivery.return_tick = Some(tick);
        }
        if let Some(mut delivery) = self.awaiting_return.take().filter(|delivery| delivery.unload_tick == unload_tick) {
            delivery.return_tick = Some(tick);
            self.evidence.proven_delivery = Some(delivery);
        }
    }
}

#[derive(Serialize)]
pub struct OpeningReport {
    opening: TownState,
    minimum_treasury_inner: i64,
    infill_housing_capacity: u32,
    days: Vec<DayReport>,
    traffic: Traffic,
    passenger_examples: BTreeMap<&'static str, PassengerArrival>,
    freight: FreightEvidence,
    failures: Vec<String>,
}

#[derive(Serialize)]
struct DayReport {
    elapsed_day: u64,
    residents_at_close: u64,
    resident_ticks: u64,
    baskets_due: u64,
    baskets_filled: u64,
    effective_food_stockout_ticks: u64,
    units_fulfilled_by_sku: BTreeMap<String, u64>,
    basket_failures: BTreeMap<String, u64>,
    work_visits: u64,
    park_visits: u64,
    transit_completed: u64,
    line_boardings: BTreeMap<&'static str, u64>,
    line_loops: BTreeMap<&'static str, u64>,
    line_completed_journeys: BTreeMap<&'static str, u64>,
    recurring_balance_inner: i128,
    budget_inner: BTreeMap<String, i128>,
    closing_treasury_inner: i64,
    utility_failures: u64,
    failed_coverage_samples: u64,
    coverage_at_close: Vec<String>,
    unstocked_retail_samples: BTreeMap<&'static str, u64>,
    coal_stockout_ticks: u64,
    coal_road_units: u64,
    coal_station_units: u64,
}

pub fn check_loaded_opening(
    mut sim: Simulation,
    plan: &TownPlan,
    manifest: &OpeningManifest,
) -> Result<OpeningReport, String> {
    let infill_housing_capacity = verify_opening_ids(&sim, manifest)?;
    let opening = snapshot(&sim, plan, manifest);
    let mut failures = coverage_failures(&opening);
    if opening.utilities.is_empty()
        || opening.utilities.iter().any(|net| !net.adequate)
        || !opening.unserved_buildings.is_empty()
    {
        failures.push("opening utilities lack required service or reserve".into());
    }
    if u64::from(infill_housing_capacity) * 10 < opening.residents {
        failures.push(format!(
            "infill capacity {infill_housing_capacity} is below 10% of {} opening residents",
            opening.residents
        ));
    }
    for district in &plan.districts {
        if opening
            .districts
            .get(district.key)
            .is_none_or(|data| !data.opening_uses_met(district.key))
        {
            let required_uses = if district.key == "employment" {
                "jobs or stocked shops"
            } else {
                "two active blocks, residents or stocked shops"
            };
            failures.push(format!("{} lacks {required_uses}", district.key));
        }
    }
    let load = elapsed_tick(&sim).0;
    let start = load.div_ceil(TICKS_PER_BUDGET_DAY) * TICKS_PER_BUDGET_DAY;
    let end = start + SOAK_DAYS * TICKS_PER_BUDGET_DAY;
    let mut schedule = Simulation::schedule();
    let mut traffic = Traffic::default();
    let mut loops = LoopEvidence::default();
    let mut days = Vec::new();
    let mut minimum_cash = sim.read::<Government>().money;
    for _ in load..start {
        sim.tick(&mut schedule, []);
        minimum_cash = minimum_cash.min(sim.read::<Government>().money);
    }
    let mut freight = FreightMonitor::new(&sim, &manifest.freight);
    let mut passengers = PassengerEvidence::new(&sim);
    let initial_car_trips = sim.read::<TripStats>().car.completed;
    let plant = manifest
        .sites
        .get("coal-power-plant")
        .and_then(|&building| sim.read::<BuildingInfos>().owner(building))
        .ok_or("coal plant has no operating company")?;
    let coal = ItemID::new("coal");
    for day in closed_window(load, end) {
        let initial_boardings = sim.read::<TransitStats>().boardings.clone();
        let initial_basket_failures = sim.read::<Market>().basket_failures().clone();
        let initial_loops = loops.loops.clone();
        let initial_passengers = passengers.completed.clone();
        let initial_transit = sim.read::<TripStats>().transit.completed;
        let mut resident_ticks = 0;
        let mut utility_failures = 0;
        let mut failed_coverage_samples = 0;
        let mut coverage_at_close = Vec::new();
        let mut unstocked_retail_samples = BTreeMap::new();
        let mut coal_stockout_ticks = 0;
        let day_start = day * TICKS_PER_BUDGET_DAY;
        for dt in 1..=TICKS_PER_BUDGET_DAY {
            resident_ticks += sim.world().humans.len() as u64;
            sim.tick(&mut schedule, []);
            coal_stockout_ticks += u64::from(sim.read::<Market>().capital(plant, coal) <= 0);
            let tick = day_start + dt;
            minimum_cash = minimum_cash.min(sim.read::<Government>().money);
            {
                let transit = sim.read::<Transit>();
                loops.observe(&transit);
                passengers.observe(&sim, &transit, tick, None);
            }
            if tick < end {
                freight.observe(&sim, &manifest.freight, tick);
            }
            traffic.probe_egresses(&sim);
            if dt % SAMPLE_TICKS == 0 {
                traffic.observe(&sim, tick);
            }
            if dt % (20 * TICKS_PER_MINUTE) == 0 {
                let state = snapshot(&sim, plan, manifest);
                if state.utilities.is_empty()
                    || state.utilities.iter().any(|net| !net.adequate)
                    || !state.unserved_buildings.is_empty()
                {
                    utility_failures += 1;
                }
                let uncovered = coverage_failures(&state);
                failed_coverage_samples += u64::from(!uncovered.is_empty());
                coverage_at_close = uncovered;
                for district in &plan.districts {
                    if state
                        .districts
                        .get(district.key)
                        .is_none_or(|data| data.stocked_shops == 0)
                    {
                        *unstocked_retail_samples.entry(district.key).or_insert(0) += 1;
                    }
                }
            }
        }
        let close = elapsed_tick(&sim).0;
        freight.evidence.record_day(&sim, day);
        let economy = sim.read::<DailyEconomyMetrics>();
        let empty_day = Default::default();
        let daily = economy.closed_day(day, close).unwrap_or(&empty_day);
        let completed = |purpose| {
            daily
                .trips_by_purpose
                .get(&purpose)
                .map_or(0, |trips| trips.completed)
        };
        let work_visits = completed(TripPurpose::Work);
        let park_visits = completed(TripPurpose::Leisure);
        let effective_food_stockout_ticks = daily
            .stockout_ticks_by_sku
            .get(&ItemID::new("effective-food"))
            .copied()
            .unwrap_or(0);
        let coal_receipt = sim
            .read::<CargoBook>()
            .freight_day(day)
            .map_or_else(Default::default, |record| record.receipt(plant, coal));
        let baskets_due = daily.baskets_due.unwrap_or(0);
        let baskets_filled = daily.baskets_filled.unwrap_or(0);
        if !needs_met(
            baskets_due,
            baskets_filled,
            effective_food_stockout_ticks,
            resident_ticks,
            work_visits,
            park_visits,
        ) {
            failures.push(format!(
                "day {day}: household needs, work or park gate failed"
            ));
        }
        if utility_failures > 0 || failed_coverage_samples > 0 {
            failures.push(format!("day {day}: {utility_failures} utility samples and {failed_coverage_samples} coverage samples failed"));
        }
        if coal_stockout_ticks > 0 {
            failures.push(format!(
                "day {day}: coal plant fuel bin was empty for {coal_stockout_ticks} ticks"
            ));
        }
        let transit_completed = sim.read::<TripStats>().transit.completed - initial_transit;
        let stats = sim.read::<TransitStats>();
        let mut line_boardings = BTreeMap::new();
        let mut line_loops = BTreeMap::new();
        let mut line_completed_journeys = BTreeMap::new();
        for (&key, line) in &manifest.lines {
            let after = stats.boardings.get(&line.id).copied().unwrap_or(0);
            let before = initial_boardings.get(&line.id).copied().unwrap_or(0);
            let completed_loops = loops.loops.get(&line.id).copied().unwrap_or(0)
                - initial_loops.get(&line.id).copied().unwrap_or(0);
            let boardings = after.saturating_sub(before);
            line_boardings.insert(key, boardings);
            line_loops.insert(key, completed_loops);
            let completed_journeys = passengers.completed.get(&line.id).copied().unwrap_or(0)
                - initial_passengers.get(&line.id).copied().unwrap_or(0);
            line_completed_journeys.insert(key, completed_journeys);
            if !line_operating(key, boardings, completed_loops, completed_journeys) {
                failures.push(format!(
                    "day {day}: bus line {key} did not meet boarding and arrival gates"
                ));
            }
        }
        let budget = sim.read::<Budget>();
        let ledger = budget.days().get(&day).cloned().unwrap_or_default();
        let balance = recurring_balance(&ledger);
        if day >= end / TICKS_PER_BUDGET_DAY - 2 && balance <= 0 {
            failures.push(format!(
                "day {day}: recurring operating balance {balance} is not positive"
            ));
        }
        let record = DayReport {
            elapsed_day: day,
            residents_at_close: sim.world().humans.len() as u64,
            resident_ticks,
            baskets_due,
            baskets_filled,
            effective_food_stockout_ticks,
            units_fulfilled_by_sku: daily
                .units_fulfilled_by_sku
                .iter()
                .map(|(&item, &units)| (item.prototype().name.clone(), units))
                .collect(),
            basket_failures: sim
                .read::<Market>()
                .basket_failures()
                .iter()
                .filter_map(|(reason, &count)| {
                    let delta = count.saturating_sub(
                        initial_basket_failures
                            .get(reason)
                            .copied()
                            .unwrap_or_default(),
                    );
                    (delta > 0).then(|| (format!("{reason:?}"), delta))
                })
                .collect(),
            work_visits,
            park_visits,
            transit_completed,
            line_boardings,
            line_loops,
            line_completed_journeys,
            recurring_balance_inner: balance,
            budget_inner: ledger
                .into_iter()
                .map(|(kind, value)| (format!("{kind:?}"), value))
                .collect(),
            closing_treasury_inner: sim.read::<Government>().money.inner(),
            utility_failures,
            failed_coverage_samples,
            coverage_at_close,
            unstocked_retail_samples,
            coal_stockout_ticks,
            coal_road_units: coal_receipt.road_units,
            coal_station_units: coal_receipt.station_units,
        };
        eprintln!(
            "CITYMAP soak: {}",
            serde_json::to_string(&record).map_err(|error| error.to_string())?
        );
        days.push(record);
    }
    if !stable_population(
        opening.residents,
        opening.residents,
        opening.employed_residents,
        opening.residents,
    ) || !population_change_ok(days[days.len() - 2].residents_at_close, days[days.len() - 1].residents_at_close)
    {
        failures.push(format!("opening population/employment or final population stability failed ({} residents, {} employed)", opening.residents, opening.employed_residents));
    }
    traffic.car_arrivals = sim.read::<TripStats>().car.completed - initial_car_trips;
    if !traffic_clear(
        traffic.car_arrivals,
        traffic.max_standing_ticks,
        traffic.max_blocking_ticks,
        traffic.stranded_drivers,
    ) || traffic.directed_lanes_entered < 2
    {
        failures.push(
            "traffic did not clear all standing/blocking episodes or lacks directed arrivals"
                .into(),
        );
    }
    if !freight.evidence.has_delivery() {
        failures.push("freight recorded no attributable in-window train unload, truck receipt and market return".into());
    }
    if minimum_cash <= Money::ZERO {
        failures.push(format!("treasury reached {minimum_cash}"));
    }
    if opening.unfuelled_plant {
        failures.push("opening coal plant lacked staff or fuel".into());
    }
    if let Err(error) = verify_infill(&mut sim, plan, manifest) {
        failures.push(format!("disposable infill check: {error}"));
    }
    Ok(OpeningReport {
        opening,
        minimum_treasury_inner: minimum_cash.inner(),
        infill_housing_capacity,
        days,
        traffic,
        passenger_examples: manifest.lines.iter().filter_map(|(&name, line)| {
            Some((name, passengers.examples.remove(&line.id)?))
        }).collect(),
        freight: freight.evidence,
        failures,
    })
}

impl OpeningReport {
    pub(super) fn sava_traffic(
        &self,
        plan: &TownPlan,
        built: &BuiltNetwork,
    ) -> Result<BTreeMap<&'static str, u64>, String> {
        plan.crossings
            .iter()
            .map(|crossing| {
                let count = built.links[&crossing.bridge]
                    .lanes
                    .iter()
                    .filter_map(|lane| self.traffic.directional_entries.get(lane))
                    .sum();
                if count == 0 {
                    Err(format!("scored traffic did not traverse {}", crossing.key))
                } else {
                    Ok((crossing.key, count))
                }
            })
            .collect()
    }

    pub fn verify(&self) -> Result<(), String> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            let report = serde_json::to_string(self).map_err(|error| error.to_string())?;
            Err(format!("CITYMAP opening gates failed: {report}"))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use prototypes::TICKS_PER_HOUR;

    #[test]
    fn citymap_wet_frontage_serves_homes_and_leaves_infill() {
        use crate::citymap::{
            build_wet_town, plan_town, read_inputs, SourcePaths, SpacingLimits, TownConfig,
            DEFAULT_LESCE, DEFAULT_SAVA,
        };
        use std::path::Path;

        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let mut plan = plan_town(&inputs, TownConfig::wet(), SpacingLimits::default()).unwrap();
        configure_commissioned_network(&mut plan);
        let (mut sim, built, freight) = build_wet_town(&mut plan).unwrap();
        let opening = prepare_opening(&mut sim, &plan, &built, Some(freight)).unwrap();
        let map = sim.map();
        if ResidencePrototype::iter().any(|home| home.massing.is_some()) {
            let loading_lane = |name: &str| {
                let door = map.buildings()[opening.sites[name]].door_pos;
                let parking = ParkingManagement::default().reserve_near(door, &map).unwrap();
                let position = parking.park_pos(&map).unwrap();
                let lane = map.nearest_lane(position, LaneKind::Driving, None).unwrap();
                let points = &map.lanes()[lane].points;
                (lane, points.length_at_proj(points.project(position)))
            };
            let (mine_lane, mine_offset) = loading_lane("coal-mine-1");
            let (plant_lane, plant_offset) = loading_lane("coal-power-plant");
            assert!(mine_lane == plant_lane && mine_offset < plant_offset,
                "the first coal mine needs a direct outbound drive to the plant's loading curb");
            assert!(opening.sites.contains_key("coal-buffer"),
                "the plant needs a nearby multi-truck coal buffer");
            let buffer = opening.sites["coal-buffer"];
            let plant = opening.sites["coal-power-plant"];
            let distance = map.buildings()[buffer].door_pos.distance(map.buildings()[plant].door_pos);
            assert!(distance <= 200.0, "the coal buffer is {distance:.1} m from the plant");
            let warehouse = map.buildings()[buffer].kind.as_goods_company().unwrap().prototype();
            assert!(warehouse.n_trucks >= 4);
            assert!(warehouse.storage.as_ref().unwrap().accepted_items.contains(&ItemID::new("coal")));
        }
        let reach = ServiceReach::new(&map, true);
        let bakeries: Vec<_> = map
            .buildings()
            .values()
            .filter(|building| {
                building.kind.as_goods_company() == Some(prototypes::GoodsCompanyID::new("bakery"))
            })
            .filter_map(|building| reach.position(building.connected_road?, building.door_pos.xy()))
            .collect();
        let mut housing = 0;
        let mut near_bakeries = 0;
        let mut by_district = BTreeMap::new();
        let mut isolated_south = 0;
        let mut quay_housing = 0;
        for parcel in opening.parcels.iter().filter(|parcel| parcel.capacity > 0) {
            let home = reach
                .position(parcel.road, parcel.footprint.center())
                .unwrap();
            housing += parcel.capacity;
            *by_district.entry(parcel.district).or_insert(0) += parcel.capacity;
            if parcel.district == "south"
                && parcel.footprint.center().y < plan.config.owned_min.y + 600.0
            {
                isolated_south += parcel.capacity;
            }
            if outfall_housing(parcel, plan.config.owned_min) {
                quay_housing += parcel.capacity;
            }
            if bakeries
                .iter()
                .any(|&shop| reach.distance(home, shop) <= WET_BAKERY_WALK_M)
            {
                near_bakeries += parcel.capacity;
            }
        }
        let reserve: u32 = opening.reserves.iter().map(|parcel| parcel.capacity).sum();
        let reserve_shops = opening
            .reserves
            .iter()
            .filter(|parcel| parcel.zoning.kind == ZoneKind::Commercial)
            .count();
        eprintln!("Wet frontage: {near_bakeries}/{housing} housing places near {} bakeries, {reserve} reserve places, {reserve_shops} reserve shops, housing by district {by_district:?}", bakeries.len());
        assert!(near_bakeries * 100 >= housing * 95 && reserve >= 260 && reserve_shops >= 3,
            "{near_bakeries}/{housing} housing places within a 180 m walk of a bakery, {reserve} reserve housing places and {reserve_shops} reserve shops");
        assert!(
            housing >= 1020 && isolated_south == 0,
            "{housing} opening housing places, with {isolated_south} on the remote South fringe"
        );
        if ResidencePrototype::iter().any(|home| home.massing.is_some()) {
            let rim = super::super::layout::design_point(vec2(718.0, 1948.0), plan.config);
            let bakery = &map.buildings()[opening.sites["west/northwest-bakery"]];
            assert!(bakery.door_pos.xy().distance(rim) <= 150.0);
            for (name, x, y) in [
                ("south/east-rim-bakery", 2670.0, 1270.0),
                ("south/market-bakery", 2580.0, 1098.0),
                ("station/north-bakery", 2110.0, 1903.0),
            ] {
                let anchor = super::super::layout::design_point(vec2(x, y), plan.config);
                let building = opening.sites.get(name).unwrap_or_else(|| panic!("missing local bakery {name}"));
                let door = map.buildings()[*building].door_pos.xy();
                assert!(door.distance(anchor) <= 125.0,
                    "{name} should serve its nearby housing and work frontage, door at {door:?}");
            }
            let south = super::super::layout::design_point(vec2(2390.0, 1125.0), plan.config);
            let depot = &map.buildings()[opening.sites["south/east-garbage-depot"]];
            assert!(depot.door_pos.xy().distance(south) <= 150.0);
            for name in ["animal-farm", "slaughterhouse", "meat-facility", "vegetable-farm"] {
                let door = map.buildings()[opening.sites[name]].door_pos.xy();
                assert!(opening.parcels.iter().any(|parcel| {
                    parcel.capacity > 0 && parcel.footprint.center().distance(door) <= 250.0
                }), "{name} needs a nearby opening housing block");
            }
            let foundry = &map.buildings()[opening.sites["rail-foundry"]];
            let west = super::super::layout::design_point(vec2(1114.0, 2186.0), plan.config);
            assert!(foundry.door_pos.xy().distance(west) <= 150.0,
                "the rail foundry needs to be near its West workforce, door at {:?}", foundry.door_pos);
            assert_eq!(opening.intake_homes.len(), 12);
            assert!(opening.parcels.iter()
                .filter(|parcel| opening.intake_homes.contains(&parcel.lot))
                .all(|parcel| parcel.district == "west" && parcel.zoning.density == Density::High));
            let intake_workers: usize = sim.world().companies.values()
                .filter(|company| company.comp.proto.prototype().name == "water-pump")
                .map(|company| company.workers.0.len())
                .sum();
            assert!(intake_workers >= 16, "{intake_workers} intake workers at the end of the early hiring phase");
            let outskirts: Vec<_> = opening.parcels.iter()
                .filter(|parcel| parcel.capacity > 0 && parcel.district != "station")
                .collect();
            let medium = outskirts.iter().filter(|parcel| parcel.zoning.density == Density::Medium).count();
            assert!(medium * 4 >= outskirts.len() * 3,
                "{medium}/{} neighbourhood home parcels use Medium density under the profiled stock", outskirts.len());
            assert!(opening.parcels.iter()
                .filter(|parcel| parcel.capacity > 0 && parcel.district == "station")
                .all(|parcel| parcel.zoning.density == Density::High));
        }
        assert!(
            quay_housing >= 16,
            "{quay_housing} housing places near the outfall jobs"
        );
        let employment = plan
            .districts
            .iter()
            .find(|district| district.key == "employment")
            .unwrap();
        let store = &map.buildings()[opening.sites["employment/supermarket"]];
        assert!(contains(&employment.polygon, store.obb.center()));
        let entry = super::super::layout::design_point(vec2(2350.0, 1960.0), plan.config);
        assert!(store.obb.center().distance(entry) <= 100.0,
            "the employment supermarket should fit within 100 m of the staffed town-facing frontage");
        let east_homes = super::super::layout::design_point(vec2(2438.0, 938.0), plan.config);
        let fire_station = prototypes::GoodsCompanyID::new("fire-station");
        let nearest_fire = map.buildings().values()
            .filter(|building| building.kind.as_goods_company() == Some(fire_station))
            .map(|building| building.obb.center().distance(east_homes))
            .fold(f32::INFINITY, f32::min);
        assert!(nearest_fire <= 140.0,
            "the nearest fire station is {nearest_fire:.1} m from uncovered South frontage");
        let local_road = built.links[&SourceKey::Way { id: 1465713103, span: 1 }].road;
        let avenue = built.links[&SourceKey::Way { id: 64047075, span: 1 }].road;
        let west = super::super::layout::design_point(vec2(2446.6, 1255.7), plan.config);
        let east = super::super::layout::design_point(vec2(2569.0, 1282.0), plan.config);
        let distance = reach.distance(
            reach.position(local_road, west).unwrap(),
            reach.position(avenue, east).unwrap(),
        );
        eprintln!("South frontage to Alpska walk: {distance:.1} m");
        assert!(distance <= 170.0,
            "the walk from South homes to the Alpska shops is {distance:.1} m");
        let hraska_egress = built.junctions[&SourceKey::Node {
            id: 4193639666,
            rail: false,
        }];
        let junction = &map.intersections()[hraska_egress];
        assert_eq!(junction.light_policy, simulation::map::LightPolicy::Lights);
        assert!(!junction.turn_policy.left_turns);
        let bend = built.links[&SourceKey::Way { id: 461688158, span: 1 }].road;
        let road = &map.roads()[bend];
        assert_eq!(
            road.lanes_iter().filter(|(_, kind)| *kind == LaneKind::Driving).count(),
            2,
        );
        assert!(
            road.lanes_iter().all(|(_, kind)| kind != LaneKind::Parking),
            "the Hraška school bend must keep curb manoeuvres clear of the turn onto the collector",
        );
        let exit = built.links[&SourceKey::Way { id: 1426155807, span: 0 }].road;
        let (_, entering) = road.points().point_dir_along(road.points().length());
        let (_, leaving) = map.roads()[exit].points().point_dir_along(0.0);
        assert!(entering.xy().dot(leaving.xy()) > 0.95,
            "the collector arms at the Hraška school bend need a shared tangent");
        let ridge = built.links[&SourceKey::Way { id: 419255436, span: 2 }].road;
        let park = built.links[&SourceKey::Authored("hraska-park-approach")].road;
        let west = super::super::layout::design_point(vec2(1502.9, 2293.9), plan.config);
        let east = super::super::layout::design_point(vec2(1636.0, 2398.0), plan.config);
        let distance = reach.distance(
            reach.position(ridge, west).unwrap(),
            reach.position(park, east).unwrap(),
        );
        eprintln!("Hraška ridge to jobs and shops across the railway: {distance:.1} m");
        assert!(distance <= 200.0,
            "the Hraška walk past the railway is {distance:.1} m");
        let culdesac = built.links[&SourceKey::Way { id: 419255437, span: 0 }].road;
        assert!(map.roads()[culdesac].lanes_iter().all(|(_, kind)| kind == LaneKind::Walking));
        drop(map);
        paint(&mut sim, opening.parcels.iter()).unwrap();
        verify_votes(&sim, opening.parcels.iter()).unwrap();
        let mut updated = (*sim.map()).clone();
        updated.update();
        super::super::resolve_network(&updated, &plan.graph).unwrap();
    }

    #[test]
    fn cm2_publish_keeps_the_west_south_shortcut_walkable() {
        use crate::citymap::{build_dry_town, plan_town, read_inputs, SourcePaths, SpacingLimits, TownConfig, DEFAULT_LESCE, DEFAULT_SAVA};
        use simulation::map::LaneKind;
        use std::path::Path;

        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let mut plan = plan_town(&inputs, TownConfig::default(), SpacingLimits::default()).unwrap();
        configure_commissioned_network(&mut plan);
        let shortcut = SourceKey::Infill { number: 7, part: 4 };
        assert_eq!(plan.graph.links[&shortcut].class, RoadClass::Walk);
        let (sim, network) = build_dry_town(&plan).unwrap();
        let map = sim.map();
        let road = &map.roads()[network.links[&shortcut].road];
        assert!(road.lanes_iter().all(|(_, kind)| kind == LaneKind::Walking));
    }

    #[test]
    fn cm2_layout_keeps_the_southeast_stub_walkable_without_vehicle_egress() {
        use crate::citymap::{
            build_dry_town, plan_town, read_inputs, SourcePaths, SpacingLimits, TownConfig, DEFAULT_LESCE, DEFAULT_SAVA,
        };
        use simulation::map::LaneKind;
        use std::path::Path;

        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let mut plan = plan_town(&inputs, TownConfig::default(), SpacingLimits::default()).unwrap();
        configure_commissioned_network(&mut plan);
        let (sim, built) = build_dry_town(&plan).unwrap();
        let map = sim.map();
        let lanes: Vec<_> = map.roads()[built.links[&SOUTHEAST_STUB].road]
            .lanes_iter()
            .collect();
        assert!(lanes.iter().any(|(_, kind)| *kind == LaneKind::Walking));
        assert!(lanes.iter().all(|(_, kind)| *kind == LaneKind::Walking));
    }

    #[test]
    fn cm2_layout_restores_alpska_crosswalk() {
        use crate::citymap::{
            build_dry_town, plan_town, read_inputs, SourcePaths, SpacingLimits, TownConfig, DEFAULT_LESCE, DEFAULT_SAVA,
        };
        use simulation::map::{LaneKind, PathKind, TurnKind};
        use simulation::map_dynamic::Itinerary;
        use std::path::Path;

        let inputs = read_inputs(SourcePaths {
            lesce: Path::new(DEFAULT_LESCE),
            sava: Some(Path::new(DEFAULT_SAVA)),
        })
        .unwrap();
        let mut plan = plan_town(&inputs, TownConfig::default(), SpacingLimits::default()).unwrap();
        configure_commissioned_network(&mut plan);
        let (sim, built) = build_dry_town(&plan).unwrap();
        let map = sim.map();
        let alpska = SourceKey::Node { id: 862116385, rail: false };
        let junction = &map.intersections()[built.junctions[&alpska]];
        assert!(junction.turn_policy.crosswalks);
        assert!(junction.turns().any(|turn| turn.kind == TurnKind::Crosswalk));
        let sidewalks: Vec<_> = junction
            .roads
            .iter()
            .flat_map(|&road| {
                map.roads()[road].lanes_iter().filter_map(|(lane, kind)| {
                    let lane = &map.lanes()[lane];
                    (kind == LaneKind::Walking).then(|| {
                        let points = &lane.points;
                        let offset = points.length().min(40.0) * 0.5;
                        points.point_along(if lane.src == junction.id {
                            offset
                        } else {
                            points.length() - offset
                        })
                    })
                })
            })
            .collect();
        assert_eq!(sidewalks.len(), 6);
        for &from in &sidewalks {
            for &to in &sidewalks {
                assert!(
                    Itinerary::route(prototypes::Tick(0), from, to, &map, PathKind::Pedestrian)
                        .is_some(),
                    "missing walking route from {from:?} to {to:?}"
                );
            }
        }
    }

    #[test]
    fn cm2_publish_tracks_the_same_parcel_after_demolition() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let sim = Simulation::new_authored_flat(1, 1).unwrap();
        let mut map = sim.write::<Map>();
        let (_, road) = map
            .make_connection(
                MapProject::ground(Vec3::new(40.0, 80.0, 0.0)),
                MapProject::ground(Vec3::new(190.0, 80.0, 0.0)),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let lot = map.lots().values().find(|lot| lot.parent == road).unwrap();
        let mut parcel = Parcel {
            lot: lot.id,
            road,
            district: "south",
            block: None,
            footprint: lot.shape,
            zoning: ZoneCell {
                kind: ZoneKind::Residential,
                density: Density::Low,
            },
            capacity: 1,
            workers: 0,
            building: None,
        };
        let original = parcel.lot;
        let building = map.place_growable(original, BuildingKind::House).unwrap();
        map.remove_building(building).unwrap();
        record_development(&map, std::slice::from_mut(&mut parcel));
        assert_ne!(parcel.lot, original);
        let restored = map.get(parcel.lot).unwrap();
        assert_eq!(restored.parent, road);
        assert_eq!(restored.shape.corners, parcel.footprint.corners);
        assert!(parcel.building.is_none());
    }

    #[test]
    fn cm2_gate_retail_stock_requires_an_available_shelf_offer() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let sim = Simulation::new_authored_flat(1, 1).unwrap();
        let mut market = sim.write::<Market>();
        let seller = SoulID::GoodsCompany(Default::default());
        let groceries = ItemID::new("groceries");
        market.produce(seller, groceries, 3);
        market.sell(seller, Vec2::ZERO, groceries, 0, 3);
        assert!(!has_retail_stock(&market, seller));
        market.sell(seller, Vec2::ZERO, groceries, 3, 3);
        assert!(has_retail_stock(&market, seller));
    }

    #[test]
    fn cm2_gate_employment_and_population() {
        assert!(stable_population(2500, 2750, 2063, 2500));
        assert!(stable_population(3000, 3000, 2850, 3000));
        assert!(!stable_population(2499, 2500, 2500, 2500));
        assert!(!stable_population(2500, 2500, 1874, 2500));
        assert!(!stable_population(2500, 2751, 2751, 2500));
        assert!(!stable_population(2500, 0, 0, 0));
    }

    #[test]
    fn cm2_gate_needs_use_settled_baskets_and_integrated_population() {
        assert!(needs_met(100, 94, 10, 240, 1, 1));
        assert!(!needs_met(0, 0, 0, 240, 1, 1));
        assert!(!needs_met(100, 93, 0, 240, 1, 1));
        assert!(!needs_met(100, 100, 11, 240, 1, 1));
        assert!(!needs_met(100, 100, 0, 0, 1, 1));
        assert!(!needs_met(100, 100, 0, 240, 1, 0));
    }

    #[test]
    fn cm2_gate_capacity_and_coverage_do_not_average_away_failures() {
        assert!(service_covered(900 + 0, 900 + 100));
        assert!(!service_covered(0, 100));
        assert!(!service_covered(0, 0));
        assert!(spare_capacity(125.0, 100.0));
        assert!(!spare_capacity(124.9, 100.0));
        assert!(!spare_capacity(125.0, 0.0));
        assert!(!spare_capacity(f64::INFINITY, 100.0));
    }

    #[test]
    fn cm2_publish_school_cap_allows_ten_percent_headroom() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let school = try_prototype(ServiceID::new("school")).unwrap();
        let residents = 3_000;
        assert!(MAX_SCHOOLS * school.capacity as usize * 10 >= residents * 11);
    }

    #[test]
    fn cm2_freight_rejects_unattributed_station_receipts() {
        let mut freight = FreightEvidence::default();
        assert!(!freight.has_delivery());
        for item in ["goods", "coal", "flour"] {
            freight.station_receipts.insert(item.into(), 1);
            assert!(!freight.has_delivery(), "credited an unrelated receipt for {item}");
        }
    }

    #[test]
    fn cm2_freight_tracks_a_scored_unload_across_later_train_calls() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let item = ItemID::new("goods");
        let mut monitor = FreightMonitor {
            market_position: Vec2::ZERO,
            train_state: None,
            latest_unload: None,
            stock: BTreeMap::from([(item, 100)]),
            inventory: BTreeMap::new(),
            trucks: BTreeMap::new(),
            deliveries: BTreeMap::new(),
            awaiting_return: None,
            evidence: FreightEvidence::default(),
        };
        assert!(monitor.update_stock(BTreeMap::from([(item, 140)]), 10, true, true, 0).is_none());
        assert!(monitor.update_stock(BTreeMap::from([(item, 90)]), 11, false, true, 0).is_none());
        monitor.record_train_return(15);
        assert!(monitor.update_stock(BTreeMap::from([(item, 130)]), 20, true, true, 0).is_none());
        let draw = monitor.update_stock(BTreeMap::from([(item, 60)]), 21, false, true, 0).unwrap();
        assert_eq!(draw.loaded, 70);
        assert_eq!(draw.attributed, 20);
        assert_eq!(draw.batch.unload_tick, 10);
        assert_eq!(draw.batch.return_tick, Some(15));
    }

    #[test]
    fn cm2_freight_requires_loaded_units_a_buyer_receipt_and_train_return() {
        let mut stock = StationInventory {
            unproved: 20,
            batches: VecDeque::from([RailStock {
                unload_tick: 10,
                unloaded: 12,
                remaining: 12,
                return_tick: None,
            }]),
        };
        assert!(stock.withdraw(20).is_none());
        stock.account_for_possible_restock(5);
        assert!(stock.withdraw(5).is_none());
        assert_eq!(stock.withdraw(6).unwrap().1, 6);

        for item in ["goods", "coal", "flour"] {
            let mut proof = FreightDelivery {
                train: TrainID::default(),
                station: BuildingID::default(),
                buyer: BuildingID::default(),
                truck: VehicleID::default(),
                item: item.into(),
                unload_tick: 10,
                dispatch_tick: 11,
                receipt_tick: None,
                return_tick: None,
                observed_unload_units: 12,
                loaded_units: 25,
                units_from_train: 5,
            };
            let check = |delivery| FreightEvidence {
                proven_delivery: Some(delivery),
                ..Default::default()
            }.has_delivery();
            assert!(!check(proof.clone()));
            proof.receipt_tick = Some(12);
            assert!(!check(proof.clone()));
            proof.return_tick = Some(13);
            assert!(check(proof.clone()));
            proof.units_from_train = 0;
            assert!(!check(proof));
        }
    }

    #[test]
    fn cm2_infill_rejects_a_new_junction_inside_the_spacing_floor() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let sim = Simulation::new_authored_flat(1, 1).unwrap();
        let mut map = sim.write::<Map>();
        map.make_connection(
            MapProject::ground(Vec3::new(40.0, 80.0, 0.0)),
            MapProject::ground(Vec3::new(120.0, 80.0, 0.0)),
            None,
            &LanePatternBuilder::new().build(),
        ).unwrap();
        assert!(verify_infill_spacing(&map, 40.0).is_ok());
        map.make_connection(
            MapProject::ground(Vec3::new(100.0, 110.0, 0.0)),
            MapProject::ground(Vec3::new(190.0, 110.0, 0.0)),
            None,
            &LanePatternBuilder::new().build(),
        ).unwrap();
        assert!(verify_infill_spacing(&map, 40.0).is_err());
    }

    #[test]
    fn cm2_infill_rejects_missing_opening_frontage() {
        let lot = LotID::default();
        let (_, commercial) = downtown_cells();
        let approved = BTreeMap::from([(lot, commercial)]);
        assert!(audit_votes(std::iter::empty(), &approved).is_err());
    }

    #[test]
    fn cm2_gate_transit_requires_new_boardings_and_real_loop() {
        assert!(line_operating("employment", 5, 1, 1));
        assert!(!line_operating("employment", 5, 1, 0));
        assert!(!line_operating("employment", 4, 1, 1));
        assert!(!line_operating("employment", 5, 0, 1));
        assert!(line_operating("neighbourhood", 10, 1, 1));
        assert!(!line_operating("neighbourhood", 9, 1, 1));
        assert!(!line_operating("neighbourhood", 15, 0, 1));
        assert!(!line_operating("neighbourhood", 15, 1, 0));
    }

    #[test]
    fn cm2_gate_employment_requires_jobs_and_retail() {
        let mut district = DistrictState {
            jobs: 1,
            stocked_shops: 1,
            ..Default::default()
        };
        assert!(district.opening_uses_met("employment"));
        district.jobs = 0;
        assert!(!district.opening_uses_met("employment"));
        district.jobs = 1;
        district.stocked_shops = 0;
        assert!(!district.opening_uses_met("employment"));
        district.stocked_shops = 1;
        district.residents = 1;
        district.operating_blocks.insert(1, 2);
        district.operating_blocks.insert(2, 1);
        assert!(!district.opening_uses_met("west"));
        district.operating_blocks.insert(2, 2);
        assert!(district.opening_uses_met("west"));
    }

    #[test]
    fn cm2_publish_traffic_allows_signal_wait_and_rejects_persistent_episodes() {
        assert!(traffic_clear(1, 7 * TICKS_PER_HOUR / 60, 0, 0));
        assert!(!traffic_clear(1, 0, 20 * TICKS_PER_HOUR / 60, 0));
        assert!(traffic_clear(1, 120 * TICKS_PER_MINUTE, 0, 0));
        assert!(!traffic_clear(1, 120 * TICKS_PER_MINUTE + 1, 0, 0));
        assert!(!traffic_clear(1, 0, 0, 1));
        assert!(!traffic_clear(0, 0, 0, 0));
    }

    #[test]
    fn cm2_stationary_vehicle_keeps_permissive_time_across_a_red() {
        use simulation::transportation::spawn_driving_vehicle;

        let (mut sim, approach, _) = super::super::spacing::cm0_boundary_fixture();
        let junction = sim.map().lanes()[approach].dst;
        sim.write::<Map>()
            .update_intersection(junction, |junction| junction.light_policy = LightPolicy::Lights);
        let (pos, dir, green, red) = {
            let map = sim.map();
            let lane = &map.lanes()[approach];
            let (pos, dir) = lane.points.point_dir_along(lane.points.length() - 3.5);
            let green = (0..3600).find(|&seconds|
                matches!(lane.control.get_behavior(seconds), TrafficBehavior::GREEN)
            ).unwrap();
            let red = (0..3600).find(|&seconds|
                matches!(lane.control.get_behavior(seconds), TrafficBehavior::RED)
            ).unwrap();
            (pos, dir, green, red)
        };
        spawn_driving_vehicle(&mut sim, VehicleKind::Car, Transform::new_dir(pos, dir)).unwrap();
        let mut traffic = Traffic::default();
        for (sample, seconds) in [green, green, red, green].into_iter().enumerate() {
            sim.write::<GameTime>().seconds = seconds;
            traffic.observe(&sim, sample as u64 * SAMPLE_TICKS);
        }
        assert_eq!(traffic.max_standing_ticks, 2 * SAMPLE_TICKS);
    }

    #[test]
    fn cm2_egress_continuity_uses_permissive_phase_and_crossing_progress() {
        let lane = LaneID::default();
        let empty = BTreeSet::new();
        let minute = TICKS_PER_MINUTE;
        let mut traffic = Traffic::default();

        traffic.observe_egresses(0, BTreeMap::from([(lane, false)]), &empty);
        traffic.observe_egresses(7 * minute, BTreeMap::from([(lane, false)]), &empty);
        assert_eq!(traffic.max_blocking_ticks, 0);
        // A different queued vehicle or turn keeps the same egress occupied.
        traffic.observe_egresses(8 * minute, BTreeMap::from([(lane, true)]), &empty);
        traffic.observe_egresses(20 * minute, BTreeMap::from([(lane, false)]), &empty);
        assert_eq!(traffic.max_blocking_ticks, 20 * minute);
        traffic.observe_egresses(21 * minute, BTreeMap::from([(lane, true)]), &BTreeSet::from([lane]));
        traffic.observe_egresses(29 * minute, BTreeMap::from([(lane, true)]), &empty);
        assert_eq!(traffic.max_blocking_ticks, 20 * minute);
        assert_eq!(traffic.blocked_egresses[&lane].start, 21 * minute);
    }

    #[test]
    fn cm2_live_queue_keeps_blockage_when_the_approach_vehicle_changes() {
        use simulation::transportation::spawn_driving_vehicle;

        let (mut sim, approach, _) = super::super::spacing::cm0_boundary_fixture();
        let egress = {
            let map = sim.map();
            let lane = &map.lanes()[approach];
            let egress = map.intersections()[lane.dst].turns_from(approach)
                .map(|(turn, _)| turn.dst)
                .find(|&id| map.lanes()[id].kind == LaneKind::Driving
                    && map.lanes()[id].points.length() > 40.0)
                .unwrap();
            egress
        };
        let spawn = |sim: &mut Simulation, lane, from_end: Option<f32>| {
            let (pos, dir, route) = {
                let map = sim.map();
                let points = &map.lanes()[lane].points;
                let offset = from_end.map_or(9.0, |offset| points.length() - offset);
                let (pos, dir) = points.point_dir_along(offset);
                let route = Itinerary::route_to_lane(Tick(sim.get_tick()), pos, &map, egress, 38.0)
                    .unwrap();
                (pos, dir, route)
            };
            let id = spawn_driving_vehicle(sim, VehicleKind::Car, Transform::new_dir(pos, dir)).unwrap();
            sim.world_mut_unchecked().vehicles[id].it = route;
            id
        };
        let blocker = spawn(&mut sim, egress, None);
        let first = spawn(&mut sim, approach, Some(3.5));
        let second = spawn(&mut sim, approach, Some(13.0));
        let mut traffic = Traffic::default();
        let minute = TICKS_PER_MINUTE;
        traffic.observe(&sim, 0);
        traffic.observe(&sim, 10 * minute);
        assert_eq!(traffic.max_blocking_ticks, 10 * minute);
        {
            let vehicles = &mut sim.world_mut_unchecked().vehicles;
            vehicles[first].vehicle.state = VehicleState::ParkedAtDepot;
            vehicles[second].trans.pos = vehicles[first].trans.pos;
        }
        traffic.observe(&sim, 20 * minute);
        assert_eq!(traffic.max_blocking_ticks, 20 * minute);

        // One car clears the turn entirely between traffic samples while
        // another takes its place in the upstream conflict.
        let replacement = spawn(&mut sim, approach, Some(25.0));
        {
            let vehicles = &mut sim.world_mut_unchecked().vehicles;
            vehicles[replacement].trans.pos = vehicles[second].trans.pos;
        }
        let map = (*sim.map()).clone();
        let car = &mut sim.world_mut_unchecked().vehicles[second];
        car.it = Itinerary::route_to_lane(Tick(0), car.trans.pos, &map, egress, 38.0).unwrap();
        let start = car.trans.pos;
        car.trans.pos = car.it.update(start, 100.0, Tick(0), 0, &map);
        assert_eq!(car.it.get_travers().unwrap().kind, TraverseKind::Lane(egress));
        assert!(map.lanes()[egress].points.first().distance2(car.trans.pos) > 30.0_f32.powi(2));
        car.speed.0 = 1.0;
        traffic.probe_egresses(&sim);
        traffic.observe(&sim, 21 * minute);
        assert_eq!(traffic.blocked_egresses[&egress].start, 21 * minute);

        sim.world_mut_unchecked().vehicles[blocker].vehicle.state = VehicleState::ParkedAtDepot;
        sim.world_mut_unchecked().vehicles[replacement].vehicle.state = VehicleState::ParkedAtDepot;
        traffic.observe(&sim, 22 * minute);
        assert!(traffic.blocked_egresses.is_empty());
    }

    #[test]
    fn cm2_gate_budget_uses_only_complete_days_and_recurring_entries() {
        use BudgetCategory::*;
        let day = TICKS_PER_BUDGET_DAY;
        assert_eq!(closed_window(day + 1, 4 * day + 1), 2..4);
        assert_eq!(closed_window(day, 4 * day), 1..4);
        assert_eq!(
            recurring_balance(&BTreeMap::from([
                (MilestoneGrant, 1000),
                (StarterInfrastructure, 500),
                (Construction, -500),
                (LandPurchase, -250),
                (TradeExport, 25),
                (TradeImport, -50),
                (TransitFares, 10),
                (RoadUpkeep, -15),
                (SpecialBuildingUpkeep, -10),
                (ServiceUpkeep, -5),
                (TransitUpkeep, -5),
            ])),
            -50
        );
    }
    #[test]
    fn cm2_gate_protected_frontage_rejects_zone_leakage() {
        let lot = LotID::default();
        let (_, commercial) = downtown_cells();
        assert!(audit_votes([(lot, None)].into_iter(), &BTreeMap::new()).is_ok());
        assert!(audit_votes([(lot, Some(commercial))].into_iter(), &BTreeMap::new()).is_err());
        let approved = BTreeMap::from([(lot, commercial)]);
        assert!(audit_votes([(lot, Some(commercial))].into_iter(), &approved).is_ok());
        assert!(audit_votes([(lot, None)].into_iter(), &approved).is_err());
    }

    #[test]
    fn cm2_gate_bus_watchdog_skip_breaks_the_arrival_sequence() {
        let mut bus = (0, None, 0);
        let mut loops = 0;
        track_arrival(&mut bus, 0, Some(Tick(10)), false, 3, &mut loops);
        track_arrival(&mut bus, 1, Some(Tick(20)), false, 3, &mut loops);
        track_arrival(&mut bus, 0, None, true, 3, &mut loops);
        track_arrival(&mut bus, 0, Some(Tick(40)), false, 3, &mut loops);
        assert_eq!(loops, 0);
        track_arrival(&mut bus, 1, Some(Tick(50)), false, 3, &mut loops);
        track_arrival(&mut bus, 2, Some(Tick(60)), false, 3, &mut loops);
        track_arrival(&mut bus, 0, Some(Tick(70)), false, 3, &mut loops);
        track_arrival(&mut bus, 0, Some(Tick(70)), false, 3, &mut loops);
        assert_eq!(loops, 1);
    }
}
