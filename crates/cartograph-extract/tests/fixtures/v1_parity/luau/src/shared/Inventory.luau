--!strict
local Types = require(script.Parent.Types)
local Signal = require("Signal")

export type Item = { id: number, label: string }

local Inventory = {}
Inventory.__index = Inventory

local MAX_SLOTS: number = 20
local count, onChange = 0, function(n: number) notify(n) end

function Inventory.new(owner: Types.User): Inventory
  local self = setmetatable({}, Inventory)
  self.owner = owner
  self.items = {} :: { Item }
  return self
end

function Inventory:add(item: Item): boolean
  if #self.items >= MAX_SLOTS then
    return false
  end
  table.insert(self.items, item)
  self:changed()
  return true
end

function Inventory:changed()
  notify(#self.items)
end

local function format(item: Item): string
  return item.label .. "#" .. tostring(item.id)
end

function notify(n: number)
  print(format({ id = n, label = "slot" }))
end

render = function(inv: any): string
  return tostring(inv)
end

return Inventory
