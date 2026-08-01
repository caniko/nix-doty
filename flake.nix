{
  description = "Do That Yourself: a NixOS-driven cleanup orchestrator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rs-harbor.url = "github:caniko/rs-harbor/e2778ff3beca1bd4c1f5183313251d1fb5b46dd6";
  };

  outputs = inputs @ {
    self,
    nixpkgs,
    crane,
    flake-parts,
    rust-overlay,
    rs-harbor,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];

      perSystem = {system, ...}: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        inherit (pkgs) lib;

        toolchain = rs-harbor.lib.mkToolchain { inherit pkgs; toolchainProfile = "nightly"; };
        craneLib = toolchain.craneLib;
        atticAdapter = rs-harbor.lib.mkAdapter {
          attic = {
            endpoint = "https://attic.candee.baby";
            cache = "canix";
          };
        };
        cross = rs-harbor.lib.mkCross {
          inherit pkgs system;
          enableOsxcross = false;
        };

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        defaultPackage = craneLib.buildPackage (commonArgs
            // {
              inherit cargoArtifacts;
              meta = {
                mainProgram = "doty";
                description = "Do That Yourself: NixOS cleanup orchestrator";
                license = lib.licenses.mit;
                maintainers = ["caniko"];
              };
            });

        crossPackageSet = rs-harbor.lib.mkCrossPackages {
          inherit pkgs craneLib cross commonArgs;
          pname = "doty";
          targets = ["native" "aarch64-linux"];
        };
      in {
        packages = {
          default = defaultPackage;
          doty = defaultPackage;
          "doty-aarch64-linux" = crossPackageSet."doty-aarch64-linux";
        };

        apps.push-flake-inputs = rs-harbor.lib.mkAtticPush {
          inherit pkgs;
          adapter = atticAdapter;
          flake = ".";
        };

        checks = {
          clippy = craneLib.cargoClippy (commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "-- -D warnings";
            });
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [nh];
        };
      };

      flake = {
        crossPackages."x86_64-linux"."aarch64-linux".doty = self.packages."x86_64-linux"."doty-aarch64-linux";
        nixosModules.default = {pkgs, ...}: {
          imports = [./module/default.nix];
          services.doty.package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      };
    };
}
