# rastflow

`rastflow` 是一个基于 Rust + GPUI 的 Windows 桌面快速启动器

## 主要功能

- 程序搜索与快速启动
- 系统托盘常驻（显示/隐藏、退出）
- 全局快捷键唤出（默认 `Alt + Space`）
- 自定义程序管理
- 文件搜索模式（通过 `Tab` 切换）：自建索引，**不依赖 Everything**
- 自动升级（启动后自动检查新版本，设置 → 关于 里一键升级）
- 基础设置持久化（主题、语言、开机自启、热键等）

## 文件索引

文件搜索使用 `src/search` 里的自研索引：直接枚举 NTFS 主文件表（MFT）建立全量索引，
再用 USN 日志维护增量，不遍历目录树，也不依赖 Any 第三方搜索工具。

需要留意两点：

- **首次建立索引需要管理员权限**（读取卷主文件表要求写权限）。非管理员启动时
  仍然可以搜索已经建好的索引，只是拿不到实时增量；界面会给出提权入口。
- 首次启动需要等索引建完（几十秒量级，取决于磁盘文件数），之后启动即可直接搜索。

## 自动升级

启动 30 秒后查一次远端更新清单（推迟是为了不和首次建索引抢磁盘 IO），之后每 24 小时一次；
也可以在 **设置 → 关于** 里手动检查并一键升级。检查失败一律静默，不会弹框打扰。

链路是：请求 `latest.json` → 比较 semver → 下载安装包 → **校验 minisign 签名** →
交给打包器生成的 NSIS 安装器静默安装（`/P /R`）→ 应用自动重启。

用现成的安装器而不是自己替换 exe，是因为被动模式下它会**直接结束正在运行的同名进程**
再覆盖安装，装完自动把应用拉起来；这条路径不会碰 `%LOCALAPPDATA%\rastflow`，
索引与设置都不受影响。自己写这套时序（重命名运行中的镜像、失败回滚）不划算。

几个需要知道的行为：

- 升级时进程是**被强制结束**的，内存索引来不及落快照。这不是问题：下次启动会从
  USN 日志补齐，只是首次启动略慢。
- 安装器按**镜像名**结束进程，所有叫 `rastflow.exe` 的进程都会被结束。
  升级时不要同时跑 `target/release` 里的副本。
- 没有版本回滚入口；`Cargo.toml` 里的 `allowDowngrades = false` 也会让安装器拒绝降级。
- 程序是**单实例**的（`Local\` 命名互斥体）：重复启动只会把已有窗口唤出来，不会多开一份。
- 会**自动跟随 Windows 的「系统代理」设置**：启动时读注册表并转成 `HTTP_PROXY` /
  `HTTPS_PROXY` 环境变量，因为 reqwest 只认环境变量、不读系统代理设置。少了这一步，
  开着代理的机器上会表现为「检查更新一直转圈然后超时」，而
  `Invoke-WebRequest`、`git push` 却是正常的，很难查。已经设过这两个环境变量的不会被覆盖。

## 功能实现

- [x] 应用搜索与启动
- [x] 全局快捷键设置
- [x] 文件搜索与自建索引（MFT + USN）
- [x] 自动升级（minisign 签名校验 + NSIS 静默安装）

## 运行项目

```bash
cargo run
```

首次运行后，应用会进入托盘，可通过快捷键或托盘菜单唤出窗口

## 快捷操作

- `Alt + Space`：显示/隐藏启动器（默认，可在设置中修改）
- `Tab`：在应用启动器模式与文件搜索模式之间切换
- `Enter`：确认（启动应用，或打开选中的文件）
- `Esc`：退出文件搜索模式或隐藏窗口

## 发布新版本

```bash
git tag v0.1.1      # tag 必须与 Cargo.toml 的 version 一致，CI 会校验
git push origin v0.1.1
```

推送 tag 后 `.github/workflows/release.yml` 会打包、签名、生成 `latest.json` 并创建 Release。

需要配置的仓库 secret：

| Secret | 内容 |
| --- | --- |
| `CARGO_PACKAGER_SIGN_PRIVATE_KEY` | 签名私钥文件的内容 |

生成密钥对（只做一次）：

```bash
cargo packager signer generate --ci --path <私钥路径>
```

- `<私钥路径>` 是私钥，只放 CI secret，**绝不进仓库**；`<私钥路径>.pub` 是公钥；
- 公钥（`.pub` 文件原文，它本身已是 base64）要填进 `src/update.rs` 的 `PUBKEY` 常量。
  这串换了而旧安装包还在，升级就会校验失败，所以尽量保持稳定。

本地验证整条链路（打包 → 签名 → 生成清单 → 测试）：

```powershell
cargo packager --release --formats nsis -k <私钥路径> --password=
./scripts/gen-update-manifest.ps1 -Repository <owner/repo> -Tag v0.1.1
cargo test
```

`cargo test` 里有两条依赖 `dist/` 产物的测试，会校验真实安装包的签名与 `latest.json`，
缺少产物时自动跳过；CI 里是先打包再跑测试，所以一定会执行到。
