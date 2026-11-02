# 部署与运维

## 机器与域名

- 一台就能跑。要让很多设备稳定使用，建议至少两台，放在不同地区，一台出问题时设备会自动走另一台。
- 配置：1 核 / 1 GB 足够，瓶颈是带宽。按月流量计费的套餐，见下面的「带宽估算」。
- DNS：每台一条 A 记录（有 IPv6 就再加 AAAA），**直接指向机器，不要开 CDN 代理**，因为 UDP 端口和 WebSocket 都要直连。
- 端口：TCP 80、TCP 443、UDP 7842。云厂商的安全组也要放行。
- 客户端里内置的中继列表要和实际部署一致。加机器或改名后，记得同步客户端的列表。

## 首次部署

1. 从本仓库的 GitHub Release 下载对应架构的包（`baocode-relay-vX.Y.Z-linux-x86_64.tar.gz` 或 `-aarch64`），传到机器上：

   ```sh
   gh release download vX.Y.Z -R baocode-dev/relay -p '*x86_64*'
   scp baocode-relay-*-x86_64.tar.gz root@<机器>:/root/
   ```

2. 在机器上安装：

   ```sh
   tar xzf baocode-relay-*-x86_64.tar.gz && cd baocode-relay-*-x86_64
   sudo ./deploy/install.sh relay.example.com ops@example.com
   ```

   脚本会：建 `baocode-relay` 系统用户；装二进制；配置不存在时写 `/etc/baocode-relay/relay.toml`；写空的 `banned.txt`；调大 QUIC 用的内核缓冲区；ufw / firewalld 开着的话就放行端口；装 systemd 服务并启动。

3. 等一分钟，Let's Encrypt 证书签下来后，在任何地方检查：

   ```sh
   curl -fsS https://relay.example.com/healthz
   ```

   第一次部署想先试流程，可以在 `relay.toml` 里 `[tls]` 加 `staging = true`，用 Let's Encrypt 的测试环境（证书不受信任，客户端连不上，只用来验证签发），确认没问题后删掉 `/var/lib/baocode-relay/certs` 再改回去。

4. 用 Docker 也可以：

   ```sh
   docker build -t baocode-relay .
   docker run -d --name relay --restart always --network host \
     -v /etc/baocode-relay:/etc/baocode-relay:ro -v baocode-relay:/var/lib/baocode-relay \
     baocode-relay
   ```

   推荐用 `--network host`，因为 UDP 打洞要用到真实的源地址和端口。

## 机器上已有 Caddy / nginx 占着 80/443

域名和证书交给反向代理，中继在后面：

- `relay.toml` 里 `behind_proxy = true`：中继在 `[bind] http` 上用明文提供服务，不绑 `https`；`[tls]` 用 `mode = "files"` 指向代理的证书，只给 UDP 7842 上的 QUIC 地址发现用（每小时重读一次，代理续期后自动跟上）。
- Caddyfile：

  ```
  relay.example.com {
      reverse_proxy baocode-relay:3340
  }
  ```

- compose 里中继和代理在同一个网络，只映射 `7842:7842/udp`；把代理的证书目录只读挂进去。Docker 映射的 UDP 端口走 iptables DNAT，源地址不变，QUIC 地址发现看到的是设备的真实地址。
- QUIC 用的内核缓冲区（`net.core.rmem_max` / `wmem_max`）是整机的，在宿主机的 `/etc/sysctl.d/` 里设，容器里设不了。
- 升级：把新的 `baocode-relay` 二进制放进构建目录，`docker compose up -d --build baocode-relay`。

## 升级

下载新版本的包，再执行一次 `install.sh`。配置、封禁名单和证书都会保留。重启时已连着的设备会断开几秒，然后自动重连。如果有另一台中继，设备在这期间会走那一台。

## 日常操作

| 要做的事 | 怎么做 |
|---|---|
| 看日志 | `journalctl -u baocode-relay -f` |
| 封禁设备 | 往 `/etc/baocode-relay/banned.txt` 加一行 `<设备 id> 原因`，几秒内生效，已连接的会被踢掉 |
| 解封 | 删掉那一行 |
| 改限速 | 改 `relay.toml` 的 `[limits]`，几秒内对所有连接（包括已连着的）生效 |
| 其它配置 | 改完 `systemctl restart baocode-relay` |
| 看指标 | 在机器上 `curl -s 127.0.0.1:9090/metrics`，或者用 SSH 隧道接 Prometheus |

设备 id 就是 EndpointId（64 位十六进制）。

`reload` 出错时（比如名单文件写坏了），旧规则继续生效，日志里只报一次。

## 指标

我们自己的（前缀 `baocode_relay_`）：

| 指标 | |
|---|---|
| `admitted_total` | 放行的连接数 |
| `denied_banned_total` / `denied_unentitled_total` | 拒绝的连接数（被封 / 没有权益） |
| `kicked_total` | 规则变化后踢掉的设备数 |
| `connected` / `connected_standard` / `connected_other` | 当前在线设备数（总数 / 按档位） |
| `reloads_total` / `reload_errors_total` / `fetch_errors_total` | 规则重载、出错、远端拉取失败的次数 |

iroh-relay 自带的（前缀 `relayserver_`）：`bytes_sent`、`bytes_recv`、`bytes_rx_ratelimited_total`、`conns_rx_ratelimited_total`、`accepts`、`disconnects` 等。

这些指标都只有总数，不含设备 id、IP 或流量内容。

## 带宽估算

大多数连接都会打洞直连，中继只承担打不通的那部分，以及打洞成功之前的头一两秒。

- 走中继的流量 = 两端实际产生的流量 × 走中继的比例。
- 限速默认是每台设备持续 4 MB/s、突发 16 MB。带宽上限 = 同时活跃的走中继设备数 × 实际速率，正常远低于这个上限。

## 浸泡测试

`examples/soak.rs` 模拟很多对手机和电脑只经一台中继通信：每对每秒各 20 条小消息往返，每 30 秒传 1 MB，每 2 分钟断开重连。上线前（以及升级 iroh 之后）在真实机器上跑一次：

```sh
# 在另一台机器上（不要和中继同机），对着已部署的中继跑 200 对、2 小时
cargo build --release --example soak
./target/release/examples/soak https://relay.example.com 200 120 | tee soak.log

# 同时在中继机器上每 30 秒记一次内存和 CPU
while sleep 30; do echo "$(date +%T) $(ps -o rss=,%cpu= -C baocode-relay)"; done | tee rss.log
```

看什么：

- 每 30 秒一行的 `errors` 一直是 0，`over 1 s`（超过 1 秒的往返）很少；
- 中继内存涨一段后稳定，不随重连持续增长；
- 指标里 `relayserver_conns_rx_ratelimited_total` 不涨（200 对的流量远低于限速）。

本机跑过一轮（20 对、10 分钟，中继和客户端同机）：24 万次往返 0 失败、最慢 37 ms，100 次重连全部成功，中继内存稳定在 30 MB 左右、CPU 单核 5–6%。

## 准入与权益（现在都是同一档）

- `default_tier = "standard"`：所有设备都能用，都是同一档。
- 以后收费：把 `default_tier` 设成 `""`（只放行有权益的设备），权益由账号服务通过 `entitlements_url` 提供，格式是 `{"endpoints": {"<id>": "<档位>"}, "banned": {"<id>": "原因"}}`。中继每 `refresh_secs` 秒拉取一次，成功后写入 `cache`。**拉不到的时候一直用最后一次的缓存**，准入不依赖账号服务在线。
- 本地文件 `entitlements`（格式同上，但只认 `endpoints`）适合手动给某些设备加档位。

## 还没做的（以后再做）

- **按档位分别限速。** iroh-relay 1.4.1 的限速对所有连接是同一个值（`RelayService::set_client_rate_limit`），没有按连接设置的接口。要做的话，需要给 iroh-relay 打一个小补丁：在 `http_server.rs` 的 `accept` 里，把 `RateLimited::from_watcher(io, self.rate_limit.subscribe(), …)` 换成按 `AccessControl` 结果选出的 watcher（各档位各一个 `watch::Sender`）。准入是在限速器创建之后进行的，所以还要让 watcher 能在准入之后更换（`RateLimited` 本身已经支持运行中换值）。配置里的 `[tiers]` 已经预留好了。
- 出站限速：不需要单独做。设备是一对一通信的，中继发给 A 的每个字节，都是 B 发给中继、已经限过速的入站流量。
- 连接数上限（`accept_conn_limit`）：iroh-relay 还没有实现，需要的话在防火墙上做（`iptables -m connlimit`）。
