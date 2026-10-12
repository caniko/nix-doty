{
  lib,
  config,
  pkgs,
  ...
}: let
  cfg = config.services.doty;
  policy = import ./policy.nix {inherit lib;};
  targetsJson = pkgs.writeText "doty-targets.json" (builtins.toJSON (policy.document "system" cfg));
in {
  options.services.doty = {
    enable = lib.mkEnableOption "doty system cleanup orchestrator";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Doty package to use.";
    };
    schedule = lib.mkOption {
      type = lib.types.str;
      default = "weekly";
      description = "Systemd OnCalendar schedule for system cleanup.";
    };
    inspectionRequirements = policy.requirementsOption;
    targets = policy.targetsOption;
  };
  config = lib.mkIf cfg.enable {
    assertions = policy.assertions "services.doty" cfg;
    environment.systemPackages = [cfg.package];
    environment.etc."doty/targets.json".source = targetsJson;
    systemd.services.doty-all = {
      description = "doty: run configured system cleanup targets";
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${lib.getExe cfg.package} --mode system run --apply";
        User = "root";
      };
    };
    systemd.timers.doty-all = {
      description = "doty: scheduled system cleanup";
      wantedBy = ["timers.target"];
      timerConfig = {
        OnCalendar = cfg.schedule;
        Persistent = true;
      };
    };
  };
}
