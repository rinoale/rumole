# rumole

**Rust + mole.** A mole digs tunnels; rumole digs yours — a TUI for managing
SSH tunnels — local (`-L`), remote (`-R`), and dynamic (`-D`) — with live
traffic stats, reconnect supervision, event history, and sane unix-socket
cleanup. Built on the [rustui](https://github.com/rinoale/rustui) framework
(ratatui + tokio).

```
┌ rumole  ssh tunnels   ● 2 up  ○ 1 idle   ↑ 1.2M/s ↓ 340K/s ──────────────┐
├ Tunnels (3) ──────────────────────────────────────────────────────────────┤
│   NAME        T  ROUTE                                CONN  ↑ RATE ↓ RATE │
│ ● prod-db     L  127.0.0.1:5433 → prod → 10.0.0.5:5432  2/17  1.1M/s 12K/s│
│ ● expose-dev  R  prod:/var/run/dev.sock → local 3000    1/3   80K/s 320K/s│
│ ○ socks       D  1080 → bastion → SOCKS                 –     –      –    │
├ Detail — prod-db ──────────────────────────────────────────────────────────┤
│ LOCAL 127.0.0.1:5433  ══▶  prod (ssh)  ══▶  REMOTE 10.0.0.5:5432          │
│ ● connected  up 12m34s        conns 2 active / 17 total                   │
│ $ ssh -N -T -o ExitOnForwardFailure=yes … -L … prod    ↑ 1.1M/s  ▂▄▇█▆▄   │
└────────────────────────────────────────────────────────────────────────────┘
```

## What it does

- **Connect / disconnect / reconnect** tunnels defined in
  `~/.config/rumole/tunnels.toml` (`c`, `d`, `r`, or `Enter` to toggle;
  `C`/`D` for all). Sessions that drop are re-established automatically with
  exponential backoff (per-tunnel `auto_reconnect`).
- **Explicit visualization**: the header counts tunnels by state, each row
  shows the full route with direction (`listen → host → target`), and the
  detail pane shows the state, uptime, connection counts, and the exact `ssh`
  command being run.
- **Traffic**: metered tunnels (default) run through an in-process relay on
  the local leg, giving per-tunnel byte counters, ↑/↓ rates, sparklines, and
  active/total connection counts — for `-L`, `-R`, and `-D` alike.
- **History**: every connect/disconnect/failure (with uptime and transferred
  bytes) is appended to `~/.config/rumole/history.jsonl` and browsable with
  `h`.
- **Authentication is ssh's job**: `host` is a `Host` alias from
  `~/.ssh/config`. rumole runs plain `ssh` with `BatchMode=yes`, so use keys
  or an agent as you already do.

## Unix sockets

Both `listen` and `target` may be unix socket paths (`/var/run/app.sock`,
`~/x.sock`) on either side. The usual "socket file already exists" annoyance
is handled three ways:

1. before binding a `-R` unix socket, rumole runs `rm -f` on the remote host
   over ssh — necessary because sshd only unlinks stale sockets itself when
   the *server's* `StreamLocalBindUnlink` is enabled, which is off by
   default, and a stale file otherwise fails every reconnect with
   "remote port forwarding failed";
2. sockets are unlinked when the tunnel stops — locally directly, remotely
   over ssh (best effort). Every session also runs with
   `-o StreamLocalBindUnlink=yes` for local socket rebinds;
3. `:clean` removes anything still left behind (e.g. after a crash) —
   locally with a refuses-to-delete-non-sockets check, remotely via
   `rm -f` over ssh. The detail pane warns when a stale local socket file
   is present.

## Configuration

`~/.config/rumole/tunnels.toml` (created with a commented sample on first
run; also editable in-app with `a`/`e`):

```toml
[[tunnel]]
name = "prod-db"
host = "prod"                 # Host alias from ~/.ssh/config
kind = "local"                # local (-L) | remote (-R) | dynamic (-D)
listen = "127.0.0.1:5433"     # port, host:port, or unix socket path
target = "10.0.0.5:5432"      # omit for dynamic
auto_reconnect = true         # default true
meter = true                  # default true; set false for a plain ssh forward

[[tunnel]]
name = "expose-dev"
host = "prod"
kind = "remote"
listen = "/var/run/dev.sock"  # unix socket on the remote host
target = "127.0.0.1:3000"
```

## Keys and commands

Interface policy (from rustui): quitting is command-mode only (`:q`, `:q!`);
`Esc` cancels transient state and never quits.

| Key | Action | Command |
| --- | --- | --- |
| `c` / `d` / `r` | connect / disconnect / reconnect selected | `:connect`, `:disconnect`, `:reconnect` `[name\|all]` |
| `Enter` | toggle selected | |
| `C` / `D` | connect / disconnect all | `:connect all`, `:disconnect all` |
| `a` / `e` / `x` | add / edit / delete tunnel | `:add`, `:edit`, `:delete` |
| `h` | history | `:history` |
| `/` | filter tunnels | |
| | clean leftover socket file | `:clean [name]` |
| | re-read tunnels.toml | `:reload` |
| `?` | help | `:help` |

`:q` asks for confirmation while tunnels are connected; `:q!` disconnects
everything and quits immediately.

## Install

```sh
scripts/install.sh                      # builds --release, installs to /usr/local/bin
INSTALL_DIR=~/.local/bin scripts/install.sh   # or anywhere else
```

Or just build: `cargo build --release` → `target/release/rumole`.

## How metering works

ssh reports no per-tunnel traffic, so metered tunnels (the default) route
through a small in-process forwarder — the metering relay — on the local
leg. That's why the ssh command shows a random `127.0.0.1:<port>` instead of
your configured endpoint: the relay sits between them, counting every byte.
Your endpoints are still used; the relay bridges to them. This is what feeds
the rate/Σ columns, sparklines, and connection counts. Set `meter = false`
for a plain, uncounted `ssh` forwarding with your endpoints verbatim.

Full explanation with per-kind wiring diagrams and trade-offs:
[docs/metering-relay.md](docs/metering-relay.md).
