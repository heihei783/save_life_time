# winSignIn Web

重庆邮电大学畅课签到网页服务。项目已经改为 **Linux + Docker + 浏览器** 架构，不再包含 Android/Kotlin 客户端。

## 功能

- 单用户管理口令保护
- 重邮 CAS 账号密码登录、会话持久化和 CAS 会话刷新
- 多账号保存与批量操作
- 点名列表
- 手机浏览器摄像头二维码签到，也支持粘贴二维码原文
- 数字签到
- 签到设置：自动提取数字码、雷达空答案/三点定位、探测点持久化、批量账号错峰间隔
- 原有 Rust 二维码解析、登录链和签到接口逻辑复用

## 技术栈

- Rust：Axum Web API + 原有 reqwest/CAS/二维码/签到核心
- 浏览器：原生 HTML/JavaScript、`BarcodeDetector` 摄像头扫码
- 部署：Docker Compose，`/data` 持久化账号会话

## 目录

```text
rust/src/cas.rs          CAS 登录和会话处理
rust/src/qr.rs           畅课二维码解析
rust/src/radar.rs        数字码解析、雷达数学计算
rust/src/lib.rs          登录、点名、二维码/数字/位置签到核心
rust/src/bin/server.rs   Linux HTTP 服务端
web/index.html           浏览器界面
Dockerfile               多阶段镜像构建
docker-compose.yml       服务器部署配置
DOCKER.md                部署和安全说明
```

## 部署

服务器安装 Docker Compose v2 后执行：

```bash
cp .env.example .env
# 编辑 .env，设置至少 32 位随机 ADMIN_PASSWORD 和 WINSIGNIN_API_TOKEN
# AstrBot 插件配置使用同一个 WINSIGNIN_API_TOKEN；它不会随网页登录变化
docker compose up -d --build
```

主站 `sign.xiaohundun.cc` 经 HTTPS 反向代理转发到本机 `127.0.0.1:1112`；认证页 `cqupt.xiaohundun.cc` 转发到 `127.0.0.1:1113`。生产环境摄像头及 Secure Cookie 都要求 HTTPS。

健康检查地址：`/health`。

## 重要限制

- 自动 CAS 登录失败时，可通过独立认证子域打开学校原认证页完成验证码/短信步骤；代理会话保存在服务内存且超时清理。
- 网页端位置签到提供三点探测、三边定位、目标坐标提交和批量签到流程。
- 上游学校接口仍使用项目原有的 HTTP 地址，这是上游协议限制；请至少用 HTTPS 保护浏览器到本服务的链路。
- `/data/state.json` 包含 LMS/CAS 会话信息，必须限制服务器权限并做好备份保护。

## License

[MIT](LICENSE)
