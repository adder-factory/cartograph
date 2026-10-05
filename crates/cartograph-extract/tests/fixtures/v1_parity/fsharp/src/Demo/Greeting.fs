namespace Demo

open System
open Demo.Math

type IGreeter =
  abstract member Greet: string -> string

type Greeter(prefix: string) =
  member this.Greet name = prefix + name
  member _.Run value = add value 1
  static member Create() = Greeter("Hello, ")
  member val Count = 0 with get, set
  interface IGreeter with
    member this.Greet name = this.Greet name

module Greeting =
  let greeter = Greeter.Create()

  let greet (p: Person) = greeter.Greet p.Name

  let describe p =
    let label = greet p
    String.Format("{0} ({1})", label, p.Age)

  let run () =
    async { return 1 } |> ignore
    printfn "%s" (describe { Name = "Ana"; Age = 3 })
