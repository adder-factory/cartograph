--!strict
export type User = {
  name: string,
  level: number,
}

export type Callback = (User) -> ()

type Priv = { hidden: boolean }

local Types = {}

function Types.isUser(value: any): boolean
  return type(value) == "table" and value.name ~= nil
end

return Types
