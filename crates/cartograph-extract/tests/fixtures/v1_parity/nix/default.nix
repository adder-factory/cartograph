{ pkgs ? import <nixpkgs> {} }:
let
  helper = x: builtins.toString x;
  localValue = helper 1;
  utils = import ./lib/utils.nix { inherit pkgs; };
  hello = pkgs.callPackage ./pkgs/hello { };
in rec {
  package = pkgs.stdenv.mkDerivation {
    name = helper 1;
    src = ./.;
    buildInputs = [ hello ];
  };
  inherit (pkgs) lib;
  inherit localValue;
  greeting = utils.greet "world";
  meta.description = "demo package";
  "quoted-name" = 1;
}
