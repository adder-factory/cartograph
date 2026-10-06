namespace Demo

module Math2 =
  let add x y = x + y
  type Person = { Name: string; Age: int }

module Program =
  let result = Math.add 1 2

  [<EntryPoint>]
  let main argv =
    let total = Math.sumSquares [ 1; 2; 3 ]
    Greeting.run ()
    printfn "%d %d" result total
    0
