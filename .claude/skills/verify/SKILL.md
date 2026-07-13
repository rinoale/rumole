---
name: verify
description: Build, launch, and drive rumole (SSH tunnel manager TUI) end-to-end with a throwaway sshd, isolated config, and tmux.
---

# Verifying rumole

rumole is a TUI that spawns real `ssh` child processes. Verify it against a
throwaway sshd on 127.0.0.1:2222 so nothing touches `~/.ssh` or
`~/.config/rumole`.

## Build & launch

```sh
cargo build                                   # binary: target/debug/rumole
tmux -L rumole new-session -d -x 190 -y 45 \
  "XDG_CONFIG_HOME=$SCRATCH/xdg target/debug/rumole"   # config isolation via XDG_CONFIG_HOME
tmux -L rumole capture-pane -p                # observe
tmux -L rumole send-keys c                    # drive (c/d/r, ':' commands, etc.)
```

Config is read from `$XDG_CONFIG_HOME/rumole/tunnels.toml`; history is written
next to it.

## Throwaway sshd

```sh
mkdir -p $SCRATCH/sshd
ssh-keygen -q -t ed25519 -f $SCRATCH/sshd/host_key -N ''
ssh-keygen -q -t ed25519 -f $SCRATCH/sshd/client_key -N ''
cp $SCRATCH/sshd/client_key.pub $SCRATCH/sshd/authorized_keys
# sshd_config: Port 2222, ListenAddress 127.0.0.1, HostKey/AuthorizedKeysFile
# from above, PubkeyAuthentication yes, PasswordAuthentication no, UsePAM no,
# StrictModes no, AllowTcpForwarding yes, AllowStreamLocalForwarding yes,
# StreamLocalBindUnlink yes, PidFile $SCRATCH/sshd/sshd.pid
/usr/sbin/sshd -f $SCRATCH/sshd/sshd_config -E $SCRATCH/sshd/sshd.log
```

Point tunnels at it with per-tunnel `extra_ssh_args`:

```toml
[[tunnel]]
name = "web-local"
host = "127.0.0.1"
kind = "local"
listen = "127.0.0.1:7071"
target = "127.0.0.1:7070"      # run: python3 -m http.server 7070
extra_ssh_args = ["-p","2222","-i","$SCRATCH/sshd/client_key","-o","StrictHostKeyChecking=accept-new","-o","UserKnownHostsFile=$SCRATCH/sshd/known_hosts"]
```

## Flows worth driving

- `c` on a tunnel → `●` in table/header, then `curl http://127.0.0.1:7071/`
  through it; big transfer shows Σ totals + sparkline in the detail pane.
- `-R`/`-L` unix socket tunnels → `curl --unix-socket /tmp/….sock`; after
  `d`, sockets on both sides must be gone (remote via best-effort ssh `rm`).
- `-R` reconnect over a stale server socket: remove `StreamLocalBindUnlink`
  from the sshd_config (real servers default to off), bind once with plain
  ssh, kill it, then connect in rumole — the pre-bind `rm -f` in
  `run_once` must clear the stale file or every reconnect fails with
  "remote port forwarding failed". All remote `rm` calls must pass the
  tunnel's `extra_ssh_args`.
- Failure path: tunnel with `host = "no.such.host.invalid"`,
  `auto_reconnect = false` → `✕ failed` with ssh's stderr in the log tail.
- `:q` with a connected tunnel → confirm dialog; after quit, check
  `pgrep -fl "ssh -N -T"` is empty.

## Gotchas

- Unix socket paths must stay under ~104 bytes (`sun_path` limit) or ssh
  rejects the forwarding spec — use `/tmp/…`, not deep scratchpad paths.
- The ssh command preview in the detail pane shows `127.0.0.1:<relay>` for
  metered tunnels; the log tail shows the real relay port.
- Rebuild before relaunching: `cargo clippy`/`cargo test` don't refresh
  `target/debug/rumole`.
- Kill sshd via its PidFile and `pkill -f "http.server 7070"` when done.
