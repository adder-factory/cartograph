open Shapes

let shapes = [ Circle 1.0; Rect (2.0, 3.0) ]

let run () =
  Registry.Registry.register "main" "ok";
  print_all shapes;
  log_area (Circle 2.0);
  let n = Registry.make_counter () in
  Printf.printf "%d %f\n" n (total shapes)

let () = run ()
