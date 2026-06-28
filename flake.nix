{
  description = "Do That Yourself: a NixOS-driven cleanup orchestrator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      ...
    }:
    (flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        inherit (pkgs) lib;

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = ["rust-src" "rustfmt" "clippy"];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in {
        packages = {
          default = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            meta = {
              description = "Do That Yourself: NixOS cleanup orchestrator";
              license = lib.licenses.mit;
              maintainers = ["caniko"];
            };
          });

          doty = self.packages.${system}.default;
        };

        checks = {
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "-- -D warnings";
          });
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [nh];
        };
      }
    ))
    // {
      nixosModules.default = { pkgs, ... }: {
        imports = [ ./module/default.nix ];
        services.doty.package = self.packages.${pkgs.system}.default;
      };
    };
}
