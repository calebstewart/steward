# steward in a winpkgs home configuration: home-manager's own
# `systemd.user.services` written as steward's units, in
# %APPDATA%\steward\units.
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
# none gets no files. After an apply, `stewctl switch` makes the running
# manager follow them -- by hand until winpkgs can run it (winpkgs#13).
{
  config,
  lib,
  ...
}:
let
  units = config.systemd.user.services;

  # home-manager's toSystemdIni: booleans as true/false, lists as repeated keys.
  toIni = lib.generators.toINI {
    listsAsDuplicateKeys = true;
    mkKeyValue =
      key: value: "${key}=${if lib.isBool value then lib.boolToString value else toString value}";
  };

  # home-manager's triggers name store paths, which winpkgs will not write into
  # a file. steward ignores X- keys; all a trigger has to do is change the
  # file, so `stewctl switch` restarts the unit. Their hash does that.
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

  # Kinds of unit steward does not run (yet). Targets are not among them:
  # they run nothing, steward has its own, and home-manager declares one
  # (tray.target) in every configuration.
  otherKinds = lib.filter (kind: config.systemd.user.${kind} != { }) [
    "timers"
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
    lib.nameValuePair "AppData/Roaming/steward/units/${name}.service" {
      text = render unit;
    }
  ) units;

  warnings = lib.optional (otherKinds != [ ]) (
    "steward runs services only; these systemd.user units are not written: "
    + lib.concatMapStringsSep ", " (
      kind: "${kind} (${lib.concatStringsSep ", " (lib.attrNames config.systemd.user.${kind})})"
    ) otherKinds
  );
}
