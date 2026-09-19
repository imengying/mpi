# mpi

极简终端 AI 编程代理。单个二进制，无插件、无会话树、无账号体系。

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

装好后的升级不用重新跑脚本，`mpi update` 就地自更新：下载最新 Release、
按 GitHub 提供的 sha256 校验、原子替换自身。

脚本只依赖 POSIX sh、curl（或 wget）、tar。产物覆盖 Linux / macOS 的
x86_64 / aarch64；Linux 产物要求 glibc ≥ 2.39（ubuntu-24.04 构建），脚本会先检查
再下载，musl（Alpine 等）暂无产物。需要代理时设 `https_proxy` 环境变量即可，
curl / wget 会自己认。

mpi 用 zsh 执行命令（默认 `/usr/bin/zsh`），系统里得有它。

### 手动安装

到 [Releases](https://github.com/imengying/mpi/releases) 下载对应 target 的
`mpi-<版本>-<target>.tar.gz`（targets 见[发布](#发布)），解压后放进 PATH：

```sh
tar -xzf mpi-*-*.tar.gz --strip-components=1
install -m755 mpi ~/.local/bin/
```

### 从源码构建

```sh
cargo build --release
install -m755 target/release/mpi ~/.local/bin/mpi
```

要求 rustc 1.98+（edition 2024）。

## 配置

唯一的配置文件是 `~/.config/mpi/config.json`，启动时读一次，没有热重载。

```json
{
  "shell": { "path": "/usr/bin/zsh" },
  "providers": [
    {
      "name": "work",
      "api": "openai-completions",
      "base_url": "http://192.168.1.16:1221/v1",
      "api_key_env": "WORK_API_KEY",
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
  "default_model": "work/deepseek-v4.1-flash"
}
```

`providers` 是必填的，其余都有默认值。**配置里写了几个模型，`/model` 就只列几个** ——
mpi 没有内置模型目录，也不会去猜。没配置 providers 会直接报错退出。

| 字段 | 说明 |
|---|---|
| `api` | `anthropic-messages` 或 `openai-completions` |
| `base_url` | 缺省按 `api` 取官方地址；任意 OpenAI 兼容网关直接写它即可 |
| `api_key_env` | 读哪个环境变量取 key，缺省为 `<PROVIDER>_API_KEY` |
| `api_key` | 直接写 key（不推荐）；两者都缺时请求会报错 |
| `models[].context_window` | 底栏容量与告警阈值；缺失时显示 `?` |
| `models[].max_tokens` | 单次回复上限，缺省 8192 |
| `models[].reasoning` | 是否支持推理，决定 `/model` 选完要不要问级别 |
| `models[].thinking_levels` | 该模型支持的级别；`reasoning` 为真但缺失时表示五档全支持 |
| `models[].compat` | 覆盖兼容开关，见下 |

思考级别的词汇表只有五档：`low`、`medium`、`high`、`xhigh`、`max`。
写别的值会在启动时报错，不会静默忽略。

### 兼容开关

不同网关对「OpenAI 兼容」的理解不一样。mpi 按 `base_url` 自动探测一组开关，
配置里只写例外（`compat` 可以是 provider 级或 model 级，model 级优先）：

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
`supports_cache_control`、`send_session_affinity`、`supports_long_cache`。

## 启动

```sh
mpi              # 新会话
mpi resume       # 继续最近一次会话（等价于 /resume）
mpi check        # 只检查配置与环境，不进交互
mpi update       # 更新到最新 Release
```

## 发布

推一个 `v*.*.*` 标签就会自动编译并发布 GitHub Release，**tag 就是版本号**：

```sh
git tag v0.1.0
git push origin v0.1.0
```

产物为 `mpi-<版本>-<target>.tar.gz`（内含二进制、README、LICENSE）；校验和由 GitHub 在 Release 页面自行提供。Release 标题就是 tag 本身（如 `v0.1.3`）。
targets：

| target | runner |
|---|---|
| `x86_64-unknown-linux-gnu` | ubuntu-24.04（glibc） |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm（glibc） |
| `x86_64-apple-darwin` | macos-15-intel |
| `aarch64-apple-darwin` | macos-15 |

Linux 二进制依赖 runner 自带的 glibc（当前 2.39），构建摘要里会列出它实际引用到的
最高 `GLIBC_x.y` 符号版本，方便确认需要多新的系统。

`--version` 取的是 tag：工作流把 tag 以 `MPI_BUILD_VERSION` 传入构建（`build.rs` 把它
声明为 rerun 触发条件，避免缓存留下旧值），并在打包前校验 `mpi --version` 与 tag 一致，
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
| `/resume` | 恢复历史会话 |
| `/exit` | 退出 |

快捷键：`Ctrl+O` 展开/收起最近一块工具输出，`Ctrl+C` 清空当前输入行（空行时退出），
`↑`/`↓` 翻输入历史。

## 工具

`read`、`write`、`edit`、`bash`、`grep`、`find`、`ls`。

`bash` 通过 zsh 执行。`grep` / `find` / `ls` 优先用系统的 `rg` / `fd` / `eza`，
没有就回退 `grep` / `find` / `ls`。

工具输出最多保留 2000 行或 50KB，超出时**保留末尾**并把完整输出写进临时文件，
在结果里给出路径。

## 授权

危险操作在执行前弹出贴底的全宽面板，标题固定为「需要用户授权 · 等待确认」。
`↑↓`/`Tab` 切换，「Enter」确认，`a`/`1` 允许本次，`Esc`/`Ctrl+C`/`q`/`n`/`2` 拒绝。
等待没有超时，授权只对当次有效。

**自动放行**：白名单里的简单只读命令（`pwd ls cat head tail wc stat readlink realpath
printf echo true false cut tr du df uname rg grep find sort file sed git`），
其中 `git` 只放行 `status diff log show rev-parse ls-files ls-tree`，
`sed` 只放行纯行范围打印；以及当前项目目录内的普通 `edit` / `write`。

**需要授权**：删除、提权、Git 写操作、网络传输、脚本、重定向、变量或命令替换、
写到项目目录外、敏感路径（`~/.ssh`、`.env*`、`id_rsa`、`~/.pi`、`~/.codex`、`*.pem` 等）、
以及任何未被识别的选项或语法。

路径检查逐参数进行，覆盖 `--file=/path`、`-f/path` 与 `-nf/path`（按 `-n -f /path` 理解）
三种写法，`git show <rev>:<path>` 冒号后的路径也检查。

被拒时返回给模型的文本是 `未获得用户授权，操作未执行（<具体原因>）`，
让模型知道为什么被拦下，而不是反复重试同一条命令。无 UI（headless）时一律拒绝，
绝不静默放行。

## 压缩

长会话靠上下文压缩维持可用。三个触发路径共用一套实现：

- `manual`：`/compact`
- `threshold`：每轮请求前估算用量，超过 `上下文窗口 − 16384` 就先压缩
- `overflow`：上游报溢出后压缩并**重试这一轮**（每轮只重试一次）

保留窗口是最近 20000 token。压缩产出的不是一段摘要字符串，而是一个检查点：
**保留全部用户消息**（意图，体积小且不能丢），丢掉绝大部分助手消息与工具往返
（过程，占了绝大多数体积）。压缩请求本身不会再触发压缩。

底栏在压缩期间显示 `?`，压缩结束后告知压缩前后的 token 量。

## 会话

线性 JSONL，一行一条记录，存在 `~/.local/share/mpi/sessions/`。

记录分类型存放：会话头、对话项、每轮环境快照、事件（含 token 统计）、压缩检查点。
环境快照与对话分开存，所以它不会污染上下文，也不会在压缩时被当成对话处理。

文件只追加，不改写已有字节：会话名是单独的 `session_info` 记录（读时取最后一条），
压缩是新的 `compacted` 检查点。因此单个会话文件的增长上限是
「用户消息总量 + 最近 20000 token + 检查点」，而不是全部历史。

## 设计取舍

- **提示缓存优先**：系统提示词逐轮逐字节一致（不放 cwd、时间、分支、模型名），
  这些每轮固定但会变的信息放在会话首条消息里。工具定义顺序固定，消息用 struct 序列化
  以保证字段顺序稳定。Anthropic 打三个 `cache_control` 断点（系统提示词、最后一个工具、
  最后一条消息），OpenAI 兼容接口发 `prompt_cache_key`。
- **流式 + 就地重绘**：只需要底部一块面板，用 crossterm 手写即可；不引入全屏缓冲模型，
  聊天记录留在终端自己的 scrollback 里。
- **依赖少**：不引入 `toml`（配置是 JSON）、`ratatui`（全屏缓冲模型不匹配）、
  任何编辑器/语法高亮 crate（命令着色是关键字级的手写着色）。禁止 `unsafe`。
