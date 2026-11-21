local function residence(density, level, capacity, power, lot_size_min)
    return {
        type = "residence",
        name = "residence-" .. density .. "-l" .. level,
        label = "Residence (" .. density .. ", level " .. level .. ")",
        density = density,
        level = level,
        capacity = capacity,
        power_consumption = power,
        lot_size_min = lot_size_min,
        bgen = "house",
        size = 20,
        asset = "assets/ui/housebrush.png",
        price = 0,
    }
end

data:extend {
    residence("low", 1, 2, "200W", 300),
    residence("low", 2, 4, "400W", 300),
    residence("low", 3, 6, "600W", 300),
    residence("high", 1, 4, "400W", 400),
    residence("high", 2, 8, "800W", 400),
    residence("high", 3, 12, "1.2kW", 400),
}
