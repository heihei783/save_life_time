# Docker 网页部署

本项目现在是 Linux 服务端项目。Docker 镜像只包含 Rust HTTP 服务和浏览器网页，不包含 Android/Kotlin/Compose/CameraX 组件。

## 快速部署

服务器安装 Docker Compose v2 后：

```bash
cp .env.example .env
# 编辑 .env，设置随机 ADMIN_PASSWORD 和固定 WINSIGNIN_API_TOKEN
docker compose up -d --build
```

给 AstrBot 插件使用的 `WINSIGNIN_API_TOKEN` 与管理网页会话 token 相互独立，不会在网页登录时更换。生成随机值的示例：

```bash
openssl rand -hex 32
```

将生成值写入服务器 `.env`：

```dotenv
WINSIGNIN_API_TOKEN=这里填入生成的随机值
```

同一个值只需再填一次到 AstrBot 插件 Web 配置的“固定 API token”字段。修改 `.env` 后执行 `docker compose up -d --force-recreate winsignin` 让容器读取新环境变量。此 token 仅授权读取 `/api/rollcalls`，不授权管理及签到操作；请勿提交到 Git 或公开。

容器端口映射到服务器本机 `127.0.0.1:1112`（主站）和 `127.0.0.1:1113`（CAS 代理）。用 HTTPS 反向代理将两个域名分别转发到对应端口；不要将这两个 HTTP 端口直接暴露到公网。

## HTTPS 反向代理示例（Nginx）

先确认 `sign.xiaohundun.cc` 和 `cqupt.xiaohundun.cc` 的 DNS 都指向此服务器，且公网 80/443 端口可访问。在 Debian/Ubuntu 上安装并申请覆盖两个域名的证书：

```bash
sudo apt update
sudo apt install -y certbot python3-certbot-nginx
sudo certbot --nginx -d sign.xiaohundun.cc -d cqupt.xiaohundun.cc
```

Certbot 会为这两个域名签发同一张证书，默认证书目录为 `/etc/letsencrypt/live/sign.xiaohundun.cc/`。原有 `/etc/letsencrypt/live/chat.xiaohundun.cc/` 证书只有在证书 SAN 确实包含这两个域名（或 `*.xiaohundun.cc`）时才能复用；不能只凭目录名判断。

Nginx 对外监听 HTTPS，再按域名分别转发到本机端口：

```nginx
server {
    listen 443 ssl;
    server_name sign.xiaohundun.cc;
    ssl_certificate     /etc/letsencrypt/live/sign.xiaohundun.cc/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sign.xiaohundun.cc/privkey.pem;

    location / {
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_pass http://127.0.0.1:1112;
    }
}

server {
    listen 443 ssl;
    server_name cqupt.xiaohundun.cc;
    ssl_certificate     /etc/letsencrypt/live/sign.xiaohundun.cc/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sign.xiaohundun.cc/privkey.pem;

    location / {
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_pass http://127.0.0.1:1113;
    }
}
```

Certbot 的 Nginx 插件可能会调整 Nginx 配置。申请后检查并重载 Nginx；续期可用 `sudo certbot renew --dry-run` 检查。防火墙开放 80/443，保持 1112/1113 仅供本机反向代理访问。Compose 中 `CAS_PROXY_PUBLIC_URL` 与 `MAIN_ORIGIN` 已按上述域名设置。

## 生产环境必须做的事

1. **使用 HTTPS 反向代理**（Nginx/Caddy/Traefik）。浏览器摄像头权限通常要求 HTTPS；不要直接把 1112/1113 作为明文 HTTP 暴露到公网。
2. `ADMIN_PASSWORD` 是整站唯一访问口令，必须使用随机长口令，不要提交 `.env`。
3. `/data` 保存学校会话 Cookie、CAS Cookie 和设备标识，务必备份并限制服务器文件权限。
4. 上游学校接口当前包含明文 HTTP 地址，这是原项目既有依赖；公网部署仍建议通过 HTTPS 反代保护浏览器到本服务这一段，并注意上游链路风险。
5. CAS 网页登录通过独立认证子域代理学校认证页面。此代理仅允许 `identity.tc.cqupt.edu.cn`、`ids.cqupt.edu.cn` 和 LMS 白名单，并由主站 Origin 校验限制管理 API。

## 功能说明

- 学校网页登录：管理页通过独立 HTTPS 子域代理学校原认证页面；可在学校页面完成密码、图片验证码和短信验证，服务端检测回到 LMS 后保存 Cookie 会话。
- 数字签到：手动输入 4 位签到码，或在设置中启用自动从点名接口取码。
- 位置签到：设置三个探测点后执行三边定位并批量提交；也支持空答案策略或手动输入经纬度。
- 设置：探测点、雷达策略、自动取码和账号签到错峰间隔会持久化保存。
- 手机扫码：使用 jsQR 对摄像头视频逐帧解码，并支持选择二维码图片或粘贴原文；识别库按需从 jsDelivr 加载并以 unpkg 备用。摄像头访问要求 HTTPS 和浏览器授权。

## 配置文件

`.env` 示例：

```dotenv
ADMIN_PASSWORD=请替换为至少32位随机管理口令
WINSIGNIN_API_TOKEN=请替换为至少64位十六进制随机值（给 AstrBot 插件使用）
```
