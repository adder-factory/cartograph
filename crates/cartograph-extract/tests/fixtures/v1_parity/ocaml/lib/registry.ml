(** A registry module with a signature and a functor. *)
module type STORE = sig
  type key
  val find : key -> string option
end

module StringMap = Map.Make (String)

module Registry = struct
  let table : string StringMap.t ref = ref StringMap.empty

  let register name value = table := StringMap.add name value !table

  let lookup name = StringMap.find_opt name !table
end

module MakeStore (K : Map.OrderedType) : STORE with type key = K.t = struct
  type key = K.t
  let find _ = None
end

class counter init =
  object (self)
    val mutable count = init
    method incr = count <- count + 1
    method get = count
    method reset = self#set 0
    method set n = count <- n
  end

class type printable = object
  method print : unit
end

let make_counter () =
  let c = new counter 0 in
  c#incr;
  c#get

exception Not_registered of string

let require name =
  match Registry.lookup name with
  | Some v -> v
  | None -> raise (Not_registered name)
