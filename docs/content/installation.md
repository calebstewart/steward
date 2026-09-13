+++
title = "Installation"
weight = 1
description = "Building it, registering it with Windows once, and trying it without installing anything."
+++

steward is registered once, by an administrator, as a *per-user service
template*. After that, Windows starts a manager in every session that signs
in, as that user, on that user's desktop — and restarts it if it dies. No Run
key, no scheduled task, and no privileged code of steward's own: the Service
Control Manager plays the part `user@.service` plays for systemd.

There are two ways to get there: [with winpkgs](#with-winpkgs), which also
writes your units from a home-manager configuration, or [by hand](#by-hand)
with `sc.exe`. Either way you can [try the manager first](#trying-it-first)
without registering anything.

## Requirements

- **Windows 10 1709 or later**, which is when the SCM gained per-user services
  (`type= userown`). Windows 11 is where it is used day to day.
- **An administrator, once**, to register the template and to install or
  upgrade the binaries. Nothing else runs elevated, ever: the manager and every
  service it starts run with the user's own, non-elevated token.
- **Binaries.** There are no prebuilt releases; [build them](#building) with
  Cargo on Windows or with Nix, cross-compiled.

## Building

On Windows, with a Rust toolchain:

```console
cargo build --release
cargo test
```

The binaries are `target\release\steward.exe` and `target\release\stewctl.exe`.

With Nix, on Linux or in WSL, cross-compiled for Windows:

```console
nix build          # result/bin/steward.exe, result/bin/stewctl.exe
nix flake check    # the platform-free crates' tests natively, and the Windows build
```

The Windows binaries import nothing but Windows' own DLLs, so they run on a
machine with no MinGW or Rust installed.

## With winpkgs

The flake exports two [winpkgs](https://github.com/calebstewart/winpkgs)
modules, under winpkgs' own name for its module trees. The **system** one
installs steward and registers it; the **home** one writes a user's units from
home-manager's `systemd.user.services`, `systemd.user.targets` and
`systemd.user.timers`. A home has nothing to enable, exactly as it would where
home-manager runs on systemd: declaring units is all it takes.

```nix
# flake inputs
steward = {
  url = "github:calebstewart/steward";
  inputs.nixpkgs.follows = "nixpkgs";
  inputs.winpkgs.follows = "winpkgs";
};

# the system configuration
imports = [ inputs.steward.windowsModules.system ];
services.steward.enable = true;

# the home configuration: nothing to enable, only units to declare
imports = [ inputs.steward.windowsModules.home ];
systemd.user.services.whkd = {
  Unit.Description = "Hotkey daemon";
  Unit.After = [ "graphical-session.target" ];
  Service.ExecStart = ''"C:\Program Files\whkd\bin\whkd.exe"'';
  Service.KillMode = "process";
  Install.WantedBy = [ "graphical-session.target" ];
};
```

### The system module

| Option | Default | |
| --- | --- | --- |
| `services.steward.enable` | `false` | Install steward and register the per-user service template. |
| `services.steward.package` | built from this flake | The build to install: `steward.exe` and `stewctl.exe` in its `bin`. |
| `services.steward.directory` | `C:\Program Files\steward` | Where the binaries live. It is on the machine `PATH`, for `stewctl`. |

The template is registered to start automatically, with failure actions that
restart a manager 5 s after it dies, up to three times; the count resets after
a minute without a failure. The first manager
starts at your **next sign-in**: registering a template does not start it in a
session that is already signed in.

The directory is fixed, not versioned, on purpose. A running instance is a
copy of the template made at sign-in, and Windows refuses to change an instance
afterwards, so a new path would only reach you at your next sign-in. An
upgrade instead replaces the binaries in place — winpkgs moves the running
`steward.exe` aside to write the new one — and then sends each running
instance control 128, *hand over*. The old manager detaches, leaving every
service running, and the SCM starts the instance again on the new
`steward.exe`, which adopts them. The services never notice.

### The home module

Each `systemd.user.services.<name>` becomes `<name>.service` in
`%APPDATA%\steward\units`, and likewise for targets and timers, rendered the
way home-manager renders them. The targets steward has built in —
`default`, `graphical-session`, `tray` and `timers` — are not written;
home-manager declares `tray.target` in every configuration, and on Windows it
is steward's.

An apply that changes the units runs `stewctl switch --if-running` at the end,
after pruning, so the running manager restarts what changed, starts what is
new and stops what is gone — while a unit you stopped stays stopped. A home
applied before the system is harmless: without `stewctl` on the `PATH` the
step does nothing, and with no manager running in the session the next one
reads the files as they are.

> [!NOTE]
> `ExecStart=` is a Windows command line. A unit that names a Nix store path
> cannot work on Windows, and winpkgs refuses to write one.

home-manager's `X-Restart-Triggers=` and `X-Reload-Triggers=` name store
paths too, so they are written as their hash: a changed trigger still changes
the file, and `switch` still restarts the unit. Sockets, paths, slices and
mounts are not steward's, and the module warns about any it is given.

## By hand

Build or copy the two binaries, then once, from an **administrator Command
Prompt**:

```bat
mkdir "C:\Program Files\steward"
copy steward.exe "C:\Program Files\steward"
copy stewctl.exe "C:\Program Files\steward"
sc create steward type= userown start= auto binPath= "\"C:\Program Files\steward\steward.exe\""
sc failure steward reset= 60 actions= restart/5000/restart/5000/restart/5000
```

The space after each `=` is part of `sc`'s syntax. From PowerShell, spell it
`sc.exe`: plain `sc` is an alias for `Set-Content` there. Add the directory to
your `PATH` for `stewctl`.

Windows starts an instance named `steward_<suffix>` at every sign-in — so
**sign out and back in**. The suffix changes at each sign-in; nothing needs to
know it, since `stewctl` finds the manager through its named pipe.

> [!WARNING]
> Anything a unit now runs must no longer be started by a Run key or the
> Startup folder, or it runs twice — and a second hotkey daemon cannot
> register the hotkeys the first one holds.

### Upgrading

Stopping the instance stops every service, as signing out does. To upgrade
without stopping them, hand over to the new manager instead:

```bat
sc control steward_<suffix> 128
copy /y steward.exe "C:\Program Files\steward"
copy /y stewctl.exe "C:\Program Files\steward"
sc start steward_<suffix>
```

Control 128 makes the manager detach and exit, its services left running and
recorded; the new manager adopts them. Find the suffix with
`sc query type= userservice state= all | findstr steward_`.

### Removing it

`sc stop` and `sc delete` the instance, `sc delete steward`, and delete the
directory. Your units in `%APPDATA%\steward` and logs in
`%LOCALAPPDATA%\steward` are left for you to remove.

## Trying it first

The manager runs in a console just as well as under the SCM:

```console
steward --console
```

It runs in the foreground until Ctrl+C, which stops every service. A second
Ctrl+C exits at once and leaves them running, for the next manager to adopt.

Only one manager runs per session, so to try it beside one that already runs —
or just without touching your own directories — point these at scratch
locations in that console first:

| Variable | What follows it |
| --- | --- |
| `APPDATA` | The unit directory, `%APPDATA%\steward\units`. |
| `LOCALAPPDATA` | Logs, the state file and timer stamps, under `%LOCALAPPDATA%\steward`. |
| `STEWARD_PIPE` | The control pipe's name: a single name, with no `\` or `/` in it. Set it for the manager and for the `stewctl` that talks to it. |

The services themselves still get your real environment: each is started with
an environment built fresh from your account, not the manager's.

The [`examples/`](https://github.com/calebstewart/steward/tree/main/examples)
directory has units to try, each saying what it shows: a console program
stopped with Ctrl+C, a crash loop and its backoff, ordering after another unit
and the shell, whkd as a real daemon in a tiling target, and a timer.

## Checking it came up

```console
> stewctl status
steward 0.1.0 (pid 7212, session 1)
   Shell: ready (graphical-session.target reached)
    Tray: ready (tray.target reached)
   Units: 5 in C:\Users\alice\AppData\Roaming\steward\units
          4 active, 0 failed, 0 restarting
    Logs: C:\Users\alice\AppData\Local\steward\logs
```

If `stewctl` says steward is not running, the manager's own log is
`%LOCALAPPDATA%\steward\steward.log`, and `stewctl verify` checks your unit
files without a manager at all.
