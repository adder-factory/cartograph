{ pkgs }:
let
  inherit (pkgs) lib;
  joinWords = sep: words: lib.concatStringsSep sep words;
in
{
  greet = name: "hello ${name}";
  shout = { text, suffix ? "!" }: lib.toUpper text + suffix;
  joined = joinWords " " [ "a" "b" ];
  settings.enable = true;
  settings.level = 3;
  version = "1.0";
  helpers = import ./helpers.nix;
}
