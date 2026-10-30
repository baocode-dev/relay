# baocode-relay

BaoCode's relay for the desktop ↔ phone link. Desktop and phone talk QUIC
over [iroh](https://github.com/n0-computer/iroh): straight to each other
when the network allows it (same LAN, or a hole punched through NAT), and
through this relay when it does not. The relay only forwards encrypted
packets between two device keys. It cannot read them.

This is [iroh-relay](https://crates.io/crates/iroh-relay) 1.4.1 embedded in
our own binary, plus:

- **Admission by device key.** The key is proven in the relay handshake.
  The ban list is checked first, then entitlements (a file, an optional
  URL whose last answer is cached), then the default tier. Lists are read
  again every few seconds, and a newly banned device that is connected now
  is disconnected.
- **One rate limit for everyone.** It is a token bucket on what each
  connection sends, and it can be changed while running.
- **Metrics with no identities in them.** Prometheus format, on loopback
  only.
- **QUIC address discovery** (UDP 7842), which hole punching needs.

Clients use no n0 services. The app ships a list of our relays (at least
two) and tells the phone the desktop's relays when they pair. A device
sends to all of them until a path is chosen, so one relay going down needs
no DNS change.

## Layout

| | |
|---|---|
| `src/config.rs` | the TOML config ([`deploy/relay.toml`](deploy/relay.toml) is the commented example) |
| `src/access.rs` | admission: bans, entitlements, who is connected; our metrics |
| `src/lib.rs` | `Relay::spawn`: iroh-relay server, reload loop, entitlements fetcher |
| `src/main.rs` | the binary: `--config <file>` or `--dev` |
| `tests/relay.rs` | real iroh endpoints with no IP transport, through a dev relay |
| `tests/nat/run.sh` | two NATs in Linux network namespaces (root) |
| `examples/peer.rs` | a listener and a dialler for trying real networks |
| `deploy/` | systemd unit, install script, example config and ban list |
| `docs/operations.md` | deploying, operating, what is not done yet |

## Try it

```sh
cargo run -- --dev                          # plain HTTP on 127.0.0.1:3340
cargo run --example peer -- listen http://127.0.0.1:3340
cargo run --example peer -- dial http://127.0.0.1:3340 <id it printed>
```

## Test

```sh
cargo test                       # unit + integration (dev relay, 2 relays, bans, limits, TLS)
sudo tests/nat/run.sh            # Linux, root: cone / symmetric / no-UDP NATs
cargo run --release --example soak -- <relay-url> [pairs] [minutes]   # a long run (docs/operations.md)
```

On macOS, run the NAT test in a privileged Linux container:

```sh
docker run --rm --privileged -v "$PWD":/src -w /src rust:1-bookworm bash -c \
  'apt-get update -qq && apt-get install -y -qq iproute2 iptables conntrack openssl >/dev/null &&
   update-alternatives --set iptables /usr/sbin/iptables-legacy; bash tests/nat/run.sh'
```

## Release

Tag `v*`: CI tests, runs the NAT cases, builds Linux x86_64 and aarch64
(glibc 2.35+), and attaches `baocode-relay-<tag>-linux-<arch>.tar.gz`
(binary + `deploy/` + docs) to a GitHub release. See
[docs/operations.md](docs/operations.md) for deploying it.
