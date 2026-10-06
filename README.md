# Phantun：与上游原版的区别

本项目基于 [dndx/phantun](https://github.com/dndx/phantun)，当前自定义版本为 `0.8.1-reconnect.1`。上游的项目介绍和通用用法请查看原仓库。

## 连接自动恢复

修复客户端持续发送 UDP 时，远端连接已经失效却一直被当作活跃连接保留的问题，减少断线后必须重启 Phantun 才能恢复的情况。

- 连接健康检测以收到远端数据为依据，本地发送成功不会刷新远端健康状态。
- 默认连续 60 秒没有远端输入后发送探测，每 10 秒一次；3 次探测无回应后清理失效连接，后续 UDP 数据触发重新建连，默认约 90 秒判定失效。
- 新增客户端参数：`--keepalive-time`（默认 `60`，设为 `0` 关闭检测）、`--keepalive-interval`（默认 `10`）、`--keepalive-retries`（默认 `3`）。
- 共享的 fake-TCP 实现增加探测及响应处理，客户端和服务端均包含这部分改动。
- 新增连接健康检测单元测试及网络隔离环境中的断线恢复回归测试。

## Docker 启动与防火墙修复

- 防火墙后端改为显式选择：`USE_IPTABLES_NFT_BACKEND=0` 使用 legacy，`1` 使用 nft，非法值会使入口脚本报错退出。修复非交互 Bash 中 alias 不生效、规则被写入错误后端的问题。该配置由调用方提供，镜像入口不自动检测宿主机后端。
- 仅在 IP 转发未开启时尝试设置 sysctl；容器无权限设置时给出警告，便于在宿主机处理。
- 镜像构建时显式赋予入口脚本执行权限；运行依赖使用 `--no-install-recommends` 安装，并清理 apt 缓存。

## 三平台二进制与自动发布

版本 Tag 推送后，通过一个 `Build Linux Binaries` 工作流完成三平台静态编译；只有全部成功并通过包校验后，才自动创建对应的 GitHub Release。

| 架构 | Rust target |
| --- | --- |
| x86_64 / amd64 | `x86_64-unknown-linux-musl` |
| ARMv7 | `armv7-unknown-linux-musleabihf` |
| ARM64 | `aarch64-unknown-linux-musl` |

- Tag 匹配 `v*.*.*`，例如 `v0.8.1-reconnect.1`。
- 每个平台生成一个 `.tar.gz` 原生包，包含 `phantun_client`、`phantun_server` 和 `VERSION`；Release 同时提供总 `SHA256SUMS`。
- 固定 Rust `1.90.0` 并使用锁定依赖，构建静态 musl ELF，不依赖目标系统的 glibc；发布流程不生成 Docker TAR。
- 保留 Actions artifacts（14 天）及手动编译入口；普通分支手动运行只生成 artifacts。
- 替换上游多架构 Release 工作流；Rust 检查在分支推送或 PR 时运行，Docker 检查仅在分支推送时运行，均不再由 Tag 触发。

[本 fork 的 Releases](https://github.com/zcb617/phantun-include-nettools/releases)
