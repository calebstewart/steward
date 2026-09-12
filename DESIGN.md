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

- Timers (replacing per-user Scheduled Tasks).
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

Not yet observed: what an instance receives at sign-out, and how long it has.

### Consequences

- **Services start on the desktop by default.** The manager starts processes
  normally; they inherit `WinSta0\Default`.
- **A "shell is ready" stage is required.** The manager is up before Explorer's
  desktop and taskbar exist. Units that need them order themselves after
  `graphical-session.target`, which steward reaches when the taskbar window
  (`Shell_TrayWnd`) exists. Explorer re-broadcasts `TaskbarCreated` when it
  restarts, which is how tray-icon services can be told to re-register.
- **The service name is not an address.** It changes every sign-in; `stewctl`
  finds the manager through its named pipe.
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
   named job object created *without* `KILL_ON_JOB_CLOSE`. The manager records
   each service's job name, main PID and process creation time (PIDs are
   reused; the pair is not) in a state file. A restarted manager re-opens the
   jobs and adopts the processes instead of starting duplicates.
3. **The manager restarts services.** `Restart=`, `RestartSec=` with backoff,
   and `StartLimitBurst=`/`StartLimitIntervalSec=`; a service that exhausts its
   limit is `failed`, shown as such, and stays down until started again.
4. **A stop is deliberate.** A service stopped with `stewctl` stays stopped
   until it is started or the user signs in again; enabled units start at
   every sign-in.
5. **Upgrades are ordinary.** The system configuration (elevated) stops the
   instance, replaces `steward.exe`, and starts it again; by layer 2 the
   services never notice. A service's own binary can be replaced by stopping
   the unit first -- which fixes today's "file in use" failures when winpkgs
   mirrors a portable package over a running program.

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
  (`komorebic stop`, `thide stop`); Ctrl+Break for console programs (each is
  started in its own process group); `WM_CLOSE` to the top-level windows of GUI
  programs; after `TimeoutStopSec=`, terminate the job.
- **No console windows.** Console programs are started with `CREATE_NO_WINDOW`
  and their stdout/stderr piped to the manager -- no `conhost --headless`, and
  no Windows Terminal window from the default-terminal handoff. A pseudoconsole
  mode can come later for programs that behave differently without a terminal.

## Logs

A per-user journal under `%LOCALAPPDATA%\steward\logs`: append-only, rotated,
one record per line of service output (time, unit, invocation, PID, stream)
interleaved with the manager's own records (started, exited with code N,
restarting in 5 s, failed). `stewctl logs [-f] <unit>` reads it through the
manager. The Windows Event Log needs an administrator to register a source;
it may carry state transitions later, installed with the template.

## Control plane

A named pipe, `\\.\pipe\steward-<user SID>`, whose DACL admits only the user.
The manager creates it with `FILE_FLAG_FIRST_PIPE_INSTANCE` so nothing can
squat the name first, and `stewctl` checks the server process's identity
before trusting it. Requests and replies are line-delimited JSON. The verbs
follow `systemctl`: `start`, `stop`, `restart`, `status`, `list-units`,
`enable`, `disable`, `daemon-reload`, `logs`, `is-active`.

## Unit files

Units use systemd's syntax -- sections, `Key=Value`, `#`/`;` comments,
backslash continuation, repeated keys that accumulate, an empty assignment
that resets a list -- and systemd's section and key names wherever the meaning
carries over. It is familiar, pleasant to write by hand, and home-manager
already renders `systemd.user.services` in exactly this form.

They live in `%APPDATA%\steward\units\*.service` (the XDG config home, as
winpkgs lays it out on Windows).

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
`Before`, `Wants`, `Requires`, `StartLimitBurst`, `StartLimitIntervalSec`;
`[Service]` `Type` (`simple`, `exec`, `forking`, `oneshot`), `ExecStart`,
`ExecStartPre`, `ExecStartPost`, `ExecStop`, `Restart`, `RestartSec`,
`RestartSteps`, `RestartMaxDelaySec`, `TimeoutStartSec`, `TimeoutStopSec`,
`TimeoutSec`, `WorkingDirectory`, `Environment`, `KillMode`; `[Install]`
`WantedBy`.

**The defaults favour durability over systemd's.** A unit that says nothing
gets `Restart=on-failure` (systemd: `no`) and a backoff from `RestartSec=1s`
to `RestartMaxDelaySec=1min` over `RestartSteps=5` (systemd: 100 ms, flat).
With the default start limit (5 starts in 10 s), a service that keeps failing
is retried a minute apart indefinitely -- and shows its restart count --
instead of giving up half a second after sign-in. A unit that wants to fail
fast says so. Timeouts are shorter than systemd's: `TimeoutStartSec=30s`,
`TimeoutStopSec=10s`, because sign-out does not wait a minute and a half.

`Type=forking` requires `KillMode=control-group`: with `KillMode=process` the
job tracks only the main process, which is the one that exits.

Built-in targets: `default.target` (sign-in) and `graphical-session.target`
(the shell is ready).

## Nix and winpkgs

steward is its own flake. It exports the Windows binaries, cross-built
(`pkgsCross.mingwW64`; they import nothing but Windows' own DLLs), and an
overlay for package sets that already target Windows, such as `pkgs` inside a
winpkgs module. M3 adds winpkgs modules, for a consumer to import:
  - **system**: put `steward.exe` in `%ProgramFiles%\steward` and register the
    template. winpkgs has no resource for registering a service yet; one is
    needed (`sc create`/`sc config`/`sc failure`, stopping instances around a
    binary change).
  - **home**: write unit files into `%APPDATA%\steward\units` and have
    `stewctl daemon-reload` plus restarts of changed units follow an apply,
    the way home-manager's `sd-switch` does. Whether home-manager's own
    `systemd.user.services` can be the option surface (as `programs.gh` and
    `oh-my-posh` reuse home-manager's options) is to be checked.

Binaries that pass through winpkgs must not contain `/nix/store/` (its closure
build refuses such files); Rust embeds source paths in panic locations, so the
build remaps them.

## Roadmap

- **M0** -- this repository: design, the unit-file parser, a manager that
  runs as a per-user service (or in a console) and loads its units, a CLI that
  checks unit files, the flake.
- **M1** -- supervision: job objects, restart policy, stop ladder, state file
  and re-adoption, the graphical-session stage.
- **M2** -- control plane and journal: the pipe, `stewctl` verbs, logs.
- **M3** -- winpkgs integration: the service resource in winpkgs, the two
  modules, activation hooks.
- **M4** -- move whkd, komorebi, masir, Flow Launcher and thide off Run keys.
- **Later** -- timers, event triggers.

## Open questions

- **`%` in unit files.** systemd reads `%h`, `%u` as specifiers; Windows users
  write `%LOCALAPPDATA%`. Leaning towards systemd's meaning plus `${VAR}`,
  since `%VAR%` expansion belongs to cmd, not to `CreateProcess`. Undecided;
  v1 passes `%` through untouched.
- **Sign-out.** What the instance is sent (stop? shutdown? session change?)
  and the time it has to stop services in order.
- **After the failure actions run out.** Whether the SCM repeats the last
  action or gives up, and so how many to register.
- **Readiness.** Whether any Windows program is worth a `Type=notify`
  equivalent (a pipe named in the environment), or whether "a window of class
  X exists" is the readiness signal that matters here.
