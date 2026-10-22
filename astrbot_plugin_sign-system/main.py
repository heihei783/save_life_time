"""Poll winSignIn and notify an AstrBot session when rollcalls change."""
import asyncio
import json
import logging
import os
import urllib.error
import urllib.request

from astrbot.api.event import AstrMessageEvent, MessageChain, filter
from astrbot.api.message_components import Plain
from astrbot.api.star import Context, Star, register

logger = logging.getLogger("astrbot")


@register("astrbot_signin_monitor", "personal", "Monitor winSignIn rollcalls", "1.4.0")
class SigninMonitor(Star):
    def __init__(self, context: Context, config=None):
        super().__init__(context)
        self.config = config or {}
        self._task = None
        self._seen = {}
        self._has_snapshot = False
        self._pending_notices = []
        self._target_umo = None

    async def initialize(self):
        """AstrBot plugin lifecycle entry point."""
        self._task = asyncio.create_task(self._poll_loop())

    @filter.event_message_type(filter.EventMessageType.ALL)
    async def capture_group_route(self, event: AstrMessageEvent):
        """Remember the framework-provided route from an incoming whitelisted group message."""
        target_group = str(self._get("session_id", "ASTRBOT_SESSION_ID")).strip()
        allowed_groups = self._allowed_groups()
        if not target_group or target_group not in allowed_groups:
            return
        origin = str(getattr(event, "unified_msg_origin", "") or "")
        try:
            event_group_id = str(event.get_group_id() or "")
        except Exception:
            event_group_id = ""
        is_target_group = event_group_id == target_group if event_group_id else (
            origin.endswith(":" + target_group) and "group" in origin.lower()
        )
        if not is_target_group or not origin:
            return
        self._target_umo = origin

    @filter.command("ping")
    async def ping(self, event: AstrMessageEvent):
        """Check this plugin and the winSignIn API from an allowed group."""
        try:
            group_id = str(event.get_group_id() or "").strip()
        except Exception:
            group_id = ""
        if not group_id or group_id not in self._allowed_groups():
            return

        try:
            rollcalls, snapshot_complete = await asyncio.to_thread(self._fetch_rollcalls)
            active = [item for item in rollcalls if not self._is_expired(item)]
            account_count = sum(len(self._account_map(item)) for item in rollcalls)
            if snapshot_complete is True:
                completeness = "完整"
            elif snapshot_complete is False:
                completeness = "不完整（请检查 winSignIn 服务和账号会话）"
            else:
                completeness = "服务端未提供完整性标记（请更新 winSignIn 服务端）"
            result = (
                "🏓 签到监听插件在线\n"
                "winSignIn API：连接成功\n"
                f"签到记录：{len(rollcalls)} 条，进行中 {len(active)} 条\n"
                f"本系统账号状态记录：{account_count} 条\n"
                f"点名快照：{completeness}"
            )
        except urllib.error.HTTPError as exc:
            if exc.code in (401, 403):
                reason = f"winSignIn API 鉴权失败（HTTP {exc.code}），请检查固定 API token"
            else:
                reason = f"winSignIn API 返回 HTTP {exc.code}"
            result = "⚠️ 插件在线，但 " + reason
        except urllib.error.URLError as exc:
            result = f"⚠️ 插件在线，但无法连接 winSignIn：{exc.reason}"
        except Exception as exc:
            logger.warning("/ping 检查失败：%s", exc)
            result = "⚠️ 插件在线，但健康检查失败，请查看 AstrBot 日志"

        yield event.plain_result(result)

    def _allowed_groups(self):
        raw = str(self._get("allowed_groups", "ASTRBOT_ALLOWED_GROUPS"))
        return {x.strip() for x in raw.replace("，", ",").split(",") if x.strip()}

    async def terminate(self):
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    def _get(self, key, env_key, default=""):
        return self.config.get(key) or os.getenv(env_key, default)

    def _fetch_rollcalls(self):
        base = self._get("base_url", "WINSIGNIN_URL", "http://127.0.0.1:1112").rstrip("/")
        token = str(self._get("api_token", "WINSIGNIN_API_TOKEN")).strip()
        if not token:
            raise RuntimeError("请配置固定的 winSignIn API token（需与服务端 WINSIGNIN_API_TOKEN 一致）")
        request = urllib.request.Request(base + "/api/rollcalls", headers={"Authorization": "Bearer " + token})
        with urllib.request.urlopen(request, timeout=15) as response:
            data = json.loads(response.read().decode("utf-8"))
            complete_header = response.headers.get("X-Rollcalls-Complete")
            snapshot_complete = None if complete_header is None else complete_header.lower() == "true"
        if not isinstance(data, list):
            raise RuntimeError("winSignIn 返回格式异常")
        return data, snapshot_complete

    def _started_notice(self, item):
        course = item.get("course_title") or "课程未知"
        title = item.get("title") or "签到"
        accounts_text = self._format_accounts(item)
        return (
            f"📣 老师开始签到：{course} / {title}\n"
            f"本系统账号当前状态：\n{accounts_text}\n"
            "请核对未完成签到的账号。"
        )

    def _ended_notice(self, item):
        course = item.get("course_title") or "课程未知"
        title = item.get("title") or "签到"
        accounts_text = self._format_accounts(item)
        return (
            f"✅ 签到已结束：{course} / {title}\n"
            f"时间：{item.get('rollcall_time') or '未知'}\n"
            f"本系统账号签到情况：\n{accounts_text}"
        )

    async def _poll_loop(self):
        try:
            interval = max(10, int(self._get("interval", "WINSIGNIN_POLL_SECONDS", "20")))
        except (TypeError, ValueError):
            interval = 20
        while True:
            try:
                rollcalls, snapshot_complete = await asyncio.to_thread(self._fetch_rollcalls)
                if not snapshot_complete:
                    logger.warning("winSignIn 点名快照不完整，本轮不更新状态以避免误报结束")
                    await asyncio.sleep(interval)
                    continue
                current = {str(item.get("id")): item for item in rollcalls if item.get("id")}
                notices = []
                if not self._has_snapshot:
                    # On startup, report currently active calls so a plugin restart does
                    # not silently miss a teacher's already-started rollcall.
                    for item in current.values():
                        if not self._is_expired(item):
                            notices.append(self._started_notice(item))
                    self._seen = current
                    self._has_snapshot = True
                else:
                    for rid, item in current.items():
                        old = self._seen.get(rid)
                        if old is None:
                            if not self._is_expired(item):
                                notices.append(self._started_notice(item))
                            continue
                        if not self._is_expired(old) and self._is_expired(item):
                            notices.append(self._ended_notice(item))
                            continue
                        changes = self._status_changes(old, item)
                        if changes:
                            course = item.get("course_title") or "课程未知"
                            title = item.get("title") or "签到"
                            notices.append(
                                f"🔔 签到状态更新：{course} / {title}\n"
                                + "\n".join(changes)
                            )
                    # Some LMS responses omit completed calls instead of returning an
                    # explicit is_expired=true record. Only infer end from disappearance
                    # after the server confirms every enabled account was queried.
                    if snapshot_complete is True:
                        for rid, old in self._seen.items():
                            if rid not in current and not self._is_expired(old):
                                notices.append(self._ended_notice(old))
                    self._seen = current
                self._pending_notices.extend(notices)
                still_pending = []
                for text in self._pending_notices:
                    try:
                        if not await self._notify(text):
                            still_pending.append(text)
                    except Exception:
                        still_pending.append(text)
                        raise
                self._pending_notices = still_pending
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                logger.warning("签到监听失败：%s", exc)
            await asyncio.sleep(interval)

    @staticmethod
    def _is_expired(item):
        value = item.get("is_expired", False)
        if isinstance(value, bool):
            return value
        return str(value).strip().lower() in {"1", "true", "yes"}

    @staticmethod
    def _account_map(item):
        accounts = item.get("accounts")
        if isinstance(accounts, list):
            return {
                str(account.get("account_id") or account.get("name") or index): account
                for index, account in enumerate(accounts)
                if isinstance(account, dict)
            }
        # Backwards-compatible handling for the old flat /api/rollcalls response.
        if "status" in item or "account_name" in item or "name" in item:
            return {"legacy": {
                "name": item.get("account_name") or item.get("name") or "本系统账号",
                "status": item.get("status", ""),
            }}
        return {}

    @staticmethod
    def _status_text(status):
        value = str(status or "").strip()
        return {"on_call_fine": "已签到", "absent": "未到"}.get(value, value or "未知")

    def _format_accounts(self, item):
        accounts = self._account_map(item)
        if not accounts:
            return "未获取到账号状态"
        lines = []
        for account in accounts.values():
            name = account.get("name") or "未命名账号"
            status = self._status_text(account.get("status"))
            lines.append(f"• {name}：{status}")
        return "\n".join(lines)

    def _status_changes(self, old, current):
        before = self._account_map(old)
        after = self._account_map(current)
        changes = []
        for account_id, account in after.items():
            previous = before.get(account_id)
            if not previous:
                continue
            old_status = str(previous.get("status") or "")
            new_status = str(account.get("status") or "")
            if new_status and old_status != new_status:
                name = account.get("name") or previous.get("name") or "未命名账号"
                changes.append(
                    f"• {name}：{self._status_text(old_status)} → {self._status_text(new_status)}"
                )
        return changes

    async def _notify(self, text):
        # AstrBot routes via the unified origin captured from an incoming message in the
        # configured whitelisted group. No platform instance ID needs to be entered.
        session_id = str(self._get("session_id", "ASTRBOT_SESSION_ID")).strip()
        message_type = self._get("message_type", "ASTRBOT_MESSAGE_TYPE", "group")
        allowed_groups = self._allowed_groups()
        if not allowed_groups:
            logger.warning("未配置群白名单，已阻止签到通知发送")
            return True
        if message_type != "group" or not session_id or session_id not in allowed_groups:
            logger.warning("目标不是白名单群，已阻止签到通知发送 (session_id=%s)", session_id)
            return True
        target_umo = self._target_umo
        if not target_umo:
            logger.warning("尚未捕获目标群会话，签到通知暂存；请先在白名单群内发送一条消息给机器人")
            return False
        chain = MessageChain()
        chain.chain.append(Plain(text))
        await self.context.send_message(target_umo, chain)
        return True
