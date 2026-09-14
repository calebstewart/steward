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

    # The other half of the elevated install: the Scheduled Task that gives
    # each user an Event Log channel of their own, `Steward/<their SID>`, for
    # their units' output. It runs as SYSTEM at the logon of any user,
    # because creating a channel is administrative and the manager is not,
    # and because this step cannot know which accounts will ever sign in.
    #
    # An activation rather than a resource: winpkgs can enable and disable a
    # scheduled task but does not create one, and the task carries a security
    # descriptor (users may run it, not change it) that only the Task
    # Scheduler's own API can set. steward registers it itself, from the path
    # it is installed at, so there is one definition of the task rather than
    # one here and one in the README.
    #
    # Re-run when the binaries change, which is the only thing that can
    # change what the task should say. Idempotent either way: it replaces the
    # task with the same definition and imports nothing it has imported
    # before.
    winpkgs.activation.steward-eventlog = {
      command = ''& "${exe}" provision-eventlog --install'';
      triggers = [ cfg.package ];
    };
  };
}
