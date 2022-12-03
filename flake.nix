{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    cargo2nix = {
      url = "github:cargo2nix/cargo2nix/release-0.11.0";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.follows = "cargo2nix/flake-utils";

  };

  outputs = inputs: with inputs;
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [cargo2nix.overlays.default];
        };

        rustPkgs = pkgs.rustBuilder.makePackageSet {
          rustVersion = "1.61.0";
          packageFun = import ./Cargo.nix;
        };

        samply = (rustPkgs.workspace.samply {}).bin;

      in rec {
        packages = {
          # replace hello-world with your package name
          inherit samply;
          default = samply;
        };
        overlays= final: prev: {
          inherit samply;
        };
      }
    );
}