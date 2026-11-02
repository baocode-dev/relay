# baocode-relay

A relay for the BaoCode desktop ↔ phone link.

Desktop and phone talk QUIC over [iroh](https://github.com/n0-computer/iroh):
straight to each other when the network allows it (the same LAN, or a hole
punched through NAT), and through this relay when it does not. The relay only
forwards encrypted packets between two device keys. It cannot read them, and
it holds no secret the clients trust.

This is [`iroh-relay`](https://crates.io/crates/iroh-relay) embedded in one
binary, plus:

- **Admission by device key.** The key is proven in the relay handshake. The
  ban list is checked first, then entitlements (a file, or an optional URL
  whose last answer is cached), then the default tier. Lists are read again
  every few seconds, and a device that is banned while connected is
  disconnected.
- **One rate limit for everyone.** A token bucket on what each connection
  sends, changeable while running.
- **Metrics with no identities in them.** Prometheus format, on loopback by
  default.
- **QUIC address discovery** (UDP 7842), which NAT hole punching needs.

Clients are not tied to n0's public services. A client ships a list of relays
(at least two is recommended) and tells the other device which relays it keeps
to when they pair. A device sends to all the relays it knows of the other until
a path is chosen, so one relay being down needs no DNS change.

## Run it

Try it locally, no configuration or TLS:

```sh
cargo run -- --dev                          # plain HTTP on 127.0.0.1:3340
cargo run --example peer -- listen http://127.0.0.1:3340
cargo run --example peer -- dial http://127.0.0.1:3340 <id it printed>
```

Deploy it on a Linux host with a domain pointing straight at it (no CDN
proxy: UDP 7842 and the WebSocket need the machine's own address):

```sh
cargo build --release --locked --bin baocode-relay
sudo ./deploy/install.sh relay.example.com ops@example.com
```

`install.sh` creates a `baocode-relay` system user, installs the binary and a
systemd unit, writes `/etc/baocode-relay/relay.toml` if there is none, and
opens TCP 80, TCP 443 and UDP 7842 in ufw/firewalld when they are active.
Certificates come from Let's Encrypt on first start.

Docker also works:

```sh
docker build -t baocode-relay .
docker run -d --name relay --restart always --network host \
  -v /etc/baocode-relay:/etc/baocode-relay:ro \
  -v baocode-relay:/var/lib/baocode-relay \
  baocode-relay
```

`--network host` is recommended: QUIC address discovery needs the device's
real source address and port.

## Configure

Every field has a default, so `--dev` needs no file.
[`deploy/relay.toml`](deploy/relay.toml) is the commented example; the schema
lives in [`src/config.rs`](src/config.rs).

The three settings most deployments touch:

```toml
hostname = "relay.example.com"      # its domain; the certificate is for it
[access]
default_tier = "standard"           # "" admits only entitled devices
banned = "/etc/baocode-relay/banned.txt"
```

To run behind Caddy, nginx or another TLS terminator, set
`behind_proxy = true` and point the proxy at `bind.http`. See
[docs/operations.md](docs/operations.md) for that and for the rest of the
operational notes.

## Test

```sh
cargo test                       # unit + integration (dev relay, two relays, bans, limits, TLS)
sudo tests/nat/run.sh            # Linux, root: cone / symmetric / no-UDP NATs
cargo run --release --example soak -- <relay-url> [pairs] [minutes]
```

On macOS, run the NAT test in a privileged Linux container:

```sh
docker run --rm --privileged -v "$PWD":/src -w /src rust:1-bookworm bash -c \
  'apt-get update -qq && apt-get install -y -qq iproute2 iptables conntrack openssl >/dev/null &&
   update-alternatives --set iptables /usr/sbin/iptables-legacy; bash tests/nat/run.sh'
```

## Layout

| | |
|---|---|
| `src/config.rs` | the TOML configuration |
| `src/access.rs` | admission: bans, entitlements, who is connected, our metrics |
| `src/lib.rs` | `Relay::spawn`: the iroh-relay server, reload loop, entitlements fetcher |
| `src/main.rs` | the binary: `--config <file>` or `--dev` |
| `tests/relay.rs` | real iroh endpoints, no IP transport, through a dev relay |
| `tests/nat/run.sh` | two NATs in Linux network namespaces (root) |
| `examples/peer.rs` | a listener and a dialler for trying real networks |
| `examples/soak.rs` | many pairs through one relay for a long while |
| `deploy/` | systemd unit, install script, example config and ban list |
| `docs/operations.md` | deploying, operating, and what is not done yet |

## Release

Tag `v*`: CI tests, runs the NAT cases, builds Linux x86_64 and aarch64
(glibc 2.35+), and attaches
`baocode-relay-<tag>-linux-<arch>.tar.gz` (binary + `deploy/` + docs) to a
GitHub release.

## License

MIT. See [LICENSE](LICENSE).
