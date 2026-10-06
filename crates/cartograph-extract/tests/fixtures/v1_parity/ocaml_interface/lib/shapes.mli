(** Public interface of the geometry library. *)

(** A 2D point. *)
type point = { x : float; y : float }

type shape =
  | Circle of float
  | Rect of float * float

type color = [ `Red | `Green ]

(** Abstract handle. *)
type t

val square : float -> float
(** Area of a shape. *)
val area : shape -> float
val describe : shape -> string
val ( +: ) : point -> point -> point

external c_sqrt : float -> float = "caml_sqrt_float"

exception Invalid_shape of string
