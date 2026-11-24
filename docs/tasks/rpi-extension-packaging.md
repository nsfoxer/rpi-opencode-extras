# 把 rpi-opencode-extras 改造成合规 rpi 扩展（多平台）

依据 skill：`~/.rpi/agent/skills/rpi-extension/skill.md`（name: `rpi-extension-packaging`）。
参考仓库：`pi-rust/rpi-extension`。

## 目标

把当前仓库从「本地能用、只在 Gitee、只有两个平台产物」提升为 skill 清单里
**可被 rpi 扩展目录收录**的状态：清单完整、CI 覆盖三平台、宿主冒烟、可发 Release、
带 topic。同时补 macOS 支持（当前 macOS 上**根本编译不过**）。

## 现状对照（已核实）

| skill 交付物 | 现状 | 差距 |
| --- | --- | --- |
| 根 `rpi-extension.json` | 有，`targets` 仅 windows-msvc + linux-gnu | 需扩到目标矩阵 |
| `Cargo.toml` `[package.metadata.rpi.prebuilt] supported-targets` | 同上两个 | 需与清单一致 |
| tag `<name>-v<version>` | `git tag` 为空 | 无任何 tag |
| topic `rpi-extension` | GitHub 仓库 404（origin 是 `gitee.com/muwuren/...`） | 仓库不存在 |
| `<name>-checks.yml` 三平台 | 文件名是 `checks.yml`，矩阵仅 ubuntu + windows | 缺 macOS，命名不带包名 |
| 宿主冒烟测 PR 产物 | 已正确实现（`--extensions-dir smoke` + grep `loaded 1 plugin`） | 无需改 |
| Release 资产 + manifest | `tools/prebuilt_manifest.py` 已实现 fragment/merge 两段式 | 从未跑过（无 tag） |
| README 权限/网络/卸载 | 齐全 | 需补 macOS 平台说明与矩阵变更 |

## 验收标准

打包与清单：

- [ ] `rpi-extension.json` 的 `targets` 与 `Cargo.toml` 的 `supported-targets`、`release.yml`
      的构建矩阵**三者完全一致**。
      证明：`python -c` 读两个文件做集合比较，输出相等。
- [ ] `rpi-extension.json` 通过 JSON 解析且 `type == "extension"`、`schemaVersion == 1`、
      `install` 为完整命令且不同时含 `--path` 与 `--github`。
      证明：脚本断言这三条。
- [ ] `.github/workflows/` 下检查工作流命名为 `<name>-checks.yml`，矩阵含
      `ubuntu-latest`、`windows-latest`、`macos-latest`。
      证明：`grep` 断言矩阵三平台齐全 + `ls` 断言文件名。

macOS 可编译（本项为本次唯一的 `src/` 改动）：

- [ ] `src/features/notify/worker.rs` 不再引用 macOS 上不存在的 API：
      `get_server_information`、`NotificationHandle::close`、`Notification::urgency`、
      `notify_rust::Urgency`。
      证明：`cargo check --target aarch64-apple-darwin`（或 CI macos runner 的
      `cargo clippy -D warnings`）通过，0 error。
- [ ] macOS 与 Windows/Linux 的能力差异有明确降级，不 panic、不静默错乱：
      macOS 上调研测（无 D-Bus 探测）→ 首次 `show()` 失败后整体禁用，行为与 Windows 后端一致。
      证明：`cfg` 分支的单元测试 + README「已知限制」写明 macOS 探测方式。

本地门禁：

- [ ] `cargo fmt --check`、`cargo clippy --all-targets --no-deps -- -D warnings`、
      `cargo test --locked` 在本机（Windows/MSVC）全绿。
      证明：三条命令 exit 0；改动前基线为 167 passed / 0 failed。
- [ ] 宿主加载冒烟：`RPI_STARTUP_DIAGNOSTICS=1 rpi --offline --local-only
      --extensions-dir smoke --verbose --list-models` 输出含 `extensions: loaded 1 plugin(s)`。
      证明：grep 命中。

发布链路：

- [ ] `tools/prebuilt_manifest.py fragment` 对每个 target 都能产出片段，`merge` 能合成
      单份 `rpi-extension.json`，且 `sdk.plugin_abi == 4`、`repository` 为
      `https://github.com/<owner>/<repo>`、`source_commit` 为 40 位小写 hex。
      证明：本地对已构建的归档跑一次 fragment + merge，检查字段。

GitHub 侧（用户手动执行，我给命令）：

- [ ] 仓库 `nsfoxer/rpi-opencode-extras` 存在且 `main` 分支含 `rpi-extension.json`。
      证明：`curl -s "https://api.github.com/repos/nsfoxer/rpi-opencode-extras/contents/rpi-extension.json?ref=main" -H 'Accept: application/vnd.github.raw'`
- [ ] 仓库 Topics 含 `rpi-extension`。
      证明：`gh api repos/nsfoxer/rpi-opencode-extras/topics`
- [ ] 存在 tag `<name>-v<version>` 且 Release 资产齐全（每 target 一个归档 + 最后上传清单）。
      证明：`gh release view <tag> --json assets --jq '.assets[].name'`

## 影响面

代码（仅 notify 的跨平台分支）：

- `src/features/notify/worker.rs`：`get_server_information` → 按平台探测；
  `close()` → 按平台关闭；`urgency()` / `Urgency` 导入 → 按平台条件编译。
- `src/features/notify/mod.rs`：`register()` 里如有平台相关诊断需同步。
- 不改 `msys2_shell`（已有 `cfg!(target_os="windows")` 运行期门禁 +
  `sandbox.rs` 的 `#[cfg(windows)]`，在 Linux 已验证可编译）。
- 不改 `provider` / `deepseek_search` / `go_usage`（无平台专属 API）。

打包与 CI：

- `rpi-extension.json`：`targets` 扩到目标矩阵。
- `Cargo.toml`：`[package.metadata.rpi.prebuilt] supported-targets` 同步；
  若 macOS 需要，补 `[target.'cfg(target_os="macos")'.dependencies]`（当前无需，
  `notify-rust` 自带 `mac-notification-sys`）。
- `.github/workflows/checks.yml` → 重命名 `<name>-checks.yml`，矩阵加 `macos-latest`，
  `RUSTFLAGS: -D warnings` 保持。
- `.github/workflows/release.yml`：构建矩阵扩到目标列表；macOS 归档为 `tar.gz`。
- `tools/prebuilt_manifest.py`：`package_library_name` 已支持 darwin/musl，核对即可。
- `README.md`：安装命令、平台矩阵、macOS 支持性与限制、卸载方式。

无数据库、无表、无服务端接口改动。

## 风险

1. **macOS 无本机验证能力**：本机只有 `x86_64-pc-windows-msvc` 与
   `x86_64-unknown-linux-gnu` 两个 target，装不了 Apple SDK。
   → macOS 是否真的编译通过，**只能由 CI 的 macos-latest 证明**。我不会声称"已验证"。
2. **musl 静态链接**：`ring` 需 `cc`/musl 工具链，`zbus` 纯 Rust 无碍。
   CI 需装 `musl-tools`；本地无法交叉验证。
3. **`notify-rust` macOS 语义变化**：`NSUserNotificationCenter` 后端在
   `NotificationHandle` **drop 时**才真正发送。现有代码"拿 handle 以便替换/关闭"的
   语义在 macOS 上退化为「drop 即发送且不可撤回」。需在 README 明确。
4. **平台覆盖面取舍**：若把 `x86_64-unknown-linux-gnu` 换成 musl，现有 gnu 用户
   `rpi extension update` 会报「no prebuilt extension for exact target」。见未决问题 1。
5. **tag 不可变**：首发版本一旦打 tag 就固定，改包必须升 `version`。
6. **GitHub 仓库 404**：`rpi install --github` 与目录收录都依赖它；未建仓前
   所有 GitHub 侧验收项都无法通过。

## 已定的决策（用户 2026-11-24 确认）

| # | 问题 | 决定 |
| --- | --- | --- |
| 1 | Linux 矩阵 | **保留 gnu + 新增 musl**，最终 5 个 target：`x86_64-pc-windows-msvc`、`x86_64-unknown-linux-gnu`、`x86_64-unknown-linux-musl`、`x86_64-apple-darwin`、`aarch64-apple-darwin` |
| 2 | 仓库标识 | `nsfoxer/rpi-opencode-extras`，版本保持 `0.1.0` |
| 3 | macOS 通知语义 | **接受降级**：仅修编译错误；macOS 上首次 `show()` 失败即整体禁用，`close()` 不发撤回 |
| 4 | 通知失败时的输出 | **真静默**：`Err(_) => enabled = false`，不打印任何东西（用户 2026-11-24 确认） |
| 5 | 能力探测 | **删除**：不再调 `get_server_information()`，直接用 `show()` 的 `Result` 驱动禁用 |

### 决策 4/5 的依据：宿主没有给插件任何日志能力

查证结果（均为源码证据）：

| 查证项 | 结果 |
| --- | --- |
| `PluginApi` 槽位 | 29 个，**无** log / print / diagnostic 任一 |
| 宿主 `log` crate logger | **未安装**（无 `set_logger` / `env_logger` / `tracing-log`） |
| 宿主 `PluginDiagnostics` | `crates/rpi-cli/src/session.rs:2732` 生产路径是 **`NullDiagnostics`**，`warn()` 为**空函数** |
| `PluginDiagnostics` 实现 | 只在测试里（`extension_compat.rs` / `plugin_stub_smoke.rs` / `voice_ptt_smoke.rs`） |
| 宿主自身输出 | `eprintln!` 264 处 + `println!` 361 处 + `tracing` 97 处，靠 `--verbose` / `RPI_STARTUP_DIAGNOSTICS` 门控 |

推论：`log::debug!` 在插件里是**空操作**（无 logger，同进程 dispatch 直接丢弃）。
即使插件 `register` 返回非零，宿主包成的 `diagnostics.warn(...)` 在生产路径上也被 `NullDiagnostics` 吞掉。
所以「打 debug」在本宿主上只能靠 `eprintln!` + 环境变量模拟；用户选择**连这个都不要**。

### 决策 3 的实现陷阱（必须处理）

macOS 默认后端（`notify-rust 4.19.0`, `src/macos/nsusernotifications.rs`）：

- `show_notification()` **只 `Ok(NotificationHandle::new(...))`，不发通知**（:268）
- 真正发送在 `impl Drop` 里，且错误被 `.ok()` **吞掉**（:154-165）

因此现有 `close()` 的 `handle.take()` 在 macOS 上会 **触发发送**（而非撤回），语义正好相反。
macOS 分支必须「`show` 后就地 drop 立即发送，不保存 handle」，而不是沿用 `take()`。

决策 1 的依据（两次修正后定稿）：

- 宿主 `scripts/install.sh` 在 Linux 上 `linux_libc="gnu"` 为默认，仅当 `ldd` 自报 musl 才切；
  官方 Release 也是 gnu/musl 双发。**musl-only 会让 Ubuntu/Debian/Fedora 等主流用户
  全部装不上**（`validate_manifest` 对 target 精确匹配，缺则报
  `no prebuilt extension for exact target`）。
- `rustc --print cfg --target x86_64-unknown-linux-musl` 含 `target_feature="crt-static"`，
  而宿主 `release-binaries.yml` 只设 `RUSTFLAGS="-D warnings"`，未开 `crt-static`，
  即默认 **静态链接**。静态 musl 的 `dlopen` 是空桩（musl 邮件列表 / 多个 downstream
  实测 `Dynamic loading not supported`），而宿主用 `libloading::Library::new` 加载插件。
  → musl 分支能否真加载插件**尚无证据**，必须先用 CI 验证再宣称可用。

## 未决问题（不阻塞本期，需后续跟踪）

1. **musl 宿主 dlopen 能力未验证**：本机只有 Windows，musl 二进制跑不起来；
   pi-rust 官方 CI 的 `extension-compatibility` job 用 glibc runner，无 musl-host 加载插件的证据。
   本期先在 `ubuntu-latest` 上交叉编 musl 并发布，**不声称已在本机验证**。
   跟踪方式：发布后找一台 Alpine（`docker run --rm -it alpine sh`）跑
   `rpi --extensions-dir <dir> -p 'echo ok'`；若报动态加载不支持，则撤下 musl 条目。
2. **是否同时发 crates.io**：影响 `rpi install rpi-opencode-extras` 能否省略 `--github`。
   本期默认**不发**。
3. **`aarch64-unknown-linux-gnu`** 是否也要（宿 `host_target()` 支持）？本期不发。
