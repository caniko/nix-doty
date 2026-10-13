{lib}: {
  targetsOption = lib.mkOption {
    type = lib.types.attrsOf (lib.types.submodule {
      options = {
        enable = lib.mkEnableOption "this doty cleanup target";
        variant = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Single variant; prefer variants for new configuration.";
        };
        settings = lib.mkOption {
          type = lib.types.attrsOf lib.types.anything;
          default = {};
          description = "Provider settings for the single-variant form.";
        };
        variants = lib.mkOption {
          type = lib.types.listOf (lib.types.submodule {
            options = {
              variant = lib.mkOption {
                type = lib.types.str;
                description = "Cleanup variant to use.";
              };
              settings = lib.mkOption {
                type = lib.types.attrsOf lib.types.anything;
                default = {};
                description = "Provider-specific configuration.";
              };
            };
          });
          default = [];
          description = "Cleanup variants for this framework.";
        };
      };
    });
    default = {};
    description = "Configured Doty cleanup policy.";
  };
  requirementsOption = lib.mkOption {
    type = lib.types.listOf (lib.types.submodule {
      options = {
        name = lib.mkOption {type = lib.types.str;};
        variant = lib.mkOption {type = lib.types.str;};
        path = lib.mkOption {type = lib.types.str;};
      };
    });
    default = [];
    description = "Read-only provider/path coverage required by doty doctor.";
  };
  assertions = namespace: cfg:
    lib.mapAttrsToList (name: t: {
      assertion = t.variants != [] || t.variant != null;
      message = "${namespace}.targets.${name} must set variant or variants";
    }) (lib.filterAttrs (_: t: t.enable) cfg.targets);
  document = mode: cfg: {
    inherit mode;
    requirements = cfg.inspectionRequirements;
    targets = lib.concatLists (lib.mapAttrsToList (name: t:
      if t.variants != []
      then
        map (v: {
          inherit name;
          inherit (v) variant settings;
        })
        t.variants
      else [
        {
          inherit name;
          inherit (t) variant settings;
        }
      ]) (lib.filterAttrs (_: t: t.enable) cfg.targets));
  };
}
