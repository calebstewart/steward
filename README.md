# steward

A per-user service manager for Windows, in the spirit of `systemd --user`: it
starts at sign-in as a per-user service, keeps your desktop daemons running,
and tells you what they are doing.

- `steward` -- the manager, hosted by the SCM as a per-user service.
- `stewctl` -- the command line.

Early days: see [DESIGN.md](DESIGN.md) for the design, what has been
established on a real machine, and the roadmap.

## Building

On Windows, with a Rust toolchain:

```
cargo build --release
cargo test
```

With Nix (on Linux or in WSL), cross-compiled for Windows:

```
nix build          # result/bin/steward.exe, result/bin/stewctl.exe
nix flake check    # the parser's tests natively, and the Windows build
```

## Trying the manager without installing it

```
cargo run -p steward -- --console
```

runs the manager in the foreground against your unit directory
(`%APPDATA%\steward\units`) until Ctrl+C.

```
cargo run -p stewctl -- verify
```

checks every unit file in that directory.

## License

MIT; see [LICENSE](LICENSE).
