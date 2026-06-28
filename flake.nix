{
  description = "Do That Yourself: a NixOS-driven cleanup orchestrator";

  nixConfig = {
    extra-substituters = ["https://attic.candee.baby/canix"];
    extra-trusted-public-keys = ["canix:e/lZjnNC0xQB6r0Q9n+i+CEvQqA/hDHZZ3EjtYbnEhI="];
  };

  inputs = {
    rs-harbor.url = "git+https://codeberg.org/caniko/rs-harbor.git";
    nixpkgs.follows = "rs-harbor/nixpkgs";
    rust-overlay.follows = "rs-harbor/rust-overlay";
    crane.follows = "rs-harbor/crane";
    flake-utils.follows = "rs-harbor/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      rs-harbor,
      ...
    }:
    (flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        inherit (pkgs) lib;

        toolchain = rs-harbor.lib.mkToolchain {
          inherit pkgs;
          channel = "stable";
        };
        inherit (toolchain) craneLib rustToolchain;
        cross = rs-harbor.lib.mkCross {inherit pkgs system;};
        cargoConfig = rs-harbor.lib.mkCargoConfig {
          inherit pkgs;
          channel = "stable";
        };

        commonArgs = {
          inherit (craneLib) cargoArtifacts;
          inherit cargoConfig;
          src = craneLib.cleanCargoSource ./.;
          doCheck = false;
        };

        cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
          pname = "doty-deps";
          cargoArtifacts = null;
        });
      in {
        packages = {
          default = craneLib.buildPackage (commonArgs // {
            pname = "doty";
            meta = {
              description = "Do That Yourself: NixOS cleanup orchestrator";
              license = lib.licenses.mit;
              maintainers = ["caniko"];
            };
          });

          doty = self.packages.${system}.default;
        };

        checks = {
          default = craneLib.cargoClippy (commonArgs // {
            pname = "doty-clippy";
            cargoClippyExtraArgs = "-- -D warnings";
          });

          clippy = self.checks.${system}.default;
        };

        devShells = rs-harbor.lib.mkDevShells {
          inherit pkgs craneLib cross cargoConfig;
          packages = with pkgs; [nh];
        };
      }
    ))
    // {
      nixosModules.default = import ./module/default.nix;
    };
}
