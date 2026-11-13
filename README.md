# rpi-opencode

rpi 原生 Rust `cdylib` 插件，包含两个互相独立的能力：

| 能力 | 作用 | 是否默认启用 |
| --- | --- | --- |
| **`provider`** | 给所有 baseUrl 指向 `opencode.ai` 的 provider 自动注入请求头 | **是**（自动生效，无需配置） |
| **`msys2-bash`** | 把 `bash` 工具切换到 MSYS2 (MINGW64) 执行，并提供 3 个命令管理工具 | **否**（写配置文件才启用） |

- 宿主：rpi `0.22.1`，插件 ABI `4`，Windows x64
- 产物：**单个** `rpi_opencode.dll`，单个 `export_plugin!` 入口
- 依赖：`rpi-plugin-sdk = 0.22.1`（`provider` feature）、`rpi-ai = 0.22.1`、`serde_json`、`serde`、`regex`、`async-trait`、`url`
- 不引入 `tokio` / `reqwest` / `ureq`（HTTP 客户端与异步运行时都由宿主提供）

---

# 一、功能说明

## 1. provider：opencode.ai 请求头注入

**解决的问题**：opencode.ai 的端点要求请求携带 `x-opencode-session`，缺失会直接返回
`400 MissingSessionID`。这个 header 必须由客户端补上，而 `models.json` 里手写 session id
只能写死一个固定值，无法跟随真实会话。

**做法**：插件按 baseUrl 找到所有指向 opencode.ai 的 provider，在**每次请求**上注入
真实的宿主 session id。

| header | 值 |
| --- | --- |
| `x-opencode-session` | `ses_<宿主真实 session id>`；无 session 时 `rpi-<pid>`；已带 `ses_` 前缀则不重复 |
| `user-agent` | `rpi-opencode/0.1` |

已有 header 全部保留，header 名统一小写，同名（大小写不同）只留一份，插件值优先。

### 匹配规则

只按 URL 判断，**provider 名字不参与决策**（改名不影响工作）：

- `host == "opencode.ai"` → 命中
- `host` 以 `.opencode.ai` 结尾 → 命中
- 其他 → 不处理（`notopencode.ai`、`opencode.ai.evil.example` 都不命中）
- URL 解析失败 → 不处理

### 它不改什么

- **不 declare 模型目录**：`--list-models` 的内容完全来自你的 `models.json`，插件不会覆盖或补充模型
- **不新增工具**：只包装已有 provider 的请求路径
- **不碰其他 provider**：DeepSeek 等非 opencode.ai 的端点原样透传

## 2. msys2-bash：MSYS2 命令执行（可选）

**解决的问题**：rpi 内置的 `bash` 工具跑在 Git Bash 上。如果你的项目依赖 MSYS2
（`pacman` 装的工具链、`/mingw64` 下的编译器、`MSYSTEM` 环境），内置 bash 跑不了。

**做法**：启用后用一个 MSYS2 版本的 `bash` 工具**覆盖**内置 `bash`，并额外提供 3 个工具
用于管理长命令。

移植自独立的 `msys2-mcp` 项目，重新实现为插件内的 feature 模块。

### 配置文件就是开关

| 情形 | 行为 |
| --- | --- |
| 找到配置且校验通过 | 注册 `bash`（**覆盖内置 bash**）+ `get_cmd_result` + `kill_cmd` + `list_cmds` |
| 三个路径都没有配置 | 不注册任何工具，内置 Git Bash 原样保留 |
| 配置存在但无效 | 打印一行 warning，不注册任何工具；不回退 Git Bash、不中断插件加载 |

### 启用后的 4 个工具

| 工具 | 用途 | 参数 |
| --- | --- | --- |
| `bash` | 执行命令，最多等 `waitSeconds` 秒 | `command`（必填）、`waitSeconds`、`timeout`、`cwd` |
| `get_cmd_result` | 取回某次命令的输出与状态 | `id`（必填）、`waitSeconds` |
| `kill_cmd` | 杀掉运行中的命令（整棵进程树） | `id`（必填） |
| `list_cmds` | 列出运行中 + 最近完成的命令 | 无 |

### 两个不能混淆的时间参数

这是最容易踩错的地方：

| 参数 | 含义 | 默认 | 范围 |
| --- | --- | --- | --- |
| `waitSeconds` | **本次调用**阻塞多久；到点就返回"running + id"，命令继续跑 | 配置 `waitSeconds` | 1-120 |
| `timeout` | 命令**总截止**；到点杀进程树并标记 `timed_out` | 配置 `defaultTimeoutSeconds` | 1-1800 |

`waitSeconds` 绝不是总超时。做不到「等 30s 就把命令杀了」——那会让长命令失效。

### 取消与 kill 的区别

| 动作 | 命令进程 | 之后能否取结果 |
| --- | --- | --- |
| 模型取消等待（cancel） | **继续跑** | 能，用 `get_cmd_result` |
| `kill_cmd` | 被 `taskkill /F /T` 杀掉 | 能，状态为 `killed` |

`kill_cmd` 幂等：命令已结束时返回 no-op，不报错。

---

# 二、如何使用

## 步骤 1：构建

```bash
cd /c/Users/nsfoxer/Desktop/src/foxer/rpi-plugins/opencode
cargo build --release
```

产物：`target/release/rpi_opencode.dll`。

## 步骤 2：安装

```bash
# 先退出正在运行的 rpi，否则 DLL 被占用会复制失败（Device or resource busy）
cp target/release/rpi_opencode.dll ~/.rpi/agent/extensions/
```

如果 rpi 正在运行且无法退出，可以先把旧文件改名（Windows 允许对已映射的 DLL 重命名），
再放入新文件：

```bash
cd ~/.rpi/agent/extensions
mv rpi_opencode.dll rpi_opencode.dll.old
cp <项目>/target/release/rpi_opencode.dll .
rm rpi_opencode.dll.old     # 下次重启后删也可以
```

## 步骤 3：确认加载

```bash
RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1" 2>&1 | grep 'extensions: loaded'
```

看到 `loaded 11 plugin(s) (11 tool(s))` 就说明插件已加载（基线是 `10 plugin(s)`）。

## 使用 provider 功能（无需任何操作）

只要你的 `models.json` 里有 baseUrl 指向 opencode.ai 的 provider，它就会自动被包装。
典型配置：

```json
{
  "providers": {
    "opencode-go": {
      "baseUrl": "https://opencode.ai/inference/go/openai/v1",
      "apiKey": "your-key",
      "models": ["deepseek-v4.1-flash", "glm-5.3", "kimi-k3"]
    }
  }
}
```

注意：**不要**在 `models.json` 里再手写 `x-opencode-session`，插件会注入真实 session id，
手写的固定值反而会让所有会话共用一个 id。

验证：

```bash
rpi --model opencode-go/deepseek-v4.1-flash -p "只回复 GO-OK"
rpi --list-models       # opencode-go 的模型应仍然可见
```

> header 缺失时 opencode.ai 返回 `400 MissingSessionID`，因此请求成功即证明注入生效。

## 使用 msys2-bash 功能

### 启用：写配置文件

任选一个路径（优先级从高到低）：

1. 环境变量 `MSYS2_BASH_CONFIG` 指向的文件
2. `$RPI_CODING_AGENT_DIR/msys2-bash.json`
3. `~/.rpi/agent/msys2-bash.json` ← 通常用这个

```bash
cat > ~/.rpi/agent/msys2-bash.json <<'EOF'
{
  "msys2Root": "C:\\msys64",
  "msystem": "MINGW64",
  "shell": "usr/bin/bash.exe",
  "waitSeconds": 30,
  "defaultTimeoutSeconds": 900,
  "maxOutputBytes": 1048576,
  "maxJobs": 20
}
EOF
```

> **Windows 路径要写成 `"C:\\msys64"`**（JSON 里反斜杠必须转义）。直接写 `"C:\msys64"`
> 会让配置解析失败，插件会报 `invalid escape at line N column M` 并拒绝启用。
> 也可以改用正斜杠：`"C:/msys64"`。

**配置项说明**

| 字段 | 必需 | 默认 | 说明 |
| --- | --- | --- | --- |
| `msys2Root` | **是** | — | MSYS2 安装根目录，必须存在且是目录。没有内置默认值 |
| `shell` | 否 | `usr/bin/bash.exe` | 相对 `msys2Root`，也可写绝对路径；必须是文件 |
| `msystem` | 否 | `MINGW64` | 设置 `MSYSTEM` 环境变量 |
| `waitSeconds` | 否 | `30` | `bash` 默认等待秒数，夹紧 1-120 |
| `defaultTimeoutSeconds` | 否 | `900` | 命令总截止，夹紧 1-1800 |
| `maxOutputBytes` | 否 | `1048576` | 单次输出上限（字节），必须 > 0 |
| `maxJobs` | 否 | `20` | 保留的已完成命令条数，必须 >= 1 |
| `security.blacklistEnabled` | 否 | `true` | 是否启用命令黑名单 |
| `security.blacklistPatterns` | 否 | 内置列表 | 自定义正则列表 |

内置黑名单（与 `msys2-mcp` 一致）拦截：`rm -rf /`、`mkfs.*`、`dd ... of=/dev/`、
`format X:`、`reg add|delete`、`shutdown`、`rd /s /q X:\`。命中直接拒绝，不创建进程。

### 验证启用成功

```bash
RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1" 2>&1 | grep 'extensions: loaded'
# → loaded 11 plugin(s) (15 tool(s))     ← 工具数从 11 变成 15
```

同时会打印一行诊断：`rpi-opencode: msys2-bash enabled (shell=C:\msys64\usr\bin\bash.exe, wait=30s, timeout=900s)`

### 日常使用

启用后你不需要额外做什么，模型会自动用新的 `bash`。也可以直接这样要求它：

```bash
# 短命令：直接返回结果
rpi -p "用 bash 执行 echo hello"

# 确认跑在 MSYS2 而不是 Git Bash
rpi -p "用 bash 执行 uname -o"      # → Msys

# 长命令：返回 running + id，再取结果
rpi -p "用 bash 执行 sleep 40，报告 id" \
    -p "用 get_cmd_result 取那个 id 的结果"
```

### 工具返回格式

**执行完成**

```json
{
  "content": [{ "type": "text", "text": "<命令输出>" }],
  "details": { "id": 1, "status": "completed", "exitCode": 0 }
}
```

**仍在运行**（`waitSeconds` 内没跑完）

```
{"content":[{"type":"text","text":"running; use get_cmd_result with id 1"}],
 "details":{"id":1,"status":"running"}}
```

**总截止到点**

```json
{ "error": "timeout", "id": 1 }
```

**命令无输出**（如 `sleep`）：text 回落为状态摘要 `[completed] exit=0`，
否则模型看不到退出码。

**`list_cmds` 的文本输出**

```
2 command(s), 1 running:
#1 completed exit=0 sleep 35
#2 running npm run build
```

### 关闭：删掉配置文件

```bash
rm ~/.rpi/agent/msys2-bash.json
```

不需要卸载 DLL。删掉后内置 Git Bash 立即恢复（重启 rpi 生效）。

---

# 三、故障排查

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 请求报 `400 MissingSessionID` | provider 未被包装 | 确认 `models.json` 里 baseUrl 就是 `opencode.ai` 或 `*.opencode.ai` |
| `--list-models` 少了模型 | 插件 declare 了目录 | 本插件不 declare；检查是否有其他插件冲突 |
| `cp` 报 `Device or resource busy` | rpi 正在运行并持有 DLL | 退出 rpi，或按上文改名法替换 |
| 工具数仍是 11（期望 15） | 配置文件没找到或校验失败 | 看启动输出的 `msys2-bash disabled: ...` 那行 |
| `msys2Root is not a directory` | 路径写错或不存在 | 确认 `C:\msys64` 存在；JSON 里反斜杠要写 `\\` |
| `invalid escape at line N column M` | JSON 里的反斜杠没转义 | `"C:\msys64"` 改成 `"C:\\msys64"` 或 `"C:/msys64"` |
| `shell is not a file` | `shell` 相对路径拼错 | 默认应是 `usr/bin/bash.exe` |
| 命令里中文乱码 | 环境编码不对 | 插件已设 `LANG=C.UTF-8` 并做 UTF-8 lossy 解码；若仍乱码说明命令自身输出非 UTF-8 |
| 长命令 30 秒就被杀 | 把 `waitSeconds` 当成了总超时 | 调大 `timeout`，不是 `waitSeconds` |
| 取消后任务不见了 | cancel 杀了进程 | 本插件 cancel 只停等待；若观察到杀进程请报告 |
| `get_cmd_result` 报 `no such job` | 跨进程了（每次 `rpi -p` 都是新进程） | 在同一会话内多轮调用查同一个 id |
| 子进程杀不干净 | 未杀进程树 | 插件用 `taskkill /F /T`；若残留请报告 |

---

# 四、架构（开发向）

单个 cdylib、单个 `export_plugin!` 入口；内部是「共享核心 + 可插拔 feature 模块」。

```
src/
  lib.rs            只做入口装配：mod 声明 + 一次 export_plugin!
  registry.rs       register_all：按 FEATURES 顺序执行，失败即中止
  config.rs         共享核心：定位 agent 目录、解析 models.json、按 URL 筛选 provider
  headers.rs        共享核心：纯函数 is_opencode_ai_url / build_headers
  error.rs          共享核心：统一错误类型 GoError
  features/
    mod.rs          Feature 结构 + FEATURES 列表
    provider.rs     provider feature：factory + HeaderInjectingProvider + ErrorProvider
                    + ABI request/stream + pub fn register(api)
    msys2_bash/
      mod.rs        msys2-bash feature：注册 4 个工具 + bash 提示词 + 工具生命周期 ABI
      config.rs     三路径定位、解析、校验、黑名单
      job.rs        JobManager：线程、超时杀树、保留策略、cwd 解析
      output.rs     TailBuffer：保留最后 N 字节 + UTF-8 lossy
      tools/
        mod.rs      工具名 / schema / wait_for
        bash.rs     bash（覆盖内置）
        get.rs      get_cmd_result
        kill.rs     kill_cmd
        list.rs     list_cmds
        result.rs   结果封装与参数解析辅助
```

注册流程：`export_plugin!` → `registry::register_all` → 遍历 `FEATURES` → 每个
`Feature::register(api)`。任一 feature 返回非 0 就立即中止并上报，避免半注册状态。

### msys2-bash 的执行细节

- 可执行：`<msys2Root>/<shell>`，参数 `-lc "<command>"`
- 环境：`MSYSTEM=<msystem>`、`CHERE_INVOKING=1`、`LANG=C.UTF-8`
- `cwd` 优先级：工具参数 `cwd` → 宿主 `__rpi.cwd` → 进程当前目录
  支持 MSYS2 路径：`/c/Users/x` → `C:\Users\x`；`/tmp` → `C:\msys64\tmp`
- 输出：stdout+stderr 合并，**保留最后 `maxOutputBytes` 字节**（看错误尾巴），
  前置 `[truncated N bytes]`，按 UTF-8 边界对齐后再 lossy 解码
- job 状态：`running` / `completed` / `failed` / `timed_out` / `killed`
- 已完成保留最近 `maxJobs` 条；运行中的 job 始终保留
- 每个 job 一个 `std::thread`；id 从 1 单调递增

### 工具数量参考（实测）

| 情形 | 输出 |
| --- | --- |
| 无插件 | `10 plugin(s) (11 tool(s))` |
| 插件加载、msys2-bash 未配置/无效 | `11 plugin(s) (11 tool(s))` |
| 插件加载、msys2-bash 启用 | `11 plugin(s) (15 tool(s))` |

启用时是 **15** 而不是 11+3=14：注册了 4 个工具，其中 `bash` 覆盖同名内置工具，但宿主
计数时**内置与插件各算一次**（实测），因此净增 4。

### 如何新增一个 feature

1. 新建 `src/features/<name>.rs`（或目录），实现
   `pub fn register(api: &rpi_plugin_sdk::PluginApi) -> i32`。
2. 在 `src/features/mod.rs` 里加 `pub mod <name>;`，并在 `FEATURES` 追加一行。
3. `cargo test` + 重启 rpi 验证（`RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1"`）。

```rust
// src/features/example.rs
use rpi_plugin_sdk::PluginApi;

/// 示例 feature：不做任何事，只演示注册签名。
pub fn register(_api: &PluginApi) -> i32 {
    // 实际能力在这里注册；返回 0 表示成功。
    0
}
```

```rust
// src/features/mod.rs 的改动（两行）
pub mod example;
// FEATURES 里追加：
// Feature { name: "example", register: example::register },
```

注意：`export_plugin!` 只能有一个，固定在 `lib.rs`；新 feature 不要自己导出入口。

---

# 五、构建与校验

```bash
cargo build --release      # 0 warning、0 error，只产出 rpi_opencode.dll
cargo test                 # 73 个单测：URL 判定 / header 构造 / provider 筛选 /
                           #            配置校验 / 输出截断 / job 保留 / 工具边界
```

运行时校验：

```bash
# 插件数 +1；msys2-bash 未配置时工具数不变
RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1"

rpi --list-models                                       # models.json 的模型依然可见
rpi --model opencode-go/deepseek-v4.1-flash -p "只回复 GO-OK"
rpi --model deepseek/deepseek-flash -p "只回复 DS-OK"    # 不经过插件
```

---

# 六、已知限制

- **配置只在加载时读**：`models.json` 与 `msys2-bash.json` 都在插件注册阶段读取一次，
  之后修改需要重启 rpi（或触发宿主重载）才会生效。
- **不 declare 模型目录**：插件不向宿主声明任何模型，`--list-models` 的内容完全来自
  `models.json`；这也意味着插件无法自带默认模型。
- **header 注入只看 baseUrl**：若同一 provider 在不同模型上使用不同端点，仍按每次请求的
  `model.base_url` 判断；不读取 provider 名字。
- **job 状态是进程内状态**：`get_cmd_result` 只能取回**同一个 rpi 进程**内创建的 job。
  每次 `rpi -p` 都是新进程，所以跨命令行调用拿不到上一次的 id（返回 `no such job`）。
  同一会话内多轮调用正常。
- **`bash` 覆盖内置工具**：启用后宿主内置的 Git Bash 版本被插件的 MSYS2 版本替代。
  想恢复内置 bash，删掉配置文件即可（无需卸载 DLL）。
- **`poll` 是同步阻塞的**：宿主工具驱动没有 wake 回调，工具在 `poll` 内最多阻塞
  `waitSeconds`（≤120s）；因此不存在 Pending 分支，长命令会占住该次工具调用。

# 七、行为边界

- 构造 provider 失败（传输层）不会 panic：转成 `StopReason::Error` 终态交给宿主。
- 某个 provider 注册被宿主拒绝时插件返回非 0，宿主会报告
  `register returned nonzero code`；`models.json` 缺失或无匹配 provider 时返回 0，
  插件照常加载且不改变任何模型目录。
- 所有 `extern "C"` 边界都用 `rpi_plugin_sdk::guard` 包住，panic 不会 unwind 到宿主。
- 日志与输出中不出现 apiKey。
