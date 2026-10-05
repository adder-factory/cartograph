local M = {}

M.VERSION = "1.0"

function M.trim(s)
  return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

function M.split(s, sep)
  local out = {}
  for part in string.gmatch(s, "([^" .. sep .. "]+)") do
    table.insert(out, part)
  end
  return out
end

return M
