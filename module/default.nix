{ lib, config, pkgs, ... }:

let
  cfg = config.services.doty;
  targetEntries =
    lib.flatten (
      lib.mapAttrsToList (
        name: t:
          map (v: {
            inherit name;
            variant = v.variant;
            settings = v.settings or {};
          }) t.variants
      ) (lib.filterAttrs (n: t: t.enable) cfg.targets)
    );
  targetsJson = pkgs.writeText "doty-targets.json" (builtins.toJSON {
    targets = targetEntries;
  });
in {
  options.services.doty = {
    enable = lib.mkEnableOption "doty cleanup orchestrator";
    package = lib.mkOption {
      type = lib.types.package;
      description = "doty package to use";
    };
    schedule = lib.mkOption {
      type = lib.types.str;
      default = "weekly";
      example = "daily";
      description = "Systemd OnCalendar schedule for combined cleanup timer";
    };
    targets = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options = {
          enable = lib.mkEnableOption "this doty cleanup target";
          variants = lib.mkOption {
            type = lib.types.listOf (lib.types.submodule {
              options = {
                variant = lib.mkOption {
                  type = lib.types.str;
                  description = "Cleanup variant to use (e.g. nh-clean, size-cap)";
                };
                settings = lib.mkOption {
                  type = lib.types.attrsOf lib.types.anything;
                  default = {};
                  description = "Variant-specific configuration";
                };
              };
            });
            default = [];
            description = "Multiple cleanup variants to configure for this framework";
          };
        };
      });
      default = {};
      description = "Doty cleanup targets to enable on this host";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = lib.mapAttrsToList (name: t: {
      assertion = t.variants != [];
      message = "services.doty.targets.${name} must set variants";
    }) (lib.filterAttrs (n: t: t.enable) cfg.targets);

    environment.systemPackages = [ cfg.package ];
    environment.etc."doty/targets.json".source = targetsJson;

    systemd.services.doty-all = {
      description = "doty: run all configured cleanup targets";
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${lib.getExe cfg.package} run --apply";
        User = "root";
      };
    };

    systemd.timers.doty-all = {
      description = "doty: weekly cleanup of all targets";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.schedule;
        Persistent = true;
      };
    };
  };
}
