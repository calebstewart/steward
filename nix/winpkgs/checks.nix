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

  # Other channel sizes (#31), and one Windows would raise -- 1 MiB, below
  # its least of 1028 KiB -- which must not evaluate at all.
  sized =
    size: machine.extendModules { modules = [ { services.steward.eventlog.channelSize = size; } ]; };
  kib = sized (1028 * 1024);
  bytes = sized 100000000;
  tooSmall = builtins.tryEval (sized (1024 * 1024)).config.services.steward.eventlog.channelSize;

  # The channels of the accounts a configuration names (#41): the homes it
  # declares by default, and whatever else it lists.
  homed = machine.extendModules {
    modules = [ { winpkgs.homes = [ user ]; } ];
  };
  named = machine.extendModules {
    modules = [
      {
        services.steward.eventlog.accounts = [
          "Caleb Stewart"
          "guest"
        ];
      }
    ];
  };
  # A `"` in a name would end the quotes the action puts around it; refused
  # at evaluation, not written into a task.
  quoted = builtins.tryEval (
    (machine.extendModules {
      modules = [ { services.steward.eventlog.accounts = [ ''a"b'' ]; } ];
    }).config.system.build.document
  );

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
            X-SwitchMethod = "keep-old";
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
  # keys, the trigger as its hash, the switch method as written.
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
    X-SwitchMethod=keep-old
  '';
in
{
  winpkgs-system =
    pkgs.runCommand "steward-winpkgs-system"
      {
        doc = document machine;
        kibDoc = document kib;
        bytesDoc = document bytes;
        homedDoc = document homed;
        namedDoc = document named;
        tooSmallEvaluates = lib.boolToString tooSmall.success;
        quotedEvaluates = lib.boolToString quoted.success;
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

        # The elevated install also declares the Event Log provisioning task
        # (#24). Declared, not registered by steward itself, so that it is
        # deleted again when steward leaves a configuration.
        task() { jq -r --arg f "$1" '.resources[] | select(.type == "winpkgs/task") | .properties[$f] | tostring' <<<"''${2:-$doc}"; }
        test "$(task command)" = 'C:\Program Files\steward\steward.exe'
        # The size is a literal in the action, which is what keeps it safe to
        # let users run (#31).
        test "$(task arguments)" = 'provision-eventlog --channel-size 128MiB'
        # As SYSTEM, at the logon of any user: no `user` on the trigger.
        test "$(task runAs)" = 'S-1-5-18'
        test "$(jq -r '.resources[] | select(.type == "winpkgs/task") | .properties.triggers[0].type' <<<"$doc")" = logon
        test "$(jq -r '.resources[] | select(.type == "winpkgs/task") | .properties.triggers[0].user' <<<"$doc")" = null
        # The settings whose defaults would be wrong here: a second logon must
        # not lose its run, and a laptop on battery must still get one.
        test "$(task multipleInstances)" = queue
        test "$(task disallowStartIfOnBatteries)" = false
        # Users may run it and may not rewrite it; it runs as SYSTEM.
        test "$(task securityDescriptor)" = 'D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;0x1200a9;;;AU)'
        test "$(jq -r '.resources[] | select(.type == "winpkgs/task") | .scope' <<<"$doc")" = machine

        # And a run at install, for whoever is signed in already: the task
        # started, since only SYSTEM can find who that is (#28).
        eventlog() { jq -r --arg f "$1" '.resources[] | select(.id == "Activation steward-eventlog") | .properties[$f] | tostring' <<<"''${2:-$doc}"; }
        test "$(eventlog command)" = '& "$env:SystemRoot\System32\schtasks.exe" /run /tn steward-provision-eventlog'
        [[ "$(eventlog revision)" =~ ^[0-9a-f]{64}$ ]]

        # Another size reaches the task, and the run happens again for it:
        # the channels are resized at the apply.
        test "$(task arguments "$kibDoc")" = 'provision-eventlog --channel-size 1028KiB'
        test "$(eventlog command "$kibDoc")" = "$(eventlog command)"
        test "$(eventlog revision "$kibDoc")" != "$(eventlog revision)"
        test "$(task arguments "$bytesDoc")" = 'provision-eventlog --channel-size 100000000'
        # And a size Windows would raise is refused at evaluation, not left
        # for a task that would fail at every logon.
        test "$tooSmallEvaluates" = false

        # The accounts a configuration names get their channels at the
        # install rather than at their first sign-in (#41): by default the
        # account each of `winpkgs.homes` is for, read out of the home's own
        # name, and quoted, since a Windows account name may hold spaces.
        test "$(task arguments "$homedDoc")" = 'provision-eventlog --channel-size 128MiB --account "user"'
        test "$(task arguments "$namedDoc")" = 'provision-eventlog --channel-size 128MiB --account "Caleb Stewart" --account "guest"'
        # The same one command line, so the install-time run happens again
        # when an account is added.
        test "$(eventlog command "$namedDoc")" = "$(eventlog command)"
        test "$(eventlog revision "$namedDoc")" != "$(eventlog revision)"
        # A `"` in a name would end those quotes and make the rest of it
        # arguments of its own; refused at evaluation. Windows does not allow
        # one in an account name either.
        test "$quotedEvaluates" = false
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
