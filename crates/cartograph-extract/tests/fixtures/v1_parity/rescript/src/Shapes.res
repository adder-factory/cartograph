// Shapes and helpers.
open Belt
include Js.Promise

/** A colour variant. */
type color = Red | Green | Blue

type person = {name: string, age: int}

type id = string

type box<'a> = {value: 'a}

exception NotFound
exception Invalid(string)

@module("fs") external readFile: (string, string) => string = "readFileSync"

external parseInt: string => int = "parseInt"

let maxSize = 10

let total: int = 42

let add = (a: int, b: int): int => a + b

let double = x => add(x, x)

let describe = (p: person): string => p.name ++ " " ++ Int.toString(p.age)

let colorName = (c: color) =>
  switch c {
  | Red => "red"
  | Green => "green"
  | Blue => "blue"
  }

let run = () => {
  let x = 1
  x->add(1)->double
}

let fetchData = async (url: string): promise<string> => await readFileAsync(url)

let readFileAsync = (path: string) => Js.Promise.resolve(path)
