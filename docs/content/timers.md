+++
title = "Timers"
weight = 4
description = "Starting a unit on a schedule, in place of a Scheduled Task: calendar events, relative triggers, and runs made up after sleep or sign-out."
+++

A `*.timer` file starts another unit when it elapses, as systemd's timers do.
It is what a per-user Scheduled Task was for, with the schedule written next
to the unit it runs and shown by the same tool.

```ini
# backup.timer, which starts backup.service
[Unit]
Description=Nightly backup

[Timer]
OnCalendar=*-*-* 03:00
Persistent=true

[Install]
WantedBy=timers.target
```

A timer is `[Unit]`, `[Timer]` and `[Install]`, and runs nothing itself. By
default it starts the service named as it is — `backup.timer` starts
`backup.service` — and `Unit=` names another. Timers are installed into
`timers.target`, which is reached at sign-in. The service it starts usually has
no `[Install]` section of its own: the timer is what starts it.

```ini
# backup.service
[Unit]
Description=Back up the documents folder

[Service]
Type=oneshot
Environment=RESTIC_REPOSITORY=D:\Backup RESTIC_PASSWORD_FILE=C:\Users\alice\.restic
ExecStart=restic backup "C:\Users\alice\Documents"
```

The command's exit code is what counts, so it has to exit 0 when it worked.
`robocopy`, for one, exits 1 after a successful copy; run a program like that
through a wrapper that says what success is.

## [Timer]

| Key | Default | |
| --- | --- | --- |
| `OnCalendar=` | | Elapse at the times a [calendar event](#calendar-events) names. |
| `OnActiveSec=` | | Elapse this long after the timer started. |
| `OnBootSec=` | | Elapse this long after Windows started. |
| `OnStartupSec=` | | Elapse this long after sign-in. |
| `OnUnitActiveSec=` | | Elapse this long after the unit it starts last started. |
| `OnUnitInactiveSec=` | | Elapse this long after the unit it starts last stopped. |
| `Unit=` | the service named as the timer | What to start: a `.service` or a `.target` of your own. |
| `Persistent=` | `false` | Make up an `OnCalendar=` elapse missed while the timer was not running — while you were signed out. |
| `RandomizedDelaySec=` | `0` | Put each elapse off by a random delay of up to this long. |
| `FixedRandomDelay=` | `false` | Use the same random delay every time, rather than a new one. |
| `RemainAfterElapse=` | `true` | Stay `active` once there is nothing more to wait for, rather than going `inactive`. |
| `AccuracySec=` | `1min` | Accepted, and has nothing to loosen: steward is never more than a second late. |

A timer needs at least one trigger, and may have several: it elapses at the
earliest of them. Every trigger key accumulates, and an empty assignment to
any of them — `OnCalendar=` — clears them all, as in systemd. `WakeSystem=` is
not supported: a timer elapses once the machine is awake.

## Calendar events

`OnCalendar=` takes systemd's calendar events (systemd.time(7)):

```text
[weekdays] [year-month-day] [hour:minute[:second]] [UTC]
```

A date left out is every day, a time left out is midnight, and seconds left out
are `:00`. Each field is `*`, a number, a range `a..b`, any of those with a
repetition `/n`, or a comma-separated list of them. Weekdays are `Mon`..`Sun`,
or spelled out.

| Event | Elapses |
| --- | --- |
| `daily` | every day at midnight |
| `Mon..Fri 09:00` | weekdays at nine |
| `Sat,Sun 10:00` | weekends at ten |
| `*-*-* 03:00` | every night at three |
| `*:0/15` | every quarter of an hour |
| `*-*-* 8..17:00` | on the hour, from eight to five |
| `*-*-01 03:30` | on the first of each month |
| `12-25` | every Christmas, at midnight |
| `*-02~01` | on the last day of February |

`~` in place of the last `-` counts the day back from the end of the month:
`~01` is the last day, `~07/1` each of the last seven. The shorthands
`minutely`, `hourly`, `daily`, `weekly`, `monthly`, `quarterly`,
`semiannually` and `yearly` (or `annually`) are what they say, at the start of
each; `weekly` is Monday.

Events are in local time, or in UTC if they end in `UTC`. Other time zones are
refused: systemd names them from the IANA database, which Windows' own zones
are not.

## How triggers count

- **`OnCalendar=`** counts from the timer's last elapse, or from its start if it
  has not elapsed yet.
- **`OnActiveSec=`, `OnBootSec=` and `OnStartupSec=`** elapse once, counted
  from the timer's start, from boot, and from sign-in. One that has already
  passed when the timer starts is due at once, unless the timer has elapsed
  before.
- **`OnUnitActiveSec=` and `OnUnitInactiveSec=`** count from the later of the
  unit's last start (or stop) and the timer's last elapse. Until one of those
  has happened they do not count at all, which is why they come with another
  trigger:

  ```ini
  [Timer]
  OnBootSec=5min
  OnUnitActiveSec=1h
  ```

- **One run at a time.** Having elapsed, a timer waits for what it started to
  be at rest again before it can elapse again, so it never starts a unit that
  is still running from its last elapse.
- **What it starts is started like any unit**, its ordering and requirements
  included, and ordered after the timer. Nothing else binds the two: stopping
  the timer leaves a run it started running.

### The wall clock, time asleep included

systemd counts its relative timers on a monotonic clock, which stops while the
machine sleeps. steward counts time asleep, as a Scheduled Task's repetition
does: `OnUnitActiveSec=1h` means an hour on the clock since the unit last
started, however much of it the machine slept.

The manager works out each timer's next elapse afresh at least once a second,
from what has happened: when the timer started, when it last elapsed, and when
its unit last started and stopped. So an elapse missed asleep is due on waking —
once, however many were missed — and a clock set right, or a new time zone,
counts at once.

### Surviving the manager and sign-out

An active timer's schedule is recorded with the rest of the manager's state, so
a manager that takes over after a crash or an upgrade has every timer where the
last one left it: a trigger already spent does not elapse again, and an elapse
missed in between is made up once.

Across sign-out, only `Persistent=` remembers. It keeps a stamp of each
elapse in `%LOCALAPPDATA%\steward\timers\`, which belongs to you rather than
to one session, and a timer starting at sign-in reads it — so a nightly job
whose night was spent signed out runs as soon as you sign in.

## Seeing them

```console
> stewardctl list-timers
NEXT                     LEFT          LAST                     PASSED         UNIT          ACTIVATES
Sun 2026-09-13 14:31:00  in 42s        Sun 2026-09-13 14:30:00  17s ago        hello.timer   hello.service
Mon 2026-09-14 03:00:00  in 12h 29min  Sun 2026-09-13 03:00:00  11h 30min ago  backup.timer  backup.service
```

The soonest comes first. `stewardctl status backup.timer` shows the same for one
timer, with its state: `waiting` for its next elapse, `running` while what it
started runs, or `elapsed` once it has nothing left to wait for.

A changed timer takes its new definition as soon as the units are read again,
and its next elapse follows it; `stewardctl switch` does not restart a timer.
