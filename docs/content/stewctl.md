+++
title = "stewctl"
weight = 5
description = "Every command, after systemctl's own: what it does, and what it prints."
+++

`stewctl` talks to the manager running in the session it is run from, over a
named pipe only you can open. Its verbs follow `systemctl`. A unit name without
an extension is a service: `whkd` means `whkd.service`, while a target or a
timer is named in full.

| Command | |
| --- | --- |
| [`stewctl`](#list-units) | List the units. The same as `list-units`. |
| [`list-timers`](#list-timers) | The timers: when each next elapses, when it last did, and what it starts. |
| [`status [UNIT...]`](#status) | The manager, or units in detail with the end of their logs. |
| [`start UNIT...`](#start-stop-restart) | Start units, and what they want or require. |
| [`stop UNIT...`](#start-stop-restart) | Stop units, until started again or the next sign-in. |
| [`restart UNIT...`](#start-stop-restart) | Stop and start units; a changed unit starts with its new definition. |
| [`is-active UNIT...`](#is-active) | Exit 0 if every unit is active, 3 otherwise. |
| [`switch`](#switch) | Read the unit files and make what runs match them. |
| [`daemon-reload`](#daemon-reload) | Read the unit files, and only take note. |
| [`logs [-f] [-n N] UNIT`](#logs) | A unit's output, and steward's lines about it. |
| [`verify [FILE...]`](#verify) | Check unit files, without a manager. |

Every command but `logs`, `verify` and `switch --if-running` needs a running
manager; without one it says so and exits 1.

## list-units

Also `list` or `ls`, and what `stewctl` alone runs.

```console
> stewctl
UNIT                STATE              PID  RESTARTS  DESCRIPTION
crash-loop.service  auto-restart         -         4  Exits with code 3 a second after it starts
komorebi.service    active           10412         0  Tiling window manager
whkd.service        active*           9876         0  Hotkey daemon

* changed on disk; restart it (or `stewctl switch`) to use the new definition
```

A `*` after the state marks a unit whose file has changed since it started:
it is still running its old definition.

### States

| State | |
| --- | --- |
| `active` | Running; for a target or a timer, started. |
| `inactive` | At rest: never started, stopped, or — for `Type=oneshot` — run to completion. |
| `failed` | Ended badly with no restart to come: its policy said no, or its start limit ran out. It stays down until started again. |
| `auto-restart` | Waiting out the delay before an automatic restart. |
| `start-pre`, `start`, `start-post` | On its way up: running `ExecStartPre=`, starting the main process (or a oneshot's commands), running `ExecStartPost=`. |
| `stop`, `stop-asked`, `stop-killed` | On its way down: running `ExecStop=`, asked to exit, terminated. |

## list-timers

```console
> stewctl list-timers
NEXT                     LEFT          LAST                     PASSED         UNIT          ACTIVATES
Sun 2026-09-13 14:31:00  in 42s        Sun 2026-09-13 14:30:00  17s ago        hello.timer   hello.service
Mon 2026-09-14 03:00:00  in 12h 29min  Sun 2026-09-13 03:00:00  11h 30min ago  backup.timer  backup.service
```

The soonest first; a timer with nothing left to wait for shows `-`. Times are
local. See [Timers](@/timers.md).

## status

With no units, the manager: its version and PID, whether the shell and the
tray are ready yet, how many units are active, failed and restarting, and where
the units and logs are.

```console
> stewctl status
steward 0.1.0 (pid 7212, session 1)
   Shell: ready (graphical-session.target reached)
    Tray: ready (tray.target reached)
   Units: 5 in C:\Users\alice\AppData\Roaming\steward\units
          4 active, 0 failed, 1 restarting
    Logs: C:\Users\alice\AppData\Local\steward\logs
```

With units, each in detail, then the last ten lines of its log:

```console
> stewctl status crash-loop
○ crash-loop.service - Exits with code 3 a second after it starts
     Loaded: C:\Users\alice\AppData\Roaming\steward\units\crash-loop.service
     Active: auto-restart since 2026-09-13 14:29:55.422 (3s ago)
    Restart: in 8.7 s
   Restarts: 4 (last ended: it exited with code 3)
  Wanted by: default.target

-- 2026-09-13 14:29:54.387 steward: start
-- 2026-09-13 14:29:54.389 steward: active, main process 6128
-- 2026-09-13 14:29:55.422 steward: exited with code 3; restarting in 11.7 s (restart 4)
```

`Main PID` and `Processes` appear while it runs; a timer shows its next
`Trigger`, what it `Triggers`, and when it `Last` elapsed. A service's
`Output` line says where its output goes, your Event Log channel or its log
file, and the ten lines come from there, read as [`logs`](#logs) reads them.

## start, stop, restart

```console
stewctl start whkd
stewctl stop tiling.target
stewctl restart komorebi whkd
```

**`start`** starts the units and what they `Wants=` or `Requires=`, in order,
waiting for the shell or the tray first if they are ordered after it. Starting
a `failed` unit resets its start limit and backoff.

**`stop`** stops the units, and what `Requires=` them or is `PartOf=` them;
what only `Wants=` them keeps running. A unit stopped this way stays stopped —
through a manager crash, an upgrade and every `switch` — until it is started
again or you next sign in, when every enabled unit starts afresh.

**`restart`** stops and starts the units, and restarts what `Requires=` them
or is `PartOf=` them. A unit whose file changed starts with its new
definition.

`start` and `restart` wait, for up to 90 s, until none of the units is still on
its way up, and then say where each ended:

```console
> stewctl restart whkd
whkd.service: active
```

They exit 1 if any unit ended anywhere but `active`, or finished cleanly as a
oneshot does. `--no-block` returns as soon as the manager has the request.

## is-active

Prints each unit's state, one per line, and exits 0 if every one of them is
`active` and 3 otherwise, as `systemctl is-active` does — for scripts.

## switch

```console
stewctl switch
```

Reads the unit files and makes what runs match them, as home-manager's
`sd-switch` does:

- a **removed** unit is stopped;
- a **changed** unit that is running is restarted with its new definition —
  except a target or a timer, which takes its new definition at once without
  one;
- of the units at rest, what is **new** starts: a new unit, one a target now
  wants that it did not, or a `failed` one whose definition changed.

A unit you stopped on purpose stays stopped, since `switch` runs after every
apply that changes a unit, and an apply is no reason to undo a stop.

`--if-running` makes a session with no manager a success, doing nothing: the
next manager to start reads the units as they are. It is what the [home
module](@/installation.md#the-home-module) runs.

## daemon-reload

Also `reload`. Reads the unit files again, and only takes note: a removed unit
is stopped, a new one is loaded but not started, and a changed one keeps
running as it was started — its old `ExecStop=` included — marked changed until
it is restarted. `switch` afterwards applies what `daemon-reload` noted.

## logs

```console
stewctl logs whkd           # the last 50 lines
stewctl logs -n 200 whkd    # the last 200
stewctl logs -f whkd        # and keep printing what is appended
```

A unit's standard output and error go to your Event Log channel where the
machine has one ([below](#a-unit-in-the-event-log)), and otherwise, or when
its file says `StandardOutput=file`, to
`%LOCALAPPDATA%\steward\logs\<unit>.log`. Either way the manager writes its
own lines about the unit among the output — started, exited with code 3,
restarting in 5 s, failed — marked `-- <time> steward:`. `logs` reads the
file or the channel itself, so it works with the manager down, falling back
to this shell's `%LOCALAPPDATA%`. The file's path or the channel's name is
printed first, on standard error, so the output pipes clean; piping it into
something that stops reading early, like `Select-Object -First 3`, is fine.

A misspelt unit gets a suggestion:

```console
> stewctl logs komorebbi
stewctl: no unit named komorebbi.service; did you mean komorebi?
```

In a file, service output carries no timestamps of its own, since it goes
straight to the file rather than through the manager — which is what keeps a
service's output working through a manager crash. In the channel each line
is an event with its own time. A log file over 8 MiB is set aside as
`<unit>.log.1` and begun again, at a start and every 10 s while the unit
runs, replacing the previous `<unit>.log.1`; a line from steward near the
top of the new log says so. While the unit runs, the log is copied aside and
emptied in place, since its processes keep writing to the same file, and a
line written during the copy can be lost. The manager's own log,
`%LOCALAPPDATA%\steward\steward.log`, is set aside as `steward.log.1` past the
same size.

### A unit in the Event Log

On a machine with steward's elevated install, a unit writes to your Event
Log channel, `Steward/<your SID>`, one event per line, unless its file says
`StandardOutput=file`. Without that install there is no channel, and a unit
that does not say writes to its file, as above. `logs` reads the channel for
a unit whose output goes there, and prints the same thing it would from a
file: the lines in order, the manager's lines among them marked `-- <time>
steward:`, and the channel's name first, on standard error.

```console
> stewctl logs -n 3 komorebi
-- Steward/S-1-5-21-2571842103-1957994488-3489912835-1001 for komorebi.service
-- 2026-09-14 15:34:43.065 steward: active, main process 4242
2026-09-14T15:34:43.4104 INFO  komorebi::process_command: processing komorebic start
2026-09-14T15:34:43.5021 INFO  komorebi::window_manager: managing 4 windows
```

`-n` counts lines here too: the newest N events are read from the end of
the channel, which costs the same however much it holds. `-f` subscribes to
the channel and prints each of the unit's events as the Event Log takes it,
from the last line the tail printed, with nothing repeated or skipped in
between. The stream a line came from — standard output or standard error —
is a field of the event, which Event Viewer and `Get-WinEvent` show and
`logs` does not. If a line says the shim `steward-cat` lost output, it was
written while nothing was listening to the channel, which happens on an
account's first sign-in before the channel exists; how much was lost is in
the line.

A `%` in a line is kept in the channel as `％`, the fullwidth percent sign,
because the Event Log shows most lines with a `%` in them as empty. Event
Viewer and `Get-WinEvent` show `％`; `logs` prints `%` again.

Terminal escape sequences — colours, cursor movement, window titles — are
taken out of a line before it goes into the channel, so a program that
colours its output shows as plain text in Event Viewer and in `logs`. A
unit whose output goes to its file keeps them as the program wrote them.

`logs` asks the running manager where each unit's output goes. With no
manager it reads the unit file in `%APPDATA%\steward\units`, and for a unit
that does not say, it reads the channel if you have one and the file if you
do not, so it needs no manager for either. It reads the channel with
`EvtQuery` and `EvtRender` only. steward's provider has no message file, so
`wevtutil gp` and `Get-WinEvent` complain about that on every call; `logs`
never goes through the part that complains. Neither does Event Viewer: the
message is in the record, so its General tab shows the line and its Details
tab the fields. `Get-WinEvent`'s own `Message` property is the one place the
line does not appear — it reads "Cannot retrieve event message text." — so
read the event's `Properties` there, or use `wevtutil qe /f:text`, which
prints the line as the description. What a channel holds is bounded
by its size, 64 MiB unless the install chose otherwise
(`services.steward.eventlog.channelSize`, or `--channel-size` on the task),
which is roughly 50,000 short lines for all of your units together, or
about 28,000 lines of a daemon like komorebi, whose lines are longer; there
is no `.log.1`.

Until your channel exists, `logs` says so and exits 1. The manager asks the
provisioning task to create it as soon as it starts a unit and finds it
missing, which takes well under a second, and holds the units' output until
then; from then on the channel is there before the manager is. An install
that named your account (`services.steward.eventlog.accounts`, which is every
account winpkgs manages a home for, or `--account` on the task) made your
channel then, so you never see this at all.

If the manager cannot start a unit's `steward-cat` at all, that unit's output
falls back to its log file for the run, and `stewctl status` says so. `logs`
then reads the file, as for any `file` unit: it asks the manager where the
output went for this run rather than trusting the unit file alone.

## verify

```console
stewctl verify                          # every unit in %APPDATA%\steward\units
stewctl verify .\backup.timer .\backup.service
```

Parses unit files and prints every error and warning with its line, without a
manager. It exits 1 if any unit has an error, which would keep it from
loading. See [Checking a unit](@/units.md#checking-a-unit).
