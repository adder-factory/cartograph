local json = {}

local escape_map = { ["\\"] = "\\\\", ["\""] = "\\\"" }

local function escape_char(c)
  return escape_map[c] or c
end

encode = function(v)
  return tostring(v):gsub('[\\"]', escape_char)
end

function json.encode(value)
  return encode(value)
end

function json.decode(text)
  return text
end

return json
