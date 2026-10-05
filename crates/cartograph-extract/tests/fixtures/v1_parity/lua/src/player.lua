local json = require("json")
local strings = require "util.strings"
local config = require('config')
local inspect = require [[inspect]]

local Player = {}
Player.__index = Player

local MAX_HEALTH, DEFAULT_NAME = 100, "hero"
local a, b = 1, function(q) return qux(q) end
local helper_fn = function(x)
  return strings.trim(x)
end

function Player.new(name)
  local self = setmetatable({}, Player)
  self.name = name or DEFAULT_NAME
  self.health = MAX_HEALTH
  return self
end

function Player:damage(amount)
  self.health = self.health - amount
  if self.health <= 0 then
    self:die()
  end
end

function Player:die()
  print("dead: " .. self.name)
  log_event("death")
end

function Player:serialize()
  return json.encode({ name = self.name, health = self.health })
end

local function clamp(v, lo, hi)
  return math.max(lo, math.min(hi, v))
end

function log_event(name)
  return clamp(#name, 0, 10)
end

function qux(v)
  return helper_fn(v)
end

return Player
