# 使用部分：网络、续跑与 API 中转

更新：2026-11-02。第 7、9、12 件代码已接入，第 3、4 件沿用已有电源、模式和任务实现；第 2 件脚本已补齐，实际合盖仍需用户操作。页面和托盘属于公共部分的第 10、11 件，不在本轮“使用”模块范围。

## 网络（第 7 件）

默认每 1.5 秒发起一轮外网检查，连续 3 次失败才认定断网；API 检查与外网检查并行，Windows 单个网络阶段超时由 750 ms 放宽到 3000 ms，慢探测后保留至少 500 ms 的调度间隔；它不是整轮请求的硬截止时间。使用配置的同一条系统 / 指定代理路线，保留网络变化通知。默认外网探测是微软 connecttest 文本；可添加实际 API 的 HTTPS 探测地址。默认 `api_urls` 为空，仅代表通用互联网状态，`api_probes_configured` 会显示这一点，不声称已验证 API。API 返回认证错误表示能连到服务器，不把它当成断网。

普通模式只通知。离开模式及打开 `hotspot_failover` 的自定义模式，会先降低备用网卡 metric；没有可用备用线路才依次连接已保存网络，手机热点放最后。不读取密码、不创建新 Wi-Fi 配置。认证门户、只有 API 不通但普通外网正常时，不切换网络。

每次连接尝试默认最多 10 秒，以状态机推进；不会在命令处理里长时间等待连接。正常连接到新的非手机网络时更新主网络；旧主网络不可见时优先选择当前可见的非手机网络。主网络信号消失后重新出现会立即尝试回去；主网络一直可见时，每 10 分钟试一次，失败则切回手机。网卡备用路径使用定期恢复 metric 后重新检查的方法回试主路，不声称在后台对每张网卡独立绑定探测。

手机热点连接成功后尝试设为计费网络，失败会报告。记录接口累计收发字节的差值，持久化手机 Wi-Fi 流量；流量按 UTC 日历月重置，可设限额提醒，达到限额后拒绝新的自动热点切换，不主动断开现有连接。接口 GUID 比较忽略大小写和花括号。计数是接口流量，包含其他程序，不是运营商账单；USB 备用线路暂不纳入手机 Wi-Fi 流量统计。

网络切换、计费设置和 metric 都先记恢复日志。回来、正常停止或下次崩溃恢复时撤回本模块改动；用户期间手动改连别的网络时保留用户选择。metric 的管理员部分见 [管理员服务](ADMIN_SERVICE.md)，服务没装时明确报告该项未生效。

```powershell
away-guard network show --json
away-guard network profiles --json
away-guard network check --json
away-guard network history --limit 20 --json
away-guard network settings --json
```

协议命令 `network.configure` 只允许页面 / 托盘身份调用，普通 CLI 和 agent 不能改。配置结构示例：

```json
{"poll_millis":1500,"failures":3,"internet_url":"https://www.msftconnecttest.com/connecttest.txt","internet_expected":"Microsoft Connect Test","api_urls":["https://api.anthropic.com/"],"proxy":{"kind":"system"},"lines":[{"interface":"网卡 GUID","profile":"家里 Wi-Fi","hotspot":false},{"interface":"网卡 GUID","profile":"已保存的手机热点","hotspot":true}],"backup_interface":null,"connect_timeout_seconds":10,"retry_main_seconds":600,"mobile_limit_mb":null}
```

`lines` 为空时使用系统已保存配置的顺序及热点名称提示；明确列出可避免把普通热点名称认错。实际字段以 `network.settings` 返回值为准。

事件：`network.down / up / switched / reading / notice`。`reading` 仅在状态、原因、事故或流量发生变化时发布。断网与线路变化存到 `network_outages`，模式模块把断网次数、时长写入离开报告。

## 网恢复续跑（第 9 件）

复用 agent 适配层与统一 `agents.*` 事件。只处理已经识别为网络故障的一轮结束，认证、限额、未知结束原因不自动重试。所有模式都可工作，默认最多续 3 次，等待最多 120 分钟；同一事故的次数持久化，不因后台重启清零。

Claude Code 的 `StopFailure` 只报告网络失败。原进程已经退出（包括 `-p` 的 `SessionEnd(other)`）时，网恢复后直接 `--resume` 原编号；原窗口仍开着时，按 ARCHITECTURE 的决定用 `--fork-session --session-id <新编号>` 接着做。给 print 模式保留两秒退出尾声后再决定是否分叉。新编号由后台事先生成，映射先入库，分叉后的失败重试仍计入原对话同一断网的三次上限；已经退出的分叉直接续其编号。

Codex 当前适配器没有可靠的 API 失败 hook 能力，先通知，不猜测会话失败、不自动创建重复任务；后续确认其版本和错误事件后可在适配层启用。[Codex hooks 官方说明](https://learn.chatgpt.com/docs/hooks) 与适配器声明应一起核对。

自动启动前核对实际 agent 进程、用户、原生可执行文件、适配器名称与已测试版本范围；不执行 shell / `.cmd` 包装。工作目录失效、未知版本等均转为需要用户处理。

续跑和原来跑得一样。`--resume` 不带回启动方式，所以每次都明确带上：

- **权限模式：** 这个对话最后一次 `UserPromptSubmit` 或 `Stop` 报告的模式（`StopFailure` 不报告），查不到时用原进程命令行上的。只有原对话本来就跳过权限确认，续跑才跳过；默认模式仍是默认模式，绝不比原来高。不是不用批准的模式时，启动就通知"后台没人批准"，因为需要批准的操作会被拒绝；查不到后来的模式也通知。
- **模型：** 原进程命令行的 `--model`，别名和 `[1m]` 这类后缀原样保留；对话记录显示后来换过模型时，用最后真正回答的那个。启动时没写 `--model` 且没换过时不带，这样配置文件里的模型仍然生效。
- **其他启动参数：** 按名单从原进程命令行原样带上：`--add-dir`、`--allowedTools`、`--disallowedTools`、`--tools`、`--mcp-config`、`--strict-mcp-config`、`--settings`、`--setting-sources`、`--system-prompt`、`--append-system-prompt`（及 `-file`）、`--agent`、`--agents`、`--effort`、`--fallback-model`、`--betas`、`--plugin-dir`、`--plugin-url`、`--max-turns`、`--max-budget-usd`、`--permission-prompt-tool`、`--autocompact`、`--restricted`、`--safe-mode`、`--disable-slash-commands`、`--chrome`、`--no-chrome`。名单对应 2.1.309 的帮助，新版本加的参数要补；提示词和只影响输出的参数不带。

做不到的：会话中途用 `/effort` 改的力度不带（只带启动参数）；中途 `/model` 切到带 `[1m]` 的模型时后缀会丢，因为对话记录里没有后缀；Codex 还没接上这一步，因为它还只通知、不续跑。

队列状态 `completed` 只表示这一轮结束，不表示任务做完，也可能是操作被拒后 agent 自己停了。

还有续跑在等或在跑时不算"任务全部做完"：离开模式、防睡眠、断网切换都继续，续跑结束或放弃后才恢复。手动 `back` 仍然立刻退出，队列留着。受保护对话的续跑和分叉进程自动接上同样的保护（名称、依赖、内存上限），原来没受保护的不自动登记。不会在本轮直接修改用户的 hook 配置；安装方式沿用 `agents` 模块已有的计划 / 应用流程。

```powershell
away-guard resume show --json
away-guard resume check --json
away-guard resume settings --json
away-guard resume cancel --id 1 --json
```

设置项：`enabled`、`max_attempts`（1–3）、`wait_minutes`（1–120）、`retry_delay_seconds`（1–60）、`prompt`。用户提交新消息（队列转 `user_active`）或自己结束对话（转 `user_ended`）时，取消尚未启动的等待。自己结束指 `SessionEnd` 的 `reason` 是 `prompt_input_exit`（`/exit`、Ctrl+C、Ctrl+D）、`clear`、`logout`、`resume`，或交互窗口的 `other`（关窗口）；`-p` 跑完的 `other` 不取消，崩溃和强杀不发 `SessionEnd`，也不取消。`logout` 和 `resume` 没实测。分叉已经启动后原窗口再输入，只提醒两边都在运行，不杀掉新进程；再关原窗口也不影响分叉。分叉通知含原编号、新编号、接手命令和同目录冲突提醒；过期、达到上限、启动结果不确定均通知，并进入离开报告的 `agent_forks` / `agent_notices`。

真实 hook 在网络失败时启动同用户的短期续跑辅助进程。它直接继承原 agent 的环境和配置位置，因此 API 基址、大小写不敏感的 Windows 代理变量、登录/认证配置和自定义配置目录都保留。凭据值不被读取、序列化、写入数据库或经 IPC 交给后台。启动参数也只留在它的内存里：它趁 agent 还在等 hook 时读 agent 的命令行，读不到就不启动，队列转为需要用户处理。它不继承任何句柄，钩子一退出，agent 读钩子输出的管道就关闭。辅助进程的本地管道只接受同目录后台，核对其 OS 进程身份；只能启动原 agent / 原对话的固定续跑或分叉操作，没有任意命令接口。用完即退出；后台退出或等待到期也退出。后台校验不通过时不用这个辅助进程，但这次失败照样入队，转为需要用户处理并通知。

工作线程、手动检查和取消共用串行锁；数据库条件更新领取任务。后台在启动中途重启时会报告不确定结果，不重复启动。适配器版本和原生可执行文件校验仍保留。验收结果见 [进度](STATUS.md)，只使用本机假 API 和占位凭据，不调用付费模型。

## API 中转（第 12 件）

按本轮“整个使用部分”的要求补做原先暂缓的可选模块。新增独立 `away-guard-relay.exe`，不自动启动、不改 agent 的地址或凭据。每个实例固定一个上游，只监听字面回环地址，不接收 CONNECT / 任意绝对目标，也不跟随重定向。

```powershell
# 从项目目录构建
cargo build -p relay
# Anthropic 兼容 HTTP API 示例；随后由用户把 agent 的 API 地址指向此回环地址
.\target\debug\away-guard-relay.exe --upstream https://api.anthropic.com --listen 127.0.0.1:14599 --mode auto
# 独立测试时可选 --mode normal 或 --mode away
```

`auto` 每秒读取后台模式，普通模式流式转发，离开 / 自定义模式先完整缓存在内存。后台暂不可连接时保留上一次读到的模式，初次默认普通模式。可用 `--endpoint` 指定后台管道。默认跟随系统 / `HTTPS_PROXY`；可用 `--proxy direct` 或不含凭据的 HTTP(S) 代理地址。

模型 POST 与 GET / HEAD / OPTIONS 都按架构约定进行有上限的传输重试，接受可能重复计算/token 的代价。普通模式在尚未交付正文前断线会重发，交付正文后断线交由 agent 处理。缓冲模式会丢弃中途失败的整次回复并从头重发，最多三次；agent 只收到完整回复和等待注释心跳。必须收到 HTTP 完整正文和已知 SSE 结束标记才交付；语义错误、认证/限额错误不重试。

次数用尽后返回明确的 `Network connection error`，包括缓冲 SSE 错误；agent 适配器优先识别这个传输错误，再处理一般 HTTP 5xx。这样真实 CLI 的 StopFailure 能把最终失败交回续跑队列。


默认请求 / 回复缓存分别最多 64 MiB，可用 `--buffer-mb` 把回复上限设为 1–256 MiB；并发最多 32。超限、最终失败只回通用错误，不把失败尝试的正文交给 agent。客户端取消 SSE 后取消上游读取和后续重试。重发可能重复计算和计费，因此中转需要用户明确配置后启用。

不持久化请求、回复、Cookie、API key 或 Authorization；转发传入的认证头，只在内存处理。健康检查 `http://127.0.0.1:14599/_arcturus/health` 只含模式、计数和 worker PID。守护父进程会重启意外退出的 worker；Ctrl+C 停止自身 worker。

目前支持 HTTP / SSE，不支持 WebSocket。订阅账号 OAuth 地址、客户端是否允许回环 API、真实模型和付费调用尚未验收；本轮未替换任何现有认证配置。

自动测试用本机模拟服务器覆盖半条断流、正常流式、首字节前重试、HTTP 正常 EOF 却缺少 SSE 完成事件、认证 / 限流不重试、三次上限、内存上限、客户端取消、目标限制和守护重启；另经真实命名管道验证 `auto` 跟随后台普通 / 离开 / 回来切换，后台系统调用均用假实现。没有调用真实模型。

## 合盖实测（第 2 件）

`tests/lid-test.ps1` 已改为原生读取原始 AC / DC 合盖动作，先记日志再改，保留崩溃恢复；上次日志未恢复时拒绝开始下一次。`lid-restore.ps1` 恢复失败保留日志。`lid-report.ps1` 统计心跳断档与最高温度。

```powershell
# 用户插着电运行，然后实际合盖；按现场情况选择时长
.\tests\lid-test.ps1 -Minutes 20 -MaxTempC 85
.\tests\lid-report.ps1 -Log '测试打印的日志绝对路径'
# 如果测试窗口被关闭或崩溃，先恢复
.\tests\lid-restore.ps1
```

85°C 是脚本的停止保护线，不是本机已验证的安全合盖温度。已验证脚本解析、只读原值和模拟断档报告；尚未合盖、修改真实电源设置或得出最终温度阈值。
