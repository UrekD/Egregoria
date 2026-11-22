local function warehouse(name, label, size, capacity, low, high, trucks, workers, price)
    return {
        type = "goods-company",
        name = name,
        label = label,
        kind = "factory",
        size = size,
        bgen = {kind = "centered_door", vertical_factor = 0.7},
        asset = "flour_factory.glb",
        n_workers = workers,
        n_trucks = trucks,
        price = price,
        cargo_capacity_units = 16,
        dispatch_min_units = 8,
        storage = {
            capacity_units = capacity,
            accepted_items = {"goods", "retail-goods", "groceries", "coal", "flour", "cereal",
                              "meat", "vegetable"},
            low_water_units = low,
            high_water_units = high,
            default_policy = "balanced",
        },
    }
end

data:extend {
    warehouse("small-depot", "Small depot", {40, 40}, 64, 24, 48, 2, 4, 2500),
    warehouse("large-warehouse", "Large warehouse", {80, 60}, 256, 96, 224, 4, 10, 8000),
}
