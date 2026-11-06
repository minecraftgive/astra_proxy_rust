# Astra GPUI GUI：详细实现计划（阶段 2–7）

本文是 [GPUI_GUI.md](GPUI_GUI.md) 的后续。总体架构和"功能对照表"以那份文档为准，本文只规定框架之后**怎么做、按什么顺序做、做到什么程度算完成**。

## 0. 前置条件

- **先提交现有框架。** 目前框架工作还没提交，工作区里有 79 个已改文件和 86 个新文件。按 GPUI_GUI.md 的要求，第 0 阶段按模块分组提交，然后提交第 1 阶段，之后每个小节（A1、B1……）各自单独提交。
- **已知失败的测试。** `self_update::tests::resolution_rejects_stale_selection_and_corruption_in_any_component` 在原始提交 `abd5ce1` 上就失败，不算回归，也不要为了让它通过去改测试。
- **每次提交都要通过的门槛：**
  - `cargo fmt --all -- --check`
  - `cargo clippy -p astra-client -p astra-tui -p astra-gui --all-targets`，不能有新增警告
  - `cargo test -p astra-client -p astra-tui -p astra-gui`

## 1. 贯穿全程的规则

1. **共享逻辑不能写在 GUI 里。** 凡是"按下某个操作会生成哪个 `Action`/`Input`"或者"界面上显示什么数据"的逻辑，都必须放在 `astra-client`。GUI 视图只负责布局和事件绑定。如果写 GUI 时发现要把 TUI 里的某段逻辑抄一遍，就先把它下沉到 client（见阶段 A）。
2. **共享模型内部会调用 `tokio::spawn`**，在 GPUI 线程上直接调用会 panic（没有 tokio reactor）。涉及的地方：
   - `logs::Viewer::new`、`diagnostics::View::request`、`nettools::View::start`、`monitor::View::key`
   - `feeds` 里的 `spawn_local` 只在后台线程运行，不受影响
   - 处理办法：
     - `Backend` 新增 `pub handle: tokio::runtime::Handle`，在后台线程里用 `Handle::current()` 取得，通过启动通道传回主线程。
     - `AppModel` 新增 `fn tokio<R>(&self, f: impl FnOnce() -> R) -> R`，内部先 `let _g = self.handle.enter();` 再执行 `f`。凡是可能调用 spawn 的共享方法，都必须经过这个包装。
     - 后台是 current-thread runtime，后台线程在 `block_on` 里驱动它，所以从这里 spawn 的任务能正常推进。
     - 这些模型的 `Waker` 统一改为向一个 `async_channel::Sender<()>` 发通知（容量 1，用 `try_send`，满了就丢弃，相当于合并重复唤醒）。GUI 端在 `cx.spawn` 里收到通知后调用对应模型的 `poll()`，再 `cx.notify()`。
     - 要加一个单元测试：在 GPUI 的 `TestAppContext` 里通过 `tokio()` 创建 `logs::Viewer`，断言不会 panic。
3. **按键焦点上下文。** 根节点是 `Astra`，各页面和组件再加上各自的上下文：`AstraLive`、`AstraLogs`、`AstraEditor`、`AstraDialog`、`AstraPalette`。
   - 单键快捷键一律加 `&& !Input` 条件，沿用 keys.rs 现有的写法。
   - 共享模型的按键处理（`Editor`、`logs::Viewer`、`diagnostics::View`、`nettools::View`、`monitor::View`）只在各自的上下文获得焦点时，通过 `on_key_down` 接收 `keys::from_keystroke` 转换后的按键。
   - 已经被 GPUI action 处理掉的按键，要 `cx.stop_propagation()`，避免同一个键处理两次。
4. **列表一律虚拟化。** 连接、日志、规则、编辑器树、任务事件这些可能很长的列表，统一用 `uniform_list`（行高固定）或 gpui-component 的 `Table`/`List`。不允许 `for` 循环一次性生成上千个子元素。
5. **所有用户可见文本都要经过 `t()`/`tr!`。** 新增的字符串要在 `astra-client/src/i18n.rs` 里补齐 en、zh-CN、zh-TW 三种语言。i18n 现有测试会检查是否有缺失，必须保持通过。
6. **本地状态和共享状态分开。** GPUI 侧只保存纯视图状态，比如滚动位置、悬停、展开的面板、`InputState` 实体。业务状态只保存在 `Client` 里。退出前通过 `Checkpoint` 持久化。

## 2. 代码目录（astra-gui/src）

```
lib.rs  backend.rs  model.rs  keys.rs  theme.rs
window.rs                # 根视图：侧边栏、状态栏、任务条、弹层宿主（瘦身后约 300 行）
widgets/                 # 无业务逻辑的通用组件
  card.rs status_dot.rs delay_chip.rs sparkline.rs chart.rs
  confirm.rs prompt.rs   # 由 Prompt 驱动的对话框
  token_text.rs          # highlight::Token 列表渲染为 StyledText
  empty.rs toast.rs
views/
  overview.rs proxies.rs connections.rs rules.rs logs.rs profiles.rs settings.rs
  palette.rs chooser.rs help.rs tasks.rs monitor.rs diagnostics.rs nettools.rs
  updates.rs installation.rs recovery.rs server.rs marquee.rs
  editor/{mod.rs, tree.rs, form.rs, raw.rs, members.rs, transfer.rs}
askpass.rs               # --askpass 模式下的独立小窗口
```

现在 `window.rs` 里的 `content()` 和 `todo()` 按页面拆到 `views/`，每完成一个页面，就删掉 `pages.rs` 里对应的 `pending` 条目。所有条目删完之后，`pages.rs` 只剩标题表。

## 阶段 A：把交互意图和派生数据下沉到 astra-client

TUI 里还有一批跟渲染无关的逻辑，GUI 必须复用。它们目前写在 `astra-tui/src/ui/*.rs` 的 `impl Ui` 里。

处理方法：
- 把它们改成 `impl Client`，或者改成接收 `&Client` 的普通函数，放到 `astra-client/src/` 下的对应模块。
- TUI 改为调用这些共享实现。
- 对应的测试跟着移过去。

| 来源（astra-tui/src/ui） | 迁移目标 | 内容 |
|---|---|---|
| `live.rs:49-130` `bytes`、`rate_value`、`exit_chain`；`runtime_health`(381)、`connection_badges`(418)、`connections_paused`、`track_new_connections` | `client::live` | 字节和速率格式化、出口链、健康状态文案、连接角标 |
| `dashboard.rs:128 notices` 和 `expire_notices`、`indicator_state`(46) | `client::overview` | 告警列表（级别用 `Severity` 枚举表示，不用颜色）、实例状态 |
| `dashboard.rs:77/88 navigate`、`navigate_back`、`remember_page` | `client::navigation` | 页面切换和返回栈 |
| `shell.rs:123-225` `active_rule`、`toggle_active`、`active_operation`、`rule_operation`、`open_logs`、`log_source`(409)、`rule_search`、`find`/`find_direction`(307) | `client::intents` | 每个页面操作对应的 `Input` 生成器 |
| `shell.rs:459 palette_entries_in`、`palette_matches`(794)、`palette_run`(921)、`command_help`(771) | `client::palette` | 命令面板条目、不可用原因、模糊匹配、执行 |
| `shell.rs:110 duration` | `client::format` | 时长格式化 |
| `settings.rs:110-345` `Field` 的 `label`/`description`/`choices`/`get`/`set`/`range`/`value_label`；`change_preferences`、`save_preferences`、`apply_preferences`、`load_startup_context`、`change_windows_service`、`marquee_hold_at` | `client::settings` | 设置项表，GUI 据此直接生成表单 |
| `update.rs:18-211` 更新命令的 `label`、`run`、`unavailable`、`request`、`candidate`、`defer`；`package_action` | `client::update`（已有，补全） | 更新流程 |
| `installation.rs:65-352` 中跟按键和鼠标无关的部分：`initialize_installation`、`open_installation`、`installation_request`、`start_installation`、`dismiss_installation_notice` | `client::installation`（已有，补全） | 安装流程 |
| `node_transfer.rs` 的 `export_path`、`start_node_export`、`prepare_node_import`、`apply_node_import` | `client::node_transfer` | 节点导入导出 |
| `server.rs:5 server_key` 中的动作映射 | `client::intents::server_action(Id)` | 服务端实例的操作 |
| `ui.rs:677 operations()` | `client::intents::operations(&Client)` | 实例操作菜单 |

要求：
- 原来接收 `KeyEvent` 的函数要拆成两层。一层是"语义操作"，参数是 `enum` 或 `Id`，由 TUI 和 GUI 共用；另一层是 TUI 的按键映射，留在 TUI。
- 原来返回 ratatui `Line`/`Span`/`Color` 的函数，改为返回 `Vec<highlight::Token>` 或语义枚举，由各前端自己决定颜色。
- **验收：**
  - `astra-tui/src/ui/` 下除了 `*key*`、`mouse`、`render_*`、`hit*` 之外，不再有业务分支。
  - TUI 测试全部通过，测试数量不减少。

A 完成之后，B–F 各阶段可以按页面并行推进。

## 阶段 B：通用组件和外壳

- **B1 主题**（theme.rs）
  - `Accent` 映射到 `theme.primary`、`ring`、`accent`。
  - `ColorDepth` 在 GUI 里映射为饱和度和对比度：True 和 256 用完整调色板，16 用高对比，Mono 用灰阶。
  - 跟随系统的亮色/暗色模式。
  - `motion` 为 Reduced 或 Off 时，关闭 GPUI 动画和 `with_animation`。
- **B2 widgets**
  - `Card`、`StatusDot`（running、stopped、starting、error）、`DelayChip`（延迟分级配色，超时显示 `—`）、`TokenText`。
  - `Sparkline` 和 `Chart`：用 `canvas` 加 `PathBuilder` 画折线，数据取自 `Client` 的速率历史和 `monitor::Sample`。
- **B3 对话框宿主**
  - 每种 `state::Prompt` 对应一个对话框：
    - `picker()` 为真：用列表加搜索
    - `confirmation()` 为真：用确认框
    - 其他情况：用文本输入框
  - 选择路径的 Prompt（AddPath、Import、UpdatePackage、Export*）额外提供一个"浏览…"按钮，调用 `cx.prompt_for_paths` 或 `prompt_for_new_path`。
  - 提交时走和 TUI 一样的共享提交函数（阶段 A 中 `Prompt` 的提交逻辑也要下沉）。
  - 输入框的初始值、校验报错都显示在对话框里，不能弹出新窗口。
- **B4 状态栏和任务条**
  - **顶部状态栏：** 当前配置名（点击打开切换器）、连接开关、模式、出口链、上下行速率。
  - **底部任务条：** 显示最近一条 `client.message` 和正在运行的任务数。点击或按 F6 打开 Tasks 面板。
  - **Toast：** 错误类消息额外弹一个 toast，5 秒后自动消失。
- **B5 命令面板、配置切换器、帮助**
  - 基于 `client::palette`。不可用的条目置灰，并显示不可用原因。
  - 键盘操作：↑↓ 选择，Enter 执行，Esc 关闭。
  - 帮助页由 `commands::help()` 生成，按 Scope 分组。
- **B6 多窗口**
  - Alt+1–7 和 F8 打开新窗口，与主窗口共享同一个 `Entity<AppModel>`，各自保存当前页。
  - 最后一个窗口关闭时，走现有的退出流程。

## 阶段 C：实时页面

每个页面一个 `views/*.rs`，用 `Entity` 保存视图状态，在 `render` 里读取 `model.read(cx).client`。

- **C1 Overview**
  - 头部卡片：状态、模式、出口链（含延迟）、监听地址、速率。
  - 告警区：取 `client::overview::notices`。
  - 模式分段控件：发出 `Runtime{SetMode}`。
  - 系统代理、TUN、局域网三个开关：发出 `Runtime{SystemProxy|Tun|AllowLan}`。在 Linux 和 macOS 上开 TUN 需要提权，走 F2。
  - 实时连接 Top-N，边框上叠加 sparkline。
  - 代理组摘要；可展开的流量面板（对应 TUI 的 `z`）；暂停按钮（Space）。
  - 按键：`m`、`S`、`T`、`A`、`D`、`N`、`L`、`e` 通过 `AstraLive` 上下文映射到相同的语义操作。
- **C2 Proxies**
  - 左栏：代理组列表。右栏：成员网格，显示 DelayChip。
  - 点击成员发出 `Runtime::Select`；"测试"按钮发出 `Runtime::TestGroup`，测试期间显示加载状态。
  - 支持 `/` 查找。
  - 选中当前成员时，用乐观更新：先在界面上标为"选择中"，等运行时快照回来确认后再定下来。
- **C3 Connections**
  - 用 `Table`（虚拟化）展示，列为：目标、网络、出口、上行、下行、时长。
  - 行数据来自 `connections::build_rows`，分组行可以折叠。
  - 筛选框；右侧详情面板；右键菜单"关闭连接"发出 `Disconnect(Some(id))`。
  - "全部关闭"要先弹 `Prompt::CloseAll` 确认框。
  - 新出现的连接要高亮，过期时间沿用 `expire_connection_highlights` 的逻辑。
- **C4 Rules**
  - 规则核心状态卡片：规则数、日志级别、共享警告、"磁盘上已修改"提示。
  - "测试路由"打开 `Prompt::Explain` 对话框，结果用 `explanation::View` 渲染成可滚动的报告。
  - 编辑和搜索按钮：打开编辑器，并预填搜索内容。
- **C5 Profiles**
  - 用 Tabs 切换 Proxy/Rule/Server；实例表格；详情卡片（配置、PID、运行时长、日志级别、启动绑定）。
  - 工具栏和右键菜单覆盖 `n c s x r e L t b d y E P v`，菜单项由 `client::intents::operations` 生成，包括强制启动和重启。
  - 危险操作（删除、强制启动、全部停止）一律走确认对话框。
- **C6 Recovery**
  - `client.recovery` 有值时，整个窗口替换成恢复页，提供"返回"和"继续"两个按钮。

## 阶段 D：可观测性

- **D1 Logs**
  - 布局：顶部工具栏（日志来源 proxy/rule/`@tui`/`@gui`、级别、捕获级别、内存上限、换行、跟随、重连），下方是虚拟化的日志行，再下面是搜索栏（`n`/`N`）。
  - 数据完全来自 `logs::Viewer`，`visible()` 的结果直接作为列表数据源。
  - 横向滚动在不换行模式下用 `overflow_x_scroll`。
  - 共享 `Viewer::key` 返回的 `Command` 由 GUI 执行，比如复制、关闭。
  - 创建和重连都必须经过 `tokio()` 包装（规则 2）。
- **D2 GUI 日志**
  - 新增日志来源 `@gui`。GUI 自身的日志写入 `astra_platform::tui_log` 的同一套机制，文件名用 gui 后缀，`@tui` 改名后保持兼容。
- **D3 Tasks**
  - 列表展示最近 32 个任务；点开显示事件流（最多 128 条）。
  - 进行中的任务可以中止，发出 `BackendCmd::Cancel`。
  - Updates 相关任务提供"刷新"（F5）。
- **D4 Monitor**
  - CPU、内存、网络三张折线图，数据取 `monitor::View` 的 `Sample`，时间窗口为 `LIMIT`。
  - 支持暂停和清空。
  - 每隔 `INTERVAL` 发一次 `Query::Monitor`。
- **D5 Diagnostics**
  - 去掉 `model.rs` 里"Diagnostics 计划在下一阶段实现"的拦截。
  - 用勾选列表选择诊断目标；提供全选、清空、运行、中止；结果实时流式更新，每项显示状态图标和耗时；"打开网络工具"跳转到 D6。
- **D6 Net tools**
  - 表单字段由 `nettools::View::fields()` 驱动：出口（下拉，可搜索）、类型（DNS/TCP/ICMP）、目标、记录类型、服务器、次数。
  - 运行和停止调用 `start`/`stop`，结果区可滚动。

## 阶段 E：编辑器（工作量最大，单独排期）

`Editor` 是共享的状态机。GUI 提供两个视图，用 Tab 切换：

- **E1 结构树**（tree.rs）
  - 数据来自 `Editor::rows()` 的 `FlatRow`，用 `uniform_list` 展示。
  - 支持：折叠展开、多选标记（v/V）、上下移动（K/J，同时提供拖拽）、重命名、复制、删除（可撤销）、新建（打开 NewOutbound 向导）。
  - 键盘在 `AstraEditor` 上下文中直接转发给 `Editor::key`，`zM`、`zR`、`zc`、`zo`、`u`、`Ctrl-Z`、`Ctrl-Y` 的行为与 TUI 一致。
  - 鼠标操作调用与键盘相同的语义方法，不要伪造按键事件。如果 `Editor` 缺少这些语义方法，就在阶段 E 开头补到 client 里。
- **E2 表单**（form.rs）
  - 选中节点后，右侧显示 `schema_rows()` 生成的字段行，控件按 `Spec` 选择：
    - 布尔：Switch
    - 枚举：Select
    - 数字和文本：Input
    - 列表：可增删的 chip
    - 引用：带 `suggestions()` 的下拉
  - 字段级的校验报错显示在字段下方。
- **E3 成员选择器**（members.rs）
  - 对应 Members 模式，用可搜索的多选列表，数据来自 `member_choices`。
- **E4 原始 TOML**（raw.rs）
  - 用 gpui-component 的代码编辑器，语法高亮来自 `highlight::spans`。
  - 失去焦点或按 Ctrl-S 时调用 `Editor::import(text)` 或 `update(doc)`，解析出错时在行内显示错误。
- **E5 草稿和保存**
  - 草稿保存时机与 TUI 相同：`remember_draft`、`suspend_editor`。
  - 冲突时显示横幅，提供"与磁盘对比"（`compare_draft`，用并排或统一 diff 视图）和"丢弃"（`Prompt::Discard`）。
  - Ctrl-S 调用 `prepare_save`，再发出 `Action::Save{original, text}`。
  - 离开编辑器前检查 `can_close`。
- **E6 导入导出**（transfer.rs）
  - Ctrl-O：导入到草稿。Ctrl-I：导入节点，用多选对话框，即 `ImportNodeSelection`。
  - Ctrl-E 和 Ctrl-Shift-E：导出 TOML 或分享链接，目标可以是文件或剪贴板。
  - 订阅页面触发 `UpdateSubscriptions`。
- **E7 外部编辑器**
  - 调用 `prepare_external` 打开外部编辑器；窗口重新获得焦点时提示"读取外部修改"，调用 `read_external`。
- **E8 vim 开关**
  - `client.vim_keys` 同时作用于编辑器、日志和列表视图的 hjkl 导航。
- **验收：** 把 `editor_test.rs` 里的主要场景各用一个 GUI 测试复现，至少覆盖新建节点、删除后撤销、保存冲突、导入节点。

## 阶段 F：设置与系统集成

- **F1 Settings 页**
  - 顶部是程序信息卡片：本地构建与管理节点的版本对比、平台能力、"刷新"按钮。
  - 下面是由 `client::settings::Field` 表生成的表单，分为外观、交互、启动、安装四组。
  - 修改设置后，按 `apply_preferences` 立即生效：切换语言后整个界面重新渲染，切换主题色立刻生效。
- **F2 提权 askpass**（Linux/macOS 开启 TUN）
  1. 发出 `Action::PrepareElevation`，拿到 `unix_handoff::Offer`。
  2. 依次调用 `inspect_program(&offer.binary)` 和 `ensure_root_controlled(&offer.sudo)`。这一步完全照搬 `astra-tui/src/ui.rs` 的 `elevate`，**不得删减**。
  3. 在后台线程用 `std::process::Command` 执行 `sudo -A <program> --unix-elevated <socket> <ticket>`，环境变量 `SUDO_ASKPASS` 指向当前可执行文件的 `--askpass` 入口。
  4. `astra-gui --askpass "<prompt>"`：askpass.rs 弹出一个最小的 GPUI 密码窗口，确认后把密码写到 stdout 并退出码 0，取消则退出码 1。
     - 密码不能写日志，不能进 Session，不能经过任何通道。
     - 用完立即覆盖清零，可以用 `zeroize`。新增依赖时要锁定精确版本。
  5. 发出 `WaitElevation`，失败时发出 `CancelElevation`。
  - 如果 `program.warning` 非空，用确认框展示给用户。
  - Windows 继续走 `windows/installation.rs` 现有的 UAC 路径。
  - 测试：`--askpass` 模式的参数解析，以及取消时退出码为 1。
- **F3 安装**
  - 安装对话框调用 `client::installation`，状态包括检测中、需要提权、进行中、完成或失败。
- **F4 更新**
  - Updates 面板提供：导入包（选择文件）、状态、激活（先确认）、回滚、恢复。
  - 激活完成后，用 `self_update::resolve` 和 `restart` 重启 GUI 本身，启动参数中带上 `--resume-ui`，恢复原来的窗口和当前页。
- **F5 开机启动和静默启动**
  - 用 `startup_unix`（launchd/systemd）和 Windows 的 `startup`。
  - 新增选项"启动 GUI / 启动 TUI"，写入不同的可执行文件路径。
  - 静默启动沿用 lib.rs 现有的无窗口路径。
- **F6 终端选择器**
  - 用 `terminals::discover` 列出可用终端，"在终端中打开 TUI"调用 `terminals::launch`。
- **F7 Windows 特权服务开关**
  - 对应 `change_windows_service`。
- **F8 Marquee**
  - 在主题色这一行按住 Ctrl+Home 3 秒后解锁（GPUI 侧需要同时处理 key down 和 key up，并维护按住计时）。
  - 渲染成顶部的滚动横幅。边框、文字、背景、速度、宽度、帧率都取自 `MarqueePreferences`。motion 为 Off 时不显示动画。

## 阶段 G：服务端模式（astra-server-gui）

- `kind == Server` 时隐藏页面 1–4，默认打开 Profiles。
- 头部和统计卡片：名称、状态、PID、运行时长；连接数（活跃、已接受、已拒绝）；上下行字节数。
- 操作 `c r e L v`，用 `client::intents::server_action` 生成。
- 测试：在 Server 模式下，页面切换和快捷键不能进入 1–4 页。

## 阶段 H：打包与 CI 收尾

- 在 `scripts/package_gui.py` 里补上：
  - macOS：`.app` 的 Info.plist（`LSUIElement` 为 false，设置 `NSHighResolutionCapable`）和图标
  - Windows：图标资源
  - Linux：`.desktop` 文件
- CI 的 gui job 改为：
  - `cargo test -p astra-client -p astra-gui`
  - 在 Linux 上用 `xvfb-run` 跑一次窗口冒烟测试：启动 `astra-gui --endpoint <测试 manager>`，5 秒后发 SIGTERM，要求退出码为 0。
- release 使用 fat LTO，GUI 二进制会显著拉长链接时间。不要新增 profile，可以把 GUI 构建放到单独的 job 里并行跑。

## 3. 测试策略

| 层级 | 做法 |
|---|---|
| client 单元测试 | 阶段 A 下沉的每个函数都带着原测试一起迁移。新的语义操作要测"输入 `Client` 状态 → 输出 `Input`/`Action`" |
| GUI 模型测试 | 在 `TestAppContext` 中构造 `AppModel`，注入伪造的 `BackendEvent`，断言 `client` 状态，以及在页面、选中项变化后结果被正确丢弃 |
| GUI 视图测试 | `cx.add_window` 渲染各页面，再用 `cx.simulate_keystrokes`/`simulate_input` 验证：在输入框中输入 `q`、`1`、`/` 不会触发命令；Esc 能关闭弹层；Enter 会提交对话框 |
| 端到端测试 | 复用 `astra-tui/src/test/ui_live_test.rs` 里启动测试 manager 的辅助函数（必要时放到 `astra-client` 的 `test-support` feature 下），新增 `astra-gui/tests/live_test.rs`，覆盖连接、切换模式、选择代理、关闭连接、保存配置 |
| 手动验收 | 按 GPUI_GUI.md 功能对照表逐行勾选，Server 模式也要过一遍 |

## 4. 推荐顺序与里程碑

1. **M1：** 阶段 A，加上 B1–B4。完成后，GUI 能展示真实数据，所有弹窗都能用。
2. **M2：** 阶段 C 全部，加上 D3。日常使用的功能全部可用。
3. **M3：** 阶段 D 剩余部分，加上 F1、F3、F4。可观测性、设置、更新可用。
4. **M4：** 阶段 E 编辑器。
5. **M5：** F2、F5–F8，加上 G 和 H，达到完全对等。

每个里程碑结束时，交给 Claude 按 GPUI_GUI.md 的审查清单，再加本文第 1 节的规则，做一次验收。
