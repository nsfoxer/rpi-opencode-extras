# rpi-opencode-extras

rpi 原生 Rust `cdylib` 插件，包含五个互相独立的能力：

| 能力 | 作用 | 开关（`opencodeExtras.*`） | 默认 |
| --- | --- | --- | --- |
| **`provider`** | 给所有 baseUrl 指向 `opencode.ai` 的 provider 自动注入请求头 | `provider` | **开** |
| **`msys2-bash`** | 把 `bash` 工具切换到 MSYS2 (MINGW64) 执行，并提供 3 个命令管理工具 | `msys2Bash.enabled`（段存在才启用；仅 Windows） | 关 |
| **`deepseek-search`** | 提供 `deepseek_search` 工具：调 DeepSeek Search API 检索网页 | `deepseekSearch` | **开** |
| **`go-usage`** | 在 TUI 页脚显示 opencode.ai Go 套餐用量（含月度到期倒计时） | `goUsage` | **开** |
| **`notify`** | agent 跑完或等待用户回答时发桌面通知 | `notify` | **开** |

- 宿主：rpi `0.22.1+`（插件 ABI `4`；已在 rpi `0.24.0` Linux x86_64 上验证）
- 产物：**单个** `librpi_opencode_extras.so` / `rpi_opencode_extras.dll`，单个 `export_plugin!` 入口
- 依赖：`rpi-plugin-sdk = 0.22.1`（`provider` feature）、`rpi-ai = 0.22.1`、`serde_json`、`serde`、`regex`、`async-trait`、`url`、`ureq`（rustls）、`notify-rust`（D-Bus）
- 不引入 `tokio` / `reqwest`（异步运行时与 provider HTTP 客户端都由宿主提供；
  `ureq` 仅供 `deepseek-search` 的同步网络调用，`notify-rust` 只在通知线程里使用）

## 配置开关

所有开关都在 rpi 全局 `settings.json`（`~/.rpi/agent/settings.json`，可用
`RPI_CODING_AGENT_DIR` 重定位）的 `opencodeExtras` 段里，全部可选：

```jsonc
{
  "defaultProvider": "opencode-go",
  "defaultModel": "deepseek-v4.1-flash",

  "opencodeExtras": {
    "provider": true,
    "deepseekSearch": true,
    "goUsage": true,
    "notify": true,
    "msys2Bash": {
      "enabled": true,
      "msys2Root": "C:\\msys64"
    }
  }
}
```

规则：

- 段或字段缺失 ⇒ **默认开启**，行为与旧版本一致。
- 显式 `false` ⇒ 该能力完全不注册（不订阅事件、不起线程、不注册工具/页脚）。
- 字段类型不对 ⇒ 打印一行 warning，该字段回落到默认值，不影响插件加载。
- 配置只在**插件加载时读一次**，修改后需要重启 `rpi`。
- 历史兼容：`msys2Bash` 曾经位于 `settings.json` 顶层；新位置
  `opencodeExtras.msys2Bash` 优先，旧顶层位置仍可读。
- 关闭 `provider` 会影响 opencode.ai：缺少 `x-opencode-session` 时端点返回
  `400 MissingSessionID`，因此 opencode-go 将不可用。

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
| `user-agent` | `rpi-opencode-extras/0.1` |

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

### 会话头的一致性

`x-opencode-session` 的值按「会话 id」确定性生成：宿主给 `abc`，发出 `ses_abc`；
已是 `ses_abc` 则不重复加前缀。取值只由会话 id 决定，**不含 pid、时间或随机数**。

因此下面三种情况下 header 都保持稳定（已实测）：

| 场景 | 结果 |
| --- | --- |
| 同一会话内多轮请求 | header 相同 |
| 重启进程后 `--session <id>` 恢复同一会话 | header 与之前相同（即使 pid 变了） |
| 不同会话 | 各自不同（证明不是写死的常量） |

> **唯一的例外**：如果宿主**没有**传入会话 id（正常流程不会发生：新会话 / `--session`
> 恢复 / `-c` 继续 都会传），插件回落到 `rpi-<pid>`。这个值**每次进程启动都会变**，
> 不具备跨进程稳定性。它只用于“无会话上下文”的退化场景（如离线诊断），
> 此时本来也不需要会话亲和性。

## 2. msys2-bash：MSYS2 命令执行（可选）

**解决的问题**：rpi 内置的 `bash` 工具跑在 Git Bash 上。如果你的项目依赖 MSYS2
（`pacman` 装的工具链、`/mingw64` 下的编译器、`MSYSTEM` 环境），内置 bash 跑不了。

**做法**：启用后用一个 MSYS2 版本的 `bash` 工具**覆盖**内置 `bash`，并额外提供 3 个工具
用于管理长命令。

移植自独立的 `msys2-mcp` 项目，重新实现为插件内的 feature 模块。

### 配置文件就是开关

配置写在 rpi **全局 `settings.json`** 的 `opencodeExtras.msys2Bash` 段里，**不需要额外的配置文件**。

| 情形 | 行为 |
| --- | --- |
| 段存在、未写 `"enabled": false` 且校验通过 | 注册 `bash`（**覆盖内置 bash**）+ `get_cmd_result` + `kill_cmd` + `list_cmds` |
| 段存在但 `"enabled": false` | 保留配置内容，但不注册任何工具 |
| `settings.json` 不存在，或没有该段 | 不注册任何工具，内置 Git Bash 原样保留 |
| 段存在但无效 | 打印一行 warning，不注册任何工具；不回退 Git Bash、不中断插件加载 |

只读**全局** agent 目录（`~/.rpi/agent/settings.json`），**不读**项目级 `.rpi/settings.json`，
避免同一份工具配置随项目漂移。旧版的独立配置文件 `msys2-bash.json` **已不再读取**。

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

### 启动性能：非 login shell + 环境变量注入

这是本 feature 的关键设计，默认开启，**不需要任何配置**。

MSYS2 的 login shell（`bash -lc`）每次都 source `/etc/profile`（locale、`which`、
`uname`、`hostname` 等），单次命令实测多花 **约 540-640ms**；而非 login shell（`bash -c`）
只要约 63ms。

因此插件在**启动期捕获一次**目标 shell 的完整环境变量，写进磁盘缓存；之后每条命令
都用 `-c` 启动并注入这份环境，既拿到完整 PATH / 自定义变量，又不重复付 profile 的代价。

| 路径 | 单次 `bash` 启动 |
| --- | --- |
| 改造前（每次 `-lc`） | ~682 ms |
| 改造后（`-c` + 注入缓存 env） | **~66 ms** |

捕获用哪个 shell 由 `envFrom` 决定，默认 `bash`，可设为 `fish` 继承你 fish 配置里的
导出变量（fish 在非 login 模式下同样会 source `~/.config/fish/config.fish`）：

```json
{ "opencodeExtras": { "msys2Bash": { "msys2Root": "C:\\msys64", "envFrom": "fish" } } }
```

实测 `fish -c env` 约 405ms（vs `fish -lc env` 1113ms），且只在**首次**付一次，
之后走磁盘缓存（毫秒级）。

> **注意**：`envFrom: fish` 只继承**导出型环境变量**（`set -xg`）。fish 的语法、函数
> （`~/.config/fish/functions/*.fish`）、`abbr`、别名**不会**继承——命令本体始终由
> bash 执行，`for i in 1 2 3; do ... done` 这类 bash 语法正常可用。

磁盘缓存位置 `%APPDATA%\rpi\msys2-bash-env.json`，带指纹
`msys2Root|msystem|shell|envFrom`；任一字段变化即视为失效并重新捕获。
读写失败一律降级为"无缓存"，绝不阻塞插件加载。删掉该文件即可强制下次重新捕获。

## 3. deepseek-search：网页检索工具

**解决的问题**：模型需要即时网页信息（最新版本号、文档链接、新闻）时，从训练数据里
回答容易过时或编造 URL。本能力提供一个真实联网的检索工具。

合并自独立的 `rpi-deepseek-search` 项目，改为本插件的 feature 模块。

### 工具

| 工具 | 用途 | 参数 |
| --- | --- | --- |
| `deepseek_search` | 调 DeepSeek Search API 返回排序后的网页结果 | `query`（必填）、`count`、`need_content` |

参数说明：

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `query` | string | — | 搜索词，trim 后 1-128 字符（按字符计，不按字节） |
| `count` | integer | `5` | 返回条数，夹紧 1-50；非整数回落默认值 |
| `need_content` | boolean | `false` | 是否请求页面正文；非布尔当 false |

### 凭据

按顺序查找，找到即用：

1. 环境变量 `DEEPSEEK_API_KEY`（空白值视为未设置）
2. `~/.rpi/agent/models.json` 里 `baseUrl` 含 `api.deepseek.com` 的 provider 的 `apiKey`

两者都没有时工具返回 `{"error":"missing DeepSeek API key"}`。
**错误文本与日志里不会出现任何 key 内容**。

### 行为

- 网络调用在**独立工作线程**内：`execute` / `poll` / `cancel` / `destroy` 都不阻塞宿主
- 单次请求超时 20s；`429` 与 `5xx` 最多重试 1 次；`cancel` 后不再发起新请求
- 响应体读取上限 4 MiB，防止异常响应打爆插件内存
- 结果每行一条：`标题 / URL / 日期 / authority / summary`，缺失字段用 `-` 占位，
  summary 超过 400 字符截断（按字符边界，不会切坏 UTF-8）
- `cancel` 优先于迟到结果：取消后即使请求成功也不回传成功

## 4. go-usage：opencode-go 套餐用量（TUI 页脚）

**解决的问题**：opencode.ai 的 Go 套餐有 5 小时 / 周 / 月三档限额，用完会直接报错中断。
这个能力把当前用量常驻在 TUI 页脚，跑一轮就能看到消耗，不用再去网页控制台查。

### 展示位置与形态

`footer.after` 槽位单行，形如：

```
Go 5h 1% · 周 23% · 月 65% ⟳ 2d17h
```

- 三个数字分别是 5 小时滚动 / 周 / 月窗口的**已用**百分比。
- `⟳ 2d17h` 是月度窗口的**到期倒计时**；按量级自动切换精度
  （`⟳ 2d17h` / `⟳ 5h13m` / `⟳ 47m` / 已到期 `⟳ now`）。
- 缺失的窗口直接跳过，不显示 `0%`（`0%` 是真实值，与“未上报”不是一回事）。

### 什么情况下才显示

**两个条件同时满足**才出现：

1. 跑在**交互式 TUI** 里（`rpi -p` 等 headless 模式不显示）。
2. 当前 provider 指向 `opencode.ai`（判定优先用 `GetExecutionContext` 的
   `model.provider`，兜底查 `models.json` 里该 provider 的 `baseUrl`）。

切到别的 provider（如 `deepseek` 直连）时组件被 `hide`（不是 `remove`），
切回来能立即恢复。

### 何时刷新

**事件驱动，不轮询**：

| 事件 | 行为 |
| --- | --- |
| `BeforeTuiStart` | 打开门控（唯一启用入口）+ 首次刷新 |
| `AgentStart` | 重判 provider，按需刷新 |
| `AgentSettled` | 一轮结束，主力刷新点 |
| `ModelSelect` | 切模型，忽略节流立即重判 |

两次成功拉取之间至少隔 **60 秒**（`percent` 是整数粒度，分钟级轮询无意义）。
节流窗口内用上次快照重放，不会发请求，也不会闪烁。
请求失败时保留上一次内容，不清空。

### 凭据

只读 `~/.rpi/agent/models.json` 里 `baseUrl` 指向 `opencode.ai` 的 provider 的 `apiKey`
（**不读环境变量**：套餐用量必须和实际发请求用的 key 一致）。
**错误文本与日志里不会出现任何 key 内容**。

### 端点

```
GET https://opencode.ai/inference/go/v1/usage    Authorization: Bearer <key>
→ {"usage":{"rolling":{"percent":0,"resetsAt":"..."},
            "weekly":{"percent":22,"resetsAt":"..."},
            "monthly":{"percent":65,"resetsAt":"..."}}}
```

`zen/go/v1/usage` 是旧别名（返回同一份数据），作为兜底自动尝试。
`401` 是凭据问题，换端点也不会好，直接放弃不再重试。

---

## 5. notify：完成/等待桌面通知

**零配置即可用**，默认开启；需要关闭时在 `settings.json` 写
`"opencodeExtras": { "notify": false }`（不订阅事件、不起通知线程）。

| 触发 | 通知 |
| --- | --- |
| run 正常结束（`agent_settled`）且耗时 ≥ 5 秒 | 「rpi · 已完成」，正文 `项目 · 12.3s` |
| run 因错误结束 | 「rpi · 运行结束（出错）」 |
| `ask_user` 工具开始（run 进行中） | 「rpi · 等待你回答」，正文是问题原文（截断 120 字） |
| 选择器/对话框打开（run 进行中） | 「rpi · 等待你回答」；与 `ask_user` 3 秒内去重 |
| 对话框关闭 / run 开始 | 自动关闭上一条等待通知 |
| 用户主动中止（`aborted`） | 不提示 |

### 支持性探测

启动时探测一次 `org.freedesktop.Notifications`（D-Bus）。**探测失败就整个功能静默禁用**：

- 本机 TUI、从桌面终端启动的 `rpi --server` → 正常通知
- systemd/无桌面启动的服务端、纯 SSH 远端 → 不发通知，不打印错误，也不回退到 BEL/OSC

### 实现约束

- 事件回调只做内存读写 + 入队，`Notify` 调用全部在 `rpi-notify` 工作线程执行，不阻塞宿主事件分发
- 同一类通知用 `NotificationHandle::close()` 替换，不会堆叠
- 插件关闭时通过 lifecycle 钩子等待工作线程退出，避免 DLL 卸载后仍在执行

---

# 二、如何使用

## 步骤 1：构建

```bash
cd /c/Users/nsfoxer/Desktop/src/foxer/rpi-plugins/opencode
cargo build --release
```

产物：`target/release/rpi_opencode_extras.dll`。

## 步骤 2：安装

```bash
# 先退出正在运行的 rpi，否则 DLL 被占用会复制失败（Device or resource busy）
cp target/release/rpi_opencode_extras.dll ~/.rpi/agent/extensions/
```

如果 rpi 正在运行且无法退出，可以先把旧文件改名（Windows 允许对已映射的 DLL 重命名），
再放入新文件：

```bash
cd ~/.rpi/agent/extensions
mv rpi_opencode_extras.dll rpi_opencode_extras.dll.old
cp <项目>/target/release/rpi_opencode_extras.dll .
rm rpi_opencode_extras.dll.old     # 下次重启后删也可以
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

### 启用：在 settings.json 里加一段

编辑 rpi 全局配置文件（路径可用 `RPI_CODING_AGENT_DIR` 重定位）：

```
~/.rpi/agent/settings.json
```

在 `opencodeExtras` 下加一个 `msys2Bash` 对象，最小配置只需一个字段：

```json
{
  "theme": "dark",
  "defaultModel": "deepseek-v4.1-flash",
  "opencodeExtras": {
    "msys2Bash": {
      "msys2Root": "C:\\msys64"
    }
  }
}
```

想继承 fish 的环境变量（推荐，若你用 fish）：

```json
{
  "opencodeExtras": {
    "msys2Bash": {
      "msys2Root": "C:\\msys64",
      "envFrom": "fish"
    }
  }
}
```

想临时停用但保留配置，加 `"enabled": false`：

```json
{
  "opencodeExtras": {
    "msys2Bash": {
      "enabled": false,
      "msys2Root": "C:\\msys64"
    }
  }
}
```

> 旧版把 `msys2Bash` 放在 `settings.json` 顶层；该位置仍兼容，但新配置建议放进
> `opencodeExtras`。

> **Windows 路径要写成 `"C:\\msys64"`**（JSON 里反斜杠必须转义）。直接写 `"C:\msys64"`
> 会让 `settings.json` 整体解析失败，rpi 会报 `invalid escape at line N column M`。
> 也可以改用正斜杠：`"C:/msys64"`。
>
> 改完 `settings.json` 后请确认整份文件仍能解析。如果 rpi 启动时报
> `Could not read config: invalid JSON`，说明整份配置坏了（不只是本插件失效）。

**配置项说明**

| 字段 | 必需 | 默认 | 说明 |
| --- | --- | --- | --- |
| `msys2Root` | **是** | — | MSYS2 安装根目录，必须存在且是目录。没有内置默认值 |
| `shell` | 否 | `usr/bin/bash.exe` | 相对 `msys2Root`，也可写绝对路径；必须是文件 |
| `msystem` | 否 | `MINGW64` | 设置 `MSYSTEM` 环境变量 |
| `envFrom` | 否 | `bash` | 从哪个 shell 捕获环境变量：`bash` / `fish` / `none`。未知值回落 `bash` |
| `loginShell` | 否 | `false` | `true` 则用 `-lc` 每次 source profile（回到旧行为，每条命令多 ~600ms） |
| `waitSeconds` | 否 | `30` | `bash` 默认等待秒数，夹紧 1-120 |
| `defaultTimeoutSeconds` | 否 | `900` | 命令总截止，夹紧 1-1800 |
| `maxOutputBytes` | 否 | `1048576` | 单次输出上限（字节），必须 > 0 |
| `maxJobs` | 否 | `20` | 保留的已完成命令条数，必须 >= 1 |
| `security.blacklistEnabled` | 否 | `true` | 是否启用命令黑名单 |
| `security.blacklistPatterns` | 否 | 内置列表 | 自定义正则列表 |

实际生效的值以启动日志为准（日志里会打印 `loginShell=`、`envFrom=`、`envVars=`）。

内置黑名单（与 `msys2-mcp` 一致）拦截：`rm -rf /`、`mkfs.*`、`dd ... of=/dev/`、
`format X:`、`reg add|delete`、`shutdown`、`rd /s /q X:\`。命中直接拒绝，不创建进程。

### 验证启用成功

```bash
RPI_STARTUP_DIAGNOSTICS=1 rpi --verbose -p "1" 2>&1 | grep 'extensions: loaded'
# → loaded 11 plugin(s) (15 tool(s))     ← 工具数从 11 变成 15
```

同时会打印一行诊断：

```
rpi-opencode-extras: msys2-bash enabled (config=<settings.json 路径>, shell=C:\msys64\usr/bin/bash.exe,
  wait=30s, timeout=900s, loginShell=false, envFrom=fish, envVars=97, captureTimeout=20s)
```

`envVars=0` 表示环境捕获失败（会另外打印 `env capture skipped: ...`）；
此时命令仍能跑（非 login 模式），但会缺 fish/bash 自定义的变量。

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
  "details": { "id": 1, "status": "completed", "exitCode": 0, "elapsedMs": 307 }
}
```

`elapsedMs` 是 job 从注册到结束的毫秒数（诊断用）。它包含插件自身固定开销，
不等于 bash 启动耗时——排查"为什么慢"时参考启动日志里的 `loginShell` / `envFrom`。

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

### 关闭：删掉 settings.json 里的那一段

```bash
# 编辑 ~/.rpi/agent/settings.json，删掉 opencodeExtras.msys2Bash 对象
# （或保留配置并加 "enabled": false）
```

不需要卸载 DLL。删掉后内置 Git Bash 恢复（重启 rpi 生效）。
环境缓存文件（`%APPDATA%\rpi\msys2-bash-env.json`）可以留着，重新启用时直接命中。

---

# 三、故障排查

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 请求报 `400 MissingSessionID` | provider 未被包装 | 确认 `models.json` 里 baseUrl 就是 `opencode.ai` 或 `*.opencode.ai` |
| `--list-models` 少了模型 | 插件 declare 了目录 | 本插件不 declare；检查是否有其他插件冲突 |
| `cp` 报 `Device or resource busy` | rpi 正在运行并持有 DLL | 退出 rpi，或按上文改名法替换 |
| 工具数仍是 11（期望 15） | `settings.json` 里没有 `opencodeExtras.msys2Bash` 段、`enabled=false`，或校验失败 | 看启动输出的 `msys2-bash disabled: ...` 那行 |
| `msys2Root is required` / 未启用 | 缺少 `opencodeExtras.msys2Bash` 段 | 在全局 `~/.rpi/agent/settings.json` 里加该段 |
| `msys2Root is required` | 段里没写 `msys2Root` | 补上，它是唯一必填项 |
| `msys2Root is not a directory` | 路径写错或不存在 | 确认 `C:\msys64` 存在；JSON 里反斜杠要写 `\\` |
| `invalid escape at line N column M` | JSON 里的反斜杠没转义 | `"C:\msys64"` 改成 `"C:\\msys64"` 或 `"C:/msys64"` |
| `Could not read config: invalid JSON` | 整份 `settings.json` 坏了 | 不只是插件失效，rpi 也启不来；修好 JSON |
| `invalid \`msys2Bash\` section` | 段结构不对（如写成了字符串） | 段必须是 JSON 对象 |
| `shell is not a file` | `shell` 相对路径拼错 | 默认应是 `usr/bin/bash.exe` |
| `envVars=0` + `env capture skipped` | 捕获失败（shell 不存在 / 非零退出） | 命令仍可用；检查 `envFrom` 指定的 shell 是否存在 |
| fish 变量没继承 | `envFrom` 不是 `fish`，或变量不是 `set -xg` 导出型 | 设 `"envFrom": "fish"`；非导出变量无法继承 |
| fish 函数/别名不可用 | 这是设计边界 | 命令本体跑在 bash，不会加载 fish 函数 |
| 改了 settings.json 不生效 | 配置只在插件注册时读一次 | 重启 rpi |
| `deepseek_search` 报 `missing DeepSeek API key` | 环境变量与 `models.json` 都没有 DeepSeek 凭据 | 设 `DEEPSEEK_API_KEY`，或在 `models.json` 里配一个 `baseUrl` 含 `api.deepseek.com` 的 provider |
| `deepseek_search` 报 `HTTP 401` | key 无效 | 核对 `DEEPSEEK_API_KEY` / `models.json` 里的 `apiKey` |
| `deepseek_search` 报 `invalid params:` | 参数不符合契约 | `query` 必填且 1-128 字符；`count` 夹紧 1-50 |
| 工具数比预期少 1 | `deepseek-search` 未注册（宿主无工具槽位） | 检查启动输出的 `feature \`deepseek-search\` failed` |
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
    provider/
      mod.rs         provider feature：注册入口 + ABI 请求/流式入口
      factory.rs     按 model/options 构造 provider（含失败兜底）
      header_provider.rs  每次请求注入 header 的包装层 + 单测
      error_provider.rs   构造错误 → Error 终态的兜底 provider
    deepseek_search/
      mod.rs         deepseek-search feature：工具 schema + 提示词 + register(api)
      params.rs      入参解析与契约校验
      api_key.rs     凭据解析（DEEPSEEK_API_KEY → models.json）
      search.rs      HTTP 调用、重试策略、结果解析与格式化
      worker.rs      工作线程、句柄表、四个生命周期函数
    go_usage/
      mod.rs         go-usage feature：订阅事件 + 注册关闭钩子 + TUI 门控
      api_key.rs     从 models.json 取 opencode.ai provider 的 apiKey（不落日志）
      fetch.rs       /usage 端点 HTTP 拉取（错误文本不含 key）
      usage.rs       响应解析的纯函数与单测
      format.rs      页脚文本与倒计时纯函数与单测（自实现 RFC 3339 解析）
      worker.rs      工作线程、60s 节流、UiComponent 调用封装
      live_check.rs  联网集成检查（#[ignore]，需显式跑）
    msys2_shell/
      mod.rs        msys2-bash feature：注册 4 个工具 + bash 提示词 + 工具生命周期 ABI
      config.rs     settings.json 定位、解析、校验、黑名单、环境捕获编排
      settings.rs   读全局 settings.json 的 opencodeExtras 段
      env_cache.rs  环境变量捕获（bash/fish）+ 磁盘缓存（%）APPDATA%\rpi）
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

- 可执行：`<msys2Root>/<shell>`
- 参数：`loginShell=false`（默认）→ `-c "<command>"`；`loginShell=true` → `-lc "<command>"`
- 环境：`MSYSTEM=<msystem>`、`CHERE_INVOKING=1`、`LANG=C.UTF-8`，
  非 login 模式下额外注入启动期捕获的环境变量（见上文"启动性能"）
- 捕获：`<shell> -c env`（不是 `-lc`；fish 非 login 也会 source `config.fish`），
  设置 `MSYSTEM` + `MSYS2_PATH_TYPE=inherit` + `MSYSTEM_NOSTART=yes`
- 缓存：`%APPDATA%\rpi\msys2-bash-env.json`，指纹 `msys2Root|msystem|shell|envFrom`，
  原子写（先写 `.tmp` 再 rename），空表不写入、读取时也拒绝空表
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
| 插件加载、msys2-bash 未配置/无效 | `11 plugin(s) (12 tool(s))` ← 基线 + `deepseek_search` |
| 插件加载、msys2-bash 启用 | `11 plugin(s) (16 tool(s))` |

`deepseek-search` 默认启用，所以基线比只有 provider 时多 1 个工具。
msys2-bash 启用时是 **16** 而不是 12+3=15：注册了 4 个工具，其中 `bash` 覆盖同名内置工具，
但宿主计数时**内置与插件各算一次**（实测），因此从 12 再净增 4。

`go-usage` **不注册任何工具**（只画 UI 组件），因此不影响工具数——
本机实测（已启用 msys2-bash）仍为 `loaded 10 plugin(s) (15 tool(s))`。

### go-usage 的关键实测结论（写代码前必须知道）

| 行为 | 结果 |
| --- | --- |
| `EventHandlerFn` 内**同步**调 `runtime_action` | **进程 Segmentation fault**（exit 139） |
| `GetExecutionContext` 在**注册期**调 | `rc=1 {"error":"runtime action invoked before harness was built"}` |
| `GetExecutionContext` 在**事件派生线程**调 | `rc=0`，返回 `model.provider` |
| `UiComponent` `register` 带 `title` | `rc=1 {"error":"unexpected component field"}` |
| `UiComponent` `update` 带 `title` | `rc=0` |
| `BeforeTuiStart` 事件 | **仅交互式 TUI** 触发，`-p` 模式从不触发 |
| `register_plugin_lifecycle` 的 begin 钩子 | `-p` 模式下**未被调用**（不能依赖它做退出保护） |

因此架构上：**事件回调只起线程，所有宿主调用都在工作线程里**；
并用 `BeforeTuiStart` 当唯一启用门控，从根上避免 `-p` 模式下的退出期噪声
（`runtime action invoked after harness was dropped`）。

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
cargo build --release      # 0 warning、0 error，只产出 rpi_opencode_extras.dll
cargo test                 # 117 个单测：URL 判定 / header 构造 / provider 筛选 /
                           #            配置校验 / 环境捕获与缓存 / 输出截断 /
                           #            job 保留 / 工具边界 /
                           #            deepseek 参数校验 / 凭据解析 / 结果格式化 /
                           #            句柄生命周期与并发
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

- **配置只在加载时读**：`models.json` 与 `settings.json` 的 `opencodeExtras` 段都在插件注册
  阶段读取一次，之后修改需要重启 rpi（或触发宿主重载）才会生效。
- **未知键保留未经上游保证**：本插件把自己的配置放在 `settings.json` 的 `opencodeExtras` 键下，
  依赖 rpi 在重写 `settings.json` 时保留它不认识的键。该结构已在 rpi `0.24.0` 实测可用
  （宿主明确忽略未知字段）；若将来被磨掉，重新加回即可。
- **不 declare 模型目录**：插件不向宿主声明任何模型，`--list-models` 的内容完全来自
  `models.json`；这也意味着插件无法自带默认模型。
- **header 注入只看 baseUrl**：若同一 provider 在不同模型上使用不同端点，仍按每次请求的
  `model.base_url` 判断；不读取 provider 名字。
- **job 状态是进程内状态**：`get_cmd_result` 只能取回**同一个 rpi 进程**内创建的 job。
  每次 `rpi -p` 都是新进程，所以跨命令行调用拿不到上一次的 id（返回 `no such job`）。
  同一会话内多轮调用正常。
- **飞 Windows 平台不生效**：本 feature 依赖 MSYS2（`taskkill /F /T` 杀进程树、
  `C:\msys64` 路径布局、`.exe` 后缀），因此在 `register()` 的第一行就做了
  `cfg!(target_os = "windows")` 门禁：非 Windows 下不读配置、不捕获环境、不注册任何
  工具、也不打印诊断（`provider` feature 不受影响）。
- **`bash` 覆盖内置工具**：启用后宿主内置的 Git Bash 版本被插件的 MSYS2 版本替代。
  想恢复内置 bash，删掉 `settings.json` 里的 `msys2Bash` 段即可（无需卸载 DLL）。
- **不继承 fish 的语法与函数**：`envFrom: fish` 只继承 `set -xg` 导出的环境变量；
  fish 的函数、`abbr`、别名、以及非导出变量都不会进入 bash 子进程。
- **`poll` 是同步阻塞的**：宿主工具驱动没有 wake 回调，工具在 `poll` 内最多阻塞
  `waitSeconds`（≤120s）；因此不存在 Pending 分支，长命令会占住该次工具调用。

# 七、行为边界

- 构造 provider 失败（传输层）不会 panic：转成 `StopReason::Error` 终态交给宿主。
- 某个 provider 注册被宿主拒绝时插件返回非 0，宿主会报告
  `register returned nonzero code`；`models.json` 缺失或无匹配 provider 时返回 0，
  插件照常加载且不改变任何模型目录。
- 所有 `extern "C"` 边界都用 `rpi_plugin_sdk::guard` 包住，panic 不会 unwind 到宿主。
- 日志与输出中不出现 apiKey。
