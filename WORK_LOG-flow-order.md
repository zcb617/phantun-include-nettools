# Phantun 流量顺序与 TUN 队列调整记录

## 目标

针对单条连接的多个 worker 同时读取同一个 fake TCP socket 并各自向 UDP 发送的问题，改为每条连接一个转发 worker；另将程序创建的 TUN 接口发送队列最低值设为 4096。不同连接仍可并行。保留既有 keepalive、quit、超时以及 IPv4/IPv6 处理逻辑。

## 本地排查证据

- 既有生产抓包记录显示：香港 `lo/tun0/ens5` 到杭州 `tun2` 的 WireGuard 计数器顺序正常；杭州 Phantun 客户端 `lo` 出现 4380 次/85926 次计数器回退。
- `client.rs` 原 worker 块与 `server.rs` 原 worker 块按 CPU 数量克隆同一连接 socket，形成多个 consumer 并竞争 UDP 发送顺序。
- 杭州 `tun2` 原 `tx_queue_len=500`；既有 8 秒观测为 `tx_dropped=1645`、TCP retransmission 1645。仅临时把队列改为 4096 后，既有观测为 `tx_drop=0`、retransmission 1、吞吐 164 Mbps，但仍观察到 3669 次乱序。
- 另一次既有对照中，单核 affinity 未改变 worker 数量，乱序观测从 4380 次降到 902 次；这不是最终修复验证。
- 上述吞吐、丢包和乱序数据是本次修改前的现场观测，不是修改后的验证结果，也不证明单 worker 能消除网络路径本身的乱序。

## 改动记录

### `phantun/src/utils.rs`

- `ensure_tun_tx_queue_len`：校验正数队列下限和接口名；使用 RAII IPv4 数据报 socket，通过 `SIOCGIFTXQLEN` 读取当前值，只在低于下限时使用 `SIOCSIFTXQLEN` 设置，并再次读取验证。错误原样返回，验证未达到下限时返回明确错误。函数注释说明队列用于吸收 TUN 发送突发。
- 安全块分别说明 `ifreq` 初始化、ioctl 参数及 union 读取的前提。
- libc 0.2.175 未暴露 `ifru_ivalue`；按已核对的 Linux ABI，将三个访问点改用 offset 0 的 `ifru_metric: c_int` 同 union 成员。`SIOC*IFTXQLEN` 将该整数解释为队列长度，因此这些访问不表示接口 metric。

### `phantun/src/bin/client.rs`

- `main`：在创建 TUN 后、初始化 fake TCP Stack 前确保其发送队列不低于 4096，并记录内核实际值。
- 每条连接的 fastpath 现在只启动一个 worker，保证同一连接的读取和 UDP 发送由单任务顺序执行；独立连接仍由 Tokio worker 线程并行处理。原 select 分支、通知、取消和 keepalive 监控语义保持。

### `phantun/src/bin/server.rs`

- `main`：在创建 TUN 后、初始化 fake TCP Stack 前确保其发送队列不低于 4096，并记录内核实际值。
- 每条连接只启动一个 worker；动态 UDP 端口、reuse、select 分支、错误取消和超时逻辑保持。

### `phantun/tests/tun_queue.rs`

- `tun_queue_rejects_invalid_input`：覆盖零/负队列值、空名、超长名和嵌入 NUL 的无特权参数校验。
- `tun_queue_preserves_minimum`：忽略的隔离环境测试，创建临时 TUN，验证 4096、8192 的下限及不会从 8192 降低；drop TUN 自动清理。
- `tun_queue_returns_missing_interface_error`：验证短的不存在接口名会返回 ioctl 错误。

## 检查和测试状态

- `cargo fmt --all -- --check`：ABI 修订后退出码 0；测试接口名参数修正后再次检查，退出码仍为 0。
- 原始命令 `cargo check --workspace --all-targets --locked --target x86_64-unknown-linux-gnu` 在 Windows 上首次运行 240 秒后因依赖下载超时停止，未进入编译。随后按主代理要求停止了仍在下载阶段的重跑。
- 主代理核实 `ifru_metric` 为 libc 0.2.175 中对应的同 union `c_int` 槽位后，依此完成 ABI 修订。首次镜像 CHECK02 越过 helper 编译后发现测试调用参数类型不匹配（`if_nametoindex` 要求 `str`）；测试改传 `candidate.as_str()` 后最终命令退出码 0：`cargo --config 'source.crates-io.replace-with="diag-mirror"' --config 'source.diag-mirror.registry="sparse+https://rsproxy.cn/index/"' check --workspace --all-targets --locked --target x86_64-unknown-linux-gnu`。输出为 `Finished dev profile [unoptimized + debuginfo]`；仅有依赖 `proc-macro-error2 v2.0.1` 的 future-incompatibility warning。
- 本实施代理未运行任何测试；主代理已在隔离 Linux Docker 完成单元测试、真实 ioctl 测试和 release 构建，现场验收记录见下节。

## 主代理验收与现场记录（2026-10-21）

### 测试与验收标准

- 主代理 Docker run `ade4cafc-c8cb-47ee-81b6-05860484d6ef` 退出码 0。`cargo test --workspace --locked` 的 19 项普通测试通过：11 项 fake-tcp、6 项 keepalive、2 项普通 `tun_queue`；显式执行 `cargo test -p phantun --test tun_queue --locked -- --ignored` 后，特权 TUN ioctl 测试 1 项通过。共 20 项通过。workspace release build 通过。Cargo 使用命令级临时 mirror；`Cargo.lock` 未修改。

| 验收项 | 结果与证据 |
| --- | --- |
| AC-01 单连接单 consumer，原取消、健康检查及连接路径保持 | 通过。客户端和服务端每条连接启动一个 worker；keepalive、quit、超时与 IPv4/IPv6 原有分支保留；Docker 测试与两端实际连接运行通过。 |
| AC-02 队列 helper 校验、ioctl、只增不降与失败传递 | 通过。普通参数校验测试及隔离容器中的真实 ioctl 测试通过；两端程序启动后 TUN 队列均为 4096。 |
| AC-03 单元测试、真实 ioctl 测试与 release 编译 | 通过。上述 20 项测试和 release build 均通过。 |
| AC-04 现场同路径包序与 TUN 丢包检查 | 通过本次测试范围：输入计数器无新增回退时，程序没有引入新增回退；传输路径本身仍可有已有乱序计数。下载、上传现场结果见下表。 |

### 部署与回滚记录

- 杭州部署的新客户端为 `/root/phantun/phantun_client`，备份位于 `/root/phantun/backups/flow-order-1792511219`；运行 PID 为 `2411806`，SHA-256 为 `fa8002b81dcba8773921f14b4ff3a051e2890281518a4d10c5330d2731c57198`。
- 香港仍使用原镜像 `zcb617/phantun:0.8.1-reconnect.2`；Compose 增加只读绑定 `/opt/one-step-wg/phantun-flow-order/phantun-server:/usr/local/bin/phantun-server:ro`。备份 Compose 位于 `/opt/one-step-wg/backups/phantun-flow-order-1792511203`；部署 server 文件 SHA-256 为 `7646928fd886ec9ac31ae678028d4fad233a657a458506188eac78497f0fb922`。仅重建 Phantun 容器，WireGuard 配置与密钥未改。
- 两端 TUN 启动后自动设置为 4096。现场抓包和统计证据仅保存在本机忽略目录 `D:/work/one-setup-wg/.codex/phantun-order-20261021`，未推送 GitHub。
- 回滚指引（未执行）：杭州恢复 `/root/phantun/backups/flow-order-1792511219` 中的旧 binary 后，以原参数重启；新建 TUN 后还需将 `tun2` 的 `txqueuelen` 设为 4096，以保留已验证的队列缓解效果。香港恢复 `/opt/one-step-wg/backups/phantun-flow-order-1792511203` 中的 Compose，再执行 `docker compose up -d --no-deps phantun`，恢复原镜像内二进制。

### 现场 8 秒观测

| 路径 | 吞吐与传输 | TUN 与包序观测 |
| --- | --- | --- |
| 下载 `fixed-summary.json` | 164.034 Mbps；TCP retransmission 0 | 两端 TUN TX/RX drop 0。香港 `lo/tun0/ens5` 计数器回退 0。杭州 `eth0/tun2` 有既有 129 次回退，杭州 `lo` 同为 129，输入与输出相对新增为 0；134770 个 counter input/output 完全一致。 |
| 上传 `upload-fixed8s` | 95.59 Mbps；Windows 发送端没有可用的 retransmission 指标，故不报告上传重传值 | 两端 TUN drop 0；所有已测阶段计数器回退 0；程序新增回退 0。每个方向的 8 个抓包器 kernel capture drop 均为 0。 |

- 下载首秒吞吐 29.3 Mbps，第二秒 179.9 Mbps，第三秒 186.9 Mbps；RTT 约 105 ms。首秒 TCP 提速仍存在，本次结果不证明网页延迟问题完全消失。
- 修改前保持旧 binary、将 TUN 队列恢复为 500 的 baseline 为 128.8 Mbps、TUN drop 1645、TCP retransmission 1645、Phantun 新增乱序 4028 次。仅把旧版 TUN 队列调为 4096 时为 164.3 Mbps、retransmission 1、程序新增乱序 3669 次。该对照说明队列扩容改善 drop/retransmission，但不能单独消除程序主动引入的乱序。
- 生产包序结论限定为：本次测量中，程序输出没有相对输入增加乱序；不能据此声称网络路径没有乱序。
- IPv6 覆盖限于既有 keepalive 单元测试；没有进行生产 IPv6 吞吐测试。
