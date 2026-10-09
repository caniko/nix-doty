{
  description = "Do That Yourself: a NixOS-driven cleanup orchestrator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
    harbor.url = "git+https://github.com/caniko/harbor.git?ref=feat/harbor-monorepo-components&rev=7d99eb50c52d0a941e2996b97c469b32a7657ef4";
    treefmt-nix.follows = "harbor/treefmt-nix";
  };

  outputs = inputs @ {
    self,
    nixpkgs,
    flake-parts,
    rust-overlay,
    harbor,
    treefmt-nix,
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
        pkgs =
          import (
            if system == "x86_64-darwin"
            then harbor.inputs.nixpkgs-darwin
            else nixpkgs
          ) {
            inherit system;
            overlays = [(import rust-overlay)];
          };
        inherit (pkgs) lib;

        toolchain = harbor.lib.rust.mkToolchain {
          inherit pkgs;
          toolchainProfile = "nightly";
        };
        inherit (toolchain) craneLib;
        treefmt = treefmt-nix.lib.evalModule pkgs {
          projectRootFile = "flake.nix";
          imports = [
            harbor.treefmtModules.core-nix
            harbor.treefmtModules.core-toml
            harbor.treefmtModules.rust-rust
          ];
          programs.rustfmt = {
            package = toolchain.rustToolchain;
            edition = "2024";
          };
        };
        cross = harbor.lib.rust.mkCross {
          inherit pkgs system;
          enableOsxcross = false;
        };

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          nativeBuildInputs = [pkgs.gitMinimal] ++ lib.optionals pkgs.stdenv.isLinux [pkgs.util-linux];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        defaultPackage = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [pkgs.makeWrapper];
            postInstall = ''
              wrapProgram "$out/bin/doty" \
                --prefix PATH : ${lib.makeBinPath commonArgs.nativeBuildInputs}
            '';
            meta = {
              mainProgram = "doty";
              description = "Do That Yourself: NixOS cleanup orchestrator";
              license = lib.licenses.mit;
              maintainers = ["caniko"];
            };
          });

        crossPackageSet = harbor.lib.rust.mkCrossPackages {
          inherit pkgs cross commonArgs craneLib;
          pname = "doty";
          targets = ["native" "aarch64-linux"];
        };
      in {
        packages = {
          default = defaultPackage;
          doty = defaultPackage;
          "doty-aarch64-linux" = crossPackageSet."doty-aarch64-linux";
        };

        checks = {
          formatting = treefmt.config.build.check self;
          clippy = craneLib.cargoClippy (commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "-- -D warnings";
            });
        };

        devShells.default = craneLib.devShell {
          packages = [pkgs.nh treefmt.config.build.wrapper];
        };
        formatter = treefmt.config.build.wrapper;
      };

      flake = {
        # Consumers can gate target configuration while upgrading an older pin.
        lib.persistentRetention = true;
        lib.managedModels = true;
        lib.persistentStorage = true;
        lib.inspectionCoverage = true;
        lib.scratchAssessmentVersion = 1;
        lib.nixOperationVersion = 1;
        lib.goalDrivenReclaimVersion = 1;
        crossPackages."x86_64-linux"."aarch64-linux".doty = self.packages."x86_64-linux"."doty-aarch64-linux";
        nixosModules.default = {pkgs, ...}: {
          imports = [./module/default.nix];
          services.doty.package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      };
    };
}
