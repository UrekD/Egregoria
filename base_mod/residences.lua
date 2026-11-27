local function residences(density, capacities, lot_size_min, massing, mixed, legacy_name)
    local prefix = mixed and "residence-mixed-"
        or legacy_name and "residence-legacy-"
        or "residence-"
    local label = mixed and "Mixed-use residence" or "Residence"
    for level, capacity in ipairs(capacities) do
        local parking
        if massing then
            if density == "high" then
                parking = {kind = "garage", spaces = 8 + 4 * (level - 1)}
            elseif level >= 4 then
                parking = {kind = "garage", spaces = 4 * (level - 3)}
                if density == "low" then parking.fallback_surface_spaces = 1 end
            elseif density == "low" then
                parking = {kind = "surface", spaces = 1}
            end
        end
        data:extend {
            type = "residence",
            name = prefix .. density .. "-l" .. level,
            label = label .. " (" .. density .. ", level " .. level .. ")",
            order = legacy_name and "base-legacy-residence" or nil,
            zone_kind = mixed and "mixed-use" or nil,
            density = density,
            level = level,
            capacity = capacity,
            power_consumption = capacity * 100,
            water_consumption = capacity,
            lot_size_min = lot_size_min,
            massing = massing,
            parking = parking,
            bgen = "house",
            size = 20,
            asset = "assets/ui/housebrush.png",
            price = 0,
        }
    end
end

local function legacy_residences(legacy_name)
    residences("low", {2, 4, 6}, 300, nil, nil, legacy_name)
    residences("medium", {3, 6, 9}, 400, nil, nil, legacy_name)
    residences("high", {4, 8, 12}, 400, nil, nil, legacy_name)
end

-- Profiled zoning and legacy manual residences share the base registry.
if zones2_profiles_enabled == true then
    local low = {
        style = "house",
        floors_by_level = {1, 2, 2, 2, 2},
        max_floors = 2,
        min_plot_width_m = 18,
        min_plot_depth_m = 18,
    }
    local medium = {
        style = "block",
        floors_by_level = {4, 5, 6, 7, 8},
        max_floors = 10,
        min_plot_width_m = 20,
        min_plot_depth_m = 20,
    }
    local high = {
        style = "tower",
        floors_by_level = {12, 15, 18, 21, 24},
        max_floors = 28,
        min_plot_width_m = 20,
        min_plot_depth_m = 20,
    }

    residences("low", {2, 4, 6, 8, 10}, 300, low)
    residences("medium", {5, 7, 9, 11, 13}, 400, medium)
    residences("high", {12, 15, 18, 21, 24}, 400, high)
    residences("medium", {4, 6, 8, 10, 12}, 400, medium, true)
    residences("high", {11, 14, 17, 20, 23}, 400, high, true)
    legacy_residences(true)
else
    legacy_residences(false)
end
