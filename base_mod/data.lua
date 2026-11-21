require("items")
require("companies")
require("residences")
require("growables")
require("services")
require("leisure")
require("colors")
require("roadvehicles")
require("rollingstock")

for _, prototype in ipairs(data) do
    local kind = prototype.type
    if kind == "growable-company" or kind == "goods-company" or kind == "solar-panel" then
        prototype.water_consumption = prototype.water_consumption or math.max(1, prototype.n_workers or 0)
    end
end

data:extend {
    {
        type = "freight-station",
        name = "freight-station",
        label = "Freight Station",
        asset = "rail_freight_station.glb",
        price = 1000,
        size = {160, 200},
    }
}
