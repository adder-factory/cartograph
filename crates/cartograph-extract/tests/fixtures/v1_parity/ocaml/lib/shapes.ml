(** Geometry primitives. *)
open Printf

(** A 2D point record. *)
type point = { x : float; y : float }

(** Shapes as a variant. *)
type shape =
  | Circle of float
  | Rect of float * float
  | Poly of point list

type color = [ `Red | `Green | `Blue ]

type 'a tagged = Tagged of string * 'a

(** Square a number. *)
let square x = x *. x

let area = function
  | Circle r -> 3.14159 *. square r
  | Rect (w, h) -> w *. h
  | Poly _ -> 0.0

let describe s = sprintf "area=%f" (area s)

let total shapes = List.fold_left (fun acc s -> acc +. area s) 0.0 shapes

let ( +: ) a b = { x = a.x +. b.x; y = a.y +. b.y }

let origin = { x = 0.0; y = 0.0 }

let shift p = p +: origin

external c_sqrt : float -> float = "caml_sqrt_float"

let print_all shapes = shapes |> List.map describe |> List.iter print_endline

let log_area s = print_endline @@ describe s
