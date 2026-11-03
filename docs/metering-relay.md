# The metering relay

## Why it exists

`ssh` does not report per-tunnel traffic. It forwards bytes between sockets,
but there is no interface to ask "how many bytes went through this `-L`
forwarding, and how many connections are open right now?" Parsing `ssh -v`
output is fragile and gives no byte counts at all.

rumole answers those questions by *being on the wire*: for a metered tunnel
(`meter = true`, the default) it inserts a small in-process TCP/unix-socket
forwarder — the **metering relay** — into the local leg of the forwarding
chain. Every byte of tunnel traffic physically passes through it, so the
counters are exact, not sampled or estimated.

The relay is what feeds everything traffic-related in the UI:

- ↑ / ↓ transfer rates (sampled once per second)
- Σ session and cumulative byte totals
- the sparkline graphs in the detail pane
- the `CONN` column (active / total connections)
- bytes-transferred summaries recorded to history on disconnect

## How it is wired

The relay always sits on the **local** side, between your endpoint and the
ssh process. rumole picks a random free loopback port for each session
(that is the `127.0.0.1:<port>` you see in the ssh command line, e.g.
`127.0.0.1:56974`) and rewrites the ssh forwarding to use it.

### `-L` (local) and `-D` (dynamic)

The relay owns the listen endpoint you configured; ssh listens on the
loopback port instead:

```
listen = "127.0.0.1:5433"        # what you configured
target = "10.0.0.5:5432"

your client → 127.0.0.1:5433 → relay → 127.0.0.1:<port> → ssh ⇄ sshd → 10.0.0.5:5432
              (relay listens)  counts  (ssh -L listens here)
```

The configured `listen` may also be a unix socket path — the relay binds it
natively, so `-D`-style SOCKS over TCP and `-L` over unix sockets both work.

### `-R` (remote)

Direction flips: sshd listens remotely and ssh delivers incoming traffic to
the relay, which forwards to your real local target:

```
listen = "/var/run/app.sock"     # on the remote host
target = "127.0.0.1:4000"        # what you configured

remote client → app.sock → sshd ⇄ ssh → 127.0.0.1:<port> → relay → 127.0.0.1:4000
                                        (ssh -R delivers here) counts
```

So an ssh command like

```
-R /home/ec2-user/…/unix.sock:127.0.0.1:56974
```

is not ignoring your configured target — `56974` is the relay for this
session, and the relay's upstream is your `127.0.0.1:4000`. The detail
pane's log tail shows the actual port; the command preview shows
`127.0.0.1:<relay>` because the port is chosen at connect time.

Because the relay reads "client → upstream" from its own perspective, rumole
flips the direction labels for `-R` tunnels so ↑ is always *toward the
remote* and ↓ *toward this machine*, regardless of kind.

## What it costs / what changes

- **One extra loopback hop.** Data is copied through a 16 KiB buffer per
  direction per connection. On loopback this is negligible next to the ssh
  encryption already happening.
- **Source address.** Your local target (for `-R`) or ssh (for `-L`) sees
  connections originating from `127.0.0.1` — the relay — rather than from
  the ssh process. For loopback-forwarded traffic this rarely matters.
- **A random loopback port appears in the ssh command.** Session-scoped;
  it is bound to 127.0.0.1 only and owned by rumole.
- **Half-close is preserved.** EOF in one direction is propagated as a
  shutdown, so protocols that close one side first still work.

## Lifecycle and cleanup

The relay lives exactly as long as the ssh session. Counters start at zero
per session; rumole carries totals across reconnects in the UI
(`Σ` = accumulated + current session). When the tunnel stops, the relay is
dropped and any unix socket it bound is unlinked — a metered `-L`/`-D`
unix-socket listener can never go stale, because rumole itself owns it.

Transient accept errors (such as a client disconnecting while queued) are
retried with a short delay. Other accept errors and relay task failures are
reported to the supervisor with their cause; it stops ssh and applies the
tunnel's normal reconnect policy. An alive ssh transport alone does not mean
the relay is healthy.

Connection copies preserve half-closes, but an I/O error in either direction
closes both sides promptly. Active connection tasks belong to the relay and
are cancelled when it stops, so reconnects do not retain old sockets.

## Turning it off

Set `meter = false` on a tunnel (or untick "Meter traffic" in the add/edit
form) and rumole runs plain ssh forwarding with your endpoints verbatim —
no relay, no extra hop, and `–` in the rate columns:

```toml
[[tunnel]]
name = "serve-api"
host = "cdi_ra"
kind = "remote"
listen = "/home/ec2-user/app/tmp/sockets/unix.sock"
target = "127.0.0.1:4000"
meter = false    # ssh -R …/unix.sock:127.0.0.1:4000 — direct, uncounted
```

Implementation: `src/relay.rs` (the forwarder and counters),
`src/manager.rs` (`relay_endpoints` — the wiring per tunnel kind),
`src/tunnel.rs` (`ssh_args` — how the ssh command is rewritten).
