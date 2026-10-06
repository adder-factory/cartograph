local Inventory = require("shared/Inventory")
local Types = require("shared/Types")

local function start(user: Types.User)
  local inv = Inventory.new(user)
  inv:add({ id = 1, label = "sword" })
  notify(1)
  if Types.isUser(user) then
    render(inv)
  end
end

start({ name = "ada", level = 1 })
