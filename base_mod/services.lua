local function service(name, label, service_kind, asset, radius_m, capacity, n_workers, upkeep_per_day)
    return {
        type = "service",
        name = name,
        label = label,
        service_kind = service_kind,
        kind = "store",
        n_trucks = 0,
        n_workers = n_workers,
        radius_m = radius_m,
        capacity = capacity,
        upkeep_per_day = upkeep_per_day,
        bgen = {kind = "centered_door", vertical_factor = 1.0},
        size = 20,
        asset = asset,
        price = 1000,
        power_consumption = "1kW",
    }
end

local pump = service("water-pump", "Water Pump", "water_pump", "assets/sprites/oil_pump.png", 1000, 2000, 4, 30)
pump.water_production = 2000

local outlet = service("sewage-outlet", "Sewage Outlet", "sewage_outlet", "assets/sprites/textile_processing_facility.png", 1000, 2000, 4, 30)
outlet.power_consumption = nil
outlet.sewage_capacity = 2000

data:extend {
    service("clinic", "Clinic", "health", "assets/sprites/hightech_store.png", 1800, 400, 8, 45),
    service("fire-station", "Fire Station", "fire", "bakery.glb", 1800, 400, 8, 45),
    service("police-station", "Police Station", "police", "assets/sprites/supermarket.png", 1800, 400, 8, 45),
    service("school", "School", "school", "cinema.glb", 1500, 300, 6, 45),
    service("garbage-depot", "Garbage Depot", "garbage", "assets/sprites/woodmill.png", 1800, 400, 6, 35),
    service("park", "Park", "park", "assets/sprites/horticulturalist.png", 1200, 200, 2, 25),
    pump,
    outlet,
}
