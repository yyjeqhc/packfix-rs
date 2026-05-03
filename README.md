# packfix-rs

packfix-rs 是一个用于 Python RPM 包自动构建和自动修复的 Rust 命令行工具。

它面向 OBS / OpenRuyi 风格的软件包维护流程，可以自动完成：

- 生成或复用 Python 包 spec
- 提交到 OBS / EBF
- 本地 osc build
- 分析构建日志
- 自动修改 spec
- 重试构建
- 多包并发调度
- 本地多个 buildroot 并发构建
- 使用 LLM 自动生成 RPM spec %description

当前主要用于 Python 包打包修复场景。


## 主要功能

- 支持从 PyPI 包名构建 Python RPM 包
- 支持构建仓库中已经存在的包
- 支持对本地 workdir 执行 fix 循环
- 支持一次输入多个包
- 支持依赖图调度
- 支持本地最多 5 个独立 buildroot 并发构建
- 支持远程 OBS 状态轮询
- 支持远程失败后回退到本地构建
- 支持分析构建日志并自动修复常见问题
- 支持解压源码包并提取 README / PKG-INFO / pyproject.toml 等信息
- 默认使用 Ollama LLM 自动生成并替换 spec 中的 %description
- 构建过程会生成 report 和 operations log，方便排查


## 依赖

需要系统中已经安装并配置好：

- Rust
- osc
- takopack
- git
- OBS 账号与 oscrc
- Ollama 或兼容 Ollama API 的服务

推荐模型示例：

    qwen3:8b


## 配置文件

packfix-rs 会读取：

    packfix.toml

或者：

    ~/.config/packfix/config.toml

示例配置：

    [obs]
    api_url = "https://pickaxe.oerv.ac.cn/"
    default_project = "home:yourname:test"
    oscrc_path = "/root/.config/osc/oscrc"

    [repo]
    url = "https://github.com/yourname/openruyi/"
    workdir = "/root/git/openruyi-repo"

    [llm]
    host = "http://100.65.29.50"
    port = 11434
    model = "qwen3:8b"

    [description]
    system_prompt = "You generate RPM spec file %description bodies for Python packages. Use only the provided package metadata, README excerpt, and module list. Do not invent unsupported facts. Output plain English text only. Do not output Markdown, bullets, headings, quotes, or the '%description' tag. Write in a concise downstream packaging style, not in marketing style."

    user_prompt = """
    Task: Generate RPM spec %description body.

    Extracted package information:
    {context}

    Hard requirements:
    - English only.
    - Output 1 to 5 lines.
    - Each line must be no longer than 100 characters.
    - Plain text only.
    - Do not include "%description".
    - Do not include a package name/version heading.
    - Do not mention sources, metadata, README, uncertainty, classifiers, or license.
    - Prefer concrete capabilities over broad claims.
    - Describe what the package provides and what it is used for.
    - The result should be suitable for direct insertion under an RPM spec %description.
    """

    timeout_secs = 180
    max_context_chars = 12000
    num_predict = 160
    temperature = 0

    [build]
    repository = "x64"
    arch = "x86_64"
    max_retries = 3
    max_dep_depth = 3


查看最终生效配置：

    cargo run -- config-show

JSON 格式查看配置：

    cargo run -- config-show --json


## 基本用法

从 PyPI 包名构建：

    cargo run -- build questionary

指定版本：

    cargo run -- build questionary --version 2.1.1

一次构建多个包：

    cargo run -- build questionary authlib fontmake

构建仓库中已经存在的包：

    cargo run -- build-existing python-questionary

一次构建多个已有包：

    cargo run -- build-existing python-questionary python-authlib python-fontmake

对本地目录执行修复：

    cargo run -- fix /path/to/python-package

对多个本地目录执行修复：

    cargo run -- fix /path/to/pkg1 /path/to/pkg2

分析构建日志：

    cargo run -- analyze-log /path/to/build.log

checkout OBS 包：

    cargo run -- checkout home:yourname:test --package python-questionary

更新本地 checkout 包：

    cargo run -- update /path/to/python-questionary

查看远程 OBS 构建状态：

    cargo run -- status home:yourname:test python-questionary


## 默认 LLM description 生成

packfix-rs 默认会使用 LLM 生成并替换 RPM spec 中的 %description。

流程大致是：

    osc checkout / osc up -S
        -> 解压源码包
        -> 读取 PKG-INFO / METADATA / pyproject.toml / setup.cfg / setup.py / README
        -> 构造结构化 JSON context
        -> 调用 Ollama chat API
        -> 生成英文 %description
        -> 清洗输出格式
        -> 替换 spec 中的 %description
        -> 开始 osc build
        -> 分析日志
        -> 自动修复
        -> 重试构建

LLM 失败不会导致构建失败。

如果 LLM 超时、不可用、返回空字符串，packfix-rs 会保留原来的 %description，然后继续构建。


## 单独生成 description

可以对任意源码目录单独生成 description：

    cargo run -- describe /path/to/source

输出到文件：

    cargo run -- describe /path/to/source --output description.txt

示例输出：

    Compile fonts from UFO, Glyphs, and designspace files into OpenType and TrueType binary formats.
    Supports variable fonts and static instances creation from source files.
    Provides command line tools for font generation and output format selection.

这个输出可以直接放进 RPM spec 的 %description 字段。


## 源码解压位置

checkout 或 update 后，packfix-rs 会在包目录中查找源码压缩包，例如：

    *.tar.gz
    *.tar.xz
    *.tar.bz2
    *.tgz
    *.zip

源码不会解压到 OBS checkout 目录里，而是放到 packfix 自己的 workspace 中：

    workspaces/<package>/.packfix/source-extract/<archive-stem>/

状态文件记录在：

    workspaces/<package>/.packfix/state.json

这样可以避免污染 osc build 的工作目录。


## 本地 buildroot 并发

packfix-rs 在一个进程内最多支持 5 个本地 buildroot slot。

路径类似：

    /var/tmp/build-root/packfix-1-x64-x86_64
    /var/tmp/build-root/packfix-2-x64-x86_64
    /var/tmp/build-root/packfix-3-x64-x86_64
    /var/tmp/build-root/packfix-4-x64-x86_64
    /var/tmp/build-root/packfix-5-x64-x86_64

多个本地 osc build 不会共享同一个 buildroot。

注意：

    当前 buildroot slot 只在同一个 packfix-rs 进程内互斥。
    如果同时启动多个 packfix-rs 进程，它们之间还没有跨进程锁。


## LLM 并发策略

LLM description 生成默认串行执行。

也就是说：

    本地 osc build 最多 5 个并发
    LLM description 同一时间最多 1 个
    LLM description 不占用 buildroot slot

这样可以避免多个包同时请求 Ollama，导致模型响应变慢或显存压力过大。


## 自动修复能力

packfix-rs 会分析 osc build 日志，并尝试自动修复常见 Python RPM 构建问题，包括：

- 缺少 BuildRequires
- 缺少 pyproject 构建后端依赖
- 缺少 Python 模块依赖
- import check 失败
- import check 为空
- installed but unpackaged files
- noarch 包中包含架构相关文件
- PEP 639 license metadata 问题
- install module 名称不匹配
- 部分 patch / test / C extension 编译错误识别

如果问题不适合自动修复，会标记为 NeedHuman。


## 日志和报告

每个包会生成独立 workspace。

示例：

    workspaces/python-questionary/

常见日志：

    workspaces/python-questionary/logs/packfix_operations.log
    workspaces/python-questionary/home:yourname:test/python-questionary/logs/build_attempt_001.log
    workspaces/python-questionary/home:yourname:test/python-questionary/logs/build_attempt_002.log

成功报告示例：

    package: python-questionary
    status: BuildSuccess
    build_attempts: 4
    fixes_applied: 3
    last_log: workspaces/python-questionary/home:yourname:test/python-questionary/logs/build_attempt_004.log
    operations_log: workspaces/python-questionary/logs/packfix_operations.log
    note: llm-description updated
    note: local build succeeded after 4 attempts, 3 fixes applied

使用 JSON 输出：

    cargo run -- build questionary --json


## 推荐工作流

构建一个 PyPI 包：

    cargo run -- build questionary

构建已有包：

    cargo run -- build-existing python-questionary

修复本地 checkout 包：

    cargo run -- fix workspaces/python-questionary/home:yourname:test/python-questionary

查看 description 是否被替换：

    grep -A8 -n '^%description' workspaces/python-questionary/home:yourname:test/python-questionary/python-questionary.spec

查看操作日志：

    cat workspaces/python-questionary/logs/packfix_operations.log


## 注意事项

- 默认会使用 LLM 更新 %description。
- 当前只自动更新 %description，不自动更新 Summary。
- LLM 调用失败不会导致构建失败。
- 多包构建时，本地 buildroot 最多 5 个并发。
- LLM description 生成是串行的。
- 当前 buildroot slot 只在单进程内互斥。
- 跨进程 buildroot 锁还没有实现。
- 如果远程 OBS 状态无法及时判断，会回退到本地 osc build。
- 如果遇到非确定性问题，会停止自动修复并提示人工处理。


## 常用命令速查

查看配置：

    cargo run -- config-show

生成 description：

    cargo run -- describe /path/to/source

构建 PyPI 包：

    cargo run -- build questionary

构建已有包：

    cargo run -- build-existing python-questionary

修复本地包：

    cargo run -- fix /path/to/python-questionary

分析日志：

    cargo run -- analyze-log /path/to/build.log

查看远程状态：

    cargo run -- status home:yourname:test python-questionary