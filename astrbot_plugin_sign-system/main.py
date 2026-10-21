"""Poll winSignIn and notify an AstrBot session when rollcalls change."""
import asyncio
import json
import logging
import os
import urllib.request

from astrbot.api.event import AstrMessageEvent, MessageChain, filter
from astrbot.api.message_components import Plain
from astrbot.api.star import Context, Star, register

logger = logging.getLogger("astrbot")


@register("astrbot_signin_monitor", "personal", "Monitor winSignIn rollcalls", "1.2.0")
class SigninMonitor(Star):
    def __init__(self, context: Context, config=None):
        super().__init__(context)
        self.config = config or {}
        self._task = None
        self._seen = {}
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
        if not isinstance(data, list):
            raise RuntimeError("winSignIn 返回格式异常")
        return data

    async def _poll_loop(self):
        try:
            interval = max(10, int(self._get("interval", "WINSIGNIN_POLL_SECONDS", "20")))
        except ValueError:
            interval = 20
        while True:
            try:
                rollcalls = await asyncio.to_thread(self._fetch_rollcalls)
                current = {str(item.get("id")): item for item in rollcalls if item.get("id")}
                notices = []
                for rid, item in current.items():
                    old = self._seen.get(rid)
                    title = item.get("title") or "签到"
                    course = item.get("course_title") or "课程未知"
                    if old is None and not self._seen:
                        continue  # first snapshot: don't produce historical alerts
                    if old is None and not item.get("is_expired", False):
                        notices.append(f"📣 检测到签到：{course} / {title}\n状态：{item.get('status', '未知')}\n请需要签到的账号尽快处理。")
                    elif old and not old.get("is_expired", False) and item.get("is_expired", False):
                        notices.append(f"✅ 签到已结束：{course} / {title}\n时间：{item.get('rollcall_time', '未知')}\n已更新本次签到状态。")
                    elif old and old.get("status") != item.get("status") and item.get("status"):
                        notices.append(f"🔔 签到状态更新：{course} / {title}\n{old.get('status')} → {item.get('status')}")
                self._seen = current
                for text in notices:
                    await self._notify(text)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                logger.warning("签到监听失败：%s", exc)
            await asyncio.sleep(interval)

    async def _notify(self, text):
        # AstrBot routes via the unified origin captured from an incoming message in the
        # configured whitelisted group. No platform instance ID needs to be entered.
        session_id = str(self._get("session_id", "ASTRBOT_SESSION_ID")).strip()
        message_type = self._get("message_type", "ASTRBOT_MESSAGE_TYPE", "group")
        allowed_groups = self._allowed_groups()
        if not allowed_groups:
            logger.warning("未配置群白名单，已阻止签到通知发送")
            return
        if message_type != "group" or not session_id or session_id not in allowed_groups:
            logger.warning("目标不是白名单群，已阻止签到通知发送 (session_id=%s)", session_id)
            return
        target_umo = self._target_umo
        if not target_umo:
            logger.warning("尚未捕获目标群会话，请先在白名单群内发送一条消息给机器人")
            return
        chain = MessageChain()
        chain.chain.append(Plain(text))
        await self.context.send_message(target_umo, chain)
