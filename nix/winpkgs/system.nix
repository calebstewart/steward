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
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.services.steward;
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
  # The one command line, shared by the task and the run at install so the
  # two cannot disagree.
  provisionArgs = "provision-eventlog --channel-size ${showSize cfg.eventlog.channelSize}";
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
      '';
    };
  };

  config = lib.mkIf cfg.enable {
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
      # what it does: the size is a literal in the action, which has no
      # `$(Arg0)` for a caller's parameters to land in -- and needed, because
      # the manager will want to run it on demand, and because a task an
      # unelevated plan cannot read counts as a change at every apply.
      securityDescriptor = "D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;0x1200a9;;;AU)";
    };

    # The task covers every logon after this one. Whoever is signed in right
    # now would otherwise wait until their next, so the apply runs it once --
    # and again when the size changes, which is part of the command and so
    # of what winpkgs decides by. Nothing to prune afterwards: unlike
    # registering the task, running it only creates channels, and those
    # outlive any configuration.
    winpkgs.activation.steward-eventlog = {
      command = ''& "${exe}" ${provisionArgs}'';
      triggers = [ cfg.package ];
    };
  };
}
