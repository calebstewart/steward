# steward

A per-user service manager for Windows, in the spirit of `systemd --user`: it
starts at sign-in as a per-user service, keeps your desktop daemons running,
and tells you what they are doing.

- `steward` -- the manager, hosted by the SCM as a per-user service.
- `stewctl` -- the command line.

The documentation is at **<https://calebstew.art/steward>**: installation,
every unit key, targets, timers and every `stewctl` command. See
[DESIGN.md](DESIGN.md) for the design, what has been established on a real
machine, and the roadmap. Supervision (M1) and the control plane (M2) are
done, and so are integration with winpkgs (M3), the desktop's daemons
running under it (M4), and timers (M5).

## Units

Units are systemd's syntax, in `%APPDATA%\steward\units\*.service` (and
`*.target` and `*.timer`, below):

```ini
[Unit]
Description=Hotkey daemon
After=graphical-session.target

[Service]
ExecStart="C:\Program Files\whkd\bin\whkd.exe"
KillMode=process

[Install]
WantedBy=graphical-session.target
```

`ExecStart=` is a Windows command line, as written. A unit that says nothing
about restarting is restarted when it fails, with a backoff from a second to a
minute. `default.target` is reached at sign-in, `graphical-session.target`
once Explorer's taskbar exists, and `tray.target` once the tray takes icons,
about a second later: a tray program orders itself `After=tray.target`.
`stewctl verify` checks unit files without a manager.

A `*.target` file is a target of your own, which runs nothing: `[Unit]` and
`[Install]` only. Units that say `WantedBy=` it start when it does, and units
that say `PartOf=` it stop and restart with it, so a target makes a group:

```ini
# tiling.target
[Unit]
Description=Tiling window management

[Install]
WantedBy=graphical-session.target
```

With `WantedBy=tiling.target` and `PartOf=tiling.target` in komorebi's,
whkd's and masir's units, `stewctl stop tiling.target` puts all three away and
`stewctl start tiling.target` brings them back. As in systemd, stopping a
unit also stops what `Requires=` it; what only `Wants=` it keeps running.

A `*.timer` file starts a unit when it elapses -- by default the service
named as it is -- in place of a Scheduled Task:

```ini
# backup.timer, which starts backup.service
[Timer]
OnCalendar=*-*-* 03:00
Persistent=true

[Install]
WantedBy=timers.target
```

`OnCalendar=` takes systemd's calendar events (`daily`, `Mon..Fri 09:00`,
`*:0/15`), in local time or UTC; `OnBootSec=`, `OnStartupSec=` (from
sign-in), `OnActiveSec=`, `OnUnitActiveSec=` and `OnUnitInactiveSec=` count
from what they say, on the wall clock, time asleep included. `Persistent=`
makes up a run missed while you were signed out. `stewctl list-timers` shows
when each timer next elapses.

[`examples/`](examples) has units to try, each saying what it shows: a console
program stopped with Ctrl+C, a crash loop and its backoff, ordering after
another unit and the shell, whkd as a real daemon in a tiling target, and a
timer.

## Using it

```
stewctl                    # list the units
stewctl list-timers        # the timers: when each next elapses, and last did
stewctl status whkd        # one unit in detail, with the end of its log
stewctl start|stop|restart whkd
stewctl logs -f whkd       # its output, and steward's lines about it
stewctl switch             # re-read the units; restart the changed, start the new
```

Each unit's output goes to `%LOCALAPPDATA%\steward\logs\<unit>.log`; the
manager's own log is `%LOCALAPPDATA%\steward\steward.log`.

## Building

On Windows, with a Rust toolchain:

```
cargo build --release
cargo test
```

With Nix (on Linux or in WSL), cross-compiled for Windows:

```
nix build          # result/bin/steward.exe, result/bin/stewctl.exe
nix flake check    # the platform-free crates' tests natively, the Windows build, and the docs
```

The documentation site is [Zola](https://www.getzola.org/) in `docs/`, built
by `nix build .#docs` and deployed to GitHub Pages from `main` by
`.github/workflows/pages.yml`. To preview it, `zola serve` in `docs/` (with
`nix shell nixpkgs#zola`, say) serves it at http://127.0.0.1:1111.

## Trying the manager without installing it

```
steward --console
```

runs the manager in the foreground until Ctrl+C, which stops every service
(a second Ctrl+C leaves them running for the next manager to adopt). Only one
manager runs per session. To try it without touching your own directories, point
`APPDATA` and `LOCALAPPDATA` at scratch directories in that console first: the
unit directory, logs and state follow them (the services still get your real
environment). Beside a manager that already runs in the session, also set
`STEWARD_PIPE` to a name of your own (a single name, no path separators), for
it and for the `stewctl` that talks to it.

## Installing it with winpkgs

The flake exports two [winpkgs](https://github.com/calebstewart/winpkgs)
modules. The system one installs steward and registers it; the home one
writes a user's units from home-manager's `systemd.user.services`:

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

A system apply installs steward in `C:\Program Files\steward` (on the
machine PATH) and registers the template; the first manager starts at the
next sign-in. A later build is installed in place and the running managers
hand their services to the new one. A home apply that changes the units runs
`stewctl switch`, so the running manager restarts what changed, starts what
is new and stops what is gone; a unit you stopped stays stopped.

## Installing it as a per-user service by hand

Once, from an administrator prompt:

```
mkdir "C:\Program Files\steward"
copy steward.exe "C:\Program Files\steward"
copy stewctl.exe "C:\Program Files\steward"
sc create steward type= userown start= auto binPath= "\"C:\Program Files\steward\steward.exe\""
sc failure steward reset= 60 actions= restart/5000/restart/5000/restart/5000
sc sdset steward D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLORC;;;IU)(A;;CCLCSWLORC;;;SU)
"C:\Program Files\steward\steward.exe" provision-eventlog --install
```

The `sc sdset` line is Windows' default descriptor for a service minus one
right for interactive users: sending user-defined controls, which would let
any other user signed in to the machine send your manager the hand-over
control below and leave your session without one until your next sign-in.

The last line registers a Scheduled Task, `steward-provision-eventlog`, and
runs it once for whoever is signed in already. It runs as SYSTEM at the logon
of any user and gives that user an Event Log channel of their own,
`Steward/<their SID>`, which their units will write their output to; it is
readable and writable by them, by administrators and by SYSTEM, and by nobody
else, so one account on the machine cannot read another's. Creating a channel
is administrative and the manager is not, which is why this is a task and not
something the manager does at sign-in.

Run the copy you installed, as above, and not the one you copied from. The
task runs whatever path you ran it as, and the manifest names that path as
the channels' resource file, which the Event Log service reads as itself
(`NT SERVICE\EventLog`) -- so it has to be somewhere that account can read.
`C:\Program Files\steward` is; a download folder under your profile is not,
and getting it wrong makes `wevtutil gp` and `Get-WinEvent` complain about
access on every call even though the events still arrive.

The task takes no arguments and does
the same thing every time, so it is registered with a descriptor that lets
users run it but not change it -- it runs as SYSTEM, and a user who could
repoint it could run anything as SYSTEM. Running it again changes nothing
unless someone has signed in who had not before.

Channels only accumulate. Signing out keeps yours, which is the point -- the
log outlives the session -- but so does deleting the account: its channel and
its `.evtx` stay until someone removes them by hand (`wevtutil sl` and
`wevtutil um`, or the uninstall below, which removes all of them).

Windows starts an instance, `steward_<suffix>`, at every sign-in, so sign out
and in. Anything a unit now runs should no longer be started by a Run key or
the Startup folder, or it will run twice.

Stopping the instance stops every service, as signing out does. To upgrade
without stopping them, hand over to the new manager instead: `sc control
steward_<suffix> 128` (the manager detaches and exits, its services left
running), copy the new binaries in, and `sc start steward_<suffix>`; the new
manager adopts them. To remove it: `steward.exe provision-eventlog
--uninstall`, `sc stop` and `sc delete` the instance, `sc delete steward`,
and delete the directory. The uninstall deletes the task and every channel
steward made, and a channel takes its records with it, so copy out anything
you want to keep first.

## License

MIT; see [LICENSE](LICENSE).
