+++
title = "Overview"
sort_by = "weight"
template = "index.html"
page_template = "page.html"
+++

steward is `systemd --user` for a Windows desktop. It runs the long-lived
programs a desktop depends on — a tiling window manager, a hotkey daemon, a
status bar, a tray utility — from unit files in systemd's own syntax, and it
keeps them running: a daemon that crashes comes back, with a backoff, and one
that keeps failing ends in a `failed` state you can see rather than a silent
give-up.

It is two programs. `steward` is the manager, which Windows starts in your
session at every sign-in; `stewardctl` is the command line you talk to it with.

If you already know what it is: [Installation](@/installation.md) gets it
running, [Units](@/units.md) is every key a unit file can use, and
[stewardctl](@/stewardctl.md) is every command.

## Why it exists

A declared service should be running whenever its user is signed in, unless
someone deliberately stopped it. Windows has no answer for that:

- **Run keys and the Startup folder** start a program once, at sign-in, and
  forget it. If it crashes, exits, or is killed by an installer upgrading it,
  it stays gone until the next sign-in, and nothing records that it ever ran.
- **Scheduled Tasks** can "restart on failure", but the failure is the task's
  result rather than the process crashing, the interval is a minute at the
  least, and the default execution limit of 72 hours stops a long-running
  program on its own.
- **Windows services** are durable, but they run in session 0, which has no
  desktop: no hotkeys, no windows, no tray icons. Everything steward is for is
  out of reach from there.

steward is itself a Windows service — a *per-user* one, which the Service
Control Manager starts inside your session, on your desktop, as you. So the
SCM keeps the manager running, and the manager keeps your programs running.

## What it looks like

A unit is a text file in `%APPDATA%\steward\units`:

```ini
# whkd.service
[Unit]
Description=Hotkey daemon
After=graphical-session.target

[Service]
ExecStart="C:\Program Files\whkd\bin\whkd.exe"
KillMode=process

[Install]
WantedBy=graphical-session.target
```

It starts once Explorer's taskbar exists, restarts if it fails, and stopping
it leaves the terminals it opened alone. From any shell:

```console
> stewardctl
UNIT              STATE              PID  RESTARTS  DESCRIPTION
komorebi.service  active           10412         0  Tiling window manager
tiling.target     active               -         0  Tiling window management
whkd.service      active            9876         0  Hotkey daemon

> stewardctl status whkd
> stewardctl restart whkd
> stewardctl logs -f whkd
```

## Features

- **Durable by default.** A unit that says nothing about restarting is
  restarted when it fails, after a delay that grows from a second to a minute.
  Past its start limit it is retried a minute apart indefinitely, and shows
  its restart count, rather than giving up half a second after sign-in.
- **A manager crash is not a service crash.** Services run in job objects that
  outlive the manager. A manager that restarts — after a crash, or an upgrade —
  adopts the services the last one left running instead of starting them
  twice, and leaves a unit you stopped stopped.
- **systemd's unit syntax and meaning.** `After=`, `Requires=`, `Wants=`,
  `PartOf=`, `Restart=`, `KillMode=`, `Type=oneshot` and `forking`, targets and
  timers, with systemd's key names wherever the meaning carries over. A unit
  written for a Linux home loads, and says what it ignored.
- **Desktop-aware stages.** `graphical-session.target` is reached when
  Explorer's taskbar exists, and `tray.target` when the tray actually takes
  icons, about a second later — so a tray program never starts too early.
- **Groups as targets.** A `tiling.target` that komorebi, whkd and the bar are
  `WantedBy=` and `PartOf=` stops and starts the whole stack with one command.
- **Timers** in place of Scheduled Tasks: systemd's calendar events and
  relative triggers, with `Persistent=` making up a run missed while you were
  signed out.
- **A stop that asks first.** `ExecStop=`, then Ctrl+C to console programs and
  `WM_CLOSE` to windows, and only after `TimeoutStopSec=` is the job
  terminated.
- **Logs per unit**, with steward's own lines about the unit — started, exited
  with code 3, restarting in 5 s — interleaved into the same file, and readable
  with `stewardctl logs` even when no manager is running.
- **Declarative with Nix.** Two [winpkgs](https://github.com/calebstewart/winpkgs)
  modules install the manager and turn a home-manager configuration's
  `systemd.user.services` into steward units; an apply runs `stewardctl switch`,
  as home-manager runs `sd-switch`.

## What it does not do

- **Elevation. Never.** A service runs with your own, non-elevated token.
  Something that needs administrator rights is a Windows service, and the SCM
  already manages those; a user service manager that hands out admin rights is
  a privilege-escalation tool.
- System-wide services, or anything in session 0.
- Running services while you are signed out.

## Status

Supervision, the control plane, winpkgs integration, the desktop's daemons
running under it, and timers are all done, and it runs the author's own
desktop. Event triggers — lock and unlock, power, network, display changes —
are next. The [design notes](https://github.com/calebstewart/steward/blob/main/DESIGN.md)
cover how each part works and what was established on a real machine before it
was built.
