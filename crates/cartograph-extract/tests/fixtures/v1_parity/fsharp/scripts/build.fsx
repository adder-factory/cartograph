open System.IO

let outDir = Path.Combine(__SOURCE_DIRECTORY__, "out")

let clean dir =
  if Directory.Exists dir then Directory.Delete(dir, true)

let build () =
  clean outDir
  Directory.CreateDirectory outDir |> ignore

build ()
