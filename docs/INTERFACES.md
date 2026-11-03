# 接口说明（第 1 件：骨架和接口）

更新：2026-11-01（第二版：按 11 条扩展要求重做；第 3 件加了模式模块、电源模块、开机自启和崩溃重启，见第 16 节；第 4 件加了任务模块，见第 17 节）。架构和已定的决定见 [../ARCHITECTURE.md](../ARCHITECTURE.md)，任务见 [../TASKS.md](../TASKS.md)。

后面每个模块都按这里的接口接进来。改接口时同步改这份文件。

## 1. 工作区和平台分层

| 目录 | 包名 | 产物 | 内容 | 平台相关？ |
|---|---|---|---|---|
| `crates/proto` | `proto` | 库 | 名字表、协议、握手和命令目录、错误码表、协议客户端（跑在任意字节流上） | 否 |
| `crates/daemon` | `daemon` | 库 | 模块接口、注册表、事件总线、请求分发、权限检查、数据库和迁移、恢复日志、连接循环 | 否 |
| `crates/platform` | `platform` | 库 | 上面那些接口的系统实现。现在只有 `src/windows/` | **是** |
| `crates/daemon-main` | `daemon-main` | `away-guard-daemon.exe`、`away-guard-launcher.exe` | 把框架和平台实现接起来；启动器（无窗口启动后台、崩溃后重启，第 16 节） | 只调用 `platform` |
| `crates/cli` | `cli` | `away-guard.exe` | 命令行，给 agent 和人用 | 只调用 `platform` |
| `crates/agents` | `agents` | 库 | agent 适配层：每个 agent 一个适配器，对话记录读成统一格式、hook、版本（第 19 节） | 否 |
| `crates/agent-hook` | `agent-hook` | `away-guard-hook.exe` | agent 的 hook 运行它；也负责装、卸 hook（第 19 节） | 只调用 `platform` |

规则：

- **共享部分不出现平台代码。** `proto` 和 `daemon` 里没有 `cfg(windows)`，不依赖 `windows-sys`，接口、协议、错误码里没有 GUID、HANDLE、注册表路径这类东西。平台代码只在 `crates/platform/src/<系统>/`。
- **怎么检查：** `cargo check -p proto -p daemon -p agents --target wasm32-wasip1` 能过，说明共享部分不靠 Windows 也能编译（这台机器装了 wasi 目标；Linux 目标 `x86_64-unknown-linux-gnu` 没装，要用的话需要先下载）。
- 包名故意不带产品名，改名时不用动。

### 构建

```bash
cargo build
```

```bash
cargo test
```

- **SQLite：** 不打包 SQLite 源码。共享部分只定义一个接口 `daemon::sqlite::SqliteLibrary`（按名字取函数地址），绑定代码是纯 Rust（`crates/daemon/src/sqlite.rs`）。Windows 实现在 `crates/platform/src/windows/sqlite.rs`：从 System32 加载系统自带的 `winsqlite3.dll`。不需要 C 编译器。
- **GNU 工具链要 MinGW binutils：** `windows-sys` 0.61 用 raw-dylib 链接，GNU 工具链下 rustc 要调用 `dlltool`，`dlltool` 又要 `as`。要把一套完整 MinGW 的 `bin` 放进 PATH。MSVC 工具链不需要这一步。
- clippy 没装，没跑。

### 前台运行后台进程

`target\debug\away-guard-daemon.exe`，Ctrl+C / Ctrl+Break / 关控制台窗口停止。

- `--endpoint <名字>`：连接点，默认 `\\.\pipe\away-guard-<当前用户 SID>`，也可用环境变量 `AWAY_GUARD_ENDPOINT`。
- `--data-dir <目录>`：数据目录，默认 `%LOCALAPPDATA%\away-guard`，也可用环境变量 `AWAY_GUARD_DATA_DIR`。里面有数据库 `away-guard.db` 和配置文件 `config.json`。
- 退出码：0 正常停止（含注销、关机）；1 启动失败（数据库比程序新等）；2 参数错误；**3 连接点已被占用＝已经有一个在跑**（`names::DAEMON_EXIT_ALREADY_RUNNING`，启动器靠它区分"别人在跑"和"崩了"）。配置文件坏了**不算**启动失败：后台照样运行、照样恢复，只是不启动任何模块（第 7 节"开关"）。
- 日志写到 stderr，本地时间。通过启动器运行时写到 `<数据目录>\daemon.log`。
- 注销、关机、重启时：隐藏窗口收到 `WM_ENDSESSION`，后台正常停止（各模块 `stop` 把设置改回去），最多等 4.5 秒再让系统继续。
- 子命令 `away-guard-daemon autostart install|remove|show`：开机自启，见第 16 节。

### 名字和位置集中在哪

- 产品级名字：`crates/proto/src/names.rs`（产品 id、显示名、各个可执行文件名、数据库、配置文件和日志文件名、两个环境变量名、"已在运行"退出码）。改名时改这个文件，再改 `crates/cli/Cargo.toml`、`crates/daemon-main/Cargo.toml` 和 `crates/agent-hook/Cargo.toml` 里的 `[[bin]] name`；每个程序都有一个测试检查它们一致。
- 按平台惯例定的位置（默认连接点、数据目录、配置文件）：`crates/platform/src/windows/paths.rs`，只此一处。

## 2. 连接层

共享部分只认两个接口（`crates/daemon/src/transport.rs`）：

- `Transport::bind(endpoint)`：占住连接点，得到一个 `Listener`。必须只给当前用户、拒绝远程、连接点已被占用就失败（`BindError::AlreadyRunning`）。
- `Listener::accept()`：等下一个客户端，返回字节流和对方进程号（来自系统）。

分行、会话、分发都在共享部分。客户端那边，`proto::client::Client` 跑在任何 `Read + Write` 字节流上；平台只负责把流打开（`platform::connect`），`platform::connect_client` 打开后顺便握手。

### Windows：命名管道

- **名字：** `\\.\pipe\away-guard-<当前用户 SID>`，同一台机器上两个 Windows 用户互不冲突。
- **只给当前用户：** 安全描述符 `D:P(A;;GA;;;<当前用户 SID>)`，受保护的 DACL 里只有一条：当前用户完全访问。别的用户、SYSTEM 都打不开。测试 `pipe_is_restricted_to_the_current_user` 直接读管道的 DACL 验证。
- **不开网络：** 只用命名管道，拒绝远程客户端（`PIPE_REJECT_REMOTE_CLIENTS`），不监听任何端口。
- **只能有一个后台进程：** 第一个实例用 `FILE_FLAG_FIRST_PIPE_INSTANCE` 创建，名字被占用就启动失败。后台进程先占管道，再开数据库、做恢复，所以两个后台进程不会同时恢复。
- **客户端核对服务端：** 连上后查管道服务端进程属于哪个用户，不是自己就拒绝（`IPC_ERROR`），防止别人抢先占这个名字冒充后台进程。
- **记下是谁发的：** 每个连接建立时用 `GetNamedPipeClientProcessId` 取对方进程号，再通过 `System::process_image_path` 读它的可执行文件路径。这两项来自系统，可信；请求里 `client.name` 是对方自报，只做参考。三项都写进操作记录。
- **边界：** 同一个 Windows 账号下，agent 可以绕过管道直接调系统命令或改数据库，这一版挡不住（见 ARCHITECTURE.md"权限边界"）。

## 3. 协议 v1

- 一行一个 JSON 对象，UTF-8，以 `\n` 结尾（`\r\n` 也接受，空行忽略）。一问一答，`id` 相同；一个连接上可以连续发多个请求。
- 一行最长 1 MiB（不含换行）。超长：回 `BAD_REQUEST`，然后断开。
- **连接上的第一个请求必须是握手 `core.hello`**，否则回 `BAD_REQUEST` + 子码 `core.HANDSHAKE_REQUIRED`。

### 请求和响应

```json
{"v":1,"id":"away-guard-1234-1","cmd":"protect","args":{"pid":1234,"ttl_minutes":60},"client":{"name":"away-guard","version":"0.1.0"}}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `v` | 是 | 握手定下的协议版本。握手之后和握手结果不一致回 `PROTOCOL_VERSION` |
| `id` | 是 | 客户端自己定，非空字符串，最多 128 个字符，原样带回 |
| `cmd` | 是 | 正式名 `模块.动作`（如 `task.protect`），或声明过的短名（如 `protect`） |
| `args` | 否 | JSON 对象，省略等于 `{}` |
| `client` | 否 | `{"name": ..., "version": ...}`，客户端自报，不可信 |

```json
{"v":1,"id":"away-guard-1234-1","ok":true,"result":{}}
{"v":1,"id":"away-guard-1234-1","ok":false,"error":{"code":"DENIED","subcode":"core.AGENT_NOT_ALLOWED","module":"permission","message":"..."}}
```

- `id`：请求坏到读不出 id 时为 `null`。
- `error.code`：固定错误码（第 4 节），程序按它分支；`error.subcode`：带模块前缀的细分码；`error.module`：出错的模块；`error.details`：附加数据；`error.message`：英文，给人看，不保证稳定。

### 握手 `core.hello`

```json
{"v":1,"id":"c-1","cmd":"core.hello","args":{"protocols":[1],"client":{"name":"away-guard","version":"0.1.0"}}}
```

`protocols` 是客户端会说的所有版本。后台进程选两边都会的最高版本，回：

```json
{"protocol":1,
 "daemon":{"product":"away-guard","version":"0.1.0","pid":16868,"min_protocol":1,"max_protocol":1},
 "caller":{"kind":"agent","pid":12896,"exe":"D:\\...\\away-guard.exe"},
 "modules":[{"name":"task","section":"usage","summary":"...","enabled":true,"health":"running",
             "error_codes":[],"events":["task.protected","task.unprotected"]}],
 "commands":[{"name":"task.protect","module":"task","verb":"protect","section":"usage","short":"protect",
              "summary":"Protect a process tree","args":[{"name":"pid","kind":"uint","required":false,"positional":false,"help":"Root process id"}],
              "read_only":false,"agent":true,"callable":true,"hidden":false,"module_enabled":true}]}
```

- `commands` 就是**命令目录**：每条命令的正式名、短名、所属部分、说明、参数格式、能不能被当前调用方调用（`callable`）。CLI 和以后的页面、托盘都从这里生成自己的命令列表，后台加了命令，客户端不用改。
- 参数类型 `kind`：`int`、`uint`、`str`、`bool`（开关）、`str_list`（可重复）、`json`。
- 没有共同版本：回 `PROTOCOL_VERSION`，子码 `core.CLIENT_TOO_OLD`（客户端太旧）或 `core.DAEMON_TOO_OLD`（后台太旧），`details` 里有两边的版本范围。

### 兼容规则（已实现）

1. **字段只加不删，含义不改。** 要改含义就升协议版本，握手时两边自动选共同版本。
2. **不认识的字段一律忽略。** 两边都没有 `deny_unknown_fields`；测试里新客户端带了多余字段、新后台回了多余字段都能用。
3. **错误码只加不改：** 已有的码不改名、不改号、不复用。客户端收到不认识的码当 `UNKNOWN`（退出码 19）。
4. **子码按模块分命名空间：** `<模块>.<名字>`，框架自己的是 `core.*`。模块只能用自己声明过的子码，用了别人的会被框架换成 `INTERNAL`。
5. **不认识的枚举值不报错：** 所属部分、参数类型遇到新值时当 `unknown`，客户端照样能列出、能传值。

新旧组合：

| 组合 | 结果 |
|---|---|
| 新 CLI + 没有握手的老后台 | 老后台对 `core.hello` 回 `UNKNOWN_COMMAND`，CLI 换成 `PROTOCOL_VERSION` + `core.DAEMON_TOO_OLD`，提示更新后台。退出码 5 |
| 新 CLI + 只会更新版本的后台 | 后台回 `PROTOCOL_VERSION` + `core.CLIENT_TOO_OLD`，提示更新客户端。退出码 5 |
| 老 CLI + 新后台（共同版本存在） | 正常用；后台新加的命令老 CLI 也能看到并调用（命令目录来自后台），新字段被忽略 |
| 双方都有共同版本 | 用最高的共同版本 |

测试：`crates/platform/tests/pipe.rs` 的 `version_mismatch_in_either_direction_is_a_clear_error`（用假的老后台、假的新后台、真后台配只会新版本的客户端）。

### 用 PowerShell 直接试

```powershell
$c = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'away-guard-<SID>', [System.IO.Pipes.PipeDirection]::InOut)
$c.Connect(3000); $w = New-Object System.IO.StreamWriter($c); $w.AutoFlush = $true; $r = New-Object System.IO.StreamReader($c)
$w.WriteLine('{"v":1,"id":"ps-0","cmd":"core.hello","args":{"protocols":[1]}}'); $null = $r.ReadLine()
$w.WriteLine('{"v":1,"id":"ps-1","cmd":"status"}'); $r.ReadLine()
```

## 4. 错误码和退出码

固定表在 `crates/proto/src/error.rs`。CLI 的退出码 = 表里的号；成功是 0。新码加在 `Unknown` 上面，用下一个空着的号。

| 退出码 | 错误码 | 谁产生 | 含义 |
|---|---|---|---|
| 0 | — | — | 成功 |
| 1 | `CLI_FAILURE` | 客户端 | CLI 自己出了意外（例如算不出默认连接点） |
| 2 | `USAGE` | 客户端 | 命令行参数不对，或没给命令 |
| 3 | `DAEMON_NOT_RUNNING` | 客户端 | 连接点上没人，后台没开 |
| 4 | `IPC_ERROR` | 客户端 | 连接断了、超时（`details.reason = "timeout"`）、回复不是合法协议、服务端是别的用户 |
| 5 | `PROTOCOL_VERSION` | 两边 | 没有共同协议版本，或请求用错了版本 |
| 10 | `BAD_REQUEST` | 后台 | 请求行不合法（不是 JSON、缺字段、超长、没先握手） |
| 11 | `UNKNOWN_COMMAND` | 两边 | 没有这个命令。CLI 里打错命令也是这个 |
| 12 | `INVALID_ARGS` | 后台 | 命令存在，参数不对（框架按声明检查，模块也可以自己报） |
| 13 | `NOT_IMPLEMENTED` | 后台 | 模块还是空壳 |
| 14 | `MODULE_DISABLED` | 后台 | 模块被配置文件关了；或配置文件坏了、所有模块都没启动（子码 `core.CONFIG_INVALID`） |
| 15 | `DENIED` | 两边 | 策略拒绝：agent 调了只给页面用的命令，或操作需要管理员权限 |
| 16 | `NOT_FOUND` | 后台 | 请求指向的东西不存在 |
| 17 | `CONFLICT` | 后台 | 和当前状态冲突 |
| 18 | `INTERNAL` | 后台 | 后台内部出错（包括模块用了没声明的子码、发了没声明的事件） |
| 19 | `UNKNOWN` | 客户端 | 收到本版本不认识的错误码 |
| 20 | `MODULE_FAULTED` | 后台 | 这个模块之前崩了（panic、启动失败、迁移失败），本次运行内停用，重启后台后恢复 |

框架的子码（`core.*`）：

| 子码 | 配哪个码 | 含义 |
|---|---|---|
| `core.HANDSHAKE_REQUIRED` | `BAD_REQUEST` | 没先发 `core.hello` |
| `core.AGENT_NOT_ALLOWED` | `DENIED` | 命令只给页面和托盘用 |
| `core.NEEDS_ELEVATION` | `DENIED` | 需要管理员权限，后台现在以普通用户运行 |
| `core.CONFIG_INVALID` | `MODULE_DISABLED` | 配置文件坏了，没有启动任何模块；修好文件后重启后台 |
| `core.CLIENT_TOO_OLD` / `core.DAEMON_TOO_OLD` | `PROTOCOL_VERSION` | 哪一边太旧 |

模块自己的子码在 `ModuleSpec::error_codes` 里声明，握手的 `modules[].error_codes` 会列出来。

## 5. 现有命令

正式名都是 `模块.动作`。常用的 10 个有短名（`proto::commands::COMMON_SHORT_NAMES`，测试 `every_common_short_name_exists_and_routes` 检查它们都在）。"agent"一栏是 agent 能不能调。

| 正式名 | 短名 | 所属部分 | agent | 只读 | 参数 | 现在 |
|---|---|---|---|---|---|---|
| `core.status` | `status` | 公共部分 | 是 | 是 | — | 可用 |
| `core.log` | `log` | 公共部分 | 是 | 是 | `limit` uint（1–1000，默认 50） | 可用 |
| `core.hello` | — | 公共部分 | 是 | 是 | 见第 3 节；不在命令行里显示 | 可用 |
| `mode.away` | `away` | 使用 | 是 | 否 | `preset` str（默认用设置里的默认方案）、`trigger` str（`button`/`idle`，只给页面和托盘） | 可用 |
| `mode.back` | `back` | 使用 | 是 | 否 | — | 可用 |
| `mode.show` | — | 使用 | 是 | 是 | — | 可用 |
| `mode.configure` | — | 使用 | **否** | 否 | `settings` json（必填，部分更新） | 可用，只给页面 |
| `mode.reports` | — | 使用 | 是 | 是 | `limit` uint（默认 5） | 可用 |
| `power.settings` | — | 使用 | 是 | 是 | — | 可用 |
| `power.configure` | — | 使用 | **否** | 否 | `settings` json（必填，部分更新） | 可用，只给页面 |
| `power.refresh` | — | 使用 | 是 | 否 | — | 可用 |
| `resource.free` | `free` | 使用 | 是 | 否 | `mb` uint；返回实测结果与待处理请求编号 | 可用，见 [权限与资源说明](PERMISSIONS_RESOURCES.md) |
| `resource.result` / `resource.settings` | — | 使用 | 是 | 是 | 查询请求、读设置 | 可用 |
| `resource.check` | — | 使用 | 是 | 否 | 立即检查 | 可用 |
| `resource.configure` | — | 使用 | **否** | 否 | `settings` json | 可用，只给页面 |
| `network.show` / `network.profiles` / `network.settings` | — | 使用 | 是 | 是 | 当前状态、已保存网络、设置 | 可用，见 [使用部分](USAGE_NETWORK_RESUME_RELAY.md) |
| `network.history` | — | 使用 | 是 | 是 | `limit` uint，默认 20 | 可用 |
| `network.check` | — | 使用 | 是 | 否 | 立即检查，并按当前模式执行策略 | 可用 |
| `network.configure` | — | 使用 | **否** | 否 | `settings` json，部分更新 | 可用，只给页面 |
| `resume.show` / `resume.settings` | — | 使用 | 是 | 是 | 队列、设置 | 可用，见 [使用部分](USAGE_NETWORK_RESUME_RELAY.md) |
| `resume.check` / `resume.cancel` | — | 使用 | 是 | 否 | 检查队列 / `id` 取消等待 | 可用 |
| `resume.configure` | — | 使用 | **否** | 否 | `settings` json，部分更新 | 可用，只给页面 |
| `task.list` | `tasks` | 使用 | 是 | 是 | `all` bool（也列最近结束的 20 个） | 可用 |
| `task.protect` | `protect` | 使用 | 是 | 否 | `pid` uint 或 `ancestor` uint、`label` str、`ttl_minutes` uint、`deps` str_list、`mem_limit_mb` uint、`transcript` str、`session_id` str、`source` str（`cli`/`hook`） | 可用 |
| `task.renew` | `renew` | 使用 | 是 | 否 | `task_id` int（必填，按位置）、`ttl_minutes` uint | 可用 |
| `task.unprotect` | `unprotect` | 使用 | 是 | 否 | `task_id`（按位置）或 `session_id` | 可用 |
| `task.prioritize` | `prioritize` | 使用 | 是 | 否 | `task_id`（必填，按位置）、`level` str（`high` 默认 / `above_normal` / `normal` 撤销） | 可用 |
| `task.check` | — | 使用 | 是 | 否 | — | 可用 |
| `task.settings` | — | 使用 | 是 | 是 | — | 可用 |
| `task.configure` | — | 使用 | **否** | 否 | `settings` json（必填，部分更新） | 可用，只给页面 |
| `permission.list` | — | 保护 | 是 | 是 | — | 可用 |
| `permission.set` | — | 保护 | **否** | 否 | `program` 完整路径、`class`: `never` / `may_end` | 可用，只给页面 |
| `permission.request` / `permission.requests` | — | 保护 | 是 | 否 / 是 | 提交申请 / 列待审批项 | 可用 |
| `permission.review` | — | 保护 | **否** | 否 | `request_id`、`approve` | 可用，只给页面 |
| `agents.list` | — | 账号和存储 | 是 | 是 | — | 可用，第 19 节 |
| `agents.detect` | — | 账号和存储 | 是 | 是 | —（后台运行各 agent 的 `--version`，结果进 `list`） | 可用 |
| `agents.hook` | — | 账号和存储 | 是（不在命令行里显示） | 否 | `agent` str、`event` json（都必填）；由 `away-guard-hook` 发 | 可用 |
| `files.scan` | — | 账号和存储 | 是 | 否 | `no_measure` bool；在后台跑，马上返回（"立即统计"） | 可用，第 20 节 |
| `files.scans` | — | 账号和存储 | 是 | 是 | —（当前进度、下次自动统计时间、最近 10 次） | 可用 |
| `files.top` | — | 账号和存储 | 是 | 是 | `limit` uint（默认 10）、`agent` str、`by_folder` bool | 可用 |
| `files.conversation` | — | 账号和存储 | 是 | 是 | `id` str（必填，按位置；可以只给前 6 个以上字符）、`agent`、`limit`（默认 50） | 可用 |
| `files.downloads` | — | 账号和存储 | 是 | 是 | `id`（同上）、`agent` | 可用 |
| `files.find` | — | 账号和存储 | 是 | 是 | `query` str（必填，按位置，路径的一部分）、`limit`（默认 20） | 可用 |
| `files.caches` | — | 账号和存储 | 是 | 是 | — | 可用 |
| `files.settings` | — | 账号和存储 | 是 | 是 | — | 可用 |
| `files.configure` | — | 账号和存储 | **否** | 否 | `settings` json（必填，部分更新） | 可用，只给页面 |
| `example.echo` | — | 公共部分 | 是 | 否 | `text` str（必填，按位置）、`shout` bool | 可用（默认关） |
| `example.history` | — | 公共部分 | 是 | 是 | `limit` uint | 可用（默认关） |

模式和电源两个模块的命令、设置、事件见第 16 节，任务模块见第 17 节。网络与续跑命令见 [使用部分说明](USAGE_NETWORK_RESUME_RELAY.md)，管理员层见 [管理员服务](ADMIN_SERVICE.md)。对话范围仍是空壳，没有命令。

第 5、6 件的命令、阈值、权限与恢复规则见 [权限名单与资源模块](PERMISSIONS_RESOURCES.md)。

### `status` 结果

```json
{"daemon":{"product":"away-guard","version":"0.1.0","protocol":{"min":1,"max":1},"pid":16868,
           "started_at":1793569608406,"uptime_ms":1443,"endpoint":"\\\\.\\pipe\\away-guard-S-1-5-21-...",
           "data_dir":"C:\\...","sqlite":"winsqlite3.dll 3.53.4","config_warnings":[],"config_error":null},
 "modules":[{"name":"task","section":"usage","enabled":true,"health":"running","fault":null,"schema_version":1,
             "commands":["task.list","task.protect","..."],"state":{"implemented":false}}],
 "journal":{"open":0,"last_recovery":{"restored":0,"adopted":0,"failed":0,"orphaned":0,"errors":[]}}}
```

- `health`：`running`、`stopped`、`disabled`（配置关了）、`config_error`（配置文件坏了，没启动）、`faulted`（`fault` 写着原因）。
- `state`：模块自己的 `status()`；模块正忙（锁被占 200 毫秒以上）时是 `{"busy": true}`，不会卡住 `status`。约定（任何模块都可以用）：`state.summary` 是一行给人看的概况，`state.notices` 是字符串数组，列出没生效的东西和原因（比如"这一项需要管理员权限，没生效"）。CLI 的 `status` 文字输出把它们打在模块那一行下面。
- `config_warnings`：配置文件里提到了不存在的模块等。
- `config_error`：配置文件读不了或不是合法 JSON 时，这里是具体错误（文件路径和出错位置），所有模块都没启动；正常时是 `null`。时间一律是 Unix 毫秒。

### `log` 结果

`{"entries":[{"id","ts","request_id","cmd","module","args","client_pid","client_exe","client_name","caller_kind","ok","error_code","subcode","duration_ms"}]}`，新的在前。

## 6. 权限：谁能调什么

- **由框架统一检查，模块不用管。** 每条命令在声明里写明 agent 能不能调（`CommandSpec::agent()`）。**不写就是只给页面和托盘**（默认拒绝）。
- 调用方分两类（`CallerKind`）：
  - `trusted`：可执行文件是我们自己的页面或托盘程序（`away-guard-ui.exe`、`away-guard-tray.exe`，且和后台进程在同一个目录）。路径来自系统，不看客户端自报。
  - `agent`：其他所有人（CLI、脚本、agent）。
- 顺序：握手检查 → 找命令（找不到 `UNKNOWN_COMMAND`）→ **权限检查**（agent 调只给页面的命令：`DENIED` + `core.AGENT_NOT_ALLOWED`）→ 参数检查 → 交给模块 → 写操作记录。
- 握手返回的命令目录里，`callable` 已经按调用方算好。CLI 只显示 `callable` 的命令，所以命令行里没有任何修改权限名单的命令；硬打 `permission set` 会在本地直接回 `DENIED`（退出码 15）。
- 判定规则在 `crates/daemon/src/access.rs`（`TrustPolicy`），测试可以换成自己的路径。

## 7. 模块接口

定义在 `crates/daemon/src/module.rs`。一个模块 = 一份静态描述 `ModuleSpec` + 一个实现 `Module` 的类型。框架从描述里自动做：路由、权限检查、参数检查、命令目录、迁移、事件投递、故障隔离。

### `ModuleSpec`

```rust
static SPEC: ModuleSpec = ModuleSpec::new("task", Section::Usage, "Protected process trees")
    .commands(&[
        CommandSpec::new("protect", "Protect a process tree").short("protect").agent().args(&[
            ArgSpec::new("pid", ArgKind::Uint, "Root process id"),
            ArgSpec::new("deps", ArgKind::StrList, "Dependency outside the tree, repeatable"),
        ]),
    ])
    .publishes(&[EventSpec::new("protected", "A task is now protected")])
    .subscribes(&["network.*"])
    .migrations(&[Migration::new(1, "CREATE TABLE task_protected (...)")])
    .error_codes(&[ErrorSpec::new("ALREADY_PROTECTED", ErrorCode::Conflict, "...")]);
```

| 部分 | 说明 |
|---|---|
| `name` | 稳定 id，`[a-z][a-z0-9_]*`，最长 32。存进数据库，发布后不能改 |
| `section` | **所属部分**（见下） |
| `enabled_by_default` / `.off_by_default()` | 配置文件没写时开不开 |
| `commands` | 动作名（`[a-z][a-z0-9_]*`）、可选短名（全局唯一，不能和模块名相同）、说明、参数、`.read_only()`、`.agent()`、`.hidden()` |
| 参数 `ArgSpec` | 名字、类型、`.required()`、`.positional()`（按位置给，`bool` 和 `str_list` 不能按位置）。`json`、`endpoint`、`timeout`、`help`、`version` 是命令行的全局选项，不能当参数名 |
| `publishes` | 本模块发的事件，主题 `<模块>.<名字>` |
| `subscribes` | 要收的主题：`network.down`、`power.*` 或 `*` |
| `migrations` | 本模块自己的表（第 9 节） |
| `error_codes` | 本模块的子码，`[A-Z][A-Z0-9_]*`，各自对应一个框架错误码 |

注册时整份描述会被检查（名字格式、重名、短名冲突、迁移编号），不合格后台就不启动。

### 所属部分 `Section`

定义在 `crates/proto/src/catalog.rs`（命令目录也用它），以后可以加新值；客户端遇到不认识的值显示为"其他"。

| 值 | 中文 | 现在的模块 | 以后 |
|---|---|---|---|
| `common` | 公共部分 | `core`（后台本身）、`example` | 页面、托盘 |
| `usage` | 使用 | `mode`、`power`、`network`、`resume`、`task`、`resource` | API 中转为独立程序 |
| `accounts_storage` | 账号和存储 | `agents`（agent 适配层）、`files`（文件归属）、`scope`（对话范围） | 搜索、清理 |
| `protection` | 保护 | `permission`（权限）、`exit_guard`（出口保护） | — |

它出现在握手的 `modules[].section`、`commands[].section` 和 `status` 的 `modules[].section` 里；CLI 帮助和 `status` 的文字输出都按它分组。

### `Module`

```rust
pub trait Module: Send {
    fn spec(&self) -> &'static ModuleSpec;
    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> { Ok(()) }
    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> { Ok(()) }
    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError>;
    fn status(&self, ctx: &ModuleCtx) -> Value { json!({}) }
    fn recover(&mut self, entry: &JournalEntry, ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError>;
    fn on_event(&mut self, event: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> { Ok(()) }
}
```

- `ModuleRequest`：`id`、`cmd`（动作名，如 `protect`）、`args`（已按声明检查过的对象）、`caller`（`pid`、`exe` 来自系统，`name` 自报，`kind` 是 agent/trusted）。
- `handle` 返回 `Ok(值)` 就是响应的 `result`，`Err(ApiError)` 就是 `error`。
- `ModuleCtx`（可克隆，模块自己的线程可以留一份）：
  - `journal()`：本模块的恢复日志；`settings()`：本模块的键值设置；`db()`：本模块自己的表；
  - `publish(事件, 数据)`：发事件，事件必须先声明；
  - `error("NAME", 信息)`：按声明生成错误（码来自声明，子码 `<模块>.NAME`）；
  - `system()`：普通系统调用；`elevated()`：需要管理员权限的操作（第 11 节）。

### 生命周期

启动顺序：

1. 占住连接点（保证只有一个后台进程）；
2. 读配置文件 `config.json`（没有就用默认；坏了就记下错误，照样往下走，只是第 7 步不启动任何模块）；
3. 打开数据库，跑框架自己的迁移；
4. 注册模块并检查描述；建立事件订阅；
5. 每个模块跑自己的迁移（关着的模块也跑，以后打开不用重建）；
6. **恢复：** 把所有没恢复的日志条目交给写它的模块的 `recover`（关着的模块也恢复）；
7. 按注册顺序 `start` 打开的模块；
8. 发 `core.started` 事件，开始接受连接。

停止时：不再接受连接，断开现有连接，按相反顺序 `stop`。

### 故障隔离

- 每个模块有自己的锁，方法拿 `&mut self`。`handle`、`status`、`on_event` 要快；要等几十秒的事放在模块自己的线程里。
- 模块在任何方法里 panic、`start` 返回错误、迁移失败：**只把这个模块标成 `faulted`**，发 `core.module_faulted` 事件，它的命令此后回 `MODULE_FAULTED`（退出码 20），直到重启后台。后台进程和其他模块照常。故障模块的恢复日志照样交给它的 `recover`（第 10 节）；`stop` 时对已启动过的故障模块仍尽量调一次 `stop`，让它有机会把设置改回去。
- `status` 取模块状态最多等 200 毫秒，等不到就显示 `{"busy": true}`。
- 测试：`crates/daemon/tests/isolation.rs`。

### 开关

配置文件 `<数据目录>\config.json`：

```json
{"modules": {"example": {"enabled": true}, "network": {"enabled": false}}}
```

没写的模块用 `enabled_by_default`。对话范围（`scope`）和示例（`example`）默认关，其余默认开。关掉的模块：命令回 `MODULE_DISABLED`（退出码 14），但迁移和恢复照做。不认识的键忽略，方便以后加。

**配置文件坏了**（读不了、不是合法 JSON、格式不对）：

- 后台照样启动、照样接受连接；数据库照样打开，迁移和恢复日志照样执行，所以崩溃前改过的系统设置照样改回去。
- **不启动任何模块**，也不按默认配置启动：写错一个字不能悄悄打开用户关掉的模块。模块的 `health` 是 `config_error`，命令回 `MODULE_DISABLED` + 子码 `core.CONFIG_INVALID`，消息里带具体错误。
- `core.status`、`core.log`、握手照常可用；`status` 的 `daemon.config_error` 写着具体错误，CLI 的 `status` 文字输出会单独一行提示。
- 修好文件后重启后台恢复正常。

### 现有模块

都在 `crates/daemon/src/modules/<名字>/mod.rs`。空壳用 `StubModule`（`modules/stub.rs`）：命令回 `NOT_IMPLEMENTED`，`status` 回 `{"implemented": false}`，从不改系统。

| 模块 id | 中文 | 所属部分 | 默认 | 说明 |
|---|---|---|---|---|
| `mode` | 模式 | 使用 | 开 | **可用**（第 3 件）：普通 / 离开 / 自定义，`away`、`back`、方案、离开报告；有表 `mode_reports` |
| `power` | 电源 | 使用 | 开 | **可用**（第 3 件）：防睡眠、合盖、过热和低电量、读数；第 16 节 |
| `network` | 网络 | 使用 | 开 | 已实现探测、已保存网络切换、备用网卡优先级、恢复与断网历史，见 [使用部分](USAGE_NETWORK_RESUME_RELAY.md) |
| `resume` | 续跑 | 使用 | 开 | 已实现持久队列、适配器能力降级、网恢复后重开原会话，见 [使用部分](USAGE_NETWORK_RESUME_RELAY.md) |
| `resource` | 资源 | 使用 | 开 | 已实现冻结、实测腾内存、限额和恢复，见 [权限与资源](PERMISSIONS_RESOURCES.md) |
| `task` | 任务 | 使用 | 开 | **可用**（第 4 件）：跟踪进程树、依赖、有效期、优先级；第 17 节 |
| `permission` | 权限 | 保护 | 开 | 已实现完整路径 / 签名名单、不可改内置项、申请审批，见 [权限与资源](PERMISSIONS_RESOURCES.md) |
| `exit_guard` | 出口保护 | 保护 | 开 | 检查、进程动作、管理员防火墙与报告已接通；名单默认空，服务未装时明确显示未应用；见第 18 节 |
| `agents` | agent 适配层 | 账号和存储 | 开 | **可用**（第 21 件）：收 hook、发 `agents.*` 统一事件、列出适配器和装了哪些 agent；第 19 节 |
| `files` | 文件归属 | 账号和存储 | 开 | **可用**（第 14 件）：从对话记录归属路径、统计大小、共享缓存单列；启动后不马上扫，第一次自动统计在 10 分钟后；第 20 节 |
| `scope` | 对话范围 | 账号和存储 | **关** | 空壳 |
| `example` | 示例 | 公共部分 | **关** | 能用的完整示例，见第 13 节 |

## 8. 事件总线

`crates/daemon/src/bus.rs`。**模块之间不直接调用**，只通过事件来往：

- 发：`ctx.publish("down", json!({...}))`，主题自动成为 `network.down`。没在 `publishes` 里声明的事件会被拒绝（`INTERNAL`）。
- 收：在 `subscribes` 里写主题（`network.down`、`network.*`、`*`），框架调 `on_event`。
- 每个订阅的模块有自己的投递线程，按发出顺序投递；发事件从不等订阅方。模块只有在 `running` 时才收事件。
- 框架自己的事件：`core.started`、`core.module_faulted`（`{"module", "reason"}`）。
- 测试里用 `bus().wait_idle(超时)` 等投递完。
- 模块之间已定的事件内容（谁发、谁收）见第 16 节"事件约定"。

## 9. 数据库和迁移

文件 `<数据目录>\away-guard.db`，WAL 模式，`synchronous = FULL`。只有后台进程读写它，其他程序一律走协议。

### 每个模块自己管自己的表

- 框架是组件 `core`，每个模块也是一个组件。各自在 `migrations` 里按 1、2、3… 列出升级步骤；表 `schema_versions(component, version, updated_at)` 记每个组件升到了第几步。
- 启动时只跑没跑过的步骤，每步一个事务。**数据库从不重建**，加模块、加列都是新的一步。
- 已经发布的步骤不能改，只能往后加（例子：`example` 模块的第 2 步给第 1 步的表加了一列）。
- **名字前缀强制：** 一步里新建的表、索引、视图、触发器必须以 `<组件>_` 开头（框架的是 `core_`）。违反就整步回滚，模块标成 `faulted`。
- 某步 SQL 出错：这一步回滚，版本停在上一步，模块 `faulted`，其他模块不受影响。
- 数据库里的版本比模块新（用旧程序开了新数据库）：不动它，模块 `faulted`。框架自己（`core`）比程序新：后台不启动。
- 测试：`crates/daemon/tests/migrations.rs`。

### 框架的表（`core` 第 1 步，`crates/daemon/src/store.rs`）

`core_settings`：`scope`（`core` 或模块 id）、`key`、`value`（JSON）、`updated_at`，主键 `(scope, key)`。

`core_op_log` 操作记录：

| 列 | 说明 |
|---|---|
| `id`、`ts` | 主键、Unix 毫秒 |
| `request_id`、`cmd`、`module`、`args` | 请求 id、正式命令名（未知命令原样）、处理它的模块、参数 JSON |
| `client_pid`、`client_exe` | 来自系统 |
| `client_name` | 客户端自报 |
| `caller_kind` | `agent` 或 `trusted` |
| `ok`、`error_code`、`subcode`、`duration_ms` | 结果 |

写入规则：只读命令成功时不写（页面和托盘会频繁轮询 `status`），其他一律写，包括不合法的请求和未知命令。

`core_journal` 恢复日志：`id`、`module`、`key`、`original`（原值 JSON）、`applied`（最近一次改成的值）、`note`、`status`（`open`/`restored`）、`created_at`、`updated_at`、`restored_at`、`restored_by`（`module` 或 `startup`）、`attempts`（恢复失败次数）、`last_error`。唯一索引：同一个 `(module, key)` 最多一条 `open`。

### 模块的表

| 表 | 模块 | 说明 |
|---|---|---|
| `mode_reports` | `mode` | 离开报告：`id`、`started_at`、`ended_at`、`preset`、`report`（JSON）、`seen` |
| `task_protected` | `task` | 受保护的任务。第 1 步（草案）：`id`、`label`、`root_pid`、`root_exe`、`root_created_at`、`deps`（JSON）、`priority`（0 没提、1 高于正常、2 高）、`mem_limit_mb`、`status`（`active`/`ended`）、`created_at`、`expires_at`（`renew` 给的"至少保护到"）、`ended_at`、`requested_by_pid`、`requested_by_exe`。第 2 步加了：`source`、`session_id`、`transcript`、`members`（JSON：`[[pid, 启动时间], …]`）、`last_active_at`、`end_reason`、`ended_by`（JSON：谁）、`requested_by_name`、`requested_by_kind` |
| `example_echoes` | `example` | `id`、`text`、`ts`，第 2 步加了 `caller_pid` |

## 10. 恢复日志

用途：改任何系统设置之前，先把原值写进数据库；不管后台是正常退出、崩溃、被杀还是断电，下次启动都能改回去。

```rust
// 1. 改之前先记原值（落盘后才返回）
let entry = ctx.journal().record("lid_action_ac", &json!(original), Some(&json!(new_value)))?;
// 2. 再改系统
apply(new_value)?;
// 3. 正常撤销时：先把原值改回去，再关掉这条记录
apply(entry.original.clone())?;
ctx.journal().mark_restored(entry.id)?;
```

- `record(key, 原值, 新值)`：同一个 `key` 同时最多一条没恢复的记录；已经有了就保留最早的原值，只更新"新值"。所以同一个设置连改两次，原值也不会丢。
- `mark_restored(id)`：模块自己改回去之后调用，只能关自己模块的记录。`open_entries()` / `open_entry(key)` 查本模块还没恢复的记录。
- `stop()` 里应该把本模块还开着的改动撤销。没撤销的，下次启动时恢复。

启动时，所有没恢复的记录按从新到旧交给对应模块的 `recover(entry)`。**模块是什么状态都照样交：** 被配置关掉的、配置文件坏了没启动的、出了故障的（比如数据库升级失败）都一样。把设置改回去比开关和故障都重要。

| 结果 | 处理 |
|---|---|
| `Ok(Restored)` | 原值已改回，记录关闭（`restored_by = "startup"`） |
| `Ok(Adopted)` | 模块决定继续保持这个改动（例如"离开"模式是持久的、还开着），记录保持打开 |
| `Err(..)` 或模块 panic | 兜住，只影响这个模块：记录保持打开，失败次数 +1，记下原因，下次启动再试；panic 的模块标成 `faulted` |
| 找不到这个模块（没注册、没有可调用的对象） | 唯一跳过的情况：算"孤儿"，保持打开，等模块回来再恢复 |

恢复不依赖配置文件：配置文件坏了时，数据库照样打开、恢复照样执行，之后才是"不启动模块"（第 7 节"开关"）。

`recover` 必须幂等：崩溃可能发生在"记下原值"之后、"真正改系统"之前。结果在 `status` 的 `journal` 里。

测试 `crates/daemon/tests/recovery.rs` 用一个假模块（内存里的一个整数当"系统设置"，后台"崩溃"后它还在）证明：改了→崩溃→重启后改回；连改两次恢复的是最早的原值；正常撤销后不会再恢复；记下原值后、改系统前崩溃也正确；恢复失败下次重试；`Adopted` 保持打开；孤儿等模块回来；关着的模块照样恢复；改系统之前另一个数据库连接已经能看到记录（先落盘再改）；**配置文件坏了照样恢复，且后台在运行、`status` 报配置错误、模块都没启动**（`broken_config_file_still_recovers_and_starts_no_module`）；**数据库升级失败的模块照样恢复**（`module_whose_migration_failed_still_recovers`）；**故障模块的 `recover` 自己崩了也被兜住、记下、下次重试，别的模块不受影响**（`faulted_module_whose_recovery_panics_is_contained_and_retried`）。

## 11. 系统调用接口

`crates/daemon/src/sys/`。模块和框架**只通过这些接口碰系统**，接口签名里没有平台专有的类型。

| 接口 | 用途 | Windows 实现 | 测试用的假实现 |
|---|---|---|---|
| `System` | 普通权限的调用：`process_image_path(pid)`、`uptime_ms()`；第 3 件加了：读 `power_status()`（电量、插电、省电）、`temperatures()`、`memory()`、`connectivity()`、`power_capabilities()`、`restart_pending()`；电源计划 `power_setting(s)` / `set_power_setting(s, 值)`（`PowerSetting`：合盖动作、CPU 上限、亮度、省电阈值、USB 省电、无线网卡省电；值带所属计划 GUID 和插电/电池两个值）、`power_mode()` / `set_power_mode()`（能效/平衡/性能）；`keep_awake(原因, 屏幕也亮)` 返回一个守卫，丢掉即取消；`lock_screen()`、`display_off()`、`suspend(睡眠/休眠/关机)`；`watch(回调)` 订阅系统通知（`SystemEvent`：合盖、电源、锁屏、屏幕、睡眠/唤醒），返回守卫；第 4 件加了：`processes()`（进程快照：pid、父 pid、程序名）、`process_info(pid)`（完整路径、启动时间；pid 会被复用，pid + 启动时间才认得出同一个进程）、`process_usage(pid)`（CPU、I/O、私有内存）、`track_tree(根, 成员)`（作业对象跟踪整棵树，**从不结束任何进程**）、`gpu_usage()`、`process_priority(pid)` / `set_process_priority(pid, 优先级)`（`ProcessPriority` 里**没有"实时"**；权限不够回 `DENIED` + `core.ACCESS_DENIED`）、`file_modified_ms(路径)`、`now_ms()`（假实现可以拨快） | `platform::WindowsSystem` | `sys::fake::FakeSystem`（每一项都可设、都记下；`emit()` 模拟系统通知；假进程表 `spawn` / `exit` / `use_cpu` / `use_io` / `set_gpu` / `set_elevated`，子进程像真的一样自动进父进程所在的树；`touch` 写假文件时间；`advance_ms` 拨快时钟） |
| `Elevated` | **需要管理员权限的操作，单独一层，故意很窄**：`backend()`、`set_interface_metric(接口, 跃点数)`、`terminate_process(pid)`；第 3 件加了 `update_pause()` / `set_update_pause(到期时间)`（暂停系统更新）、`adapter_power_saving()` / `set_adapter_power_saving()`（网卡"允许关闭此设备以节约电源"）、`power_requests()`（全系统谁在阻止睡眠，`powercfg /requests`）；第 4 件加了 `set_process_priority`（普通权限改不了的进程，比如用管理员权限启动的） | `platform::UserModeElevated`：后台现在以普通用户运行，一律回 `DENIED` + `core.NEEDS_ELEVATION`，什么都不做 | `sys::fake::FakeElevated`：记下被要求做了什么，并在内存里保存值；`sys::fake::RefusingElevated`：和普通用户一样全部拒绝，用来测"没生效"的显示 |
| `SqliteLibrary` | 取 SQLite 函数地址 | `platform::Winsqlite3` | 测试用真的 `winsqlite3.dll` 和临时目录 |
| `Transport` / `Listener` | 连接层（第 2 节） | `platform::PipeTransport` | 测试用临时管道名 |

- 以后后台改成服务时，只换 `Elevated` 的实现（比如通过服务去做），模块代码不动。
- 测试里不碰真实系统：后台里的系统调用全用假实现；`platform` 的测试只读不写（读自己的可执行文件路径、开机时长、管道 DACL、电量、温度、内存、网络、电源计划的当前值、屏幕状态通知）。
- 新加的 `System` 方法默认实现是回 `NOT_IMPLEMENTED` + `core.UNSUPPORTED`，新加的 `Elevated` 方法默认回 `NEEDS_ELEVATION`，所以加方法不会弄坏别的实现。
- 这台机器没有的功能（比如没有电池、没有温度传感器）回 `NOT_IMPLEMENTED` + `core.UNSUPPORTED`，`status` 里显示"not available on this machine"。
- 某件任务需要新的系统调用：在 `System`（或 `Elevated`）里加一个签名与平台无关的方法，在 `crates/platform/src/windows/system.rs` 实现，在 `sys/fake.rs` 加假实现。

## 12. CLI

`crates/cli/src/main.rs`。**CLI 自己没有命令表**：

1. 先看全局选项：`--json`、`--endpoint <名字>`（或环境变量 `AWAY_GUARD_ENDPOINT`）、`--timeout <秒>`（默认 30，超时按 `IPC_ERROR`）、`--help`、`--version`。都可以放在任何位置。
2. 连上后台并握手，拿到命令目录。
3. 按目录生成命令行：每个短名一个顶层命令（`away-guard protect`），每个模块一个顶层命令、下面是它的动作（`away-guard task protect`）。只放 `callable` 且不隐藏的命令。参数按类型生成：`bool` 是开关，`str_list` 可重复，`int`/`uint` 检查是数字，`json` 原样解析；参数名里的 `_` 在命令行上写成 `-`（`--ttl-minutes`）。
4. 在同一个连接上发请求，按结果退出。

- **帮助按所属部分分组：** `away-guard --help` 列出后台现在有的所有命令，分"公共部分 / 使用 / 账号和存储 / 保护"几组，每行是"短名, 模块 动作  说明"，被关掉的模块标 `[module off]`。`away-guard help <命令>` 或 `<命令> --help` 看参数。后台没开时 `--help` 仍然成功（退出码 0），说明命令列表来自后台，并列出常用的 10 个名字。
- **输出：** 加 `--json`：stdout 只打一行 JSON，就是协议响应；客户端自己产生的错误（后台没开、超时、参数不对）也是同样形状，`id` 为 `null`。不加：`status`（按所属部分分组，每个模块下面打它的 `summary` 和以 `!` 开头的 `notices`）和 `log` 打印成表，其他成功结果打印格式化的 JSON，失败打印到 stderr：`error: [CODE 子码] 模块: 信息`。
- **退出码** 见第 4 节：打错命令 11，参数不对 2，后台没开 3，空壳 13，模块关着 14，不准调 15。

## 13. 怎么加一个新模块

不用改协议、主循环、CLI。以 `example` 模块（`crates/daemon/src/modules/example/mod.rs`）为样板：

1. **新建目录** `crates/daemon/src/modules/<名字>/mod.rs`，写一份 `static SPEC: ModuleSpec`：
   - 名字（`[a-z][a-z0-9_]*`，发布后不能改）；
   - **所属部分** `Section`：`Common`（公共部分）、`Usage`（使用）、`AccountsStorage`（账号和存储）、`Protection`（保护）。决定它在 CLI 帮助、`status`、以后页面里归在哪一组。没有合适的就在 `crates/proto/src/catalog.rs` 的 `Section` 里加一个值（只加不改，同时在 `ORDER`、`wire_name`、`title` 里补上）；
   - 命令和参数；agent 能调的加 `.agent()`，不加就只给页面；常用的加 `.short("名字")`；
   - 要发的事件、要收的主题；
   - 自己的表：`migrations`，表名以 `<名字>_` 开头；
   - 自己的错误子码：`error_codes`；
   - 默认不开就加 `.off_by_default()`。
2. **写实现：** 一个类型实现 `Module`（`spec`、`handle`、`recover` 必须写，其他有默认），再写 `pub fn create() -> Box<dyn Module>`。
   - 碰系统只通过 `ctx.system()` / `ctx.elevated()`；需要新的系统调用就按第 11 节加到接口里，平台实现写在 `crates/platform/src/windows/`，假实现写在 `sys/fake.rs`；
   - 改任何系统设置之前先 `ctx.journal().record(...)`，并实现 `recover`；
   - 和别的模块打交道只通过事件；
   - 出错用 `ctx.error("NAME", ...)`。
3. **登记一行：** 在 `crates/daemon/src/modules/mod.rs` 的 `register_modules!` 列表里加 `<名字>,`。列表顺序就是启动顺序。
4. **测试：** 在 `crates/daemon/tests/` 里用 `common::Env`（假系统调用、临时目录）启动后台，用 `agent_call` / `call` 发请求。要开关可以用 `env.write_config(...)`。

做完以后：握手的命令目录里有它的命令，CLI 帮助在对应部分列出来，`away-guard <名字> <动作>` 和短名直接能用，`status` 显示它的状态和表版本。测试 `a_new_module_needs_no_framework_change`（`crates/daemon/tests/extend.rs`）和 `a_module_defined_elsewhere_gets_cli_commands`（`crates/cli/tests/cli.rs`）用只在测试里定义的模块证明了这一点。

把空壳换成真实实现：同样的步骤，直接改对应目录里的 `mod.rs`，把 `StubModule::boxed(&SPEC)` 换成自己的类型。

### 加一个新平台（以后）

只在 `crates/platform/src/<系统>/` 里写：`Transport`/`Listener` 和 `connect`（例如 Unix socket）、`System`、`Elevated`、`SqliteLibrary`、`paths`（默认连接点、数据目录、配置文件位置），再在 `crates/platform/src/lib.rs` 里按系统选它。共享部分不动。

## 14. 测试一览

`cargo test --workspace` 共 282 个，全过（第 14 件后，2026-11-02），另有 1 个 `#[ignore]` 的本机只读统计（第 20 节）。除了下面写明"只对测试自己启动的进程"的几个，全部不改系统：

| 位置 | 内容 |
|---|---|
| `crates/proto` 单元测试 | 错误码表（唯一、不为 0、和线上名字一致、新码当 UNKNOWN）、请求解析、版本协商、不认识的字段和枚举值、客户端握手（脚本化的假流） |
| `crates/daemon/tests/dispatch.rs` | 握手必须先做、版本协商和"谁太旧"、未知字段忽略、固定错误码、10 个短名、命令目录带所属部分、权限集中检查（agent 拒绝、页面放行）、参数检查、操作记录写清楚谁发的 |
| `crates/daemon/tests/isolation.rs` | panic / 启动失败 / 事件处理 panic 只停那一个模块、事件在模块间传递、忙的模块不卡 `status`、子码命名空间、配置文件开关、配置文件坏了时后台照常服务但不启动任何模块（也不按默认启动）、管理员操作走窄接口、坏的模块描述被拒 |
| `crates/daemon/tests/migrations.rs` | 升级保留数据、前缀不对整步回滚、某步出错停在上一步、数据库比模块新、框架表比程序新 |
| `crates/daemon/tests/recovery.rs` | 第 10 节列的恢复场景 |
| `crates/daemon/tests/extend.rs` | 示例模块默认关、打开后完整可用、只在测试里定义的模块自动进目录和路由 |
| `crates/daemon/tests/sqlite.rs` | SQLite 绑定和存储 |
| `crates/daemon/tests/power.rs` | 模式 + 电源，全用假系统调用（第 16 节"测试"） |
| `crates/daemon/tests/task.rs` | 任务模块，全用假进程表（第 17 节"测试"） |
| `crates/daemon` 单元测试 | 设置的部分更新（未知键、`null` 恢复默认、开放的映射）、离开报告的累计、温度和电量分级（回差）、放电速度估计 |
| `crates/platform/tests/pipe.rs` | 真管道：握手和调用、页面身份识别、DACL 只给当前用户、第二个后台被拒、没开时报 3、原始协议行和超长行、新旧版本不匹配、真实系统层只读 |
| `crates/platform` 单元测试 | 只读：GUID 互转、读当前电源计划的六项设置、电量/温度/内存/网络读数合理、系统通知窗口能订阅并收到当前屏幕状态、计划任务 XML 的内容（不安装）、进程快照和自己进程的读数。**只对测试自己启动的进程**（`ping`、`cmd`）：作业对象跟踪后来启动的子进程、作业对象没有"关闭时结束"、丢掉跟踪后进程照样运行；优先级调高再调回 |
| `crates/daemon-main/tests/run.rs` | 后台程序前台运行并应答、同一连接点第二个起不来（退出码 3）、配置文件坏了进程照样运行并在 `status` 里报出来、**启动器把被结束的后台重新拉起来**（只结束测试自己启动的进程） |
| `crates/daemon-main` 单元测试 | 启动器的重启策略（退避 2/5/10/30/60 秒、跑满 10 分钟重新计、正常退出就停、别人在跑就等）、日志过大时移到 `daemon.log.old` |
| `crates/cli` | 单元测试：按目录生成命令行、参数类型、帮助分组顺序、只给页面的命令不出现；端到端：后台没开、`status`/`log`、配置文件坏了的提示和退出码 14、退出码表、`permission set` 被拒、帮助分组、配置打开示例模块后 CLI 直接能用、测试里定义的模块直接出现在 CLI、**管理员层拒绝时 `status` 打出"这一项需要管理员权限，没生效"** |
| `crates/agents` | 适配层：单元测试 + `tests/claude_code.rs`、`tests/codex.rs`，全在临时目录里用合成的记录和配置（第 19 节"测试"） |
| `crates/agent-hook/tests/hook.rs` | 真 hook 程序 → 真管道 → 后台，统一事件、不挡住 agent、不装就不写（第 19 节） |
| `crates/daemon/tests/agents.rs` | `agents` 模块：按假进程表找 agent 进程、只转发统一字段、错误子码 |
| `crates/daemon/tests/files.rs` | `files` 模块：临时家目录里的合成对话记录，归属、不重复计数、缓存、增量读、不认识的版本、库里没有正文（第 20 节"测试"）；另有一个 `#[ignore]` 的本机只读统计 |

## 15. 这一版没做的

- 页面、托盘、服务安装。
- 后台被 `taskkill /F` 之类强杀时不会执行 `stop`；靠下次启动的恢复兜底（通过启动器运行时几秒后就重启并恢复）。
- 操作记录不清理旧数据。
- 其他平台的实现（只留了位置）。
- 第 3 件里还没在这台机器上实测的真实改动，见第 16 节"要用户实测的"。

## 16. 模式和电源（第 3 件）

代码：`crates/daemon/src/modules/mode/`（`mod.rs`、`report.rs`）、`crates/daemon/src/modules/power/`（`mod.rs`、`levels.rs`）、`crates/daemon/src/modules/settings.rs`；平台：`crates/platform/src/windows/power.rs`、`sensors.rs`、`events.rs`、`autostart.rs`；启动器 `crates/daemon-main/src/launcher.rs`。

### 模式模块 `mode`

- 三种模式：`normal`（人在）、`away`（固定的推荐设置）、`custom`（用户自己的方案，名字 `[a-z][a-z0-9_]*`；`custom` 一定有，起始值和 `away` 一样）。
- **9 项设置**（`Preset`，`away` 的值是默认值）：`lock_screen` 锁屏（true）、`display` 屏幕 `off`/`dim`/`on`（off）、`free_memory` 内存不够时冻结强杀（true，第 6 件用）、`hotspot_failover` 断网切热点（true，第 7 件用）、`defer_updates` 推迟系统更新（true）、`lid_on_ac` 插电时合盖继续跑（true）、`lid_on_battery` 用电池时合盖继续跑 `{"enabled": false, "min_percent": 30}`、`no_power_saving` 关网卡和 USB 省电（true）、`away_report` 离开报告（true）。
- 设置（`mode.configure`，只给页面）：`default_preset`（`away`）、`idle_enter_minutes`（`null`＝关；托盘测键鼠闲置，到时调 `away --trigger idle`）、`presets`（自定义方案；`away` 内置不能改，`custom` 不能删）。存在 `core_settings`（`scope = "mode"`, `key = "settings"`）。
- **谁能让它进离开模式：** CLI / agent（`trigger = "cli"`）和页面/托盘的按钮（`button`）进去以后，没有任务也一直保持，直到 `back`；合盖（有受保护的任务时，`lid`）和闲置（`idle`，要打开设置且有任务）进去的，任务全部结束就自动回普通（`tasks_done`）。还有续跑在等或在跑（`resume.active` 的 `active` 大于 0）时不算结束，要等它们结束或放弃；受保护对话的续跑（`protected`）在合盖和闲置进入时也算受保护的任务。`mode.show` 的 `continuations` 是当前数量。agent 不能自称 `button`/`idle`（`DENIED` + `mode.TRIGGER_NOT_ALLOWED`）。
- **回来：** `back`；方案要锁屏时，解锁就算回来（`unlock`）；没锁屏的方案要托盘弹"回来了吗？"（第 11 件，现在没有）。
- 当前模式存在 `core_settings`（`key = "state"`），后台崩溃后**同一次开机**内重启时接着离开；重启电脑后回普通（`reboot`，人能登录就说明人在）；正常停止时回普通（`daemon_stopped`）。
- 离开报告（方案打开时）：离开多久、断网几次共多久、冻结/强杀了谁、最高温度、agent 续了几次、哪些任务失去保护、怎么进去怎么回来。回来时写进 `mode_reports`，发 `mode.report`，`back` 的结果里也带。

### 事件约定

| 事件 | 谁发 | 内容 | 谁收 |
|---|---|---|---|
| `mode.changed` | mode | `mode`、`preset`、`settings`（当前生效的 9 项；普通模式是 `null`）、`trigger`、`since`、`keep_running`、`previous`、`default_preset`、`default_settings`（合盖会进的方案，电源模块用它决定合盖动作）。启动时（`core.started` 之后）发一次 | power、network、resource |
| `mode.report` | mode | 报告 JSON | 页面（以后） |
| `task.*` | task | **每一个都带 `active`（现在受保护的任务数）**；各事件内容见第 17 节 | mode、power；以后 resource、permission（`task.snapshot`） |
| `power.lid` | power | `closed`、`initial`（刚订阅时的当前状态，不算动作）、`on_ac` | mode |
| `power.session` | power | `locked` | mode |
| `power.source` | power | `on_ac` | — |
| `power.reading` | power | `temp_c`、`battery_percent`、`on_ac`、`lid_closed`，每次读数 | mode（报告） |
| `power.overheat` | power | `level` 0–3、`name`（`ok`/`warn`/`cool`/`critical`）、`temp_c`、`lid_closed`、`thresholds` | 以后 resource（第 6 件：冻结） |
| `power.low_battery` | power | `level`、`name`、`percent` | 以后 resource |
| `power.critical` | power | `state`（`pending`/`cancelled`）、`reason`（`temperature`/`battery`/`battery_floor`）、`action`、`countdown_s` | 页面、托盘（以后） |
| `power.suspending` | power | `action`、`reason`，马上要睡眠/休眠/关机 | 以后 resource（先解冻） |

### 电源模块 `power`

只从事件知道模式和任务数，自己算"现在要改哪些"，和系统对齐：每一项**先记恢复日志再改**，不要了就改回去；`stop` 全部改回；崩溃后 `recover` 按 key 改回。key：`lid_action`、`cpu_max_percent`、`display_brightness`、`saver_threshold`、`usb_suspend`、`wifi_power_saving`（电源计划里插电/电池两个值一起记）、`power_mode`、`update_pause`、`adapter_power_saving`。

| 什么时候 | 做什么 |
|---|---|
| 有受保护的任务（任何模式）或离开模式 | 阻止睡眠（`keep_awake`）；方案屏幕是 `on` 时屏幕也不灭 |
| 有任务、或在离开模式 | 合盖动作按方案改成"不采取任何操作"：插电时看 `lid_on_ac`，用电池时看 `lid_on_battery`（电量低于下限就不改，或改回去） |
| 进入离开/自定义模式那一下 | 锁屏、关屏幕（按方案，只做一次） |
| 离开 + 屏幕 `dim` | 亮度降到 `dim_brightness_percent`（只降不升） |
| 离开 + `defer_updates` | 电池省电阈值改 0（不自动进省电）；暂停更新 `update_pause_days` 天（**管理员**） |
| 离开 + `no_power_saving` | USB 选择性暂停关、无线网卡省电改"最高性能"；网卡"允许关闭此设备"关（**管理员**） |
| 温度或电量到"cool" | CPU 上限降到 `cool_cpu_max_percent`、电源模式改"能效" |
| 温度、电量到"critical"，或合盖时电量低于下限 | 最后一步 `last_step`（默认休眠）。普通模式倒数 `last_step_countdown_s` 秒（让人存盘），离开模式立即 |
| 离开中最后一个任务结束，没有续跑在等或在跑，且 `when_tasks_done = hibernate` | 休眠 |

- 设置（`power.configure`，只给页面）：`poll_seconds` 10；`temperature` `{warn 80, cool 85, critical 92}` °C（**占位**，等第 2 件合盖实测再定，`thresholds_confirmed` 为 false 时 `status` 会提示）；合盖时阈值再低 `lid_closed_margin_c` 5；回差 3 °C；`battery` `{warn 20, cool 12, critical 6}` %（只在用电池时算），回差 2；`cool_cpu_max_percent` 70；`last_step` `hibernate`/`sleep`/`shut_down`/`none`；`last_step_countdown_s` 60；`when_tasks_done` `normal`/`hibernate`；`dim_brightness_percent` 20；`update_pause_days` 7。
- **需要管理员的项：** 走 `Elevated`。现在普通用户后台会拒绝，这一项状态是 `needs_admin`，`status` 里写"这一项需要管理员权限，没生效"，**不报错、不影响别的项、不反复重试**（想要的状态变了才再试）。
- `status` 的 `state`：`readings`（电量、插电、充电、省电、系统估计剩余、按最近 30 分钟放电速度估计的剩余、温度和各温区、内存、网络、合盖、锁屏、屏幕、硬件能力）、`levels`、`thresholds`（带 `placeholder`）、`critical`、`changes`（每项的 `state`：`applied`/`needs_admin`/`unsupported`/`failed` 和说明）、`last_actions`、`system_requests`（全系统谁在阻止睡眠，要管理员）、`restart_pending`（系统在等重启装更新：只提醒，不替它重启）、`summary`、`notices`。

### 设置的读写 `modules::settings`

模块的设置统一这样存：`settings::load::<T>(ctx)` 读出存的 JSON 并补上默认值（读不了就用默认值并记日志）；`settings::patch::<T>(ctx, 部分 JSON, 开放的映射, 检查函数)` 做部分更新：不认识的键回 `INVALID_ARGS`（开放的映射比如 `presets` 里可以加新名字，内容按第一项检查），值为 `null` 表示恢复默认或删除这一项，检查不过不保存。

### 开机自启和崩溃后重启

- **启动器** `away-guard-launcher.exe`（窗口程序，不弹控制台）：以无窗口方式启动旁边的 `away-guard-daemon.exe`，输出追加到 `<数据目录>\daemon.log`（超过 10 MB 时先移到 `daemon.log.old`）。后台退出后：
  - 退出码 0（注销、关机等正常停止）：启动器也退出；
  - 退出码 3（别的后台占着连接点）：每 5 秒看一次，等它没了再接手；
  - 其他（崩溃、被结束）：等 2、5、10、30 秒，之后每 60 秒再启动；连续跑满 10 分钟算正常，重新从 2 秒算。
  - `--endpoint`、`--data-dir` 原样传给后台。
- **开机自启** `away-guard-daemon autostart install`：给当前用户建一个计划任务 `away-guard-daemon-<SID>`，登录时运行启动器，之后每 5 分钟检查一次（已经在跑就不再启动：`IgnoreNew`），所以启动器本身挂了最多 5 分钟后也会回来。普通权限（`LeastPrivilege`）、只在用户登录时（`InteractiveToken`）、用电池也跑、不限时长、普通优先级。XML 由 `platform::autostart::task_xml` 生成，用 `schtasks /Create /XML` 注册。计划任务被拒时退而写 `HKCU\...\Run`（登录时启动，启动器照样重启后台，但没人重启启动器本身），并在输出里说明。`remove` 两样都删，`show` 显示现在装了什么。
- 这两样都是对这台机器的真实改动，只有用户自己运行 `autostart install` 才发生；测试只检查生成的 XML，不安装。

### 测试

`crates/daemon/tests/power.rs`（12 个，全用假系统调用）：离开时每项改动先记日志、`back` 后全部改回；管理员层拒绝时 `status` 显示没生效、`away` 照样成功、不反复重试；有任务时防睡眠、合盖进离开、任务结束回普通；解锁算回来；自定义方案起始等于离开、能存、只给页面改、坏的设置被拒、重启后还在；闲置进入要打开设置且有任务；过热先降温再最后一步、降下来全部改回、合盖时阈值更低；电量下限和低电量；任务结束后休眠（可选）；离开中崩溃→恢复→接着离开、重启电脑→回普通；正常停止全部改回；`status` 有电量、温度、内存、网络。

### 要用户实测的（都没在这台机器上做过）

改合盖动作和电源计划、阻止睡眠、锁屏、关屏幕、休眠/睡眠、`autostart install`（计划任务或 Run 键）、启动器装好后的开机和崩溃重启。需要管理员的三项（暂停更新、网卡省电、全系统阻止睡眠列表）现在一律"没生效"。

## 17. 任务模块（第 4 件）

代码：`crates/daemon/src/modules/task/`（`mod.rs`、`tree.rs` 跟踪进程树和测活动、`deps.rs` 依赖名单）；平台：`crates/platform/src/windows/process.rs`。

### 登记

- `protect --pid <n>`，或 `protect --ancestor <n>`：调用者的第 n 层父进程（1 = 运行 CLI 的那个 shell，2 = 再上一层，比如 agent 本身）。父子关系按启动时间核对，pid 被复用就报 `task.NO_SUCH_PROCESS`。
- 同一个进程（pid + 启动时间，或已经在某个任务的树里）、同一个 `session_id` 再登记一次，不新建，只合并依赖、延长保护（结果里 `already: true`）。
- `--label` 默认是程序文件名；`--mem-limit-mb` 是任务自己的内存上限（第 6 件按它决定还给不给它腾内存；超了 `status` 先提示）；`--transcript` 是对话记录文件，它被写入算活动。
- **自动登记对话：** 设置 `auto_register_conversations`，**默认关**。hook 调 `protect --source hook --session-id … --transcript … --ancestor …`；关着时回 `{"registered": false, "reason": …}`，不报错，所以 hook 不会失败。对话结束时 hook 调 `unprotect --session-id …`。hook 本身没有装（往 Claude Code / Codex 配置里写 hook 要先问用户）。

### 跟踪进程树

- 用作业对象（`track_tree`）：任务后来启动的子进程自动算进来，用量统计包括已经退出的进程。**只开"允许脱离"（breakaway OK），绝不开"关闭时结束所有进程"**：后台崩了、跟踪结束了，进程都照样运行。测试 `a_tracked_tree_takes_in_later_children_and_survives_the_tracker` 在测试自己启动的 `cmd`/`ping` 上验证。
- 作业对象被拒（比如根进程是用管理员权限启动的）：改成每次检查时按父进程号找子孙，`tracking` 显示 `parent_ids`，`status` 提示"父进程已退出的孙进程会漏掉"。
- 登记时已经在运行、但进不了作业对象的子进程（比如用管理员权限启动的），用作业对象跟踪时不算进来。
- 根进程退出、子进程还在：继续保护。整棵树都退出：结束保护（`exited`）。

### 依赖

- 名字：`wsl`、`docker` 是已知名字，代表它们的后台进程（`wslservice`、`vmmemWSL`、`com.docker.backend` 等）；别的名字按程序名认（大小写不论，`.exe` 可省）。
- 自动识别：树里有 `wsl.exe` / `wslhost.exe` 就加 `wsl`，有 `docker.exe` / `docker-compose.exe` 就加 `docker`（`auto: true`）。
- 每次检查重新按进程快照找依赖的进程号，发在 `task.snapshot` 里（给第 5、6 件：受保护期间这些进程也不能冻结、强杀）。

### 有效期（活动检查）

每 `check_seconds`（30 秒）检查一次，`task.check` 立即检查。下面任何一项算活动：

| 活动 | 怎么测 |
|---|---|
| `processes` | 树里多了或少了进程 |
| `cpu` | 树的 CPU 时间增量 ≥ `cpu_percent_min`（1%）× 一个核 × 间隔 |
| `io` | 树的 I/O 增量 ≥ `io_kb_min`（64 KB）。Windows 的进程 I/O 计数**包括网络收发**，所以网络也在这里；按树单独统计网络字节要 ETW 或管理员权限，没做 |
| `gpu` | 树里或依赖里某个进程的 GPU 引擎占用 ≥ `gpu_percent_min`（1%） |
| `transcript` | 对话记录文件的修改时间变新了 |
| `dependencies` | 依赖进程的 CPU 或 I/O 达到上面的线 |

连续 `idle_minutes`（10 分钟）没有活动，而且过了 `renew` / `ttl_minutes` 给的"至少保护到"时间，就当成卡死，结束保护（`idle`）并发 `task.unprotected`（托盘做出来后据此通知）。`renew <id> --ttl-minutes <n>` 最多 `renew_max_minutes`（1440）分钟，超了 `task.TTL_TOO_LONG`。后台重启后空闲计时从头算（后台停着时没在看）。

### 取消和记录

- 谁都能取消任何保护（agent 可以取消用户登记的）。每次结束都记下原因（`cancelled` / `exited` / `idle`）、时间和是谁（`by`：`kind`（`agent`/`trusted`/`daemon`）、`pid`、`exe`、`name`），写进表 `task_protected`、事件 `task.unprotected`（离开报告收它）、操作记录。`tasks --all` 列出最近结束的 20 个。
- 结束保护**从不结束进程**，只停止跟踪、把优先级改回去。

### 优先级

- `prioritize <id> [--level high|above_normal|normal]`：把树里每个进程的系统优先级提到这一级（只升不降），**最高到"高"**。类型里根本没有"实时"；本来就是"实时"的进程不动。
- 每个进程改之前先记恢复日志（key `priority:<pid>:<启动时间>`），后来进树的进程在下次检查时也提上去。保护结束、`--level normal`、后台正常停止时改回原值；崩溃后 `recover` 改回（进程还是同一个才改），再启动时任务仍在、还要提，就再提一次。
- 普通权限改不了（用管理员权限启动的进程）：走管理员层；现在被拒，这个进程的状态是 `needs_admin`，`status` 显示"这一项需要管理员权限，没生效"，不报错。
- 后台重启后保护照常（任务存在表里，按记下的成员 pid + 启动时间重新接上）；停着期间整棵树都退出了的，启动时结束（`exited`，`by.note` 写明）。

### 事件

| 事件 | 内容（都另带 `active`） |
|---|---|
| `task.protected` | `task_id`、`label`、`root_pid`、`source`、`by` |
| `task.unprotected` | `task_id`、`label`、`root_pid`、`reason`、`by`、`at` |
| `task.renewed` | `task_id`、`until`、`by` |
| `task.prioritized` | `task_id`、`level`、`by` |
| `task.active` | 启动时发一次 |
| `task.snapshot` | `tasks`: `[{task_id, label, priority(0–2), pids, dep_pids, memory_mb, mem_limit_mb}]`；有变化时发、启动时发 |

### 设置（`task.configure`，只给页面）

`check_seconds` 30、`idle_minutes` 10、`cpu_percent_min` 1.0、`io_kb_min` 64、`gpu_percent_min` 1.0、`renew_max_minutes` 1440、`auto_register_conversations` false。

### 测试

`crates/daemon/tests/task.rs`（11 个，全用假进程表）：后来的子进程自动进树、根退出不算结束、全退出才结束；六种活动各自都能续命、1% 以下的 CPU 不算、9 分钟不算 10 分钟、到点结束且不结束进程；`renew` 和上限；谁都能取消且记下是谁；优先级只到高、实时的不动、后进树的也提、结束时改回、日志关干净；管理员权限的进程显示没生效；崩溃后优先级改回、保护接着、停着时退出的任务启动时结束、正常停止改回优先级但保留保护；作业对象被拒时按父进程号跟踪；依赖给定和自动识别、发在快照里、超内存上限提示；对话自动登记默认关、同一对话只算一个、按对话编号取消；`--ancestor` 和各种错的目标。

### 要用户实测的

在真实的 agent 进程上 `protect`（作业对象会把那棵树套进去：不结束进程，但会改变它所在的作业，要先问）、`prioritize`（改真实进程的优先级，要先问）、GPU 活动的读数（这台机器的 GPU 计数器第一次读可能是空的）。

## 18. 出口保护（第 8 件，两段实现）

模块 `exit_guard`，所属部分 `protection`。代码在 `crates/daemon/src/modules/exit_guard/`：`mod.rs` 管命令、异步检查、事件和记录，`config.rs` 管设置和完整路径校验，`probe.rs` 管服务回退与匹配，`actions.rs` 复用共享进程接口，`firewall.rs` 管防火墙恢复日志与策略协调。用户已授权出口保护必需的管理员服务、模式报告改动；`ARCHITECTURE.md` 保持原样。两段代码均已接通，**尚未在本机安装服务或执行真实防火墙写入**。

### 通用网络系统调用（也供第 7 件使用）

定义在 `crates/daemon/src/sys/network.rs`，通过 `ctx.system().network()` 取得 `&dyn Network`。它不认识出口保护名单、国家或策略。

| 方法/类型 | 约定 |
|---|---|
| `Network::watch(NetworkSink) -> Result<Box<dyn Guard>>` | 多个消费者可以各自订阅；回调只排队，必须快速返回。守卫丢弃即退订，等待正在执行的原生回调结束 |
| `NetworkChange` | `interface`（网卡/VPN 接入或断开）、`address`（IPv4/IPv6 地址）、`route`（含默认路由及更具体路由）、`proxy`（当前用户的系统代理设置） |
| `Network::get(url, &ProxyRoute) -> Result<String>` | 有超时、正文上限 64 KiB、只接受 HTTP 200、禁止重定向；公网只用 HTTPS，明文 HTTP 只允许 IP 字面值回环地址 |
| `Network::interfaces() -> Result<Vec<NetworkInterface>>` | 只读接口清单：稳定 `id`、显示名 `name`、`up`、`tunnel`；Windows 的 `id` 是接口 UUID，不把易变的索引作为持久配置 |
| `Network::get_for_route(url, &ProxyRoute, &ExitRoute)` | 按受保护路线查询；本机代理强制使用选定端点，VPN 请求核验实际连接源地址和所选接口选路；无法证明时返回失败 |
| `ProxyRoute` | `{"kind":"system"}`（默认）、`{"kind":"direct"}`、`{"kind":"explicit","url":"http://127.0.0.1:7890"}`。`direct` 仍遵守操作系统的 VPN 路由，含义只是不用 HTTP 代理 |
| `Network::dns() -> Result<DnsAssessment>` | 只读配置的 DNS 服务器和到它们的选路，不发 DNS 探测；`state` 为 `vpn_route`、`bypass` 或 `unknown`，另带 `details`。这是路由评估，不是对应用 DoH/远端代理 DNS 的保证 |
| `System::notify_user(title, message)` | 用户桌面通知；不可用时仍保留事件、数据库记录和 `status` 提醒 |

Windows 实现在新文件 `crates/platform/src/windows/network_watch.rs`：`NotifyIpInterfaceChange`、`NotifyUnicastIpAddressChange`、`NotifyRouteChange2`；代理设置使用 `RegNotifyChangeKeyValue`（只申请读取/通知权限）。注册表订阅就绪后才返回；退订会取消原生通知，不写任何网络设置。路由变化包括更具体的分流路由，避免只关注默认路由而漏掉 DNS/VPN 选路变化。

HTTP 使用 Windows 自带 WinHTTP。`system` 优先使用后台进程的 `HTTPS_PROXY`/`https_proxy`，没有时读取当前用户系统代理，支持静态代理、PAC、自动发现及系统绕过列表。代理/PAC 失败直接算本次查询失败，**不会悄悄改成直连**。显式代理目前支持不带账号密码的 HTTP 代理；不支持 SOCKS，配置时会拒绝。代理状态接口强制 `direct`，不受公网代理设置影响。

假实现在 `crates/daemon/src/sys/network_fake.rs`（`sys::network::fake::FakeNetwork`）：脚本化响应、请求/路由记录、DNS 评估和模拟变化通知。已有 `FakeSystem` 不自动接触真实网络；新方法的默认实现为不可用。

### 命令和权限

| 命令 | agent | 参数/结果 |
|---|---|---|
| `exit_guard.settings` | 是，只读 | 当前完整设置 |
| `exit_guard.routes` | 是，只读 | 通用网络接口清单，供页面选择 VPN 的稳定接口 id |
| `exit_guard.configure` | **否** | `settings` JSON 部分更新；名单数组整体替换，`null` 恢复默认，未知设置拒绝；保存后排队检查 |
| `exit_guard.request` | 是 | `settings` JSON 建议补丁；先校验、持久化，只产生 `pending` 申请，绝不改生效设置 |
| `exit_guard.requests` | **否**，只读 | 最多 100 条待批申请，含申请人身份和建议补丁 |
| `exit_guard.review` | **否** | `request_id` int，`approve` bool（省略表示拒绝）；批准时按当前设置重新校验补丁，批准后才生效 |
| `exit_guard.check` | 是 | 异步排队立即检查，返回 `{"queued":true}`；结果看 `status`/`exit_guard.checked` |
| `exit_guard.incidents` | 是，只读 | 最近 100 条持久事件 |
| `exit_guard.reports` | 是，只读 | 最近 20 份出口保护离开报告附录 |

修改命令使用框架的可信页面身份检查，CLI/agent 无法自报成页面。与框架其他“页面专用”命令一致，当前可信类别也包括同目录的本产品托盘（第 6 节）；同账号绕过接口直接改数据库的限制没有变化。

默认设置：

```json
{
  "programs": [],
  "region": {"country": null, "ip": null, "node": null},
  "proxy": {"kind": "system"},
  "interval_seconds": null,
  "control": null,
  "firewall_route": null
}
```

页面配置示例（不要通过 agent 直接写入）：

```json
{
  "region": {"country": "SG"},
  "firewall_route": {"kind": "local_proxy", "endpoint": "127.0.0.1:7890"},
  "programs": [
    {"path": "D:\\apps\\worker.exe", "action": "block"},
    {"path": "D:\\apps\\other.exe", "region": {"country": "JP"}, "action": "freeze"}
  ],
  "control": {
    "url": "http://127.0.0.1:9090/status",
    "node_pointer": "/now",
    "mode_pointer": "/mode",
    "poll_seconds": 3
  }
}
```

`programs[]` 按完整可执行文件路径认，忽略大小写、统一路径分隔符，不按文件名认；拒绝相对路径、通配符、设备路径和 `..`，重复路径拒绝。每项的 `region`/`proxy`/`firewall_route` 没设就继承全局值；`action` 是 `block`（默认）/`freeze`/`kill`。`region` 支持国家、具体 IP、具体节点；同时填写时必须全部满足。默认没有替用户猜一个国家；名单里有未设地区的程序会明确提醒，并按不符合处理。

`firewall_route` 为 `{"kind":"vpn","interface":"接口 UUID"}`、`{"kind":"local_proxy","endpoint":"127.0.0.1:7890"}` 或 `{"kind":"deny_all"}`。VPN id 从 `routes` 取得；本机代理只接受非零端口的回环 IP，当前只放行到该端点的 TCP。未选择路线的安全默认是 `deny_all`：即使国家匹配也保持封锁，`status` 明确显示 `needs_configuration`。它和出口国家/节点是两个设置，不能用国家判断代替网络路线约束。存储设置损坏时拒绝启动模块并保留已有封锁，绝不把损坏数据当成空名单撤掉保护。

`control` 可选，只有名单非空时才每 `poll_seconds`（默认 3，范围 1–3600）秒读本机接口。只允许 `127.0.0.0/8`、`::1` 等回环 IP 字面值，不接受可解析到其他地方的主机名，不跟随重定向。通过可配置 JSON Pointer 读取当前节点字符串和模式字符串；节点或模式改变、接口从可用变不可用时触发一次公网检查。接口持续失败或节点未变时不重复查公网；接口失败会清除旧节点身份，不能拿上次成功的节点当当前节点。当前不支持带认证头的控制接口。

### 触发、检查和提示

- 默认没有公网定时器。启动、设置生效、四类系统变化、`network.switched/up/down` 事件或手动 `check` 才检查；网卡事件也覆盖 VPN 虚拟网卡的连断。事件成批到来时合并检查，HTTP/DNS 都在模块工作线程运行，不阻塞页面状态查询。
- `interval_seconds` 设为 10–86400 才开启公网定时复查，`null` 关闭。空名单不发公网请求，也不读代理控制接口。
- 每组不同的代理与防火墙路线只查一次，再分别匹配程序的目标。顺序固定为 Cloudflare `https://www.cloudflare.com/cdn-cgi/trace` → ipapi `https://ipapi.co/json/` → IPWhois `https://ipwho.is/`。均无需注册；超时、HTTP 失败、限流、无效 IP/国家或服务返回错误都继续换下一个；第一个成功即停止。Cloudflare 使用 `loc`，绝不拿 `colo` 当出口国家。全失败就是不符合，不能沿用旧成功结果。
- `dns.state=bypass` 提醒 DNS 有 VPN 外选路；本机 DNS stub、应用 DoH、无法识别的隧道等显示 `unknown` 并提醒无法确认。VPN 网卡识别依据接口类型/描述，属于启发式，不把它当无泄露证明。
- 控制接口报告 `mode=rule` 时明确提醒：代理代发的直连流量，程序级防火墙管不到，建议全局模式。模式不可读时也提示尚未核实。
- VPN 查询不使用 HTTP 代理，通过 WinHTTP 实际连接信息、源地址所属接口及 `GetBestRoute2` 核验所选 VPN；如果分流 VPN 不让后台的探测请求走该接口，就算查询失败并保持封锁，不拿其他接口的国家结果放行。本机代理查询强制走 `firewall_route.endpoint`。程序私有隧道、应用 DoH、代理规则里的全部目标不能靠单次公网查询证明；程序级规则也不能约束 DNS 服务或代理进程代发的流量。这里只评估并提醒 DNS，未修改系统 DNS、代理或 VPN 选路。

服务与原生代理行为依据：[Cloudflare trace](https://developers.cloudflare.com/fundamentals/reference/cdn-cgi-endpoint/)、[ipapi API](https://ipapi.co/api/)、[IPWhois 免费端点](https://ipwhois.io/documentation)、[WinHTTP 读取用户代理](https://learn.microsoft.com/en-us/windows/win32/winhttp/setting-wininet-proxy-configurations-in-winhttp)。

### 动作、事件、通知和离开报告

`status.state.phase=2`。`state.firewall` 含总体与逐程序结果：服务未装/不可用为 `needs_admin, applied=false`；全部安装成功才为 `applied`，部分失败为 `partial`，空名单为 `idle`。逐程序带 `blocked`、`route`，未选路线为 `needs_configuration`。每个程序另有 `action_result.state`：`applied`、`not_running`、`not_needed`、`needs_admin`、`unsupported`、`denied` 或 `failed`，不会把请求成功当成封网成功。

管理员服务使用 WFP 持久规则，以**完整程序身份和当前用户 SID 同时匹配**，覆盖 IPv4/IPv6 的出站连接与入站接受层；只增加拦截条件，不加可绕过其他防火墙的放行规则。VPN 只准选定接口（出站 next-hop / 入站 arrival），接口消失时退为全封；本机代理只准指定回环地址、端口及 TCP，其他地址、端口、UDP/WebRTC 直连及另一 IP 协议族被拦截。规则替换在 WFP 事务内完成，只管理本产品该用户的 provider/sublayer，不改其他规则。对应原生实现见 `crates/platform/src/windows/exit_firewall.rs`；条件依据 [Microsoft WFP 分层条件](https://learn.microsoft.com/en-us/windows/win32/fwp/filtering-conditions-available-at-each-filtering-layer) 和 [ALE 身份匹配](https://learn.microsoft.com/en-us/windows/win32/fwp/application-layer-enforcement--ale-)。

首次配置、启动及网络变化检查时，先保存原策略、安装临时全封锁，再查询公网；符合后只解除临时锁，VPN/代理基本限制始终保留。出口异常时保持封锁，再执行设定的冻结/强杀。后台正常停止时重新锁住，持久规则不会随后台/服务退出消失；重启先接管锁、复查后再放行。名单删除才恢复原策略/删除本模块规则并关闭日志；单纯禁用模块不等于撤掉持久策略。明确卸载管理员服务会删除该用户的本产品防火墙规则，保留部署文件和审计记录。

开发期间共享系统接口已由资源模块负责人加入，因此第一段直接复用 `System::suspension_state` / `freeze_process` / `thaw_process` / `terminate_process`，没有新写冻结/强杀系统调用。只匹配名单中的完整路径的当前实例，动作前再核对 pid、启动时间和路径。受保护任务及后来的子进程、“不能关”名单、未知身份/其他用户进程、后台自身不能冻结或强杀；共享权限/任务状态未就绪时也拒绝。此处不受资源模块“只在离开模式动手”的开关影响：出口规则是用户显式设置，所有模式都生效。

冻结前先记 `exit_guard` 的恢复日志（原进程身份和各线程挂起计数），然后调用共享冻结接口；重复检查不重复挂起。出口恢复、规则删除、正常停止、启动恢复时，只撤销本模块拥有的挂起，不影响别的工具或资源模块的挂起；PID 已复用时不碰新进程。按“回来”或同路径只改动作不会提前解冻。后来加入任务保护/不能关名单时，释放本模块的已有冻结，不因此查询公网。结束进程也先记不可逆操作的日志，不会尝试“恢复”已结束的程序。普通权限进程接口返回访问拒绝时才回退管理员接口，始终带 PID、启动时间和线程凭据；服务缺失显示 `needs_admin`，绝不盲用只接收 PID 的旧结束接口。

事件：`exit_guard.checked` 带时间、原因、各路径目标/出口/动作结果、DNS 和代理节点/模式；`exit_guard.violation` 在动作之前持久记录并发布，带单个程序的检查及 `pending` 动作意图，随后通知用户，再执行动作；`exit_guard.acted` 带最终动作结果，并更新同一条持久记录。`exit_guard.notice` 带用户提示；`exit_guard.requested` 带申请编号和申请者；`exit_guard.report` 带离开报告附录。Windows 桌面通知使用独立隐藏辅助进程显示系统托盘气泡，不装托盘、不改自启；系统禁用通知时数据库事件和 `status.notices` 仍可读。

本模块自己的表：`exit_guard_incidents`（提醒、出口异常与动作结果）、`exit_guard_requests`（申请与审核状态）、`exit_guard_reports`（离开报告附录）。设置存在 `core_settings` 的 `scope=exit_guard,key=settings`。`mode` 模块通过事件按 `incident_id` 合并到原生离开报告的 `exit_guard` 数组，最终动作替换待处理结果；`back` 收尾后才到达的动作事件仍更新相同时间段的报告，重新发 `mode.report`。`mode.reports` 能读到最终结果；出口附录按相同时间窗更新，避免重复。各模块只写自己的表，异常始终独立持久保存。

### 已接入 `Elevated` 和管理员服务的固定方法

```rust
fn exit_policy(&self, program: &str) -> Result<Option<ExitFirewallPolicy>, ApiError>;
fn set_exit_policy(&self, policy: &ExitFirewallPolicy) -> Result<(), ApiError>;
fn remove_exit_policy(&self, program: &str) -> Result<(), ApiError>;
fn set_exit_blocked(&self, program: &str, blocked: bool) -> Result<(), ApiError>;
fn suspension_state(&self, pid: u32, started_at: i64) -> Result<Vec<ThreadState>, ApiError>;
fn freeze_process(&self, pid: u32, started_at: i64, threads: &[ThreadState]) -> Result<(), ApiError>;
fn thaw_process(&self, pid: u32, started_at: i64, threads: &[ThreadState]) -> Result<(), ApiError>;
fn terminate_process_checked(&self, pid: u32, started_at: i64) -> Result<(), ApiError>;
```

`ExitFirewallPolicy { program, route: ExitRoute, blocked }`、固定四种 `ExitAdminRequest` 和纯策略计划定义在 `crates/daemon/src/sys/exit_firewall.rs`。稳定规则 id 由用户 SID/完整路径派生，不暴露 Windows HANDLE/GUID 类型。管理员客户端和服务端在 `crates/platform/src/windows/admin/`，共用严格参数协议在 `crates/proto/src/admin.rs`；服务端验证路径、回环端点、接口 id，拒绝保护程序自身。只接受受保护部署目录中的本产品后台、核验用户及管道对端身份，不提供任意命令入口。服务记录管理员冻结凭据，后台消失/服务停止时只恢复自己拥有的挂起；持久防火墙保留。

VPN 规则另有接口字段为 `FWP_EMPTY` 时的封锁条件；[ALE 重授权](https://learn.microsoft.com/en-us/windows/win32/fwp/ale-re-authorization) 可能由反方向数据包触发，不能让缺失字段绕过“不等于所选接口”的比较。无法判断接口时关闭该连接，允许程序重新连接。

冻结日志还记执行者（普通接口或管理员服务）；管理员冻结始终由服务解冻并清除其持久凭据，即使后来普通权限已经足够，也不留下过期的服务恢复记录。共享线程接口将访问拒绝正确传回，不再当成线程已退出。

部署产物为 `target/debug/away-guard-admin.exe`，`show` 只读；`install` 要求管理员终端，并复制到受保护部署目录（可用参数/环境变量指定）。必须从该受保护目录运行后台才能调用服务，不能直接信任可写工作区中的调试程序。安装器检查目录及祖先 ACL，拒绝不安全位置；安装、真实 WFP 写入及 VPN/代理切换验收，按架构要求另行征得用户同意。

### 验证与尚待决定

`crates/daemon/tests/exit_guard.rs`：22 项，全部使用假网络、假通知和假进程；包含权限申请、默认空名单、国家/IP/节点和逐程序代理、备用服务顺序/无效响应/全失败、四类变化与网络模块事件、默认不轮询/可选轮询、节点变化与控制接口失败、DNS/规则模式提示、持久报告、动作前持久记录、复用冻结/结束接口、恢复日志/PID 复用、名单冲突、新任务保护解除冻结、“回来但出口未恢复时保持冻结”，以及管理员动作回退不使用旧 PID 接口。

`crates/daemon/tests/exit_guard_firewall.rs`：9 项，全部使用假管理员/网络/进程；验证先锁再查、恢复只解除临时锁、名单删除恢复原策略、服务未装不报成功、重启接管、未选路线保持封锁、通用接口清单、原生报告与迟到动作收尾、坏设置保留封锁。另有 4 项纯防火墙计划测试覆盖 IPv4/IPv6、VPN 接口与代理地址/端口/协议，自动化不执行真实 WFP 写入。

2026-11-01 最终验证：当前整个工作区 `cargo build --offline`、`cargo test --offline`（272 项）和 `cargo check --offline -p proto -p daemon -p agents --target wasm32-wasip1` 全部通过，零警告。日志为 `target/exit-guard-build.log`、`target/exit-guard-tests.log`、`target/exit-guard-portable.log`。使用现有 D 盘 `away-guard-cargo` 缓存，临时目录为 `target/exit-guard-test-tmp`；因磁盘紧张设置 `CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0`、`CARGO_INCREMENTAL=0`。另一个对话新加的中转依赖使用旧 Windows 导入库，GNU 链接器读不到中文缓存路径，因此将现成的 `libwindows.0.52.0.a` 复制到 `target/exit-guard-link-libs` 并设 `LIBRARY_PATH` 为该英文绝对路径；这项链接修复没有修改中转模块源码或全局工具配置。

手动只读检查入口：`cargo run -p platform --example egress_readonly`，只订阅通知、读 DNS 选路、向 Cloudflare 发**一次**请求，不通知、不冻结/结束用户进程、不改网络。

需要用户决定/后续衔接：

- 实际要保护哪些完整路径、全局国家/每程序例外、代理真实路线和控制接口 JSON Pointer；默认保持空名单，不替用户填写。
- 用户于 2026-11-01 明确选择“出口恢复后再解冻（保护优先）”：按“回来”不会撤销出口冻结；出口恢复后才解冻，规则删除、正常停止和崩溃恢复仍有恢复机制。已按该决定实现并测试；`ARCHITECTURE.md` 保持原样，这项是对其中“回来时全部解冻”的出口保护例外说明。
- 第 10 件页面尚未实现，后端命令目录、申请审批、状态和原生离开报告已可直接接入。
- 两段实现已经完成；管理员服务安装和真实 VPN/代理切换、IPv6/WebRTC 防绕行测试仍需用户授权，只针对测试自己启动的程序，不改动用户应用。未安装服务时不能声称本机已获得防火墙保护。

## 19. agent 适配层（第 21 件）

代码：`crates/agents`（库，可移植）、`crates/agent-hook`（`away-guard-hook.exe`）、后台模块 `crates/daemon/src/modules/agents/`。功能代码（文件归属、搜索、任务保护、网恢复续跑）只用这里的统一格式，不出现具体 agent 的名字。

### 布局和怎么加一个 agent

- 每个 agent 一个目录 `crates/agents/src/<名字>/`（现在 `claude_code`、`codex`），里面 `pub static ADAPTER` 实现 `agents::Adapter`；在 `crates/agents/src/lib.rs` 的 `register_adapters!` 里加一行。`agents::all()`、`agents::get("claude-code")`、`agents::by_executable("claude.exe")` 按登记表查。
- `Adapter` 要回答：`info()`（id、显示名、`executables` 可执行文件名（小写，任务保护和默认白名单用）、`host_apps` 宿主桌面应用、`tested` 测过的版本范围、对话记录在哪、`hooks` 能力）；`transcripts(env)` 列出对话记录文件；`read(file, 从第几字节, 选项, 回调)` 读成统一格式；`install_locations(env)` 常见安装位置；`version_args()` / `parse_version_output()` 认版本；`hooks()`；`resume_args(对话编号, 提示, 运行设置)` 不开窗口接着这个对话；`fork_args(原编号, 后台生成的新编号, 提示, 运行设置) -> Option<Vec<String>>` 安全分叉，不支持时返回 None；`run_settings(命令行)` 从 agent 的命令行取运行设置（`RunSettings`：权限模式、模型、其他启动参数，提示词不算）；`refine_from_transcript(运行设置, 对话记录)` 按对话记录改正模型；`runs_unattended(权限模式)` 这个模式下是不是没有操作要等人批准。
- `agents::Env`：家目录和适配器关心的环境变量（`CLAUDE_CONFIG_DIR`、`CODEX_HOME`、`LOCALAPPDATA`、`APPDATA`、`PATH`…）。`Env::from_process()` 读本机，测试用 `Env::with_home(临时目录)`。

### 统一格式（`agents::model`）

- `TranscriptFile`：`agent`、`path`、`conversation_id`（子 agent 的记录归到启动它的对话）、`subagent`（子 agent 自己的编号）、`size`、`modified_ms`、`encoding`（`plain` 能读；`zstd` 是 Codex 压缩过的旧记录，这一版不读）。
- `Record`（`ts` 都是 Unix 毫秒）：
  - `Session`：对话编号、版本、工作目录；第一次出现和工作目录或版本变了时各发一次；
  - `Message`：`role`（user/assistant）、`chars`；`text` 只在 `ReadOptions { text: true }` 时才填；
  - `ToolCall`：`call_id`、`name`、`input`（同样只在 `text: true` 时填）；`ToolResult`：`call_id`、`is_error`（agent 说了才有）；
  - `FileWrite`：`call_id`、`tool`、`path`、`op`（`create` / `overwrite` / `modify` / `write`（不知道原来有没有）/ `delete` / `move {from}`）。只在 agent 报告写成功后才发，失败的写不发；
  - `Command`：`call_id`、`tool`、`shell`（`bash` / `power_shell` / `cmd` / `direct`）、`text`（shell 要执行的那段文字）、`cwd`（开始时的目录；从文件中间接着读、又没有自己的 `cwd` 时为空，不猜）；
  - `TurnFailed`：一轮因为模型 API 出错结束，`failure` 见下面"hook"。
- `ReadOutcome`：`offset`（下次从这里接着读：最后一整行之后；没写完的最后一行不算，留到下次）、`lines`、`bad_lines`（不是 JSON 或超过 64 MiB 的行，跳过并计数）、`ignored`（不用的记录类型及个数）、`version`、`stop`。
- **认不出就停手**（`stop`）：`unknown_version`（版本不在 `tested` 范围内）、`unknown_format`（要用的记录缺字段、形状不对）、`compressed`。停在那一行开头，`offset` 指向它，前面读到的照常交出；不往下猜。
- 流式读：一次一行，每行只解出要用的字段（文件内容、工具输出、`originalFile` 之类直接跳过不复制）。本机 12 GB 全量读一遍峰值内存 42 MB。

### 版本

| agent | 怎么认 | 测过的范围（2026-11-01 本机） |
|---|---|---|
| Claude Code | `claude --version` → `2.1.309 (Claude Code)`；对话记录每条都有 `version` | 2.1.293 – 2.1.309 |
| Codex | `codex --version` → `codex-cli 0.165.0-alpha.1.2`；rollout 第一行 `session_meta.cli_version` | 0.163.0-alpha.11.1 – 0.165.0-alpha.1.2 |

范围外的一律"这个版本不认识"：读记录停在那一行，装 hook 拒绝。范围在各自 `mod.rs` 的 `TESTED`，确认新版本格式没变后改这一行。`agents::detect::installations(adapter, env, 超时)` 在 `PATH` 和常见位置找可执行文件、跑 `--version`（这是唯一会启动外部程序的地方，什么都不改）。

### Claude Code

- 可执行程序 `claude.exe`（桌面应用自带的在 `%LOCALAPPDATA%\Claude-Data\<包>\claude-code\<版本>\<hash>\claude.exe`；原生安装 `~\.local\bin\claude.exe`；npm 的 `claude.cmd`）。桌面应用本身也叫 `Claude.exe`。
- 配置目录 `%CLAUDE_CONFIG_DIR%`，没设就是 `~\.claude`。对话记录 `projects\<目录名>\<对话编号>.jsonl`；子 agent 在 `<对话编号>\subagents\[workflows\<id>\]agent-<id>.jsonl`，归到这个对话；`journal.jsonl`、`*.meta.json` 等不是对话记录。
- 用到的记录：`user` / `assistant`（`sessionId`、`timestamp`、`cwd`、`version`、`message.content`）。写文件：`Write` / `Edit` / `MultiEdit` / `NotebookEdit`，以对应的 `tool_result` 不是错误为准，路径取 `toolUseResult.filePath`（`Write` 的 `type: create|update` 分出新建和覆盖）；命令：`Bash` / `PowerShell` 的 `input.command`；`isApiErrorMessage: true` 是 API 报错结束的一轮。`api-request-blob` 里重复了一遍工具调用，**不读**，否则会重复计数。

### Codex

- 可执行程序 `codex.exe`（桌面应用放在 `%LOCALAPPDATA%\OpenAI\Codex\bin\<hash>\`；窗口程序是 `OpenAI.Codex` 包里的 `ChatGPT.exe`）。
- 家目录 `%CODEX_HOME%`，没设就是 `~\.codex`。rollout 在 `sessions\YYYY\MM\DD\rollout-<时间>-<对话编号>.jsonl`；大约一周后 Codex 把它压成 `.jsonl.zst`（这台机器 10-23 之前的 1766 个都是），这一版只计数不读；`archived_sessions\` 同样。
- 用到的记录：`session_meta`（`cwd`、`cli_version`）、`turn_context.cwd`、`response_item` 的 `message` / `function_call` / `custom_tool_call` 及输出、`event_msg` `item_completed` 里的 `CommandExecution`（`command` 是 argv，`powershell -Command <脚本>` 之类会拆出脚本；`cwd` 可能是 `file:///C:/...`）和 `FileChange`（`changes`：路径 → `add`/`update`/`delete`/`move_path`，只取 `status: completed` 的）。

### hook：统一格式

`agents::hooks::HookEvent`（不含对话正文：hook 收到的提示词和最后一条回复只用来判断错误类型，然后丢掉）：

| 字段 | 说明 |
|---|---|
| `agent` | `claude-code` / `codex` |
| `kind` | `session_start`、`prompt_submit`、`turn_end`、`session_end` |
| `native_event` | agent 自己的事件名，如 `StopFailure` |
| `session_id`、`cwd`、`transcript_path` | 对话编号、工作目录、对话记录文件 |
| `outcome`（只有 `turn_end`） | `{"kind":"completed"}`；`{"kind":"api_error","failure":{...}}`；`{"kind":"unknown"}`（agent 不说为什么结束） |
| `continuing`（只有 `turn_end`） | agent 已经是因为 hook 要求而在继续 |
| `detail` | `session_start`：startup / resume / clear / compact / fork；`session_end`：结束原因 |

`failure`：`class`（`network`、`rate_limit`、`overloaded`、`auth`、`billing`、`server`、`bad_request`、`output_limit`、`unknown`）、`network`（`true` 是网络/连接问题，`false` 明确不是，`null` 判断不了）、`code`（agent 自己的错误码）、`http_status`。

| | Claude Code | Codex |
|---|---|---|
| 事件对应 | `SessionStart`、`UserPromptSubmit`、`Stop`（正常结束）、`StopFailure`（API 报错结束，代替 `Stop`）、`SessionEnd` | `SessionStart`、`UserPromptSubmit`、`Stop`、`SessionEnd`；**没有失败事件** |
| 断网能不能认出来 | 能。`error` 是 `server_error` / `unknown` 且没有 5xx 状态码时，看报错文字（`Unable to connect to API`、`Can't reach the API server`、`Connection lost mid-response`、`The response stopped arriving` 等，本机记录里出现过的）：认出来 `class = network` | 不能：`outcome` 一律 `unknown`；而且一轮失败时 `Stop` 不保证触发（openai/codex#22774） |
| 能不能让它继续 | `Stop` 能：输出 `{"decision":"block","reason":"<提示>"}`，最多连续 8 次；`StopFailure` 的输出被忽略，不能续 | `Stop` 能，同样的 JSON，退出码 0 |
| 不开窗口接着对话 | `claude --resume <编号> -p <提示>` | `codex exec resume <编号> <提示>` |

Rust 里直接用：`agents::get(id)?.hooks()?.parse(stdin 字节)` → `HookEvent`；`.continue_reply(提示)` → 要打印的内容和退出码；`adapter.resume_args(编号, 提示, 运行设置)` → 参数。`HookEvent.permission_mode`：这个 hook 报告的权限模式，Claude Code 只有 `UserPromptSubmit` 和 `Stop` 带（2.1.309 实测），没有任何 hook 带模型。

### hook：从 agent 到后台

1. agent 的 hook 运行 `away-guard-hook.exe --agent <id>`（Claude Code 不经过 shell：`"command": "<完整路径>", "args": ["--agent", "claude-code"]`；Codex 是一条字符串 `<完整路径> --agent codex`，路径有空格时用 `& '<路径>'`）。
2. 它从 stdin 读 hook 输入（最多 4 MiB），用适配器转成 `HookEvent`，发 `agents.hook {agent, event}` 给后台（默认等 3 秒，`--timeout` 可改）。**从不挡住、不弄坏 agent**：后台没开、超时、输入认不出，都是退出码 0、stdout 为空、stderr 一行原因。后台回的结果里有 `reply: {stdout, exit_code}` 时照着打印和退出（现在总是 `null`，第 9 件要让 agent 继续时用它）。
3. 后台 `agents` 模块只转发统一字段（多余字段丢掉），在调用方的进程树里往上找最近的、名字是这个 agent 可执行文件的进程，发事件：

| 事件 | 内容 |
|---|---|
| `agents.session_start` | `agent`、`session_id`、`native_event`、`cwd`、`transcript`、`detail`、`hook_pid`（hook 程序，来自系统）、`agent_pid`、`agent_exe`（找不到为 `null`）、`permission_mode`（这个 hook 没报告为 `null`） |
| `agents.prompt_submit` | 同上 |
| `agents.turn_end` | 同上，加 `outcome`、`continuing`、`in_a_window`（只有网络失败才有：hook 看原进程命令行判断是不是交互窗口；判断不了或没有为 `null`） |
| `agents.session_end` | 同上 |

第 4 件（自动登记对话）和第 9 件（网恢复续跑）在模块的 `subscribes` 里写 `agents.*` 或具体主题就能收到，不用知道是哪个 agent。

### 装、卸 hook（写 agent 的配置文件，先给人看再写）

```bash
away-guard-hook detect
```

```bash
away-guard-hook status
```

```bash
away-guard-hook install --agent claude-code
```

- `detect`：找到的安装和版本，认不认识。`status`：每个 agent 的配置文件里有哪些我们的 hook。都只读。
- `install --agent <id> [--events turn_end,session_start,...] [--hook-timeout 秒] [--program 路径]`：**只显示**要怎么改（文件、每一行加什么、注意事项），不写；加 `--apply` 才写。`uninstall --agent <id> [--apply]` 同理，只删我们自己的 hook（按 `command` 的文件名是 `away-guard-hook.exe` 认），别的 hook 和设置原样保留。`--json` 输出一个 JSON 对象。
- 写之前：检测到的每个安装的版本都要在测过的范围内，否则拒绝；文件在"显示"之后被改过就拒绝（`Conflict`），要重新来一遍。写的时候：旧文件先复制成同目录下 `<文件名>.away-guard-<秒>.bak`，新内容写到临时文件再改名替换；键的顺序和格式保持（两格缩进）。
- 改哪个文件：Claude Code `<配置目录>\settings.json` 的 `hooks`；Codex `<家目录>\hooks.json`（会提醒：Codex 要在 `/hooks` 里信任新 hook 才运行；`config.toml` 里 `[features] hooks = false` 会关掉所有 hook）。
- Rust 接口：`hooks.plan_install(env, &HookInstall { program, kinds: [(kind, 超时)], agent_version })` → `HookPlan { files: [PlannedFile { path, before, after, summary }], notes }`，`plan.apply()` 才写；`plan_uninstall(env, 程序文件名)`、`installed(env, 程序文件名)`。

### 本机实测（只读，2026-11-01）

`cargo run --release -p agents --example survey`（只打印计数，不打印正文）：

| | 文件 | 对话 | 大小 | 读完 | 停下 | 用时 |
|---|---|---|---|---|---|---|
| Claude Code | 3186 | 333 | 12.0 GB | 3186（0 行读不懂） | 0 | 118 秒 |
| Codex | 3892 | 3892 | 5.0 GB | 2126 | 1766（`.zst`） | 24 秒 |

`away-guard-hook detect`：Claude Code 2.1.302（`~\.local\bin`）、2.1.308、2.1.309（桌面应用自带），Codex 0.165.0-alpha.1.2，都认识。两个 agent 的配置里现在都没有我们的 hook；`install` 只演示了计划，没有写。

### 测试

`crates/agents`（单元 15 + `tests/claude_code.rs` 6 + `tests/codex.rs` 4，全在临时目录里用合成的记录和配置）：版本比较和范围、按行增量读（没写完的最后一行留到下次、超长行跳过）、键序不变的 JSON 编辑、列文件（子 agent、压缩文件）、写文件/命令/消息/API 报错的转换、失败的写不算、`api-request-blob` 不读、从上次的位置接着读时补上上次没等到结果的写、不认识的版本和格式停在那一行、hook 输入转换（断网、限流、未知事件）、装 hook 不动用户别的设置、装两次不变、计划后文件被改就拒绝、卸掉后文件和原来一字不差。`crates/agent-hook/tests/hook.rs`（3）：真 hook 程序 → 真管道 → 后台，`StopFailure` 变成一个 `agents.turn_end`、正文不出现在事件里；认不出的输入、未知 agent、后台没开都退出码 0；`install` 找不到 agent 时什么都不写。`crates/daemon/tests/agents.rs`（2）：按假进程表找到 agent 进程、多余字段被丢掉、错误子码。

### 要用户决定 / 授权的

- **真的装 hook**：`away-guard-hook install --agent claude-code --apply`（和 `codex`）会改 `~\.claude\settings.json` / `~\.codex\hooks.json`。没有在这台机器上执行过，要用户同意后再执行；装好后要实测：Claude Code 的 `args` 写法、`StopFailure` 是否真的在断网时触发；Codex 在 Windows 上 hook 能不能跑（要 pwsh、要在 `/hooks` 里信任）。
- **版本范围很窄**：Claude Code 现在大约每天一个小版本。严格按"不认识就停手"，下个版本一出，新对话的统计就会停（`status` 会说"这个版本不认识"），要有人确认后改 `TESTED`。是否要放宽成"同一个小版本号（2.1.x）里、格式检查都通过就读"，要用户定。
- **Codex 压缩记录**：读 `.jsonl.zst` 需要一个纯 Rust 的 zstd 解压库（比如 `ruzstd`，MIT），要从 crates.io 下载，没下。不装的话 Codex 一周前的对话都读不到。

## 20. 文件归属（第 14 件）

代码：`crates/daemon/src/modules/files/`。`mod.rs` 放命令、表和定时，`scan.rs` 读记录、定归属、量大小，`downloads.rs` 认命令，`shell.rs` 拆命令，`paths.rs` 处理路径，`redact.rs` 给命令摘要脱敏，`measure.rs` 量大小，`caches.rs` 放共享缓存的位置。对话记录只通过第 19 节的统一格式读（`ReadOptions { text: false }`，消息正文和工具输入不进内存），代码里不出现具体 agent 的名字。全程只读：不改任何对话记录、agent 配置和被统计的文件。

### 什么时候统计

- 后台启动后不马上扫。第一次自动统计在 `first_scan_delay_minutes`（默认 10）分钟后，之后每 `interval_hours`（默认 24）小时一次，从上次完成的时间算起。`files.scan` 马上在后台统计（页面上的"立即统计"），进度用 `files.scans` 看。同一时间只跑一个。
- 只读新增的部分。每个对话记录文件记下读到的字节位置（`read_to`），下次从那里接着读：大小和修改时间都没变就跳过；文件变短了（被换掉了）就从头读；没写完的最后一行留到下次。认不出的版本或格式停在那一行，每次统计都重试那一行，不猜。`.jsonl.zst` 只计数（第 19 节）。
- 读完再量大小：每个归属的路径，加上每个共享缓存。文件取长度；文件夹逐层加起来，不跟随符号链接和 junction，条目超过 `max_entries_per_path` 就停，并标 `truncated`。量的是逻辑大小，不是占用的磁盘空间。

### 从记录里认出什么

- agent 报告成功的写文件：`created` 新建，`written` 写了但不知道原来有没有，`moved` 移过来，`modified` 改已有文件，以及删除。失败的写不算。
- 命令里认出来的四类：
  - `downloaded`：curl、wget、Invoke-WebRequest/iwr/irm（`-OutFile`，也认缩写）、Start-BitsTransfer、aria2c、gdown、kaggle、`huggingface-cli`/`hf download --local-dir`、`pip install -t` / `pip download -d`、从远端往本机的 scp/rsync；代码里的 `.DownloadFile(`、`urlretrieve(`、`download_url_to_file(`、`snapshot_download(local_dir=)`、`hf_hub_download(local_dir=)`。
  - `cloned`：`git clone`（包括 `-C`）、`gh repo clone`。
  - `extracted`：Expand-Archive、`tar -x -C`、`unzip -d`、`7z x -o`。
  - `written`：`>` / `>>` 重定向、Out-File / Set-Content / Add-Content / Tee、mkdir / New-Item、cp / Copy-Item / xcopy / robocopy。
- 路径会跟着 `cd` / `Set-Location` / `pushd` 走，能展开 `~`、`$env:X`、`%X%`、Git Bash 的 `/d/x`。认不出的变量（比如 `$k`）就不要这个路径。`ssh` 里执行的、`wsl` 里的路径不算，它们不是这台机器的 Windows 路径。
- 子 agent 写的算到它所属的对话。落在共享缓存里面的下载只记"用过这个缓存"。

### 归属规则

- **文件**：最后一个"做出"它的事件属于哪个对话，文件就归哪个对话。重新下载覆盖的，算后来那个。只被改过的已有文件归第一个改它的对话，记为 `modified`，不算进任何对话的数据量。
- **文件夹，看创建时间**：拥有者是在文件夹创建时间（允许 10 分钟误差）之前、最后一个"做出"它的事件。事件都在它创建之后，说明文件夹本来就有，比如对已有文件夹 `mkdir -p`、解压到已有文件夹，这时标 `pre_existing`，不算。所以后来对它 `New-Item -Force` 的对话抢不走它（`totals.reowned` 是这样改回来的个数）。
- **写文件时顺带建的文件夹**（`created_folder`）：对话写某个路径时，上面的文件夹如果是在这次写的前后 10 分钟内创建的，就算这个对话建的。这样文件夹里后来由程序生成的东西（训练输出、录下的数据）也归到它。
- **不重复计数**：每个路径计入的字节，等于它的大小减去里面被其他计入路径拥有的部分。所以所有对话加起来，就是这些路径合起来占的空间。
- **共享文件夹**：一个文件夹里，别的对话做出的路径不少于它的拥有者做出的（文件夹本身算拥有者一个），就标 `shared`。比如多个对话共用的工作文件夹、临时目录里各对话的草稿根目录。它扣掉各对话自己的路径后剩下的部分（构建输出、程序生成的文件）说不清是谁的，记在 `unattributed`，不算到拥有者头上。克隆、下载、解压出来的文件夹不按这条算，因为命令本身说明了里面是什么。
- **agent 自己的数据**：放对话记录的文件夹和它上面的所有文件夹（`~\.claude\projects\...`、`~\.codex\sessions\...`，一直到家目录）永远不归属，标 `agent_data`。
- 每个写过某个路径的对话都记一条"接触"（`files_touches`：方式、次数、时间、最近一条命令摘要）。第 17 件要"按对话改过哪些子文件夹判断项目"，可以直接用这张表。

### 共享缓存

| 名字 | 位置（存在的才列） | 命令里认出来的用法 |
|---|---|---|
| `huggingface` | `HF_HOME` 或 `~\.cache\huggingface`，另加 `HF_HUB_CACHE` | 不带 `--local-dir` 的 `huggingface-cli download`、`from_pretrained(`、`load_dataset(`、不带 `local_dir` 的 `snapshot_download(` |
| `torch` | `TORCH_HOME` 或 `~\.cache\torch` | `torch.hub.load(` |
| `pip` / `uv` | `PIP_CACHE_DIR` 或 `%LOCALAPPDATA%\pip\Cache`；`UV_CACHE_DIR` 或 `%LOCALAPPDATA%\uv\cache` | `pip install/download`、`python -m pip`、`uv pip/add/sync/run/...` |
| `conda` | `CONDA_PKGS_DIRS`，或 miniconda3/anaconda3/miniforge3/mambaforge/.conda 下的 `pkgs` | `conda/mamba/micromamba install/create/update`、`env create/update` |
| `npm` | npm、pnpm、yarn、bun 的缓存 | `npm/pnpm/yarn/bun install/ci/add/...`、`npx` |
| `docker` | `%LOCALAPPDATA%\Docker\wsl\*\*.vhdx` | `docker/podman pull/run/build/compose/...` |
| `wsl` | `%LOCALAPPDATA%\Packages\*\LocalState\ext4.vhdx`、`%LOCALAPPDATA%\wsl\*\ext4.vhdx` | `wsl ...` |
| `cargo` | `CARGO_HOME` 或 `~\.cargo` 下的 `registry`、`git` | `cargo build/test/run/install/fetch/...` |

缓存只记哪些对话用过、用了几次、最后一条命令的摘要；大小整个缓存一起量。

### 数据库（表名都以 `files_` 开头）

| 表 | 内容 |
|---|---|
| `files_transcripts` | 每个对话记录文件：读到哪里（`read_to`）、大小、修改时间、版本、停下的原因（JSON） |
| `files_conversations` | 每个对话：最早的工作目录、版本、时间范围、写文件次数、命令次数 |
| `files_paths` | 每个归属的路径：拥有者、方式（`how`）、时间、命令摘要、来源、大小、`counted`（计入字节）、`pre_existing`、`note`（`shared` / `agent_data`）、`unattributed` |
| `files_touches` | 路径 × 对话 × 方式：次数、时间、最近一条命令摘要和来源 |
| `files_cache_uses` | 缓存 × 对话：次数、时间、最近一条命令摘要 |
| `files_caches` | 每个缓存位置的大小 |
| `files_scans` | 每次统计：原因、起止时间、结果、计数（`totals`） |

**不存对话正文**。库里只有路径、大小、时间和一行命令摘要。摘要最多 240 个字，这些都会换成 `***`：URL 里的用户名和密码、带 token/key/sig 的查询参数、`--token`/`-u` 之类后面的值、`Authorization:` 头、名字里带 secret/token/password/key 的变量的值、`sk-`/`hf_`/`ghp_` 等形状的串、32 位以上字母数字混合的串。测试会检查数据库文件里找不到合成记录中的正文、文件内容、diff 和 token。

### 命令（都是 JSON，时间都是 Unix 毫秒）

- `files.top [--limit N] [--agent id] [--by-folder]`：返回 `items[]`，每项有 `agent`、`conversation_id`、`cwd`、`bytes`（计入字节）、`downloaded_bytes`、`modified_bytes`、`paths`、`first_ts`、`last_ts`。另有 `total`（包括 `unattributed_bytes`）、`shared_folders`（剩下部分最大的 10 个共享文件夹）、`measured_at`。`--by-folder` 按对话的工作目录合计；所有对话都在同一个工作目录时用处不大，项目划分见第 17 件。
- `files.conversation <id>`：基本信息、`owned`（字节、下载的字节、改过的已有文件的字节）、最大的 `paths[]`（`path`、`how`、`kind`、`bytes`、`counted`、`pre_existing`、`note`、`unattributed`、`command`、`source`）、`also_wrote`（写过但归别人的路径）、`caches`。`id` 可以只给前面 6 个以上字符；有好几个对话对得上时报 `files.AMBIGUOUS_CONVERSATION`，一个都没有时报 `files.NO_SUCH_CONVERSATION`。
- `files.downloads <id>`：给 agent 用，免得重复下载。列出这个对话下载、克隆、解压过的每个路径，带它自己的命令摘要和来源、现在多大、还在不在；后来被别的对话重新下载过的标 `now_owned_by`。另外列出它用过的缓存。
- `files.find <路径片段>`：匹配的路径（不分大小写）、拥有者、有几个对话写过。
- `files.caches`：每个缓存的位置、大小、用过的对话数、次数、最近用过的 5 个对话。
- `files.scan [--no-measure]`、`files.scans`、`files.settings`；`files.configure` 只给页面。
- 事件 `files.scanned`：一次统计结束，带 `scan_id`、`outcome`（`done` / `stopped` / `error: ...`）、`totals`。

### 设置（`files.configure`）

`auto_scan`（默认 true）、`interval_hours`（默认 24，可设 1–168）、`first_scan_delay_minutes`（默认 10）、`measure`（默认 true）、`max_entries_per_path`（默认 2000000）、`home`（默认 null。设了就只看这个家目录，也不读本机的环境变量，测试用）。

### 看不到的

- 程序自己生成的文件，除非它们落在某个对话建的文件夹里。
- 命令里没写路径的下载，只能记成"用过缓存"。
- ssh 远端上的东西。
- 用认不出的变量拼出来的路径。
- Codex 一周前的压缩记录。

补"写入时记录"的是第 15 件（ETW）。

### 本机统计（只读）

`cargo test --release -p daemon --test files -- --ignored real_machine_report`。数据库放在临时目录，对话记录只读。

### 测试

- 单元测试 15 个：
  - 命令拆分（PowerShell 和 bash、引号、here-string、续行、注释、here-document）；
  - 路径（`~`、环境变量、Git Bash、相对路径、UNC、通配符不要）；
  - 命令摘要脱敏；
  - 下载、写入、缓存的识别（包括上传不算、`ssh` 和 `wsl` 里的不算、建归档不算解压）；
  - 量大小；
  - 缓存位置；
  - 嵌套路径不重复计数、共享文件夹、文件夹上溯到根为止。
- `crates/daemon/tests/files.rs` 有 2 个测试，外加 1 个 `#[ignore]` 的本机统计。它们在临时家目录里用合成的 Claude Code 和 Codex 记录，检查：
  - 归属，以及 Codex 的文件写进 Claude 对话克隆的目录后，两边各算各的；
  - 对已有文件夹 `mkdir -p` 算 `pre_existing`；
  - 后来的 `mkdir -p` 抢不走克隆出来的文件夹；
  - 写文件时顺带建的文件夹，以及它变成共享文件夹之后的 `unattributed`；
  - pip 缓存；
  - 命令摘要里的 token 被遮住；
  - id 前缀有歧义、对话不存在时的错误子码；
  - 不认识的版本停下、什么都不归属；
  - 第二次只读新增的字节；
  - 数据库文件里没有正文；
  - 不调用就不扫；
  - 设置只能从页面改。

### 要用户决定的

- **`created_folder` 和 `shared` 的判断方式**：这两条规则对统计结果的影响很大。10 分钟误差、"别的对话做出的不少于拥有者"这两个门槛，要用户看了 `files.top` 的结果再定，是否合适。
- 第 19 节的版本范围和 `.zst`，这里同样适用：Claude Code 出了新版本，新对话的归属就会停下；Codex 一周前的对话现在归属不了。

### 2026-11-02 续跑补充协议

- `AgentResume.fork_session_id` 选择分叉；缺省为续原编号。`System::new_session_id` 提供新 UUID。Claude Code 的参数由适配器生成，不在业务模块拼 shell。
- `resume_lineage` 保存 agent、新编号、原对话根编号、事故编号和父编号。`resume_queue.session` 是根编号，context 中的 session 是本次实际续的分叉编号。
- `agents.hook` 可携带 `resume_broker` 的进程身份；后台核对它是实际 hook 的同用户、同映像子进程。环境和凭据不进入协议。辅助进程继承原环境，以受限本地管道启动原 agent。
- `resume.forked`、`resume.notice`、`resume.expired` 进入离开报告；`resume.show` 同时返回根编号 `session_id` 和 `current_session_id`。
- `resume.active`（`active`：等待、启动中、运行中的续跑数；`protected`：其中受保护对话的数量）变化时发布，启动时也发一次，模式和电源靠它判断任务是否真的做完。
- `resume.continued` 多带 `protection`（原任务的 `label`、`deps`、`mem_limit_mb`，没受保护为 `null`）：任务模块据此保护新进程。
- 对话的权限模式从各 `agents.*` 事件的 `permission_mode` 记进 `resume_session_mode`，30 天没消息的删掉。模型和其他启动参数不经过后台，只在续跑辅助进程里。
- 管理员协议回复：显式 `result:null` 是成功空值；缺少 result 且没有 error 才是非法回复。请求/回复的版本与 id 仍严格校验。
