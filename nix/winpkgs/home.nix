# steward in a winpkgs home configuration: home-manager's own
# `systemd.user.services`, `systemd.user.targets` and `systemd.user.timers`
# written as steward's units, in %APPDATA%\steward\units. The targets steward
# has built in -- default, graphical-session, tray (which home-manager
# declares in every configuration), and timers -- are steward's, and not
# written.
#
# winpkgs evaluates home-manager's modules, so the option is already there;
# on Windows home-manager's systemd module is off (`systemd.user.enable`
# defaults to Linux only) and the units would go nowhere. Units are free-form
# `Section.Key` attributes, and steward reads systemd's syntax, so they are
# rendered as home-manager renders them. `ExecStart=` is a Windows command
# line; a unit that names a Nix store path cannot work on Windows, and winpkgs
# refuses to write one.
#
# There is nothing to enable: the system configuration installs and
# registers steward (the system module), and importing this module is what
# makes a home's systemd.user.services steward's units -- as declaring them
# is all it takes where home-manager runs on systemd. A home that declares
# none gets no files. An apply that changes them runs `stewctl switch`, as
# home-manager runs sd-switch, so the running manager follows them.
{
  config,
  lib,
  ...
}:
let
  builtinTargets = [
    "default"
    "graphical-session"
    "tray"
    "timers"
  ];
  # "whkd.service", "tiling.target", "backup.timer" -> its definition.
  units =
    lib.mapAttrs' (name: unit: lib.nameValuePair "${name}.service" unit) config.systemd.user.services
    // lib.mapAttrs' (name: unit: lib.nameValuePair "${name}.target" unit) (
      removeAttrs config.systemd.user.targets builtinTargets
    )
    // lib.mapAttrs' (name: unit: lib.nameValuePair "${name}.timer" unit) config.systemd.user.timers;

  # home-manager's toSystemdIni: booleans as true/false, lists as repeated keys.
  toIni = lib.generators.toINI {
    listsAsDuplicateKeys = true;
    mkKeyValue =
      key: value: "${key}=${if lib.isBool value then lib.boolToString value else toString value}";
  };

  # home-manager's triggers name store paths, which winpkgs will not write into
  # a file. All a trigger has to do is change the file: `stewctl switch`
  # compares the files key by key, and restarts the unit for a changed
  # X-Restart-Triggers or reloads it for a changed X-Reload-Triggers. Their
  # hash does that.
  triggers = [
    "X-Restart-Triggers"
    "X-Reload-Triggers"
  ];
  hashTriggers =
    section:
    lib.mapAttrs (
      key: value:
      if lib.elem key triggers && value != [ ] then
        builtins.hashString "sha256" (builtins.toJSON value)
      else
        value
    ) section;

  # As home-manager renders a unit: keys that are null or empty lists dropped,
  # then sections left empty.
  render =
    unit:
    toIni (
      lib.filterAttrs (_: section: section != { }) (
        lib.mapAttrs (_: section: lib.filterAttrs (_: v: v != null && v != [ ]) (hashTriggers section)) unit
      )
    );

  # Kinds of unit steward does not run (yet).
  otherKinds = lib.filter (kind: config.systemd.user.${kind} != { }) [
    "sockets"
    "paths"
    "slices"
    "mounts"
    "automounts"
  ];
in
{
  # Under AppData/Roaming, which winpkgs writes as %APPDATA%: where steward
  # reads units, wherever xdg.configHome points.
  home.file = lib.mapAttrs' (
    name: unit:
    lib.nameValuePair "AppData/Roaming/steward/units/${name}" {
      text = render unit;
    }
  ) units;

  # Run when the units change -- all of them gone included, so their services
  # stop. Harmless before steward is: without stewctl on the PATH there is
  # nothing to tell, and with no manager in the session (--if-running) the
  # next one reads the files as they are.
  winpkgs.activation.steward = {
    command = "if (Get-Command stewctl -ErrorAction Ignore) { stewctl switch --if-running }";
    triggers = lib.mapAttrsToList (name: unit: {
      inherit name;
      text = render unit;
    }) units;
  };

  warnings = lib.optional (otherKinds != [ ]) (
    "steward runs services, targets and timers only; these systemd.user units are not written: "
    + lib.concatMapStringsSep ", " (
      kind: "${kind} (${lib.concatStringsSep ", " (lib.attrNames config.systemd.user.${kind})})"
    ) otherKinds
  );
}
