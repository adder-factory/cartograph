// ReScript test bed
type box<'a> = { value: 'a }

type container<'a> = { mutable items: array<box<'a>> }

let make = (): container<'a> => { items: [] }

let add = (c: container<'a>, item: box<'a>): unit => {
  c.items->Js.Array2.push(item)->ignore
}

let size = (c: container<'a>): int => Js.Array2.length(c.items)

let process = (input: box<string>): container<string> => {
  let c = make()
  add(c, input)
  c
}

let foo: int = 42
