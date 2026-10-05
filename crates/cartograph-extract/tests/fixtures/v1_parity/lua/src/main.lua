local Player = require("player")
local strings = require("util.strings")

local function run()
  local p = Player.new("ada")
  p:damage(10)
  local parts = strings.split("a,b", ",")
  print(p:serialize(), #parts)
  log_event("start")
end

run()
