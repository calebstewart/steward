# steward in a winpkgs system configuration: the binaries in a fixed
# directory, that directory on the machine PATH (stewctl), and the per-user
# service template every sign-in starts a manager from.
#
# The directory is fixed, not versioned, because a template's instance is a
# copy of the template made at sign-in, and Windows refuses to change an
# instance afterwards: a new path would reach a signed-in user only at their
# next sign-in. So an upgrade replaces the binaries where the running
# instance looks for them -- winpkgs moves the running steward.exe aside to do
# that -- and then restarts the instance with control 128, on which the old
# manager hands its services over instead of stopping them. The new one
# adopts them.
{
  config,
  lib,
  pkgs,
  winpkgsSrc,
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.services.steward;
  # The same reading of a home configuration's name that
  # `modules/system/installer.nix` makes, from the same function: the account
  # winpkgs would create for that home is the account whose channel this
  # wants. Imported the way that module imports it, rather than guessing at
  # the `@`, so a change to what winpkgs considers an account name reaches
  # here too.
  installer = import "${winpkgsSrc}/lib/installer.nix" { inherit lib winpkgsSrc; };
  isHome = h: (h.config.winpkgs.kind or null) == "home";
  # The SCM runs the command as written; a Windows path, backslashes and all.
  exe = "${cfg.directory}\\steward.exe";

  mib = 1024 * 1024;
  # What `steward provision-eventlog --channel-size` accepts: 1028 KiB or
  # more, the least Windows makes a channel. Refused here, at evaluation,
  # rather than by a task that would fail at every logon.
  channelSize = types.addCheck types.ints.positive (n: n >= 1028 * 1024) // {
    description = "size in bytes, at least 1028 KiB";
  };
  # The size as an administrator reading the task would write it: 64MiB,
  # 1028KiB, or bytes when it is neither.
  showSize =
    n:
    if lib.mod n mib == 0 then
      "${toString (n / mib)}MiB"
    else if lib.mod n 1024 == 0 then
      "${toString (n / 1024)}KiB"
    else
      toString n;
  # An account as the task's action names it. Quoted, because a Windows
  # account name may hold spaces ("Caleb Stewart") and the action is one
  # string that `CommandLineToArgvW` cuts up again. A `"` in a name would cut
  # it somewhere else, so it is refused in an assertion below rather than
  # escaped: Windows does not allow one in an account name either.
  account = name: ''--account "${name}"'';
  # The one command line, shared by the task and the run at install so the
  # two cannot disagree.
  provisionArgs = lib.concatStringsSep " " (
    [
      "provision-eventlog"
      "--channel-size"
      (showSize cfg.eventlog.channelSize)
    ]
    ++ map account cfg.eventlog.accounts
  );
in
{
  options.services.steward = {
    enable = lib.mkEnableOption ''
      steward, a per-user service manager: registered as a per-user service
      template, so Windows starts a manager in every session that signs in,
      which runs the user's units (see the home module)
    '';

    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ../package.nix { };
      defaultText = lib.literalMD "steward, built for Windows from this flake";
      description = "The steward to install: `steward.exe` and `stewctl.exe` in its `bin`.";
    };

    directory = mkOption {
      type = types.str;
      default = ''C:\Program Files\steward'';
      description = ''
        Where the binaries live. It is the template's command, and so the
        path every instance runs for as long as its user is signed in: an
        upgrade replaces the files here rather than moving them. On the
        machine PATH, for `stewctl`.
      '';
    };

    eventlog.channelSize = mkOption {
      type = channelSize;
      default = 64 * mib;
      defaultText = lib.literalExpression "64 * 1024 * 1024";
      example = lib.literalExpression "256 * 1024 * 1024";
      description = ''
        The most each user's Event Log channel, `Steward/<SID>`, may hold
        before its oldest records are overwritten, in bytes.

        A budget in lines more than in bytes: a record costs 1.2 to 1.5 KB
        of `.evtx` however short its line, so 64 MiB is roughly 50,000 lines
        for all of a user's units together. The channels live under
        `%SystemRoot%\System32\Winevt\Logs`, charged to the machine and not
        to any profile, one per account that has ever signed in.

        At least 1028 KiB, the least Windows allows a channel; `1 MiB` is
        less. Changing it resizes every existing channel at the next apply,
        without re-creating any, and every logon after puts back a size
        changed by hand.

        A smaller size takes nothing away from what a channel already holds:
        a channel whose file has grown past it keeps that file and every
        record in it, and goes on overwriting its oldest records at the size
        the file had reached. Clearing it (`wevtutil cl Steward/<SID>`, as an
        administrator, which loses those records) gives the disk back, and
        from then on the channel grows no further than the new size.
      '';
    };

    eventlog.accounts = mkOption {
      type = types.listOf types.str;
      default = map (h: (installer.splitHomeName h.config.winpkgs.name).user) (
        lib.filter isHome config.winpkgs.homes
      );
      defaultText = lib.literalMD "the account each of `winpkgs.homes` is for";
      example = lib.literalExpression ''[ "Caleb Stewart" "guest" ]'';
      description = ''
        Accounts whose channel is created at the install, rather than at that
        account's first sign-in. The default is the accounts this machine's
        homes are for, which the configuration already knows; add to it for
        an account winpkgs manages no home for.

        A channel is otherwise made the first time its account signs in,
        because a run of the task can only see the sessions signed in. The
        units start before the channel exists and their shims hold their
        output until it does -- about 0.2 s end to end, measured -- so this
        buys back a small race for every account the install could foresee,
        and leaves it only to the accounts it could not.

        Names as Windows names them: a local account (`guest`), a domain
        account (`DOMAIN\guest`), or a display name with spaces in it. A name
        that does not resolve when the task runs -- a local account the image
        has not created yet -- is passed over and named in
        `%ProgramData%\steward\provision-eventlog.log`, costs no other
        account its channel, and gets one at its first sign-in as it would
        have anyway. So does a name that resolves to something that is not a
        user: `LookupAccountName` calls `SYSTEM` and `Everyone` groups rather
        than users, and a channel for one of those is a channel nobody could
        write to.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # An account name goes into the task's action inside quotes, and the
    # action is the one string a caller cannot change. A `"` in a name would
    # end those quotes and make the rest of the name arguments of its own, so
    # it is refused here. Nothing is lost: Windows does not allow a `"` in an
    # account name.
    assertions = [
      {
        assertion = !lib.any (lib.hasInfix "\"") cfg.eventlog.accounts;
        message = ''
          services.steward.eventlog.accounts: an account name may not contain a
          double quote; Windows does not allow one either.
        '';
      }
    ];

    # The directory as a whole: an upgrade replaces it, and anything else in
    # it goes.
    windows.files.${cfg.directory}.source = "${cfg.package}/bin";

    environment.path = [ cfg.directory ];

    windows.services.steward = {
      type = "userOwn";
      command = ''"${exe}"'';
      description = "A per-user service manager: starts the user's services and keeps them running";
      # The manager itself is kept up the way it keeps its services up: a
      # crash is followed by a new manager, which adopts what the last left.
      failureActions = {
        resetAfter = 60;
        actions = lib.genList (_: {
          action = "restart";
          delay = 5000;
        }) 3;
      };
      # A new build is a new manager, handed the services by the old one.
      restartTriggers = [ cfg.package ];
      restartControl = 128;
      # Windows' default descriptor lets any interactive user send a service
      # user-defined controls, so anyone signed in to the machine -- at the
      # console or over Remote Desktop -- could send another session's
      # manager control 128 and leave that session without one until its next
      # sign-in: a clean stop runs no failure action. The default without
      # that right (CR) for interactive users (IU) and services (SU);
      # administrators and SYSTEM keep it, and an upgrade sends the control
      # elevated. Instances copy it at sign-in.
      securityDescriptor = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLORC;;;IU)(A;;CCLCSWLORC;;;SU)";
    };

    # The other half of the elevated install: the task that gives each user
    # an Event Log channel of their own, `Steward/<their SID>`, for their
    # units' output. Creating a channel is administrative and the manager is
    # not, and this step cannot know which accounts will ever sign in to the
    # machine, so the channels are made at each logon by something running as
    # SYSTEM.
    #
    # It does know some of them, though: the homes it declares. Those are
    # named in the action as `--account`, so their channels are made at the
    # install and only an account nobody foresaw waits for its first sign-in
    # (`eventlog.accounts`). Names and not SIDs, because a local account's
    # SID does not exist until the account does, and an evaluation has no way
    # to know it; the task resolves each name when it runs, which needs no
    # privilege, and passes over one that does not resolve yet.
    #
    # Declared rather than registered by steward itself, for the reason
    # winpkgs declares the service above rather than shelling out to `sc`: a
    # task winpkgs owns is deleted again when it leaves the configuration,
    # where a program that registered its own would leave a task running as
    # SYSTEM behind forever.
    windows.scheduledTasks."\\steward-provision-eventlog" = {
      command = exe;
      arguments = provisionArgs;
      description = "Creates the Windows Event Log channel that each signed-in user's steward units write their output to, one channel per user, named by SID.";
      author = "steward";
      # SYSTEM (the default `runAs`), at the logon of any user: a trigger
      # with no `user` of its own.
      runLevel = "highest";
      triggers = [ { type = "logon"; } ];
      # Windows' default drops a run that begins while one is still going,
      # and two people signing in at once is exactly when that happens: the
      # second is the one who would be left without a channel.
      multipleInstances = "queue";
      # It works in a second or it is not going to.
      executionTimeLimit = "PT3M";
      # Administrators and SYSTEM in full; authenticated users get 0x1200a9,
      # FILE_GENERIC_READ | FILE_GENERIC_EXECUTE, which the Task Scheduler
      # reads as "may see it and may run it". Deliberately no write: the task
      # runs as SYSTEM, so a user who could rewrite its action could run
      # anything as SYSTEM. Safe to grant because running it cannot change
      # what it does: the size and the accounts are literals in the action,
      # which has no `$(Arg0)` for a caller's parameters to land in -- and
      # `--uninstall` is refused unless it is the whole command line, so no
      # account name can turn a run into one that removes every channel on
      # the machine. Needed, too, because
      # the manager will want to run it on demand, and because a task an
      # unelevated plan cannot read counts as a change at every apply.
      securityDescriptor = "D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;0x1200a9;;;AU)";
    };

    # The task covers every logon after this one. Whoever is signed in right
    # now would otherwise wait until their next, and an account this
    # configuration names would wait for its first, so the apply runs it once
    # -- and again whenever the size or the accounts change, which is why the
    # whole command line is among the triggers. The apply starts the task
    # rather than running the program itself: finding who is signed in takes
    # SYSTEM's privilege, and an elevated administrator's run passes over
    # every session (seen, 2026-09-14, #28), so it would make the named
    # accounts' channels and nobody else's. Started, not waited for; its report
    # is %ProgramData%\steward\provision-eventlog.log. Nothing to prune
    # afterwards: unlike registering the task, running it only creates
    # channels, and those outlive any configuration.
    winpkgs.activation.steward-eventlog = {
      command = ''& "$env:SystemRoot\System32\schtasks.exe" /run /tn steward-provision-eventlog'';
      triggers = [
        cfg.package
        provisionArgs
      ];
    };
  };
}
