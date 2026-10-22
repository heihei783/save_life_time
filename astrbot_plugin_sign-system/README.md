# AstrBot 签到监听插件

本目录单独存放本项目配套的个人 AstrBot 插件，兼容 AstrBot 已接入的 OneBot v11/v12 平台（群消息由 AstrBot 路由发送）。它定时读取 winSignIn 的 `/api/rollcalls`，发现新签到、签到状态变化或签到过期后向指定白名单群推送通知。

## 安装与依赖

把整个 `astrbot_plugin_sign-system/` 插件目录复制到 AstrBot 配置的插件目录中（目录内直接包含 `main.py`、`metadata.yaml`、`_conf_schema.json`），然后在 AstrBot 插件管理中加载/启用。加载后打开该插件卡片的齿轮/配置按钮，即可在 Web 管理界面编辑下列配置并保存；若已有同名旧插件，请先更新整个插件目录并重新加载/重启插件，使配置表单 schema 生效。无需额外安装第三方 Python 包，运行环境需已安装 AstrBot。插件使用 AstrBot 自带 SDK 与 OneBot 平台通信。

## Web 配置表单

`_conf_schema.json` 定义 Web 表单：winSignIn 服务地址、固定 API token、目标群号、群白名单、消息类型和轮询间隔。无需填写 OneBot 平台实例 ID：插件从 AstrBot 在目标白名单群收到的消息中自动获取会话路由；启用并保存配置后，请先在目标群发一条消息。

插件监听新签到、状态变化和签到结束，通知附上课程名、签到标题，以及本项目中对应账号的姓名和个人状态。首次成功轮询时也会报告当时仍进行中的签到。签到结束支持接口明确返回过期，或新版服务端确认完整快照后观察到记录消失。请先更新并重建 winSignIn 服务端，再更新插件；新版 `/api/rollcalls` 才会返回 `accounts: [{account_id, name, status}]` 和 `X-Rollcalls-Complete` 完整性标记。

**固定 API token 只需配置一次，不会随网页登录改变。** 先在运行 winSignIn 的服务器 `.env` 中设置 `WINSIGNIN_API_TOKEN`，重建/重启容器后，再把同一个值填入 AstrBot 配置里的“固定 API token”。它只授权读取 `/api/rollcalls`，不允许管理账号或执行签到。

**群白名单为强制安全校验**：`allowed_groups` 填入允许推送的群号，多个群号以英文逗号分隔（例如 `123456,234567`）。只有 `session_id` 目标群也出现在白名单中才会发送；白名单为空、群号不匹配或消息类型不是 `group` 均拒绝推送。

## 插件指令

在群内发送 `/ping` 可检查插件是否运行、winSignIn API 是否可连接、当前签到记录数量，以及点名快照是否完整。为了避免未授权群获取服务状态，该指令仅在 `allowed_groups` 中的群响应；白名单外不回复。鉴权或连接失败时会给出简要排查提示，不会回显 API token。

- `WINSIGNIN_API_TOKEN`：固定只读 API token，必须与 winSignIn 服务器 `.env` 中的值相同；此插件凭证不会随管理网页登录变化。配置后只授权读取 `/api/rollcalls`。
- `ASTRBOT_SESSION_ID`：目标群号
- `ASTRBOT_ALLOWED_GROUPS`：**必填群白名单**，群号以英文逗号分隔，例如 `123456,234567`（也接受中文逗号）。插件只会给同时满足白名单和 `ASTRBOT_SESSION_ID` 的群推送；白名单为空、群号不匹配或消息类型不是群聊时一律拒绝发送。目标群内至少需要有一条消息被 AstrBot 收到，插件才能自动捕获该群的会话路由。
- `ASTRBOT_MESSAGE_TYPE`：须为 `group`，默认 `group`
- `WINSIGNIN_POLL_SECONDS`：轮询间隔秒数，默认 20，最低 10

插件配置对象字段为：`base_url`、`api_token`、`session_id`、`allowed_groups`、`message_type`、`interval`。无需填写 `platform_id`，插件会使用 AstrBot 提供的消息来源路由。

## 当前能力边界

winSignIn 的 `/api/rollcalls` 返回的是**本系统已启用账号**的点名记录；通知中姓名和状态只覆盖这些账号，不是课程全班花名册，不能列出未添加到本系统的同学。首次成功轮询会提示当时仍在进行的签到，状态变化和结束提醒根据后续完整轮询结果判断。

建议将 bot 放在私有群使用，并保证服务端口和 token 不暴露到公网。
