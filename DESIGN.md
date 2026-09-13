# steward

A per-user service manager for Windows, in the spirit of `systemd --user`.
`steward` is the manager; `stewctl` is how you talk to it.

It exists to run the long-lived programs a Windows desktop depends on -- a
tiling window manager, a hotkey daemon, a launcher, a tray utility -- and to
keep them running.

## The goal is durability

A declared service is running whenever its user is signed in, unless someone
deliberately stopped it. That is the requirement everything else serves, and
it is the one Windows has no answer for:

- **Run keys and the Startup folder** start a program once, at sign-in, and
  forget it. If it crashes, exits, or is killed by an installer upgrading it,
  it stays gone until the next sign-in. Nothing records that it ever ran or
  why it stopped. It inherits whatever environment Explorer had at sign-in.
- **Scheduled Tasks** can "restart on failure", but the failure is the task's
  result, not the process crashing; the interval is a minute at the least; and
  a task's default execution limit (72 hours) stops a long-running program on
  its own. What happened is buried in the task history, when it is recorded
  at all.
- **Windows services** are durable -- the Service Control Manager restarts
  them -- but they run in session 0, which has no desktop: no hotkeys, no
  windows, no tray icons. Everything this project is for is out of reach.

So the design is judged first on whether a service comes back: after its own
crash, after the manager's crash, after an upgrade of either, after sign-out
and sign-in. Observability (what is running, what failed, why) and control
(start, stop, restart, in order) come next, because a durable service you
cannot see or stop is its own problem.

### Goals

1. **Durability.** Crashed services restart with backoff. A manager crash does
   not take its services down, and the manager itself is restarted by Windows.
   A service that keeps failing ends in a visible `failed` state, never a
   silent give-up.
2. **Observability.** Status, main PID, exit codes, restart counts, and the
   service's own output, per service, from one CLI.
3. **Control.** Start, stop and restart with dependency ordering; a stop that
   asks the program to exit before it is killed.
4. **Declarative.** Units are text files in systemd's syntax, hand-written or
   generated (by winpkgs, from Nix).

### Non-goals

- **Elevated services. Never.** A service runs with the user's own,
  non-elevated token. Something that needs elevation is a Windows service, and
  the SCM already manages those; a user service manager that hands out admin
  rights is a privilege-escalation tool.
- System-wide or session-0 services; replacing the SCM.
- Running services while the user is signed out ("lingering").

### Later, with room left for them now

- Event triggers: lock/unlock, sign-in/out, power source, network, display
  changes, file changes.

## Bootstrap: the manager is a per-user service

Since Windows 10 1709 the SCM supports *per-user services*: a service
registered as a template (`type= userown`) is instantiated for every user who
signs in, runs as that user, and is deleted at sign-out. Microsoft uses them
for its own per-user plumbing (`CDPUserSvc_*`, `WpnUserService_*`). Their
documentation is aimed at administrators disabling them, not at third parties
creating them, but `sc.exe create` accepts the type.

steward is registered that way, once, by an administrator:

```
sc create steward type= userown start= auto binPath= "\"C:\Program Files\steward\steward.exe\""
sc failure steward reset= 60 actions= restart/5000/restart/5000/restart/5000
```

After that, Windows starts `steward_<suffix>` in each user's session at
sign-in and restarts it if it dies. No Run key, no scheduled task, and no
privileged code of our own: the SCM *is* `user@.service`.

### What the spike established (gaming-windows, 2026-09-12)

A throwaway probe was registered as a `userown` template and observed across
a sign-out/sign-in, a crash, and a lock/unlock.

| Question | Answer |
|---|---|
| When do instances appear? | At sign-in only. Registering the template does not instantiate it for a session already signed in, and starting the template itself is refused. |
| Instance name | `<template>_<suffix>`, `Type` 0xD0 (0x50 user-own + 0x80 instance). The suffix is per sign-in (the same one Windows' own `CDPUserSvc_*` gets). |
| Identity | The user's own session, logon session LUID and logon SID -- the same as a shell started from Explorer. Medium integrity, limited (non-elevated) token. |
| Desktop | `WinSta0\Default`: the real interactive desktop. Global hotkeys registered by the service process itself, and by children it starts without any `lpDesktop`, all fire. |
| Timing | Starts about 120 ms after `explorer.exe`, before Explorer has created the desktop window (13 top-level windows at start, 330 later). |
| Environment | Built from the registry: the user's PATH (winpkgs' bin directory included), 45 variables, nothing from whoever registered the service. Working directory `C:\WINDOWS\system32`. |
| Crash | The template's failure actions are copied to the instance. An abrupt exit was followed by a new process 5.003 s later. |
| Rights | Interactive users may query the instance and send it user-defined controls (128-255), but not start or stop it (`sc sdshow`: `CCLCSWLOCRRC` for IU). `CDPUserSvc_*` is the same. |
| Session events | `SERVICE_CONTROL_SESSIONCHANGE` arrives for lock and unlock. |
| Sign-out | A plain `SERVICE_CONTROL_STOP`, about 180 ms before Winlogon logs the session off, and no logoff session change before it (observed with steward itself, 2026-09-12). The session's processes outlive the Stop by seconds: a service steward left running was still alive 10 s later. Stopping every service on that Stop works: `ping` was stopped in order in 16 ms, and the next session's manager started it afresh. |

Not yet observed: how long an instance has at sign-out before its session's
processes are ended.

### Consequences

- **Services start on the desktop by default.** The manager starts processes
  normally; they inherit `WinSta0\Default`.
- **A "shell is ready" stage is required.** The manager is up before Explorer's
  desktop and taskbar exist. Units that need them order themselves after
  `graphical-session.target`, which steward reaches when the taskbar window
  (`Shell_TrayWnd`) exists, and tray programs after `tray.target`, reached
  when Explorer broadcasts `TaskbarCreated` (see "When the tray is ready").
- **The service name is not an address.** It changes every sign-in; `stewctl`
  finds the manager through its named pipe.
- **A manager belongs to a session, not to a user.** Its services run on the
  session's desktop, and cannot move to another. Signing out and straight back
  in overlaps two sessions of the same user -- the new one's instance starts
  while the old one's is still stopping its services (5 s apart, observed) --
  and a user can also be signed in twice, at the console and over Remote
  Desktop. So the pipe, the state file and adoption are all per session: each
  session's manager starts its own services and never takes another's. The
  unit files and logs stay per user; two sessions running the same unit
  write to the same log.
- **Start and stop go through steward, not the SCM.** The user cannot stop the
  instance, and does not need to: the manager's lifecycle belongs to the
  system configuration (install, upgrade), the services' lifecycle to the
  user. (Granting interactive users start/stop on the template is possible
  with `sc sdset`; not done unless a need appears.)
- **Session and power events come through the SCM.** The service control
  handler already receives lock/unlock; no hidden window or polling needed for
  the event triggers later.
- **The environment is rebuilt for each start** (`CreateEnvironmentBlock` from
  the user's token), not taken from the manager's own sign-in snapshot, so a
  PATH changed after sign-in reaches services started afterwards. A service's
  default working directory is `%USERPROFILE%`, as systemd uses the home
  directory for user services.

## Durability, layer by layer

1. **The SCM restarts the manager.** Failure actions on the template.
2. **The manager's crash is not its services' crash.** Each service lives in a
   job object created *without* `KILL_ON_JOB_CLOSE`, so the jobs' processes
   outlive the manager's handles. The manager records every process in each
   job -- PID and creation time, since PIDs are reused and the pair is not --
   and which is the main one, in `%LOCALAPPDATA%\steward\state-<session>.json`,
   rewritten whenever a job's membership changes. A restarted manager opens
   the recorded processes that are still the same processes, and still in its
   session, and puts them in a new job, which Windows nests inside the
   orphaned one; it adopts them instead of starting duplicates.

   The first design named the jobs and re-opened them by name. That does not
   work: a job's name goes with its last handle, even while its processes run
   on (verified 2026-09-12: `OpenJobObject` fails with error 2 once the
   creating process has closed its handle).
3. **The manager restarts services.** `Restart=`, `RestartSec=` with backoff,
   and `StartLimitBurst=`/`StartLimitIntervalSec=`; a service that exhausts its
   limit is `failed`, shown as such, and stays down until started again.
4. **A stop is deliberate.** A service stopped with `stewctl` stays stopped
   until it is started or the user signs in again; enabled units start at
   every sign-in. A manager's crash or upgrade is neither. So the state file
   also records each unit at rest that ran (or was refused) in this
   sign-in, and what put it there: stopped, finished, failed, or, for a
   timer, spent. The manager that takes over leaves it there. A failed unit
   stays failed, as the last manager concluded; `switch` still retries one
   whose definition changed. A unit waiting out a restart delay is owed its
   restart, and the new manager starts it at once.

   Session numbers are reused, so the file also records when the session's
   user signed in, and a manager ignores what a file from another sign-in
   says about rests, targets and timers. It still takes back any of that
   file's processes still running in its session. A stop of everything --
   sign-out, Ctrl+C -- is not a stop of each unit: once done it removes the
   file, and while it runs the file keeps the rests from before it began.
   The next sign-in starts afresh, and so does a console manager started
   again after a Ctrl+C.
5. **Upgrades are ordinary.** A Stop from the SCM means stop: it is what
   sign-out sends, and the services are stopped in order. An upgrade instead
   sends the instance user-defined control 128, *hand over*: the manager
   detaches, leaving every service running and recorded, and stops. The
   system configuration (elevated) then starts the instance on the new
   `steward.exe`, which adopts them by layer 2; the services never notice.
   winpkgs sends it through `windows.services.<name>.restartControl`, in
   place of the Stop with which it restarts a changed service. A service's
   own binary can be replaced by stopping the unit first -- which fixes
   today's "file in use" failures when winpkgs mirrors a portable package
   over a running program.

## Supervising a process

- **Job objects are the cgroups.** One job per service tracks every
  descendant, so a program that starts the real daemon and exits
  (`komorebic start`) is handled like `Type=forking` without a PID file. Jobs
  also carry memory/CPU limits and report "all processes exited".
- **`KillMode=` matters more than on Linux.** A hotkey daemon or launcher
  starts the user's applications; if those land in its job, stopping whkd
  closes every terminal it ever opened. `KillMode=process` gives the job
  `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`: only the main process is tracked and
  its children are the user's. `KillMode=control-group` (the default) tracks
  the tree.
- **The stop ladder.** Windows has no SIGTERM. In order: `ExecStop=` if given
  (`komorebic stop`, `thide stop`); Ctrl+C to the job's consoles and
  `WM_CLOSE` to its processes' top-level windows; after `TimeoutStopSec=`,
  terminate the job. Ctrl+C can only be sent from a process attached to the
  target's console, and attaching would cost the manager its own, so a helper
  does it (`steward --ctrl-c <pid>...`). Every process on a console hears it,
  including `KillMode=process` children that share their parent's console; a
  program started with `start /b` ignores it and is terminated at the timeout.
- **Processes are created in their job** (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so
  nothing escapes in the instant before assignment, and inherit exactly two
  handles (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`): NUL for stdin and the unit's
  log for stdout and stderr. Jobs have `DIE_ON_UNHANDLED_EXCEPTION`, so a
  crash ends the process at once instead of waiting on an error-reporting
  dialog.
- **No console windows.** Console programs are started with `CREATE_NO_WINDOW`
  -- no `conhost --headless`, and no Windows Terminal window from the
  default-terminal handoff. Their console host lives in the job too and
  outlasts the program by a few milliseconds, which the manager allows for
  before deciding the program left something behind. A pseudoconsole mode can
  come later for programs that behave differently without a terminal.
- **One thread, one completion port.** Process exits (thread-pool waits on
  the process handles), job notifications, and the SCM's or the console's
  controls all arrive as packets on one port; deadlines are the wait's
  timeout. Job notifications are not guaranteed by Windows, so the manager
  also checks each job's process count at least once a second.

## Logs

Each unit's stdout and stderr go straight to
`%LOCALAPPDATA%\steward\logs\<unit>.log` through a handle the service's
processes inherit, not through a pipe to the manager: a manager crash cannot
break a service's output (a Rust program that `println!`s into a closed pipe
panics). The manager writes its own lines for the unit -- started, exited
with code N, restarting in 5 s, failed -- into the same file, marked
`-- <time> steward:`. A log over 8 MiB is set aside as `<unit>.log.1` when the
unit next starts. The manager's own log is `%LOCALAPPDATA%\steward\steward.log`.

`stewctl logs [-f] <unit>` reads the files directly, so it works with the
manager down. The price of files over a pipe is that service output carries
no timestamps of its own. The Windows Event Log needs an administrator to
register a source; it may carry state transitions later, installed with the
template.

## Control plane

A named pipe, `\\.\pipe\steward-<user SID>-<session>`, whose DACL admits
only the user and which refuses remote clients; `stewctl` talks to the
manager of the session it runs in. The manager creates its single instance
with `FILE_FLAG_FIRST_PIPE_INSTANCE`, owned by the user in so many words, and
reuses it client after client, so the name is never free to take; `stewctl`
opens it at `SecurityIdentification` and, before sending anything, reads the
pipe's owner and checks that it is the user. The owner comes from the
creator's token and a standard user can make nobody else the owner of what
they create, where a process ID (the first design) is reused, and another
user's process cannot be opened to ask whose it is. One request and one
response, each a line of JSON (`steward-ipc`). A thread serves the pipe and
hands each request to the manager's loop, so the manager's state stays on
one thread.

The pipe is also the manager's lock: a second manager in the same session
cannot create it and does not start (a `--console` manager while the
per-user service runs, say). Pipe names are machine-wide, so another account
could take the name first. A manager that finds the name taken connects and
runs the client's owner check: the user's own pipe means another manager of
theirs, and it exits; anyone else's, or one it cannot open, it logs and tries
again with a growing delay (up to a minute) until the name is free or it is
told to stop. It does not exit, because the SCM starts a manager once per
sign-in and its failure actions apply to crashes, not to stops (winpkgs does
not set `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG`); a clean exit would leave the
session without a manager for as long as the squatter stayed. A manager that
cannot serve the pipe for another reason -- an invalid `STEWARD_PIPE`, which
must be a single name -- stops with a service-specific exit code, for the
record.

The verbs follow `systemctl`: `list-units` (the default), `list-timers`,
`status [unit...]`, `start`, `stop`, `restart` (waiting for the units to
settle unless `--no-block`), `is-active`, `daemon-reload`, and
`logs [-f] [-n N]`, which reads the log file itself. `whkd` means `whkd.service`. Two differ:

- **`switch`** reads the unit files and makes what runs match them, as
  home-manager's `sd-switch` does: removed units stop, changed running units
  restart with their new definition, and of the units at rest, what is new
  starts -- a new unit, one a target newly wants, or a failed one whose
  definition changed. A unit stopped on purpose stays stopped, since `switch`
  runs after every apply that changes a unit, and an apply is no reason to
  undo a stop. A unit a takeover left at rest is to `switch` like any other
  at rest. `daemon-reload` alone only takes note: a
  changed unit keeps running as it was started (its `ExecStop=` included)
  until it is restarted, and is marked changed until then.
- **No `enable`/`disable`.** A unit is enabled by its `[Install] WantedBy=`;
  the unit files are declared (by Nix), so there is no second source of truth
  to keep. A unit is stopped for the session with `stop`, and for good by
  removing it.

## Unit files

Units use systemd's syntax -- sections, `Key=Value`, `#`/`;` comments,
backslash continuation, repeated keys that accumulate, an empty assignment
that resets a list -- and systemd's section and key names wherever the meaning
carries over. It is familiar, pleasant to write by hand, and home-manager
already renders `systemd.user.services` in exactly this form.

They live in `%APPDATA%\steward\units\*.service` (the XDG config home, as
winpkgs lays it out on Windows), with `*.target` and `*.timer` beside them.

```ini
[Unit]
Description=Hotkey daemon
After=graphical-session.target

[Service]
ExecStart="C:\Program Files\whkd\bin\whkd.exe"
Restart=on-failure
KillMode=process

[Install]
WantedBy=graphical-session.target
```

Where Windows differs:

- **`Exec*=` values are Windows command lines**, handed to `CreateProcessW`
  as written. Windows programs parse their own command lines, and systemd's
  word splitting treats `\` as an escape, which would mangle every path. A
  leading `-` (ignore failure) is the one prefix kept.
- **`Environment=` quoting** groups with `"` or `'`, and a backslash is an
  ordinary character, for the same reason.
- **Unknown keys are warnings, not errors**, so a unit written for Linux still
  loads and says what it ignored. `X-` sections and keys are ignored silently,
  as systemd does.

Version 1 understands: `[Unit]` `Description`, `Documentation`, `After`,
`Before`, `Wants`, `Requires`, `PartOf`, `StartLimitBurst`, `StartLimitIntervalSec`;
`[Service]` `Type` (`simple`, `exec`, `forking`, `oneshot`), `ExecStart`,
`ExecStartPre`, `ExecStartPost`, `ExecStop`, `Restart`, `RestartSec`,
`RestartSteps`, `RestartMaxDelaySec`, `TimeoutStartSec`, `TimeoutStopSec`,
`TimeoutSec`, `WorkingDirectory`, `Environment`, `KillMode`; `[Timer]`
`OnCalendar`, `OnActiveSec`, `OnBootSec`, `OnStartupSec`, `OnUnitActiveSec`,
`OnUnitInactiveSec`, `Unit`, `Persistent`, `RandomizedDelaySec`,
`FixedRandomDelay`, `RemainAfterElapse`, `AccuracySec`; `[Install]`
`WantedBy`.

**The defaults favour durability over systemd's.** A unit that says nothing
gets `Restart=on-failure` (systemd: `no`) and a backoff from `RestartSec=1s`
to `RestartMaxDelaySec=1min` over `RestartSteps=5` (systemd: 100 ms, flat).
With the default start limit (5 starts in 10 s), a service that keeps failing
is retried a minute apart indefinitely -- and shows its restart count --
instead of giving up half a second after sign-in. A unit that wants to fail
fast says so. Timeouts are shorter than systemd's: `TimeoutStartSec=30s`,
`TimeoutStopSec=10s`, because sign-out does not wait a minute and a half.

A console program ended by a Ctrl+C that steward did not send, or by its
console closing (`STATUS_CONTROL_C_EXIT`), has *failed*, where systemd counts
SIGINT as clean: nothing a user does sends a windowless service Ctrl+C, so
one steward did not send is something going wrong, and the service should
come back. (A session's end is not that sender: a `ping` left running when
its session was signed out exited with code 0, observed 2026-09-12.) The
Ctrl+C steward sends when it stops a service is part of a stop that was
asked for, and that stop ends it cleanly.

`Type=forking` requires `KillMode=control-group`: with `KillMode=process` the
job tracks only the main process, which is the one that exits.

Built-in targets: `default.target` (sign-in), `graphical-session.target`
(the shell is ready), and `tray.target`, home-manager's name for "the tray is
there" (the tray takes icons, a moment after the shell is ready), so units
shared with a Linux home that order after it or require it load unchanged.
`timers.target`, where timers are installed, is another name for
`default.target`. Requiring a built-in target waits for it to be reached.
Once reached, a built-in target stays reached for the session.

**Targets of the user's own** are `*.target` files: `[Unit]` and `[Install]`
only, and nothing to run -- started, a target is active; stopped, it is not.
In the plan a target is a unit like any other. `WantedBy=` it is its
`Wants=`, so starting it starts what is installed into it, and a target
`WantedBy=` a built-in one comes up with it. The relations follow systemd, so
units keep their meaning between the two: stopping or restarting a unit on
purpose does the same to what `Requires=` it or is `PartOf=` it, and what
merely `Wants=` it keeps running. A group -- the tiling stack, stopped for a
game -- is therefore a target its members are `WantedBy=` and `PartOf=`. A
target's definition changes at once on a reload (nothing runs the old one),
and `switch` does not restart it, which would restart its group for an
edited description. An active target is recorded in the state file with the
services' processes, so the next manager has it active too. A target named
only in `WantedBy=`, with no file, is a warning, as in systemd: it cannot be
started.

### When the tray is ready

The taskbar window exists a second before its notification area takes an
icon, and a tray program started in between fails: `Shell_NotifyIcon(NIM_ADD)`
returns `E_FAIL`. thide did, at sign-in, until its restart a second later
(steward#3). Measured on gaming-windows (2026-09-12) with a probe that tried a
hidden icon every 20 ms:

| | sign-in | Explorer restarted |
|---|---|---|
| `Shell_TrayWnd` exists | 0 | 0 |
| `TrayNotifyWnd`, its notification area, exists | +75 ms | +61 ms |
| `NIM_ADD` fails with `E_FAIL` | 27 times, to +1.1 s | 22 times, to +1.0 s |
| `NIM_ADD` succeeds | +1120 ms | +1039 ms |
| `TaskbarCreated` arrives | +1143 ms | +1104 ms |

So `TrayNotifyWnd` is no later signal. `TaskbarCreated` is: Explorer
broadcasts it at its first start as well as after a restart, and only once
the tray takes icons. It is what tray programs already listen for to add their
icons again. steward hears it with a hidden top-level window of its own (a
message-only window gets no broadcasts), on a thread that pumps its messages,
and reaches `tray.target` with it. A probe like the one above would be exact
too, but even a hidden icon leaves a permanent entry in Settings' list of tray
icons.

A manager that started after the broadcast (a handover, a restart by the SCM)
cannot have heard it. If the taskbar was already there when the manager began
listening, the tray counts as ready 10 s after Explorer started, which for a
manager handed over to is at once. The same limit applies, with a warning,
if a manager that was listening in time never hears the broadcast, so that a
Windows that stopped sending it would delay tray programs instead of never
starting them.

When Explorer restarts, `tray.target` stays reached and nothing is restarted.
Tray programs hear the same broadcast and add their icons again; thide, a
tray-icon program, came through a restart untouched.

## Timers

A `*.timer` file starts another unit when it elapses, as systemd's timers
do, and is what a per-user Scheduled Task was for. It is `[Unit]`, `[Timer]`
and `[Install]`, and runs nothing itself: `Unit=` names what it starts, by
default the service named as the timer is. Timers are installed into
`timers.target`.

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

The triggers are systemd's, and so are their rules (`timer.c`):

- **`OnCalendar=`** takes systemd's calendar events (systemd.time(7)):
  `daily`, `Mon..Fri 09:00`, `*-*-01 03:30`, `*:0/15`, `*-02~01`. The
  events are in local time, or in UTC if they end in `UTC`. Other zones are
  refused: systemd names them from the IANA database, which Windows' own time
  zones are not. An event counts from the timer's last elapse, or from its
  start if it has not elapsed yet.
- **`OnActiveSec=`, `OnBootSec=`, `OnStartupSec=`** elapse once, counted
  from the timer's start, from boot, and from sign-in (the session's logon
  time, which is when steward starts). One that has already passed when the
  timer starts is due at once, unless the timer has elapsed before.
- **`OnUnitActiveSec=`, `OnUnitInactiveSec=`** count from the later of the
  unit's last start (or stop) and the timer's last elapse. Until one of
  those has happened they do not count at all, which is why they come with
  another trigger (`OnBootSec=5min`, `OnUnitActiveSec=1h`).
- **One run at a time.** Having elapsed, a timer waits for what it started
  to be at rest again before it can elapse again, so it never starts a unit
  that is still running from its last elapse.
- **Starting through the plan.** What it starts is started like any unit,
  its ordering included, and is ordered after the timer. Nothing else binds
  the two: stopping the timer leaves a run it started running.
- **The other keys.** `Persistent=`, `RandomizedDelaySec=`,
  `FixedRandomDelay=` and `RemainAfterElapse=` are as in systemd.
  `AccuracySec=` is accepted and has nothing to loosen (see below).
  `WakeSystem=` is not supported: a timer elapses once the machine is awake.

**Every time is the wall clock's.** systemd counts its relative timers on a
monotonic clock, which stops while the machine sleeps. steward counts time
asleep, as a Scheduled Task's repetition does: `OnUnitActiveSec=1h` means an
hour on the clock since the unit last started, however much of it the
machine slept. One clock also makes a schedule plain to record and to show.
The manager works each timer's next elapse out afresh on every turn of its
loop, which comes at least once a second, from what has happened: when the
timer started, when it last elapsed, and when its unit last started and
stopped. So an elapse missed asleep is due on waking, once however many
were missed, and a clock set right or a new time zone counts at once. That
is also why `AccuracySec=` has nothing to do: systemd uses it to put wake-ups
together, and steward is never more than a second late.

**A timer's schedule survives the manager.** An active timer's schedule is
recorded in the state file with the services' processes, so a manager that
takes over has the timer where the last one left it: a trigger already
spent does not elapse again, and an elapse missed in between is made up
once. A timer that went inactive once spent (`RemainAfterElapse=no`) is
recorded as a unit at rest, with its last elapse, and stays inactive.
`Persistent=` adds a stamp per timer,
`%LOCALAPPDATA%\steward\timers\<unit>`, which is the user's rather than the
session's. Starting the timer again reads the stamp, so a nightly job that
fell on a night spent signed out runs at sign-in.

A timer takes a changed definition at once, as a target does, and its next
elapse follows it; `switch` does not restart it. `stewctl list-timers` shows
each timer's next and last elapse and what it starts; `stewctl status` of a
timer shows the same.

## Nix and winpkgs

steward is its own flake. It exports the Windows binaries, cross-built
(`pkgsCross.mingwW64`; they import nothing but Windows' own DLLs), an
overlay for package sets that already target Windows, such as `pkgs` inside a
winpkgs module, and two winpkgs modules as `windowsModules` (winpkgs' own
name for its module trees), for a consumer to import. The system sets steward
up (`services.steward.enable`); a home only declares units, as it would where
home-manager runs on systemd, and has nothing to enable.

- **`windowsModules.system`** installs `steward.exe` and `stewctl.exe` in a
  fixed directory (`C:\Program Files\steward`), puts it on the machine PATH,
  and declares the template through winpkgs' `windows.services`: `userOwn`,
  started automatically, restarted by the SCM three times 5 s apart,
  `restartTriggers = [ package ]` and `restartControl = 128`.

  The directory is fixed because an instance cannot be changed: Windows
  copies the template into it at sign-in and refuses `ChangeServiceConfig` on
  it afterwards, even for no change at all (probed 2026-09-12). A versioned
  directory would reach a signed-in user only at their next sign-in. So an
  upgrade replaces the binaries in place -- winpkgs moves the running
  `steward.exe` aside to write the new one, and deletes it once nothing runs
  it -- and the new build's revision restarts every running instance with
  control 128: the old manager hands its services over, and the instance
  starts again from the same path, as the new manager, which adopts them.
- **`windowsModules.home`** writes home-manager's own
  `systemd.user.services`, `systemd.user.targets` and `systemd.user.timers`
  as unit files in `%APPDATA%\steward\units` -- all but the targets steward
  has built in, among them the `tray.target` home-manager declares
  everywhere.
  winpkgs evaluates home-manager's modules, so the option is there, and on
  Windows home-manager's systemd module is off, its units going nowhere.
  Units are free-form `Section.Key` attributes rendered as home-manager
  renders them, which steward reads as systemd would; `X-Restart-Triggers=`
  and `X-Reload-Triggers=`, which name store paths, are written as their
  hash, so a changed trigger still changes the file. The module also
  declares a winpkgs activation, triggered by the rendered units, that runs
  `stewctl switch --if-running` at the end of an apply that changed them --
  after pruning, so a removed unit's file is gone -- as home-manager runs
  `sd-switch`. `--if-running` makes no manager in the session a success (the
  next one reads the files as they are), and without `stewctl` on the PATH
  it does nothing, so a home applied before the system is harmless. A
  switch that finds no manager between a crash or a handover and the next
  manager is lost, not made up for. The next manager adopts a changed
  running unit without restarting it, and leaves a unit the last one left
  at rest there, even one now newly wanted, or failed and changed. A later
  switch sees only what changes after that manager started.

Binaries that pass through winpkgs must not contain `/nix/store/` (its closure
build refuses such files); Rust embeds source paths in panic locations, so the
build remaps them.

## Roadmap

- **M0** -- this repository: design, the unit-file parser, a manager that
  runs as a per-user service (or in a console) and loads its units, a CLI that
  checks unit files, the flake.
- **M1** (done) -- supervision: job objects, restart policy, stop ladder,
  state file and re-adoption, the graphical-session stage. Exercised in
  `--console` mode with throwaway units: ordering, crash backoff, forking
  services, `KillMode=process`, a stop that needs the kill, and adoption of
  every service by a manager started after the first was killed.
- **M2** (done) -- control plane and logs: the pipe, `stewctl` verbs,
  `switch`, `logs -f`. Exercised against a `--console` manager: queries,
  stop/start/restart, a second manager refused, a unit edited, one added and
  one removed while running, and `daemon-reload` then `switch`.
- **M3** (done) -- winpkgs integration: the service resource with
  `restartControl`, replacing a running binary, activations, and the two
  modules. Exercised on the machine: the template registered, an instance at
  sign-in, a handover under the same PIDs, stop-all at sign-out; then from
  stewos, a system apply that replaced the running `steward.exe` in place
  (moved aside, the trash emptied once the old manager had handed over) and
  restarted the instance onto it, and a home apply that wrote a unit and ran
  `stewctl switch`.
- **M4** (done) -- the desktop's daemons off Run keys. winpkgs' `programs.whkd`,
  `programs.komorebi` and `programs.masir` gained `service.enable`, declaring
  them as `systemd.user.services` (komorebi.exe directly, stopped with
  `komorebic stop`, a unit per bar part of it); on gaming-windows they run in
  a `tiling.target` group, and thide on `tray.target`. Flow Launcher stays on
  its Run entry: its stub starts the versioned program and exits, and what
  the user launches from Flow is Flow's child, which a supervisor would stop
  with it. The first start showed the readiness question below in practice:
  the bars came up before komorebi listened, failed, and were restarted a
  second later.
- **M5** (done) -- timers: `*.timer` files, `OnCalendar=` and the relative
  triggers, `Persistent=`, `stewctl list-timers`, and a home's
  `systemd.user.timers`. Exercised against a `--console` manager: calendar,
  one-shot and chained timers elapsing, a persistent timer making up a
  night it missed, a sign-in timer due at once, a timer that stops once
  spent, a manager killed and replaced (every schedule kept, a missed elapse
  made up once), and timers edited and switched without a restart.
- **Later** -- event triggers.

## Open questions

- **`%` in unit files.** systemd reads `%h`, `%u` as specifiers; Windows users
  write `%LOCALAPPDATA%`. Leaning towards systemd's meaning plus `${VAR}`,
  since `%VAR%` expansion belongs to cmd, not to `CreateProcess`. Undecided;
  v1 passes `%` through untouched.
- **Sign-out's time budget.** Sign-out sends a Stop (see the spike), and the
  manager stops every service in order, reporting `STOP_PENDING` with a 30 s
  hint. How long Windows waits before ending the session's processes is not
  known yet; the manager logs each service it stops, so a sign-out with slow
  `ExecStop=` commands will show where the limit is.
- **After the failure actions run out.** Whether the SCM repeats the last
  action or gives up, and so how many to register.
- **Readiness.** Whether any Windows program is worth a `Type=notify`
  equivalent (a pipe named in the environment), or whether "a window of class
  X exists" is the readiness signal that matters here.
