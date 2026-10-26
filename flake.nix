{
  description = "Do That Yourself: a NixOS-driven cleanup orchestrator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
    harbor = {
      url = "git+https://github.com/caniko/harbor.git?ref=feat/harbor-monorepo-components&rev=7d99eb50c52d0a941e2996b97c469b32a7657ef4";
      # harbor's simit/plinth lock still names retired Codeberg remotes, one of
      # them SSH-only, which hosted runners cannot fetch during `nix flake
      # archive`. Repoint them to their GitHub sources at the same revisions.
      inputs.simit.inputs.plinth.inputs = {
        nix-pklx = {
          url = "git+https://github.com/caniko/nix-pklx.git?ref=trunk&rev=9d8c5dac62f4402cdfee015d3999f6caae6c1818";
          # Overriding the URL drops plinth's own follows; restore them.
          inputs.crane.follows = "harbor/simit/plinth/crane";
          inputs.flake-utils.follows = "harbor/simit/plinth/flake-utils";
          inputs.nixpkgs.follows = "harbor/simit/plinth/nixpkgs";
          inputs.rust-overlay.follows = "harbor/simit/plinth/rust-overlay";
          inputs.plinth.url = "git+https://github.com/caniko/plinth.git?ref=trunk&rev=316b96c0b6628ddb42b4bce80010cc6978508dce";
          inputs.rs-harbor = {
            url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=073c7e8ffdd4d9f0b5c6735ec81377d1dde6a145";
            inputs.meta-harbor.url = "git+https://github.com/caniko/harbor-meta.git?ref=trunk&rev=d189903b657f4d0dc4f47003005f1b9260c77e89";
            inputs.nix-opencode-lsp.url = "git+https://github.com/caniko/nix-opencode-lsp.git?ref=trunk&rev=a45718d96cdde0101ef8c20942fada0b188745f0";
            inputs.plinth.url = "git+https://github.com/caniko/plinth.git?ref=trunk&rev=bfa970d7f38f51515e01092cd7f9fde81eef26ae";
          };
        };
        rs-harbor = {
          url = "git+https://github.com/caniko/harbor-rs.git?ref=archive/codeberg-20260820/rs-harbor/agent/fix-dioxus-sccache-dispatch&rev=e5e74218c43e1e5e84fe57926d510b030e32e17e";
          inputs.meta-harbor.url = "git+https://github.com/caniko/harbor-meta.git?ref=trunk&rev=d189903b657f4d0dc4f47003005f1b9260c77e89";
          inputs.nix-opencode-lsp.url = "git+https://github.com/caniko/nix-opencode-lsp.git?ref=trunk&rev=a45718d96cdde0101ef8c20942fada0b188745f0";
          inputs.crane.follows = "harbor/simit/plinth/crane";
          inputs.nixpkgs.follows = "harbor/simit/plinth/nixpkgs";
          inputs.rust-overlay.follows = "harbor/simit/plinth/rust-overlay";
        };
      };
    };
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
          nativeBuildInputs = [pkgs.gitMinimal pkgs.coreutils] ++ lib.optionals pkgs.stdenv.isLinux [pkgs.util-linux];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        defaultPackage = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [pkgs.makeWrapper];
            postInstall = ''
              wrapProgram "$out/bin/doty" \
                --prefix PATH : ${lib.makeBinPath (commonArgs.nativeBuildInputs ++ [pkgs.uv])}
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

        # Simit prebuild publication on trunk (secret ATTIC_TOKEN). Linux
        # outputs only: the package canix consumes, pushed to the public
        # canix cache.
        apps = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          push-flake-inputs = harbor.lib.rust.mkAtticPush {
            inherit pkgs;
            flake = ".";
            paths = [defaultPackage];
            adapter = harbor.lib.rust.mkAdapter {
              attic = {
                endpoint = "https://attic.candee.baby";
                cache = "canix";
                tokenEnvVar = "ATTIC_TOKEN";
              };
            };
          };
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
        lib.executionScopeVersion = 1;
        homeManagerModules.default = {
          pkgs,
          lib,
          ...
        }: {
          imports = [./module/home.nix];
          programs.doty.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
        homeModules.default = self.homeManagerModules.default;
        crossPackages."x86_64-linux"."aarch64-linux".doty = self.packages."x86_64-linux"."doty-aarch64-linux";
        nixosModules.default = {pkgs, ...}: {
          imports = [./module/default.nix];
          services.doty.package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      };
    };
}
