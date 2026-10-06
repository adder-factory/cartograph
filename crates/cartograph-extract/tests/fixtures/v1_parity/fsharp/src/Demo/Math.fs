namespace Demo

open System
open System.Collections.Generic

module Math =
  let add x y = x + y

  let square x = x * x

  let sumSquares xs = xs |> List.map square |> List.sum

  type Person = { Name: string; Age: int }

  type Color =
    | Red = 0
    | Green = 1
    | Blue = 2

  type Shape =
    | Circle of float
    | Rect of float * float

  let pi = 3.14159

  let area shape =
    match shape with
    | Circle r -> pi * square r
    | Rect (w, h) -> w * h
