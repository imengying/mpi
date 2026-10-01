# mpi

极简终端 AI 编程代理。单个二进制，无插件、无会话树、无账号体系。

项目名 `mpi`，**产物与配置目录叫 `pi`**：仓库和库仍叫 `mpi`（`mpi::` 路径不变），
但你要敲的命令、装出来的二进制、配置与数据目录是 `pi` / `~/.pi`。

```
~/文档/mpi (main) • 重构配置层
↑ 173k   ↓ 173k   󱘲 99.5%   17.3k/1M                     deepseek-v4.1-flash • high
```

## 安装

### 一键安装

自动识别系统与架构，下载最新 Release，按 GitHub 提供的 sha256 校验后装进
`~/.local/bin`：

```sh
curl -fsSL https://raw.githubusercontent.com/imengying/mpi/main/install.sh | sh
```

指定版本或安装目录：

```sh
curl -fsSL https://raw.githubusercontent.com/imengying/mpi/main/install.sh | sh -s -- v0.1.3 --dir /usr/local/bin
```

装好后的升级不用重新跑脚本，`pi update` 就地自更新：下载最新 Release、
按 GitHub 提供的 sha256 校验、原子替换自身。

脚本只依赖 POSIX sh、curl（或 wget）、tar。产物覆盖 Linux / macOS 的
x86_64 / aarch64；Linux 产物要求 glibc ≥ 2.39（ubuntu-24.04 构建），脚本会先检查
再下载，musl（Alpine 等）暂无产物。需要代理时设 `https_proxy` 环境变量即可，
curl / wget 会自己认。

pi 用 zsh 执行命令（默认 `/usr/bin/zsh`），系统里得有它。

### 手动安装

到 [Releases](https://github.com/imengying/mpi/releases) 下载对应 target 的
`pi-<版本>-<target>.tar.gz`（targets 见[发布](#发布)），解压后放进 PATH：

```sh
tar -xzf pi-*-*.tar.gz --strip-components=1
install -m755 pi ~/.local/bin/
```

### 从源码构建

```sh
cargo build --release
install -m755 target/release/pi ~/.local/bin/pi
```

要求 rustc 1.98+（edition 2024）。

## 配置

配置在 `~/.pi/config.json`，启动时读一次，没有热重载。
**第一次运行会自动写出模板**，然后停下并打印路径：填好 provider 与模型再运行。

```json
{
  "shell": { "path": "/usr/bin/zsh" },
  "providers": [
    {
      "name": "name",
      "api": "completions",
      "base_url": "url",
      "api_key_env": "NAME_API_KEY",
      "models": [
        {
          "id": "deepseek-v4.1-flash",
          "name": "deepseek-v4.1-flash",
          "context_window": 1000000,
          "max_tokens": 64000,
          "reasoning": true,
          "thinking_levels": ["low", "high", "max"]
        }
      ]
    }
  ],
  "default_model": "name/deepseek-v4.1-flash"
}
```

`providers` 是必填的，其余都有默认值。**配置里写了几个模型，`/model` 就只列几个** ——
pi 没有内置模型目录，也不会去猜。没配置 providers 会直接报错退出。

| 字段 | 说明 |
|---|---|
| `api` | `messages`、`completions` 或 `responses` |
| `base_url` | 缺省按 `api` 取官方地址；任意 OpenAI 兼容网关直接写它即可 |
| `api_key_env` | 读哪个环境变量取 key，缺省为 `<PROVIDER>_API_KEY` |
| `api_key` | 直接写 key（不推荐）；两者都缺时请求会报错 |
| `models[].context_window` | 底栏容量与告警阈值；缺失时显示 `?` |
| `models[].max_tokens` | 单次回复上限，缺省 8192 |
| `models[].reasoning` | 是否支持推理，决定 `/model` 选完要不要问级别 |
| `models[].search` | 是否使用该接口的原生搜索，缺省 `false`。整段会话要么一直开要么一直关 |
| `models[].thinking_levels` | 该模型支持的级别；`reasoning` 为真但缺失时表示五档全支持 |
| `models[].compat` | 覆盖兼容开关，见下 |
| `models[].compaction.reserve_tokens` | 压缩的安全余量；缺省为 16384 与窗口四分之一中的较小值 |
| `models[].compaction.keep_recent_tokens` | 摘要后保留的最近历史预算；缺省最多 20000，小窗口自动缩小 |

`compaction` 按模型设置，两项都可省略；显式设置时必须填写 `context_window`。
输出上限与安全余量之和必须小于窗口，最近历史预算必须小于压缩触发阈值，启动时会校验。

思考级别的词汇表只有五档：`low`、`medium`、`high`、`xhigh`、`max`。
写别的值会在启动时报错，不会静默忽略。

底栏和思考级别菜单用不同颜色区分五档：`low` 灰、`medium` 青、`high` 蓝、
`xhigh` 紫、`max` 黄。整体采用 Codex 风格的简洁配色：正文、标题沿用终端前景色，
辅助信息灰色，链接和行内代码用青色；警告与错误仍分别用黄、红色。

`api` 同样只有 `messages`、`completions`、`responses` 三个取值，**没有别名**：
协议名只有一种写法，长名字（`openai-completions` 这类）不会因为是旧写法而被接受。
少一种写法就少一件要记住的事，而写错时那句报错会把该写什么直接说出来。

**写错的字段名同样会在启动时报错。** 配置字段一律 snake_case，写成 `baseUrl`
这样的驼峰名不会生效——而一个被悄悄忽略的 `base_url` 意味着请求发给的是协议默认的
主机（比如 `api.openai.com`），带着你的 key 和整段对话。所以这类拼写错误会被点名
拦下，并提示正确写法，而不是让请求发到别处。

### 三个协议怎么选

| `api` | 什么时候用 |
|---|---|
| `completions` | 绝大多数网关与中转（DeepSeek、GLM、Kimi…）；先试这个 |
| `responses` | 只会说 Responses 的模型（GPT-5 系、Codex），或网关只对这个端点开通了某模型 |
| `messages` | Anthropic 官方或兼容 Messages 的服务 |

`responses` 会以 `store: false` 发送，会话只在本地存一份，服务端不留档；
推理摘要以 `encrypted_content` 原样回放，且只在**同一个 provider 与模型**下回放——
中途 `/model` 换了模型，旧的推理块会被丢掉而不是发给不认识它的上游。
模型自己跑的搜索也是这样：`web_search_call` 整段存下来、原样放回去，pi 不执行、
也不另造一条工具结果。Anthropic 的 `pause_turn` 同样是把这条助手消息再提交一次。

### 兼容开关

不同网关对「OpenAI 兼容」的理解不一样。pi 按 `base_url` 自动探测一组开关，
配置里只写例外（`compat` 可以是 provider 级或 model 级，model 级优先）。
对 `completions` 协议，`deepseek-*` 模型（也支持 `deepseek-ai/DeepSeek-*`）会按模型名补充
DeepSeek 的推理参数与历史回传规则，避免通用中转地址漏发 `reasoning_content`。
显式配置的 `compat` 仍然优先；同一中转里的其他模型不受此规则影响。
`responses` 只用到其中的 `supports_strict_mode`（工具定义上的 `strict`）
与 `send_session_affinity`（会话粘性头），其余开关对它没有意义——
它的思考参数、请求形状与缓存键都是这个协议固定的写法。

```json
"compat": {
  "thinking_format": "deepseek",
  "requires_reasoning_content_on_assistant": true,
  "max_tokens_field": "max_completion_tokens",
  "supports_developer_role": false,
  "send_session_affinity": true
}
```

可用开关：`max_tokens_field`、`supports_developer_role`、`supports_reasoning_effort`、
`thinking_format`（`openai` / `deepseek` / `zai` / `qwen` / `llamacpp` / `anthropic` / `none`）、
`requires_thinking_as_text`、`requires_reasoning_content_on_assistant`、
`requires_assistant_after_tool_result`、`supports_usage_in_streaming`、`supports_strict_mode`、
`supports_cache_control`、`send_session_affinity`、`supports_long_cache`、
`search_format`（`web_search` / `web_and_x` / `anthropic` / `xai` / `qwen` / `zhipu` / `off`）。

`search: true` 时按 `base_url` 选写法：Responses 追加 `web_search`（x.ai 再加 `x_search`），
Messages 追加 `web_search_20250305`，Chat Completions 用 `search_parameters`（x.ai）、
`enable_search`（通义）或智谱的 `web_search` 工具（`open.bigmodel.cn`、`z.ai`，并要回搜索结果）。
探测不到、又没有 `search_format` 时启动失败，而不是发一个上游不认识的字段。

**DeepSeek 官方接口不能原生搜索。** 对话补全没有搜索参数；Responses 接受 `web_search` 然后直接忽略，
所以对 `api.deepseek.com` 打开 `search` 会在启动时报错，而不是假装搜过。GLM 可以：模型上
`"search": true`，`base_url` 指向智谱或 Z.ai。

搜索默认关闭，未开启时请求前缀与以前一致。摘要请求会强制关掉搜索。引用只出现在界面上，
不写进发给模型的历史。

## 启动

```sh
pi              # 新会话
pi resume       # 继续最近一次会话（等价于 /resume）
pi resume <id>  # 继续指定会话，id 或其前缀（退出时会打印这条命令）
pi update       # 更新到最新 Release
```

## 系统提示词

**pi 不内置任何提示词。** 模型收到的 system 消息完全来自项目里的 `AGENTS.md`：

- 只读**项目根目录**那一份。项目根 = 从当前目录向上找到的最近一个含 `.git` 的目录；
  不在 git 项目里时，就是当前目录本身。
- 项目根之上、之下都不读。所以在任意子目录里跑，拿到的都是项目提交的那一份；
  别人在 `/tmp` 或上层目录放的文件不会被读进去。
- 项目里没有这份文件时**不发送 system 消息**，而不是发一条默认的。
- 内容只读一次（开会话时），此后整个会话里逐字节一致 —— 这是提示词缓存的前缀；
  改了文件要 `/new` 或重启才会生效。
- 启动时会提示读了哪个文件。
- 环境信息（工作目录、平台、shell、会话 id）走单独的环境块，不放进提示词，
  否则每轮变动都会让缓存失效。

想让 pi 有固定的工作方式，在项目根目录写一份即可：

```markdown
# AGENTS.md

- 需要了解代码时先读文件再下结论，不要凭猜测修改。
- 一次只做用户要求的事，不做多余的改动与重构。
- 回答用中文，简洁直接，不要复述已经说过的内容。
```

## 发布

推一个 `v*.*.*` 标签就会自动编译并发布 GitHub Release，**tag 就是版本号**：

```sh
git tag v0.1.0
git push origin v0.1.0
```

产物为 `pi-<版本>-<target>.tar.gz`（内含二进制、README、LICENSE）；校验和由 GitHub 在 Release 页面自行提供。Release 标题就是 tag 本身（如 `v0.1.3`）。
targets：

| target | runner |
|---|---|
| `x86_64-unknown-linux-gnu` | ubuntu-24.04（glibc） |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm（glibc） |
| `x86_64-apple-darwin` | macos-15-intel |
| `aarch64-apple-darwin` | macos-15 |

Linux 二进制依赖 runner 自带的 glibc（当前 2.39），构建摘要里会列出它实际引用到的
最高 `GLIBC_x.y` 符号版本，方便确认需要多新的系统。

`--version` 取的是 tag：工作流把 tag 以 `PI_BUILD_VERSION` 传入构建（`build.rs` 把它
声明为 rerun 触发条件，避免缓存留下旧值），并在打包前校验 `pi --version` 与 tag 一致，
不一致直接报错退出。本地构建没有这个变量，回退到 `Cargo.toml` 里的版本。

构建用 `--locked`，即按 `Cargo.lock` 里锁定的版本编译；依赖更新走
`cargo update` 后提交 lock 文件。

## 命令

| 命令 | 作用 |
|---|---|
| `/model` | 选择模型；选完若该模型支持推理，紧接着问思考级别 |
| `/name <文字>` | 设置会话名；不带参数则清空 |
| `/compact [指示]` | 手动压缩上下文，可带一段自定义关注点 |
| `/new` | 新会话 |
| `/resume` | 恢复历史会话（Esc 开始新会话） |
| `/delete` | 删除本会话文件并退出（需确认） |
| `/exit` | 退出 |

`/delete` 会先问一次（默认高亮在「不删」），确认后删掉会话文件并退出。
非交互时（管道输入）不执行删除，只提示手动删除的命令——被删除的文件无法恢复，
不能靠默认选择拿主意。

输入 `/` 会弹出命令菜单，继续输入即按前缀过滤。`Tab` 补全：唯一匹配补全并加一个空格，
多个匹配先补到公共前缀、补无可补时在菜单里往下走；`Shift+Tab` 往回。菜单打开时 `Enter`
选中并执行（`Tab`/`Shift+Tab`/`↑`/`↓` 移动高亮）、`Esc` 关闭——关掉之后不会自己再弹出来，
继续打字（缓冲区变了）才重新出现。

`↑`/`↓` 在菜单开着时选菜单；菜单关闭时先在输入的折行之间移动，到首尾后再翻历史。
菜单和模型、会话选择列表会限制高度，并随选中项滚动；选择列表支持 `Home`、`End` 和翻页键。
从历史里恢复一条 `/命令` 不会弹出菜单——按 `↑` 要的是上一行，不是一张没要过的列表，
菜单一开方向键就被它拿走，人就卡在那条命令里退不出来了；你再敲一个字符它就会回来。
历史往回走到头就停住；往下走过最后一条之后回到**空白行**，并且把你漫游前正在写的那半句还给你。

历史属于**会话**，不属于进程：`/resume`（或 `pi resume`）之后，这次之前你说过的每一句
都在 `↑` 的射程里，不必重新打一遍。`/new` 则是真的从零开始——新会话没有听过那些话，
把它们摆在这里会像是这段对话说过。

`←`/`→` 按可见字符移动，中文、组合音标和连续 emoji 都不会被拆开；
`Alt+←/→` 或 `Ctrl+←/→` 按词移动。`Home`/`Ctrl+A` 到当前行首，`End`/`Ctrl+E` 到当前行尾；
`Ctrl+Home/End` 到整个输入的开头或结尾，`Ctrl+K` 删到当前行尾，`Delete` 删光标处的字符。
`Shift+Enter`、`Alt+Enter` 或 `Ctrl+J` 插入换行，普通 `Enter` 提交；`Shift+Enter` 需要终端支持增强键盘协议。

输入按终端宽度折行；长粘贴内容只显示光标附近的行，用 `↑`、`↓` 标记还有内容。
输入区最多占终端高度的三分之一、上限八行，完整文本仍保留在编辑器里。

**回合进行中输入行照常可用**：模型在答、命令在跑的时候照样能打字、粘贴图片、
按 `Ctrl+O` 展开输出、输入 `/` 看菜单。输入和状态固定在终端底部，上面的输出增长不会推走光标。

输出直接使用终端自带的选择和复制快捷键。回复和流式预览由终端自动折行，
复制时按终端的软折行规则还原真实换行；代码块不添加展示缩进，原有空格缩进保持不变。

总结与普通回复使用同一套 CommonMark 渲染，支持标题（含 Setext）、嵌套列表、任务列表、
引用、代码块、表格、强调、引用式链接和字符实体。窄屏表格按“列名: 值”显示；
代码块保留换行和空格缩进，HTML 按文本展示。流式输出按完整块提交，恢复会话时仍从 Markdown 原文排版。
完整 Markdown 段落陆续进入终端滚动历史，未完成的段落留在预览区，结束时只补齐剩余部分。
每帧只重画变化的逻辑行；缩放窗口会重新排版可见历史、输入和底栏。
回合中按 `Enter` 提交的补充显示在输入行上方（`… <内容>`），连同图片在下一次模型请求前生效。
模型正在生成或工具已经开始执行时，先保留它的结果；尚未开始的工具调用标记为未执行，
让模型结合新要求重新决定。需要立即停止当前操作时按 `Esc`。
以 `/` 开头的命令等当前回合结束后执行；消息和命令始终按提交顺序处理，命令后的消息不会提前越过它。
等待授权时面板上不显示光标。

`Ctrl+V` 粘贴剪贴板里的图片（截图直接可用），图片显示为 `[图片 1280×720, 84 KB]`
挂在输入行下方，多张图片显示数量；也可以只粘图片不带文字。图片随该条消息一起发出去，存在会话文件里，
`/resume` 之后仍在。剪贴板没有图片时 `Ctrl+V` 退化为粘贴文本。

粘贴多段文字会保留换行与空行，整段放入输入框，按 `Enter` 才一次性提交。
终端粘贴（如 `Ctrl+Shift+V`）需要终端支持 bracketed paste；`Ctrl+V` 文本粘贴同样适用，
回合进行中粘贴也不会自动拆成多条排队消息。

**`Esc` 停止正在跑的这一轮**：模型边答边按就停止生成，命令跑着按就杀掉那条命令，
已经收到的内容留在转录里（记成「（已停止）」），接着说话即可继续。
菜单开着时第一个 `Esc` 只关菜单——那是一个正在问你的列表，关掉它不该把答案也扔掉；
再按一次才是停止。

其余快捷键：`Ctrl+O` 展开/收起最近一块工具输出，`Ctrl+C` 清空当前输入行（空行时退出），
`Ctrl+U` / `Ctrl+W` 删到行首 / 删一个词，`Ctrl+K` 删到行尾。
回合运行时，空输入上的 `Ctrl+C` 和 `Esc` 一样中断当前回合。

## 工具

`read`、`write`、`edit`、`bash`、`grep`、`find`、`ls`。

`bash` 通过 zsh 执行。`grep` / `find` / `ls` 优先用系统的 `rg` / `fd` / `eza`，
没有就回退 `grep` / `find` / `ls`。

相邻的 `read` / `grep` / `find` / `ls` 最多四项同时执行，每项独立检查授权，结果按模型请求的顺序记录。
`write` / `edit` / `bash` 逐项执行，并等待前面的只读调用完成；修改后的读取因此能看到最新内容。
工具结果区分成功、失败、已中止、未执行和结果未知。中止可能已有部分效果，继续任务前需要检查实际状态。

工具输出最多保留 2000 行或 50KB，超出时**保留末尾**并把完整输出写进临时文件，
在结果里给出路径。

`bash` 的 stdout / stderr 直接写入同一临时文件，避免管道堵塞，结束后只读取有界的末尾内容；
小输出的临时文件自动清理，截断时保留完整原始日志。`read` 按固定大小缓冲区扫描指定行范围，
跳过的内容不留在内存中，单行很长时也不会整行载入。

长输出折叠时**直接显示摘录，不提示「已收起 N 行」**——那一行每次工具调用都要占一行位置，
而截断本身从内容就看得出来。文件修改同理：过大的改动不会只留一句「预览已省略」，
而是先把两侧相同的首尾行剪掉、只对改动区域做 diff，所以「大文件里改一行」照样看得见那一行。

**输出里不加总结性的注记**：`read` 不再追加「仅显示第 X–Y 行，共 Z 行；继续读取请用 offset=…」，
`ls` / `grep` / `find` 也不再追加「共 N 项 / N 行匹配 / N 个结果」。这些数字都能从内容自己数出来，
而注记总落在最后一行——折叠时块只保留末尾几行，于是**注记成了唯一一定看得见的那一行**，
长结果反而只剩一句统计。真正无法从内容推断的提醒会保留：`find` 被 `max_results` 截断时
仍然报告「已截断」，否则一个前缀列表会被当成完整答案。

同一条规矩适用于转录里的所有旁白：**只写用户无从得知的那件事**。
底栏已经画着的（模型、级别、会话名、上下文用量）不重复；
屏幕自己就说明了结果的（`/new` 之后空屏、`/delete` 之后文件没了、Esc 关掉菜单）不加旁白；
能从上文推出来的操作指引（“可以接着输入新的要求”）不写。
留下来的是路径、被夹掉的思考级别、`已停止`、上游错误原文这类信息。

改动行号只占**一列**（不是 `git diff` 的新旧两列），显示的是该行在**当前文件**里的编号：
新增与上下文行用新编号，删除行用它原来的编号。是哪一种由 `+`/`-` 与底色说明。
改动两侧各留 3 行上下文，中间的空白裁掉，所以显示量取决于改动大小而不是文件大小。

## 回答的排版

模型写的是 markdown，pi 把它渲染出来而不是显示标记：`**粗体**` 变成**粗体**，
围栏变成一条带语言名的横条，表格画出边框，`|` 和 `#` 不再出现在屏幕上。

覆盖模型实际会写的那些：标题、粗体/斜体/删除线、行内代码、代码块、列表（嵌套、有序、
任务列表）、表格（含对齐）、引用、分隔线、链接。代码块只画一副带语言名的框，**内容原样
透传、不着色**：猜错的 tokenizer 会把读者正在核对的代码涂改掉，比不涂更糟。

最终总结与流式预览、历史回复共用渲染器。支持粗斜体组合、反斜杠转义、多反引号行内代码、
带括号或空格的链接目标，以及链接文字里的强调；删除线使用终端原生样式。
表格按渲染后的文字宽度对齐，支持省略外围竖线、转义竖线和行内代码中的竖线。

三条规矩：

- **没闭合的标记原样显示**。流式输出每个 token 都重渲染，吃掉半截 `**` 会让文字闪。
  半个表格也不算表格——要等 `---` 分隔行出现
- **只丢没有信息的标记**。`**` 该消失，`- ` 要留下（列表就靠它读）；
  不添加任何说明性文字
- **缩进跟着行走**，所以窄终端里折行的列表项对齐在文字下方，调窗口也不会错位

## 授权

危险操作在执行前弹出贴底的全宽面板，标题固定为「需要授权」。选中项的底色铺满整行，
不随文字长短变化。
`↑↓`/`Tab` 切换，「Enter」确认，`a`/`1` 允许本次，`Esc`/`Ctrl+C`/`q`/`n`/`2` 拒绝。
等待没有超时，授权只对当次有效。

**自动放行**：白名单里的简单只读命令（`pwd ls cat head tail wc stat readlink realpath
printf echo true false cut tr du df uname rg grep find sort file sed git`），
其中 `git` 只放行 `status diff log show rev-parse ls-files ls-tree`，
`sed` 只放行纯行范围打印；`which date nproc uptime free ps id whoami basename dirname
column lscpu seq` 这类探查命令也放行——它们改不了任何东西；
以及当前项目目录内的普通 `edit` / `write`。

`cd` 仅在命令开头、通过 `&&` 连接后续命令时自动放行；后续命令按**实际运行的位置**检查：`cd /etc && cat shadow`
读的是 `/etc/shadow`，按项目目录判断相对路径会把它看成无害的名字。
`cd -`、多参数、不存在的目录、以及目标经过符号链接的情况仍然询问。

只挪文件描述符的重定向（`2>&1`、`2>/dev/null`、`>&2`、`2>&-`、`> /dev/null`）放行，
碰文件的一律询问（`> file`、`>> file`、`< file`、`>& file`、heredoc）。
拒绝 `2>&1` 看着安全，实际会把模型逼向 `python3 - <<EOF` 那种能执行任意代码的写法。

**需要授权**：删除、提权、Git 写操作、网络传输、脚本、变量或命令替换、
会写文件的重定向、写到项目目录外、
敏感路径（`~/.ssh`、`.env*`、`id_rsa`、`~/.pi`、`~/.codex`、`*.pem` 等）、
以及任何未被识别的选项或语法。`env` 不放行：裸 `env` 打印全部环境变量（含 provider key）。

路径检查逐参数进行，覆盖 `--file=/path`、`-f/path` 与 `-nf/path`（按 `-n -f /path` 理解）
三种写法，`git show <rev>:<path>` 冒号后的路径也检查。搜索表达式按数据处理，
`rg/grep` 的 `-f`、`--file` 等参数仍按实际文件路径检查。授权面板显示判定原因。
自动放行后执行的是校验过的绝对程序路径和参数；Git 禁用外部 diff、textconv 与 hooks。

被拒时返回给模型的文本是 `未获得用户授权，操作未执行（<具体原因>）`，
让模型知道为什么被拦下，而不是反复重试同一条命令。无 UI（headless）时一律拒绝，
绝不静默放行。

## 压缩

长会话靠上下文压缩维持可用。三个触发路径共用一套实现：

- `manual`：`/compact`
- `threshold`：每轮请求前核算上下文，超过 `窗口 − 输出预算 − 安全余量` 时先裁剪，再决定是否生成摘要
- `overflow`：明确的上下文溢出后先尝试裁剪，不够时生成摘要并重试（每轮最多一次）；不删除此前有效回复

有上下文压力时，先确定性裁剪超过 8192 字符的工具结果：保留开头 4096 字符、末尾 1024 字符，
中间替换为原始会话文件的引用。调用参数、执行状态和用户消息保持完整，原始结果仍在追加式日志里。
裁剪后重新估算，低于阈值就直接继续，不为摘要额外请求模型；普通短会话不会触发裁剪。
裁剪后旧的上下文用量失效，下一次请求之前使用估算值。

参考 [DeepSeek harness](https://github.com/deepseek-ai/deepseek-harness) 的保留区间与压缩校验设计，
检查点由结构化摘要和最近历史组成；缺省保留预算取 20000 token、窗口三分之一和触发阈值一半中的最小值，
也可通过该模型的 `compaction.keep_recent_tokens` 调整。
切点不会拆开工具调用与结果；若在一轮任务中间压缩，原始任务消息及图片一起保留。
已总结的旧用户消息不会再全部拼回上下文，原文仍保存在会话文件里。

已读取、已修改的文件清单从原始工具记录重建，只有成功结果计入，失败、拒绝、未执行、
已中止或结果未知的调用不会被写成完成。每个摘要还附带有界的用户原话：优先最近的短消息，最多八条，
按窗口缩小、总计不超过 4096 字符，后来的纠正优先。长消息由摘要和当前任务原文承接；
连续压缩仍从原始记录提取这些事实和原话。

压缩请求优先复用缓存前缀：沿用会话的系统提示、工具定义、模型、思考级别、缓存键和会话粘性头，
原样回放待压缩的历史（包含推理与工具结果），仅在末尾追加摘要指令；不会重复传入上一检查点。
摘要输出最多预留 16384 token，并按模型上限、小窗口容量进一步缩小。

完整历史超出预算时，才退回有界的文字摘录：去除推理、限制单条正文和工具结果，并保留首尾内容。
回退仍保留系统提示与工具定义；若这些固定内容本身已占满预算，则报错并保留原历史。
工具定义用于缓存匹配，摘要请求通过 `tool_choice: none` 禁止调用；截断、空白、包含工具调用
或没有缩小上下文的摘要均不写入检查点。实际缓存复用程度由上游决定，可查看返回的缓存用量。

用量优先采用上游统计，加上统计后新追加的输入和工具结果；估算包含工具定义，中文不会再按四个字
一个 token 低估。DeepSeek 的缓存命中/未命中用量分别计入，每次请求只累计一次。

按 [harness 的主循环](https://github.com/deepseek-ai/deepseek-harness/blob/main/packages/core/agent-loop/src/agent.ts) 处理输出上限：
保留已生成的文字与思考，结束本轮，不自动注入“继续”再请求。只有思考而无正文时会提示调低思考级别。
输出上限与上下文溢出分别处理，不因回答很短就触发压缩重试。
截断响应中的工具调用不会执行，需要模型重新完整发出。Anthropic 的 `max_tokens` 包含推理和正文，
不再重复扣除推理预算。SSE 按完整事件解码，支持跨网络分块的中文及 CRLF 分隔。

DeepSeek 使用 `thinking.type` 和 `reasoning_effort` 控制思考，不发送未经支持的
`thinking_token_budget`。带工具的对话会完整回传历史推理，包括前一用户轮的推理；
普通对话不会为了缩短请求而丢掉它；模型配置关闭思考时显式发送 `thinking.type: disabled`。
发送对话前检查工具调用和结果是否一一配对，缺失、重复或名称不匹配时直接报告，避免发送破损历史。

新会话按项目原设计，默认取模型可用等级列表的第一项；你可以通过 `/model` 自行调整。
恢复会话保留已选择的档位，不为 DeepSeek 单独固定 `high` 或 `max`。
`max_tokens` 是推理与正文共享的输出上限，调大它不能保证模型更早给出正文。

底栏在压缩期间显示 `?`，结束后转录里只留一句 `已压缩`——省了多少从底栏的上下文数字看，
转录里不复述。

## 会话

线性 JSONL，一行一条记录，存在 `~/.pi/sessions/<短 id>/`，`dirs.json` 记录 id 对应的绝对路径。

记录分类型存放：会话头、对话项（含工具执行状态及单次 token 用量）、每轮环境快照、会话名称、压缩与裁剪检查点。
环境快照与对话分开存，所以它不会污染上下文，也不会在压缩时被当成对话处理。

文件只追加，不改写已有字节：会话名是单独的 `session_info` 记录（读时取最后一条），
压缩是新的 `compacted` 检查点，裁剪是 `pruned` 检查点，记录替换后的上下文、裁剪数量和估算节省量。
文件保留完整历史，检查点缩小的是发送给模型的上下文，恢复会话时重放最新检查点。

只接受当前会话结构；旧字段、旧事件和缺失必需字段直接报错，不提供自动迁移。
工具路径只使用 `path`，不再接受 `file_path`、`@` 或 `file://` 的旧别名。

**恢复会带回你选的模型与思考级别**：每轮都会记下当时用的模型和级别，`pi resume` 取最新的一条。
`/model` 换过模型之后再恢复，接着用的就是你换过的那个，而不是开会话时的那个。

## 设计取舍

- **提示缓存优先**：系统提示词逐轮逐字节一致（不放 cwd、时间、分支、模型名），
  这些每轮固定但会变的信息放在会话首条消息里。工具定义顺序固定，消息用 struct 序列化
  以保证字段顺序稳定。Anthropic 打三个 `cache_control` 断点（系统提示词、最后一个工具、
  最后一条消息），OpenAI 兼容接口发 `prompt_cache_key`。
- **流式 + 底部视口**：参考 [Codex CLI 0.159.3 的终端实现](https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/tui/src/insert_history.rs)，
  用绝对坐标、差量重绘和同步更新维护底部输入区，聊天记录留在终端自己的 scrollback 里。
- **依赖少**：不引入 `toml`（配置是 JSON）、`ratatui`（全屏缓冲模型不匹配）、
  任何编辑器/语法高亮 crate（代码块不着色，见「回答的排版」）。禁止 `unsafe`。
