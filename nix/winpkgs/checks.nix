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
        systemd.user.targets.tiling = {
          Unit.Description = "Tiling window management";
          Install.WantedBy = [ "graphical-session.target" ];
        };
        systemd.user.timers.backup = {
          Unit.Description = "Nightly backup";
          Timer = {
            OnCalendar = "*-*-* 03:00";
            Persistent = true;
          };
          Install.WantedBy = [ "timers.target" ];
        };
      }
    ];
  };

  expectedTarget = pkgs.writeText "tiling.target" ''
    [Install]
    WantedBy=graphical-session.target

    [Unit]
    Description=Tiling window management
  '';

  # A boolean as home-manager writes it, which steward reads as systemd does.
  expectedTimer = pkgs.writeText "backup.timer" ''
    [Install]
    WantedBy=timers.target

    [Timer]
    OnCalendar=*-*-* 03:00
    Persistent=true

    [Unit]
    Description=Nightly backup
  '';

  # The same home with whkd's unit changed: its switch must run again.
  changed = user.extendModules {
    modules = [ { systemd.user.services.whkd.Service.Restart = lib.mkForce "on-failure"; } ];
  };

  # Importing the module is all a home does; one that declares no services
  # gets no files.
  bare = winpkgs.lib.homeConfiguration {
    inherit system;
    modules = [
      self.windowsModules.home
      { winpkgs.name = "bare@check"; }
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
        # Interactive users may not send it user-defined controls (#11).
        test "$(service securityDescriptor)" = 'D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLORC;;;IU)(A;;CCLCSWLORC;;;SU)'
        [[ "$(service revision)" =~ ^[0-9a-f]{64}$ ]]

        # The binaries are in the closure, as the directory's one source.
        source=$(jq -r '.resources[] | select(.type == "winpkgs/file" and .properties.target == "C:\\Program Files\\steward") | .properties.source' <<<"$doc")
        test -f "$closure/$source/steward.exe"
        test -f "$closure/$source/stewctl.exe"

        # stewctl is on the machine PATH.
        test "$(jq -r '.resources[] | select(.type == "winpkgs/path") | .properties.dir' <<<"$doc")" = 'C:\Program Files\steward'

        # The elevated install also registers the Event Log provisioning task
        # (#24), by running the steward it just installed rather than by
        # spelling the task out here.
        eventlog() { jq -r --arg f "$1" '.resources[] | select(.id == "Activation steward-eventlog") | .properties[$f] | tostring' <<<"$doc"; }
        test "$(eventlog command)" = '& "C:\Program Files\steward\steward.exe" provision-eventlog --install'
        test "$(jq -r '.resources[] | select(.id == "Activation steward-eventlog") | .scope' <<<"$doc")" = machine
        [[ "$(eventlog revision)" =~ ^[0-9a-f]{64}$ ]]
        touch $out
      '';

  winpkgs-home =
    pkgs.runCommand "steward-winpkgs-home"
      {
        doc = document user;
        changedDoc = document changed;
        bareDoc = document bare;
        closure = user.config.system.build.toplevel;
        nativeBuildInputs = [ pkgs.jq ];
      }
      ''
        source=$(jq -r '.resources[] | select(.type == "winpkgs/file" and .properties.target == "%APPDATA%/steward/units/whkd.service") | .properties.source' <<<"$doc")
        test -n "$source"
        diff -u ${expectedUnit} "$closure/$source"

        # A target of the user's is written; home-manager's tray.target,
        # steward's own, is not.
        target=$(jq -r '.resources[] | select(.type == "winpkgs/file" and .properties.target == "%APPDATA%/steward/units/tiling.target") | .properties.source' <<<"$doc")
        diff -u ${expectedTarget} "$closure/$target"
        test "$(jq '[.resources[] | select(.id | test("tray"))] | length' <<<"$doc")" = 0

        # A timer is written too.
        timer=$(jq -r '.resources[] | select(.type == "winpkgs/file" and .properties.target == "%APPDATA%/steward/units/backup.timer") | .properties.source' <<<"$doc")
        diff -u ${expectedTimer} "$closure/$timer"

        test "$(jq '[.resources[] | select(.type == "winpkgs/file" and (.id | test("steward")))] | length' <<<"$bareDoc")" = 0

        # The switch after an apply: last, and again when a unit changes.
        switch() { jq -r --arg f "$2" '.resources[] | select(.id == "Activation steward") | .properties[$f]' <<<"$1"; }
        test "$(jq -r '.resources[-1].id' <<<"$doc")" = 'Activation steward'
        [[ "$(switch "$doc" command)" == *'stewctl switch --if-running'* ]]
        test "$(switch "$doc" revision)" != "$(switch "$changedDoc" revision)"
        touch $out
      '';
}
