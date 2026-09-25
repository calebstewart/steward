+++
title = "Units"
weight = 2
description = "Unit files: where they live, how they are read, and every key a service can use."
+++

A unit is a text file in systemd's syntax, and steward uses systemd's section
and key names wherever the meaning carries over — so a unit written for a
Linux home loads unchanged, and says what it ignored. There are three kinds,
told apart by the file's extension:

- **`.service`** runs a program. This page.
- **`.target`** runs nothing: it is a name other units hang off, which makes a
  group. See [Targets](@/targets.md).
- **`.timer`** starts another unit when it elapses. See [Timers](@/timers.md).

## Where units live

Every `*.service`, `*.target` and `*.timer` file in

```text
%APPDATA%\steward\units
```

is a unit, named after its file: `whkd.service`. That is the XDG config home as
winpkgs lays it out on Windows, and it is where the [home
module](@/installation.md#the-home-module) writes them. The directory is read
when the manager starts; after editing it, run
[`stewardctl switch`](@/stewardctl.md#switch) to make what runs match.

A unit is **enabled** by its `[Install]` section — `WantedBy=` a target that
starts — and there is no `enable` or `disable`: the files are the one source of
truth. A unit is stopped for the session with `stewardctl stop`, and for good by
removing its file.

## The syntax

- `[Section]` headers, then `Key=Value` lines. Whitespace around the key and
  the value is dropped.
- Lines starting with `#` or `;` are comments, as are blank lines.
- A line ending in `\` continues on the next line; the backslash becomes a
  space.
- A key that takes one value takes its last one. A key that takes a list —
  `After=`, `Wants=`, `Environment=`, `ExecStartPre=` and the like —
  accumulates across repeats, and an empty assignment (`After=`) resets it.
- Sections and keys starting with `X-` are ignored silently, as systemd does
  — though `switch` still notices when one changes. Anything else steward
  does not know is a warning, and the rest of the unit still loads.

> [!WARNING]
> The continuation rule is systemd's, and so is its trap on Windows: a value
> that *ends* in a backslash, like `WorkingDirectory=C:\Users\alice\`,
> continues onto the next line. Leave the trailing backslash off.

## Where Windows differs

- **`Exec*=` values are Windows command lines**, handed to `CreateProcessW` as
  written. Windows programs parse their own command lines, and systemd's word
  splitting treats `\` as an escape, which would mangle every path. Quote a
  path with spaces as you would at a prompt:
  `ExecStart="C:\Program Files\whkd\bin\whkd.exe"`. Left unquoted, Windows
  tries `C:\Program.exe` before the program you meant, so an unquoted path
  with a space is a warning. A leading `-` (ignore this command's failure) is
  the one prefix kept; `@`, `+`, `!` and `:` are errors.
- **`%` is passed through untouched.** It is neither a systemd specifier nor
  an environment variable: `%VAR%` expansion belongs to `cmd`, not to
  `CreateProcessW`. A command that needs it can go through `cmd.exe /d /c`.
- **`Environment=` quoting** groups words with `"` or `'`, and a backslash is
  an ordinary character, for the same reason.
- **Console programs get no window.** They are started with
  `CREATE_NO_WINDOW`: no console flashes up, and no Windows Terminal takes
  them over. Standard input is `NUL`; standard output and error go to the
  unit's log.

## [Unit]

| Key | Default | |
| --- | --- | --- |
| `Description=` | none | Shown by `stewardctl` beside the unit's name. |
| `Documentation=` | none | A list of URLs. Accepted; informational. |
| `After=`, `Before=` | none | Ordering only: this unit starts after (before) those that start in the same transaction, and stops in the reverse order. Neither starts anything. |
| `Wants=` | none | Starting this unit starts those too. Nothing else binds them: they may fail, or be stopped, and this unit runs on. |
| `Requires=` | none | Starting this unit starts those too, and this unit does not start if one of them fails. Stopping or restarting one of them on purpose stops or restarts this unit too. |
| `PartOf=` | none | Stopping or restarting one of those on purpose stops or restarts this unit too. Starting does not propagate; pair it with `WantedBy=` for that. |
| `StartLimitBurst=` | `5` | Starts allowed within `StartLimitIntervalSec=`. |
| `StartLimitIntervalSec=` | `10s` | The window the burst is counted over. A burst or an interval of `0` turns the limit off. |

`Wants=` and `Requires=` do not order by themselves: a unit that needs
another *up* first says both `Requires=` and `After=`. Requiring one of
steward's built-in targets waits for it to be reached. The relations are
covered with examples on the [Targets](@/targets.md#how-units-relate) page.

`StartLimitBurst=` and `StartLimitIntervalSec=` are also read from
`[Service]`, where systemd before 230 had them.

## [Service]

| Key | Default | |
| --- | --- | --- |
| `Type=` | `simple` | How the service is judged to be up — see [below](#types). |
| `ExecStart=` | *required* | The command line to run. Exactly one, except for `Type=oneshot`, which runs each in turn. |
| `ExecStartPre=`, `ExecStartPost=` | none | Commands run in turn before the main process starts, and after it is up. A failure fails the start, unless the command is prefixed with `-`. |
| `ExecStop=` | none | Commands run in turn to stop the service, before anything else is tried. See [Stopping](#stopping). |
| `ExecReload=` | none | Commands run in turn to have the running service take its configuration again, without a restart. See [Reloading](#reloading). |
| `Restart=` | `on-failure` | When an ending nobody asked for is followed by a restart — see [Restarting](#restarting). systemd's default is `no`. |
| `RestartSec=` | `1s` | The delay before the first automatic restart. |
| `RestartSteps=` | `5` | Restarts it takes for the delay to grow from `RestartSec=` to `RestartMaxDelaySec=`. `0` turns the backoff off. |
| `RestartMaxDelaySec=` | `1min` | The longest the delay grows to. |
| `TimeoutStartSec=` | `30s` | How long the whole start may take — `ExecStartPre=`, the main process, `ExecStartPost=` — before it has failed. A reload gets as long, as in systemd. |
| `TimeoutStopSec=` | `10s` | How long a stop may take before the job is terminated. |
| `TimeoutSec=` | | Sets both of the above. |
| `WorkingDirectory=` | `%USERPROFILE%` | The directory each command starts in. As written: `%` is not expanded here either. |
| `Environment=` | | `NAME=value` pairs, space-separated, added to the environment or replacing a variable of the same name (compared without regard to case, as Windows does). |
| `KillMode=` | `control-group` | Which processes are the service's — see [below](#killmode). |
| `StandardOutput=` | `eventlog`, where installed | Where its output and error go: `eventlog` (`journal` means the same), your Event Log channel — see [stewardctl logs](@/stewardctl.md#a-unit-in-the-event-log); or `file`, `%LOCALAPPDATA%\steward\logs\<unit>.log`. Left out, or empty, it is the channel on a machine whose install gives you one, and the file on a machine without the elevated install, such as one where steward only ever runs in a console. Anything else is a warning, treated as `file`. There is no `StandardError=`. |

steward's timeouts are shorter than systemd's 90 s because sign-out does not
wait a minute and a half. Any time can be `infinity`.

Each start gets an environment built afresh from your account, as a new
Explorer window would, rather than a copy of the manager's — so a `PATH`
changed after sign-in reaches every service started after the change. A
command run beside the main process — `ExecStartPost=`, `ExecReload=`,
`ExecStop=` — also gets `MAINPID`, the main process's ID, when steward knows
it: not once a `Type=forking` service's launcher has exited, and not for a
unit taken over from an earlier manager whose main process was already gone.
As with any variable, a command reads it through `cmd.exe /d /c ...
%MAINPID%` or its own code.

### Types

| `Type=` | The service is up… | And it is over when… |
| --- | --- | --- |
| `simple` | as soon as its process is created. | the main process exits. |
| `exec` | once `CreateProcessW` has succeeded — which on Windows is the same moment. | the main process exits. |
| `forking` | once the main process has exited cleanly, having started the real daemon. | every process in its job has exited. |
| `oneshot` | never: it runs each `ExecStart=` to completion, in turn, and then it is done. | its last command exits, which leaves it `inactive`. |

`Type=forking` needs no PID file: the service's job object tracks every
process the main one started, which is how `komorebic start` is supervised.
It requires `KillMode=control-group`, since with `KillMode=process` the job
tracks only the main process — the one that exits. `notify`, `notify-reload`,
`dbus` and `idle` are warnings, treated as `simple`.

### KillMode

`KillMode=` matters more on Windows than on Linux. A hotkey daemon or a
launcher starts *your* applications, and if those land in its job, stopping
whkd closes every terminal it ever opened.

- **`control-group`** (the default): the job tracks every descendant, and a
  stop ends them all.
- **`process`**: only the main process is the service's. Its children break
  away from the job and belong to you — the right mode for anything that
  launches programs on your behalf.

`mixed` is a warning, treated as `control-group`.

## [Install]

| Key | |
| --- | --- |
| `WantedBy=` | Targets that start this unit when they start: in effect, the target's `Wants=`. `default.target` starts at sign-in, `graphical-session.target` once the shell is ready, `tray.target` once the tray takes icons. |

A unit with no `WantedBy=` is not started at sign-in; something else has to
start it — another unit's `Wants=` or `Requires=`, a [timer](@/timers.md), or
`stewardctl start`. A target named in `WantedBy=` that has no file is a warning,
as in systemd: nothing can start it.

## Restarting

How a service ended decides whether `Restart=` restarts it:

| It ended… | Counted as | `on-failure` | `on-abnormal` | `on-success` | `always` |
| --- | --- | :-: | :-: | :-: | :-: |
| with exit code 0 | clean | | | ✓ | ✓ |
| with any other exit code | failure | ✓ | | | ✓ |
| by an exception (an NTSTATUS error code) | crash | ✓ | ✓ | | ✓ |
| by a Ctrl+C steward did not send, or its console closing | interruption | ✓ | ✓ | | ✓ |
| by not starting, or stopping, in time | timeout | ✓ | ✓ | | ✓ |
| by its command not starting at all | failure | ✓ | | | ✓ |
| `Type=forking`: by every process in its job exiting on its own | failure | ✓ | | | ✓ |

`Restart=no` never restarts. A stop you asked for is never followed by a
restart, whatever the policy; nor is being refused by the start limit, or a
unit it `Requires=` failing. `Type=oneshot` accepts only `no`, `on-failure`
and `on-abnormal`.

An interruption is a *failure*, where systemd counts SIGINT as clean. Nothing
a user does sends a windowless service Ctrl+C, so one that steward did not send
means something went wrong, and the service should come back. The Ctrl+C
steward sends when it stops a service is part of a stop that was asked for, and
ends it cleanly.

### The backoff

The delay grows exponentially from `RestartSec=` to `RestartMaxDelaySec=` over
`RestartSteps=` restarts. With the defaults:

| Restart | 1st | 2nd | 3rd | 4th | 5th | 6th and on |
| --- | --- | --- | --- | --- | --- | --- |
| Delay | 1 s | 2.3 s | 5.1 s | 11.7 s | 26.4 s | 1 min |

That backoff never packs five starts into ten seconds, so a service that keeps
failing is never refused by the default start limit: it is retried a minute
apart indefinitely, and `stewardctl` shows its restart count. A unit that wants to
give up says so, with a tighter start limit or `Restart=no`.

A service refused by its start limit is `failed`, and stays down until it is
started again. Starting a unit yourself resets its start limit and its backoff:
failures before it do not count against a start someone asked for.

## Stopping

Windows has no SIGTERM, so a stop is a ladder, and each rung is tried only if
the service is still running:

1. **`ExecStop=`**, if given, each command in turn — `komorebic stop`,
   `thide stop`.
2. **Ask.** Ctrl+C to the job's consoles, and `WM_CLOSE` to its processes'
   top-level windows.
3. **Terminate** the job, once `TimeoutStopSec=` has passed since the stop
   began.

Every process on a console hears the Ctrl+C, including `KillMode=process`
children that share their parent's console. A program started with `start /b`
ignores it, and is terminated at the timeout.

A service's own crash ends it at once: every job is set to die on an unhandled
exception rather than wait on an error-reporting dialog nobody will see.

## Reloading

A service that can take new configuration while it runs says how in
`ExecReload=`: `stewardctl reload` runs each command in turn, in the service's
job, and the main process carries on. On Linux the command is usually
`kill -HUP $MAINPID`; Windows has no signal to send, so a reload is whatever
the program provides — a `--reload` flag, a command-line client, a message to
its window found through `MAINPID`.

```ini
[Service]
ExecStart=notes-sync.exe --watch
ExecReload=notes-sync.exe --reload-config
ExecReload=-cmd.exe /d /c echo reloaded %MAINPID%
```

A reload is not a start. The unit stays `active` throughout (its state reads
`reload` while the commands run); it waits for nothing it is ordered after,
and counts toward no restart or start limit. A command that fails, unless it
is prefixed with `-`, ends the reload there: the failure is reported — in the
unit's log, in `stewardctl status`, and by `stewardctl reload` exiting 1 — and the
service stays up as it was. So does a reload that outlasts
`TimeoutStartSec=`: its command is terminated. A stop cuts a reload short,
terminating its command before `ExecStop=`.

Only an active service with `ExecReload=` can be reloaded; `stewardctl reload`
refuses anything else. `switch` reloads a unit whose file changed only in
`ExecReload=` or in home-manager's `X-Reload-Triggers=` — see
[stewardctl switch](@/stewardctl.md#switch).

## Time spans

Durations are systemd's: a bare number is seconds, or a sequence of numbers
with units — `90`, `500ms`, `5s`, `1min 30s`, `1h30min`, `2 days`, `infinity`.
The units are `us`, `ms`, `s`, `min` (or `m`), `h`, `d` and `w`, with their
long spellings.

## Checking a unit

```console
> stewardctl verify
C:\Users\alice\AppData\Roaming\steward\units\whkd.service: ok
C:\Users\alice\AppData\Roaming\steward\units\bar.service:
  line 7: warning: MemoryMax= is not supported in [Service]; ignored
2 unit(s): 0 with errors, 1 with warnings only
```

`stewardctl verify` reads every unit in the directory, or the files you name,
without a manager. A unit with an error does not load at all — the manager
logs why and runs the rest — so it exits non-zero if any unit has one.
