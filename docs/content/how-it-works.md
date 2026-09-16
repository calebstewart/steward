+++
title = "How it works"
weight = 6
description = "Why a service comes back after its own crash, the manager's crash, an upgrade, and a sign-out — and the files that make it so."
+++

steward is judged first on one question: does a service come back? After its
own crash, after the manager's crash, after an upgrade of either, and after
sign-out and sign-in. This page is how each of those is answered. The [design
notes](https://github.com/calebstewart/steward/blob/main/DESIGN.md) go
further, with what was measured on a real machine before each part was built.

## The manager is a per-user service

Since Windows 10 1709 the Service Control Manager supports *per-user
services*: a service registered as a template is instantiated for every user
who signs in, runs as that user in their session, and is removed at sign-out.
Windows uses them for its own per-user plumbing, and steward is registered the
same way.

So the manager runs on the real interactive desktop, with the same identity,
logon session and non-elevated token as a program started from Explorer.
Global hotkeys registered by the services it starts fire; their windows and
tray icons appear. And the SCM restarts the manager if it dies.

A manager belongs to a **session**, not to a user. Signing out and straight
back in overlaps two sessions for a moment, and a user can be signed in twice,
at the console and over Remote Desktop — so the control pipe, the state file
and adoption are all per session, and each session's manager runs its own
services and never takes another's. Unit files and logs stay per user.

## Durability, layer by layer

1. **The SCM restarts the manager**, from the template's failure actions.
2. **A manager's crash is not its services' crash.** Each service runs in a
   job object created so that its processes outlive the manager. The manager
   records every process in every job — the PID *and* its creation time, since
   PIDs are reused and the pair is not — in a state file, rewritten whenever a
   job's membership changes. A new manager reopens the recorded processes that
   are still the same processes, still in its session, and adopts them instead
   of starting duplicates.
3. **The manager restarts services**, with `Restart=`, the backoff and the
   start limit. A service that exhausts its limit is `failed`, shown as such,
   and stays down until started again — never a silent give-up.
4. **A stop is deliberate.** A service stopped with `stewctl` stays stopped
   until it is started or you sign in again. A manager's crash or upgrade is
   neither, so the state file also records each unit at rest and what put it
   there — stopped, finished, failed, or a timer spent — and the manager that
   takes over leaves it there. A unit that was waiting out a restart delay is
   owed its restart, and the new manager starts it at once.
5. **Upgrades are ordinary.** An upgrade sends the manager control 128, *hand
   over*: it detaches, leaving every service running and recorded, and exits.
   The instance starts again on the new `steward.exe`, which adopts them by
   layer 2. The services never notice. A plain Stop from the SCM, which is
   what sign-out sends, stops every service in order.

Session numbers are reused by later sign-ins, so the state file also records
when the session was signed in to. A manager ignores what a file from another
sign-in says about units at rest, targets and timers — the next sign-in starts
afresh — but still takes back any of its processes that are running in its
session.

## Supervising a process

- **Job objects are the cgroups.** One job per service tracks every descendant,
  so a program that starts the real daemon and exits is handled like
  `Type=forking`, without a PID file. `KillMode=process` lets children break
  away from the job instead, so they are yours.
- **Processes are created inside their job**, so nothing escapes in the instant
  before assignment, and inherit only the handles they need: `NUL` for
  standard input, and for standard output and error either the write ends of
  two pipes to the unit's shim or the unit's log file.
- **Output goes to the Event Log, but never through the manager.** Each unit's
  output is read by a small `steward-cat` of its own, which writes one event
  per line to your channel. A manager crash or an upgrade leaves it reading,
  so neither breaks a service's output — a program writing into a closed pipe
  can crash — and `stewctl logs` works with no manager at all. If a shim dies
  while its unit is still running, the manager starts another on the same
  pipes, within milliseconds and without the unit's writes ever failing; only
  the line the dead one was in the middle of is lost. After five replacements
  in a minute it stops and sends that unit's output to its log file. Where the
  machine has no channel for you, or a unit says `StandardOutput=file`,
  output goes straight to the unit's log file through an inherited handle.
- **One thread, one completion port.** Process exits, job notifications, and
  the SCM's or console's controls all arrive on one port, and the manager
  checks each job's process count at least once a second as well, since Windows
  does not guarantee job notifications.
- **Ctrl+C comes from a helper.** Only a process attached to a console can send
  it Ctrl+C, and attaching would cost the manager its own, so `steward
  --ctrl-c <pid>…` does it on the manager's behalf.

## The control pipe

`stewctl` finds the manager through a named pipe,
`\\.\pipe\steward-<user SID>-<session>`, whose access list admits only you and
which refuses remote clients. Before sending anything, `stewctl` checks that
the pipe was created by you: its owner is set from the creator's account, and
nobody but an administrator can make you the owner of what they create. A
request and its response are one line of JSON each.

The pipe is also the manager's lock: a second manager in the same session
cannot create it, and does not start. Pipe names are machine-wide, so another
account could create yours first; a manager that finds its name taken by
someone else logs whose it is and tries again, with a growing delay, until the
name is free. `STEWARD_PIPE` names a different pipe, which is how a test
manager runs beside the real one; it must be a single name, with no path
separators in it.

## Files

| Path | |
| --- | --- |
| `%APPDATA%\steward\units\` | Your units: `*.service`, `*.target`, `*.timer`. |
| `%LOCALAPPDATA%\steward\steward.log` | The manager's own log, always a file; set aside as `steward.log.1` past 8 MiB. |
| Event Log channel `Steward/<your SID>` | Each unit's output and steward's lines about it, one event per line, where the install gave you a channel: 64 MiB for all of your units unless the install says otherwise (`services.steward.eventlog.channelSize`), oldest overwritten. Readable by you, administrators and SYSTEM. Read by `stewctl logs`, Event Viewer or `Get-WinEvent`. |
| `%LOCALAPPDATA%\steward\logs\<unit>.log` | The same, for a unit that says `StandardOutput=file`, on a machine without the channels, or for a run whose `steward-cat` could not start or kept dying; set aside as `<unit>.log.1` past 8 MiB, at a start or during the run. |
| `%LOCALAPPDATA%\steward\state-<session>.json` | The session's processes, units at rest, active targets and timer schedules. Removed once a stop of everything completes. |
| `%LOCALAPPDATA%\steward\timers\<unit>` | When a `Persistent=` timer last elapsed. Per user, so it survives sign-out. |

The channels are machine state, written by the provisioning task that runs as
SYSTEM at every sign-in and once at the install, and shared by every account
on the machine:

| Path | |
| --- | --- |
| `%ProgramData%\steward\channels.man` | The manifest naming every channel the task has created: one per account that has signed in since the install, and one per account the install named ahead of time (`services.steward.eventlog.accounts`). It only grows, and it is what the uninstall removes. |
| `%ProgramData%\steward\provision-eventlog.log` | What the task's last run did, including any channel it could not enable. |
| `%SystemRoot%\System32\winevt\Logs\Steward%4<SID>.evtx` | Each channel's records. Charged to the machine, not to your profile, and left behind when an account is deleted until the uninstall or an administrator removes it. |
