module type Shape = {
  let area: float => float
}

module Utils = {
  let square = (x: float) => x *. x
  let cube = x => square(x) *. x
}

module Alias = Belt.Array

module MakeCounter = (Config: Shape) => {
  let next = n => n + 1
}

module Circle: Shape = {
  let area = r => Utils.square(r) *. 3.14
}

let process = (input: Shapes.box<string>): Shapes.box<string> => {
  let c = Shapes.add(1, 2)
  input->ignore
  Js.log(c)
  input
}
