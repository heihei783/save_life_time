# AstrBot 签到监听插件

本目录单独存放本项目配套的个人 AstrBot 插件，兼容 AstrBot 已接入的 OneBot v11/v12 平台（群消息由 AstrBot 路由发送）。它定时读取 winSignIn 的 `/api/rollcalls`，发现新签到、签到状态变化或签到过期后向指定白名单群推送通知。

## 安装与依赖

把整个 `astrbot_plugin_sign-system/` 插件目录复制到 AstrBot 配置的插件目录中（目录内直接包含 `main.py`、`metadata.yaml`、`_conf_schema.json`），然后在 AstrBot 插件管理中加载/启用。加载后打开该插件卡片的齿轮/配置按钮，即可在 Web 管理界面编辑下列配置并保存；若已有同名旧插件，请先更新整个插件目录并重新加载/重启插件，使配置表单 schema 生效。无需额外安装第三方 Python 包，运行环境需已安装 AstrBot。插件使用 AstrBot 自带 SDK 与 OneBot 平台通信。

## Web 配置表单

新增的 `_conf_schema.json` 定义了 Web 配置表单字段：winSignIn 服务地址、固定 API token、目标群号、群白名单、消息类型和轮询间隔。无需填写 OneBot 平台实例 ID：插件会从 AstrBot 在目标白名单群收到的消息中自动获取会话路由。启用插件并保存配置后，请先在目标群内发送一条消息，让插件捕获群会话；之后再由插件自动推送。

**固定 API token 只需配置一次，不会随网页登录改变。** 先在运行 winSignIn 的服务器 `.env` 中设置 `WINSIGNIN_API_TOKEN`，重建/重启容器后，再把同一个值填入 AstrBot 配置里的“固定 API token”。它只授权读取 `/api/rollcalls`，不允许管理账号或执行签到。

**群白名单为强制安全校验**：`allowed_groups` 填入允许推送的群号，多个群号以英文逗号分隔（例如 `123456,234567`）。只有 `session_id` 目标群也出现在白名单中才会发送；白名单为空、群号不匹配或消息类型不是 `group` 均拒绝推送。

## 配置

在插件配置或 AstrBot 进程环境变量中设置：

- `WINSIGNIN_API_TOKEN`：固定只读 API token，必须与 winSignIn 服务器 `.env` 中的值相同；此插件凭证不会随管理网页登录变化。配置后只授权读取 `/api/rollcalls`。
- `ASTRBOT_SESSION_ID`：目标群号
- `ASTRBOT_ALLOWED_GROUPS`：**必填群白名单**，群号以英文逗号分隔，例如 `123456,234567`（也接受中文逗号）。插件只会给同时满足白名单和 `ASTRBOT_SESSION_ID` 的群推送；白名单为空、群号不匹配或消息类型不是群聊时一律拒绝发送。目标群内至少需要有一条消息被 AstrBot 收到，插件才能自动捕获该群的会话路由。
- `ASTRBOT_MESSAGE_TYPE`：须为 `group`，默认 `group`
- `WINSIGNIN_POLL_SECONDS`：轮询间隔秒数，默认 20，最低 10

插件配置对象字段为：`base_url`、`api_token`、`session_id`、`allowed_groups`、`message_type`、`interval`。无需填写 `platform_id`，插件会使用 AstrBot 提供的消息来源路由。

## 当前能力边界

winSignIn 的 `/api/rollcalls` 目前返回的是**本系统已登录账号**的点名记录，且仅有每个账号自己的 `status`，不是课程全班花名册。因而通知能报告本系统账号观察到的签到状态变化，不能准确列出全班“哪些人需要签到/哪些同学已完成”。若需要人员名单，需从畅课接口额外取得班级学生名单及每位学生的点名状态，并扩展 winSignIn API 输出；本插件不会伪造这类数据。首次启动只建立基线，不推送历史签到。

建议将 bot 放在私有群使用，并保证服务端口和 token 不暴露到公网。
