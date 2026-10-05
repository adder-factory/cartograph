import Mathlib.Data.Nat.Basic

/-- A user record. -/
structure User where
  name : String
  age : Nat

structure Point where
  x : Nat
  y : Nat
  deriving Repr

inductive Role where
  | admin
  | user
  | guest

inductive Tree (α : Type) where
  | leaf : Tree α
  | node : Tree α → α → Tree α → Tree α

def greet (u : User) : String := u.name
theorem id_eq (n : Nat) : n = n := rfl
abbrev UserName := String

def double (n : Nat) : Nat := n + n

def Point.add (p q : Point) : Point := { x := p.x + q.x, y := p.y + q.y }

theorem double_eq (n : Nat) : double n = n + n := rfl
