# steward

A per-user service manager for Windows, in the spirit of `systemd --user`: it
starts at sign-in as a per-user service, keeps your desktop daemons running,
and tells you what they are doing.

- `steward` -- the manager, hosted by the SCM as a per-user service.
- `stewctl` -- the command line.

See [DESIGN.md](DESIGN.md) for the design, what has been established on a real
machine, and the roadmap. Supervision (M1) and the control plane (M2) are
done; integration with winpkgs (M3) is under way.

## Units

Units are systemd's syntax, in `%APPDATA%\steward\units\*.service`:

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
minute. `graphical-session.target` is reached once Explorer's taskbar exists.
`stewctl verify` checks unit files without a manager.

[`examples/`](examples) has units to try, each saying what it shows: a console
program stopped with Ctrl+C, a crash loop and its backoff, ordering after
another unit and the shell, and whkd as a real daemon.

## Using it

```
stewctl                    # list the units
stewctl status whkd        # one unit in detail, with the end of its log
stewctl start|stop|restart whkd
stewctl logs -f whkd       # its output, and steward's lines about it
stewctl switch             # re-read the units; restart the changed, start the wanted
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
nix flake check    # the platform-free crates' tests natively, and the Windows build
```

## Trying the manager without installing it

```
steward --console
```

runs the manager in the foreground until Ctrl+C, which stops every service
(a second Ctrl+C leaves them running for the next manager to adopt). Only one
manager runs per session. To try it without touching your own directories, point
`APPDATA` and `LOCALAPPDATA` at scratch directories in that console first: the
unit directory, logs and state follow them (the services still get your real
environment).

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

# the home configuration
imports = [ inputs.steward.windowsModules.home ];
services.steward.enable = true;
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
hand their services to the new one. After a home apply, run `stewctl switch`
for the running manager to pick up the changed units.

## Installing it as a per-user service by hand

Once, from an administrator prompt:

```
mkdir "C:\Program Files\steward"
copy steward.exe stewctl.exe "C:\Program Files\steward"
sc create steward type= userown start= auto binPath= "\"C:\Program Files\steward\steward.exe\""
sc failure steward reset= 60 actions= restart/5000/restart/5000/restart/5000
```

Windows starts an instance, `steward_<suffix>`, at every sign-in, so sign out
and in. Anything a unit now runs should no longer be started by a Run key or
the Startup folder, or it will run twice.

Stopping the instance stops every service, as signing out does. To upgrade
without stopping them, hand over to the new manager instead: `sc control
steward_<suffix> 128` (the manager detaches and exits, its services left
running), copy the new binaries in, and `sc start steward_<suffix>`; the new
manager adopts them. To remove it: `sc stop` and `sc delete` the instance,
`sc delete steward`, and delete the directory.

## License

MIT; see [LICENSE](LICENSE).
