# 上架 rpi-opencode-extras 到官方扩展目录

## 目标

让 `rpi install rpi-opencode-extras --github <owner>/<repo>` 在干净环境可安装，
并出现在 https://rpi.bigfisharc.cn/packages.html 目录中。

## 验收标准

- [ ] 仓库根目录 `rpi-extension.json` 可经 GitHub contents API 取到：
      `curl -s https://api.github.com/repos/<owner>/<repo>/contents/rpi-extension.json?ref=main -H 'Accept: application/vnd.github.raw'`
- [ ] 仓库 Topics 含 `rpi-extension`（`gh api repos/<owner>/<repo>/topics` 可证）
- [ ] 存在稳定 tag `<name>-v<version>`，且指向发布提交
      （`git ls-remote --tags` + `gh api .../git/ref/tags/<tag>` 可证）
- [ ] Release 资产齐全：每平台一个 `zip`/`tar.gz`，外加最后上传的 `rpi-extension.json`
      （`gh release view <tag> --json assets` 可证）
- [ ] Release 侧清单通过宿主校验：`schema_version=1`、`tag=<name>-v<version>`、
      `source_commit` 为 40 位小写 hex、`sdk.plugin_abi == 4`、`library.path` 为扁平文件名
- [ ] 干净环境安装成功：`rpi install rpi-opencode-extras --github <owner>/<repo> --force`
      随后 `rpi --no-session -p 'echo ok'`，启动诊断里出现该插件
- [ ] 本机宿主加载冒烟（已通过，见下）
- [ ] 目录 API 出现该包：`https://rpi.bigfisharc.cn/api/packages?perPage=100` 含
      `name: rpi-opencode-extras`

## 影响面

- `Cargo.toml`：补 `description` / `license` / `repository`（走 crates.io 必须，否则也影响审核）；
  如需固定平台矩阵，加 `[package.metadata.rpi.prebuilt] supported-targets`
- 新增 `rpi-extension.json`（根目录，目录发现用）
- 新增 `LICENSE`（当前仓库无该文件）
- 新增 `.github/workflows/*-checks.yml`（三平台 fmt/test/clippy + 宿主冒烟，PR 必须）与发布工作流
- `README.md`：构建/安装章节改为 `rpi install ... --github`，修正写错的目录名
- 无数据库、无服务端接口改动

## 风险

- ABI 固定为 `rpi-plugin-sdk = "=0.22.1"`；安装器要求清单里 `plugin_abi` **等于**宿主 ABI。
  已实测宿主 0.27.1（ABI 4）可加载，但 SDK 落后时新宿主可能拒绝（需升级 SDK 并重发版本）。
- `msys2-bash` 仅 Windows 生效；Linux/macOS 归档里该能力自动不注册。
- `notify-rust` 依赖平台通知后端（Windows toast / Linux D-Bus），macOS 未验证。
- tag 一旦发布不可变；改包只能升 `version` 重发。

## 未决问题

1. GitHub 仓库地址：`nsfoxer/rpi-opencode-extras` 目前 404，origin 指向 Gitee。
2. 平台矩阵：只发 Windows，还是同时发 Linux x86_64-gnu？
3. 是否同时发布到 crates.io（决定 `install` 是否可省略 `--github`）。

## 本地已验证证据

- `rustc -vV` → `host: x86_64-pc-windows-msvc`
- 构建产物 `target/release/rpi_opencode_extras.dll`（PE32+ x86-64）
- 宿主加载冒烟：
  `RPI_STARTUP_DIAGNOSTICS=1 rpi --offline --local-only --extensions-dir <dir> --verbose --list-models`
  → `extensions: loaded 1 plugin(s) (5 tool(s))`（宿主为 rpi 0.27.1）
