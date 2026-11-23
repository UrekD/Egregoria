local function warehouse(name, label, asset, size, capacity, low, high, trucks, workers, price)
    return {
        type = "goods-company",
        name = name,
        label = label,
        kind = "factory",
        size = size,
        bgen = {kind = "centered_door", vertical_factor = 0.7},
        asset = asset,
        n_workers = workers,
        n_trucks = trucks,
        price = price,
        cargo_capacity_units = 16,
        dispatch_min_units = 8,
        storage = {
            capacity_units = capacity,
            -- Household items stay listed for saves whose warehouses hold them. Road freight
            -- skips them: household shelves restock through the retail lane.
            accepted_items = {"goods", "retail-goods", "groceries", "coal", "flour", "cereal",
                              "meat", "vegetable"},
            low_water_units = low,
            high_water_units = high,
            default_policy = "balanced",
        },
    }
end

-- Warehouses render as lettered placeholder boxes sized to their footprint,
-- so the drawn building never overhangs the road.
data:extend {
    warehouse("small-depot", "Small depot", "assets/sprites/small_depot.png", {40, 40}, 64, 24, 48, 2, 4, 2500),
    warehouse("large-warehouse", "Large warehouse", "assets/sprites/large_warehouse.png", {80, 60}, 256, 96, 224, 4, 10, 8000),
}
