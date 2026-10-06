module type STORE = sig
  type key
  val find : key -> string option
  val mem : key -> bool
end

module Registry : sig
  val register : string -> string -> unit
  val lookup : string -> string option
end

module MakeStore (K : Map.OrderedType) : STORE with type key = K.t

class counter : int -> object
  method incr : unit
  method get : int
end

class type printable = object
  method print : unit
end

val make_counter : unit -> int
val require : string -> string
