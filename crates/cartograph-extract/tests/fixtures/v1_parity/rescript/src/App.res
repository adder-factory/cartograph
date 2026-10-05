open Shapes

@react.component
let make = (~name: string) => {
  let label = describe({name, age: 3})
  <div> {React.string(label)} </div>
}

let main = () => {
  let n = run()
  Utils.Utils.cube(2.0)->ignore
  Js.log2(n, colorName(Red))
}
