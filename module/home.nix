{
  config,
  lib,
  ...
}: let
  cfg = config.programs.doty;
  policy = import ./policy.nix {inherit lib;};
  rootArgs = lib.concatMapStringsSep " " (root: "--user-root ${lib.escapeShellArg root}") cfg.extraRoots;
in {
  options.programs.doty = {
    enable = lib.mkEnableOption "caller-local Doty housekeeping";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Doty package with executionScopeVersion = 1.";
    };
    targets = policy.targetsOption;
    inspectionRequirements = policy.requirementsOption;
    extraRoots = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Additional caller-owned roots admitted by the optional user timer.";
    };
    configFile = lib.mkOption {
      type = lib.types.str;
      readOnly = true;
      default = "${config.xdg.configHome}/doty/targets.json";
      defaultText = lib.literalExpression ''"\''${config.xdg.configHome}/doty/targets.json"'';
      description = "Generated user-mode policy document.";
    };
    timer = {
      enable = lib.mkEnableOption "scheduled Safe-tier user housekeeping";
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "weekly";
        description = "Systemd user timer OnCalendar schedule.";
      };
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = policy.assertions "programs.doty" cfg;
    home.packages = [cfg.package];
    xdg.enable = lib.mkDefault true;
    xdg.configFile."doty/targets.json".text = builtins.toJSON (policy.document "user" cfg);
    systemd.user.services.doty-user = lib.mkIf cfg.timer.enable {
      Unit.Description = "Doty caller-local housekeeping";
      Service = {
        Type = "oneshot";
        ExecStart = "${lib.getExe cfg.package} --mode user ${rootArgs} run --config ${lib.escapeShellArg cfg.configFile} --apply";
        Environment = [
          "HOME=${config.home.homeDirectory}"
          "XDG_CONFIG_HOME=${config.xdg.configHome}"
          "XDG_CACHE_HOME=${config.xdg.cacheHome}"
          "XDG_DATA_HOME=${config.xdg.dataHome}"
          "XDG_STATE_HOME=${config.xdg.stateHome}"
        ];
        NoNewPrivileges = true;
        UMask = "0077";
      };
    };
    systemd.user.timers.doty-user = lib.mkIf cfg.timer.enable {
      Unit.Description = "Scheduled Doty user housekeeping";
      Install.WantedBy = ["timers.target"];
      Timer = {
        OnCalendar = cfg.timer.schedule;
        Persistent = true;
        RandomizedDelaySec = "30m";
      };
    };
  };
}
