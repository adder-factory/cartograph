import Demo.Basic
import Std.Data.HashMap

namespace Demo

abbrev Score := Nat

def describe (r : Role) : String :=
  match r with
  | Role.admin => "admin"
  | Role.user => "user"
  | Role.guest => "guest"

def main : IO Unit := do
  let u : User := { name := "ana", age := 3 }
  IO.println (greet u)
  IO.println (toString (double 2))

theorem describe_admin : describe Role.admin = "admin" := rfl

end Demo
