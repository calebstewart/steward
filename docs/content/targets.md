+++
title = "Targets"
weight = 3
description = "The stages steward reaches at sign-in, targets of your own, and how units pull each other up and down."
+++

A target runs nothing. It is a name: units are installed into it with
`WantedBy=`, order themselves after it with `After=`, and bind themselves to it
with `PartOf=`. steward reaches four of its own as your session comes up, and
any `*.target` file is one of yours.

## Built-in targets

| Target | Reached | For |
| --- | --- | --- |
| `default.target` | as soon as the manager is up, at sign-in | anything that needs no desktop |
| `graphical-session.target` | once Explorer's taskbar window exists | anything with a window, a hotkey or a bar |
| `tray.target` | once the tray takes icons, about a second later | tray programs |
| `timers.target` | with `default.target`, which it is another name for | [timers](@/timers.md) |

Once reached, a built-in target stays reached for the session. No unit file may
be named after one; they are steward's.

The manager starts about 120 ms after `explorer.exe`, well before the desktop
and taskbar exist, which is why the stages are needed at all. A unit that needs
the shell says so:

```ini
[Unit]
After=graphical-session.target

[Install]
WantedBy=graphical-session.target
```

`graphical-session.target` and `tray.target` are home-manager's names, so units
shared with a Linux home that order after them or require them load
unchanged.

### When the tray is ready

The taskbar exists about a second before its notification area will take an
icon, and a tray program started in between fails to add one. So
`tray.target` is not reached with the taskbar: steward waits for Explorer's
`TaskbarCreated` broadcast, which Explorer sends only once the tray takes
icons — the same message tray programs listen for to add their icons again.

A manager that started after that broadcast — one handed over to by an
upgrade, or restarted by the SCM — cannot have heard it. If the taskbar was
already there when the manager began listening, the tray counts as ready 10 s
after Explorer started, which for such a manager is at once. The same limit
applies, with a warning in the manager's log, if the broadcast never comes, so
a tray program is delayed rather than never started.

When Explorer restarts, `tray.target` stays reached and nothing is restarted:
tray programs hear the same broadcast and add their icons again.

## Targets of your own

A `*.target` file is `[Unit]` and `[Install]` only. Started, a target is
`active`; stopped, it is `inactive`; there is never a process. What it does is
make a group. The tiling window manager's daemons, for instance:

```ini
# tiling.target
[Unit]
Description=Tiling window management
After=graphical-session.target

[Install]
WantedBy=graphical-session.target
```

Each member says two things: `WantedBy=tiling.target`, so starting the target
starts it, and `PartOf=tiling.target`, so stopping or restarting the target
does the same to it.

```ini
# whkd.service
[Unit]
Description=Hotkey daemon
After=graphical-session.target
PartOf=tiling.target

[Service]
ExecStart="C:\Program Files\whkd\bin\whkd.exe"
KillMode=process

[Install]
WantedBy=tiling.target
```

With komorebi, whkd and the bar all written that way, the whole stack is put
away for a game and brought back with one command each:

```console
stewctl stop tiling.target
stewctl start tiling.target
```

A unit only `WantedBy=` the target, without `PartOf=`, is started with it but
left running when it stops.

A target's new definition takes effect as soon as the units are read again —
nothing runs the old one — and `stewctl switch` does not restart a changed
target, which would restart its whole group for an edited description.

## How units relate

Take two units, A and B, and one line in A:

| A says | Starting A | Stopping B on purpose | Restarting B | Order |
| --- | --- | --- | --- | --- |
| `Wants=B` | starts B too | A keeps running | A keeps running | none |
| `Requires=B` | starts B too; A does not start if B fails to | stops A | restarts A | none |
| `PartOf=B` | — | stops A | restarts A | none |
| `After=B` | — | — | — | A starts after B, and stops before it |
| `WantedBy=B`, in `[Install]` | — | A keeps running | A keeps running | none; starting B starts A |

These are systemd's relations, so a unit keeps its meaning between the two.
The things worth knowing:

- **Nothing orders by default.** `Wants=` and `Requires=` pull a unit up but
  let it start alongside; add `After=` when A needs B *up* first.
- **Only a stop someone asked for propagates.** B crashing does not stop what
  `Requires=` it; B is restarted on its own policy, and A carries on.
- **A group is `WantedBy=` plus `PartOf=`**, as above. `Requires=` in place of
  `PartOf=` would stop and restart with the target just the same, but starting
  one member on its own would then start the target — and with it the rest of
  the group.
- **A `WantedBy=` target with no file** is a warning, as in systemd: nothing
  can start it, so what it wants is not started either.

A stop of everything — sign-out, or Ctrl+C to a console manager — stops every
unit in the reverse of the order they start in. It is not a stop of each unit:
at the next sign-in, everything that is wanted starts afresh.
