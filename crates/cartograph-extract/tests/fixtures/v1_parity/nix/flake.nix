{
  description = "demo flake";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      packages.${system}.default = import ./default.nix { inherit pkgs; };
      devShells.${system}.default = pkgs.mkShell { buildInputs = [ pkgs.hello ]; };
    };
}
