#!/usr/bin/env node
/**
 * 二维码协议探测工具：验证「一张码能不能跨课程签到」。
 *
 * 背景：
 *   签到请求是  PUT /api/rollcall/{rollcallId}/answer_qr_rollcall
 *   body        { "data": "<时间戳+签名>", "deviceId": "..." }
 *   —— rollcallId 在 URL 里，data 在 body 里，而 data 内部不含 rollcallId。
 *   所以能不能跨课程，取决于服务端重算签名时用不用 URL 里的 rollcallId。
 *   这只能实测，本工具就是干这个的。
 *
 * 用法：
 *   set ADMIN_PASSWORD=你的管理口令
 *   node tools/qr-probe.js --qr "<A课的二维码原文>" --target <B课的rollcallId> --account <账号id>
 *
 *   # 不知道 rollcallId？先列出来：
 *   node tools/qr-probe.js --list
 *
 * 判读：
 *   upstream_code 200 且 body 里 status 是 on_call/on_call_fine  → 跨课程成功，签名不绑定 rollcallId
 *   upstream_code 400/403 且提到 data/invalid/expired/rollcall     → 绑定了，跨课程不可能
 *   其他错误按 body 原话判断
 */

const { inspect } = require('./qr-inspect.js');

const BASE = process.env.WINSIGNIN_URL || 'http://127.0.0.1:1112';
const PASSWORD = process.env.ADMIN_PASSWORD || '';

function argOf(name) {
  const i = process.argv.indexOf('--' + name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}
const hasFlag = (name) => process.argv.includes('--' + name);

async function login() {
  if (!PASSWORD) throw new Error('请先设置环境变量 ADMIN_PASSWORD（网页登录用的管理口令）');
  const r = await fetch(BASE + '/api/admin/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ password: PASSWORD }),
  });
  if (!r.ok) throw new Error('管理口令错误或服务不可达（HTTP ' + r.status + '）');
  const cookies = r.headers.getSetCookie ? r.headers.getSetCookie() : [];
  await r.json();
  return cookies.map((c) => c.split(';')[0]).join('; ');
}

async function api(path, cookie, opts = {}) {
  const r = await fetch(BASE + path, {
    method: opts.method || 'GET',
    headers: { 'Content-Type': 'application/json', Cookie: cookie },
    ...(opts.body ? { body: JSON.stringify(opts.body) } : {}),
  });
  const text = await r.text();
  let json = null;
  try { json = JSON.parse(text); } catch (e) {}
  return { status: r.status, json, text };
}

async function main() {
  const cookie = await login();

  if (hasFlag('list')) {
    const rc = await api('/api/rollcalls', cookie);
    const list = Array.isArray(rc.json) ? rc.json : (rc.json && rc.json.rollcalls) || [];
    if (!list.length) {
      console.log('当前没有进行中的点名。');
      return;
    }
    console.log('进行中的点名：');
    for (const r of list) {
      const kind = r.is_number ? '数字' : r.is_radar ? '位置' : '二维码';
      console.log(
        `  rollcallId=${String(r.id).padEnd(10)} [${kind}] ${r.course_title || ''} · ${r.title || ''}`
      );
    }
    return;
  }

  const qrRaw = argOf('qr');
  const target = argOf('target');
  const accountId = argOf('account');
  if (!qrRaw || !target) {
    console.log('用法: node tools/qr-probe.js --qr "<二维码原文>" --target <目标rollcallId> [--account <账号id>]');
    console.log('      node tools/qr-probe.js --list');
    process.exit(2);
  }

  // 解析二维码，取出它自己那场点名和 data
  let parsed;
  try {
    parsed = inspect(qrRaw);
  } catch (e) {
    throw new Error('二维码解析失败：' + e.message);
  }
  const data = parsed.fields.data;
  const ownRollcall = parsed.fields.rollcallId;
  if (data === undefined) throw new Error('二维码里没有 data 字段');

  console.log('二维码解析：');
  console.log(`  它自己属于 rollcallId = ${ownRollcall}`);
  console.log(`  data = ${String(data).slice(0, 16)}... (长度 ${String(data).length})`);
  const m = String(data).match(/^(\d{10})([0-9a-fA-F]{32})$/);
  if (m) {
    const age = Math.floor(Date.now() / 1000) - Number(m[1]);
    console.log(`  data 里的时间戳 = ${m[1]}（距现在 ${age} 秒）`);
    if (age > 120) console.log('  ⚠ 这张码已经比较旧了，如果失败可能只是过期，不是"不能跨课程"');
  }

  // 选账号
  let account = accountId;
  if (!account) {
    const accts = await api('/api/accounts', cookie);
    const list = Array.isArray(accts.json) ? accts.json : [];
    if (!list.length) throw new Error('没有账号，请先在网页上登录一个学校账号');
    const enabled = list.filter((a) => a.enabled);
    account = (enabled[0] || list[0]).id;
    console.log(`\n未指定 --account，自动使用：${(enabled[0] || list[0]).name || account}`);
  }

  const sameRollcall = String(ownRollcall) === String(target);
  console.log(`\n探测：用这张码的 data 去签 rollcallId=${target}`);
  console.log(`  ${sameRollcall ? '（同一场点名——作为对照组，应该成功）' : '（不同点名——这就是"跨课程"测试）'}`);

  const res = await api('/api/sign/qr/probe', cookie, {
    method: 'POST',
    body: { account_id: account, rollcall_id: String(target), data: String(data) },
  });
  if (!res.json) {
    console.log('请求失败：HTTP ' + res.status + ' ' + res.text.slice(0, 200));
    return;
  }
  console.log(`\n上游 HTTP ${res.json.upstream_code}`);
  console.log('上游原始响应：');
  console.log('  ' + String(res.json.upstream_body || '').slice(0, 800));

  // 判读
  const body = String(res.json.upstream_body || '');
  const code = res.json.upstream_code;
  console.log('\n判读：');
  if (code === 200 && /on_call_fine|on_call|"success"\s*:\s*true/.test(body)) {
    console.log(sameRollcall
      ? '  ✔ 对照组成功——说明请求格式本身没问题'
      : '  ★ 跨课程成功！签名不绑定 rollcallId，这张码可以拿去签其它课程');
  } else if (/expired|过期/i.test(body)) {
    console.log('  这张码已过期。重新扫一张新码再试。');
  } else if (/device_used|deviceAlreadyInUse/.test(body)) {
    console.log('  设备已被占用：这个账号的 deviceId 已经签过这场点名了（换账号或换场次再试）');
  } else if (/rollcall/i.test(body) && /(invalid|not|mismatch|wrong)/i.test(body)) {
    console.log('  上游明确提到 rollcall 不匹配 → 签名绑定了 rollcallId，跨课程不可行');
  } else if (code === 400 || code === 403 || code === 422) {
    console.log('  被拒绝，但没有明确说是哪种原因，看上面的原始 body');
  } else {
    console.log('  无法自动判读，请人工看原始 body');
  }
}

if (require.main === module) {
  main().catch((e) => {
    console.error('失败：' + e.message);
    process.exit(1);
  });
}
