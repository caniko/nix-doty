# Evaluate with a pinned Home Manager consumer. Compare the emitted unit to
# NixOS's systemd argv encoder, including path characters with expansion meaning.
{
  homeManager,
  nixpkgs,
  dotyPackage,
}: let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.x86_64-linux;
  root = "/external root's $HOME %h \"quotes\"\\backslash";
  configHome = "/home/dejana/config path's $HOME %h \"quotes\"";
  home = homeManager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      ../module/home.nix
      {
        home = {
          username = "dejana";
          homeDirectory = "/home/dejana";
          stateVersion = "26.05";
        };
        xdg.configHome = configHome;
        programs.doty = {
          enable = true;
          package = dotyPackage;
          extraRoots = [root];
          timer.enable = true;
          targets.uv-cache = {
            enable = true;
            variant = "clean";
          };
        };
      }
    ];
  };
  service = home.config.systemd.user.services.doty-user.Service;
  utils = import (nixpkgs.outPath + "/nixos/lib/utils.nix") {
    inherit lib pkgs;
    config = {};
  };
  argv = [
    (lib.getExe dotyPackage)
    "--mode"
    "user"
    "--user-root"
    root
    "run"
    "--config"
    "${configHome}/doty/targets.json"
    "--apply"
  ];
in
  assert service.ExecStart == [(utils.escapeSystemdExecArgs argv)];
  assert builtins.elem ''"XDG_CONFIG_HOME=/home/dejana/config path's $HOME %%h \"quotes\""'' service.Environment; {
    inherit service argv;
    unit = ''
      [Service]
      Type=oneshot
      ExecStart=${lib.head service.ExecStart}
      ${lib.concatMapStringsSep "\n" (value: "Environment=${value}") service.Environment}
    '';
  }
