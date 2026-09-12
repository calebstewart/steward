# The winpkgs modules, evaluated and built the way a consumer's configuration
# is: what they declare, and that the closures winpkgs builds from them hold
# the binaries and the units.
{
  pkgs,
  self,
  winpkgs,
}:
let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
  document = c: builtins.toJSON c.config.system.build.document;

  machine = winpkgs.lib.windowsSystem {
    inherit system;
    modules = [
      self.windowsModules.system
      {
        winpkgs.name = "check";
        services.steward.enable = true;
      }
    ];
  };

  trigger = "whkdrc v1";
  user = winpkgs.lib.homeConfiguration {
    inherit system;
    modules = [
      self.windowsModules.home
      {
        winpkgs.name = "user@check";
        services.steward.enable = true;
        systemd.user.services.whkd = {
          Unit = {
            Description = "Hotkey daemon";
            After = [
              "graphical-session.target"
              "komorebi.service"
            ];
            X-Restart-Triggers = [ trigger ];
          };
          Service = {
            ExecStart = ''"C:\Program Files\whkd\bin\whkd.exe"'';
            KillMode = "process";
            Restart = "always";
          };
          Install.WantedBy = [ "graphical-session.target" ];
        };
      }
    ];
  };

  # home-manager's rendering: sections and keys in order, a list as repeated
  # keys, the trigger as its hash.
  expectedUnit = pkgs.writeText "whkd.service" ''
    [Install]
    WantedBy=graphical-session.target

    [Service]
    ExecStart="C:\Program Files\whkd\bin\whkd.exe"
    KillMode=process
    Restart=always

    [Unit]
    After=graphical-session.target
    After=komorebi.service
    Description=Hotkey daemon
    X-Restart-Triggers=${builtins.hashString "sha256" (builtins.toJSON [ trigger ])}
  '';
in
{
  winpkgs-system =
    pkgs.runCommand "steward-winpkgs-system"
      {
        doc = document machine;
        closure = machine.config.system.build.toplevel;
        nativeBuildInputs = [ pkgs.jq ];
      }
      ''
        service() { jq -r --arg f "$1" '.resources[] | select(.id == "Service steward") | .properties[$f] | tostring' <<<"$doc"; }
        test "$(service type)" = userOwn
        test "$(service startType)" = automatic
        test "$(service command)" = '"C:\Program Files\steward\steward.exe"'
        test "$(service restartControl)" = 128
        [[ "$(service revision)" =~ ^[0-9a-f]{64}$ ]]

        # The binaries are in the closure, as the directory's one source.
        source=$(jq -r '.resources[] | select(.type == "winpkgs/file" and .properties.target == "C:\\Program Files\\steward") | .properties.source' <<<"$doc")
        test -f "$closure/$source/steward.exe"
        test -f "$closure/$source/stewctl.exe"

        # stewctl is on the machine PATH.
        test "$(jq -r '.resources[] | select(.type == "winpkgs/path") | .properties.dir' <<<"$doc")" = 'C:\Program Files\steward'
        touch $out
      '';

  winpkgs-home =
    pkgs.runCommand "steward-winpkgs-home"
      {
        doc = document user;
        closure = user.config.system.build.toplevel;
        nativeBuildInputs = [ pkgs.jq ];
      }
      ''
        source=$(jq -r '.resources[] | select(.type == "winpkgs/file" and .properties.target == "%APPDATA%/steward/units/whkd.service") | .properties.source' <<<"$doc")
        test -n "$source"
        diff -u ${expectedUnit} "$closure/$source"
        touch $out
      '';
}
