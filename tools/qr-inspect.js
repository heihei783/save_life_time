#!/usr/bin/env node
/**
 * 畅课签到二维码解析器（与 rust/src/qr.rs 同一套编码规则）。
 *
 * 用途：把扫到的二维码原文还原成可读字段，便于对比多张二维码、观察
 * `data` / `rollcallId` / `accessCode` 之间的关系。
 *
 * 用法：
 *   node tools/qr-inspect.js "<二维码原文>"
 *   node tools/qr-inspect.js --json "<二维码原文>"
 *   node tools/qr-inspect.js --compare a.txt b.txt   # 每行一个二维码原文
 *
 * 它只做"解码"，不生成、不伪造任何二维码。
 */

// 字段名表：base36 下标 -> 字段名。顺序来自畅课前端打包代码，不能改动。
const FIELD_KEYS = [
  'courseId', // 0
  'activityId', // 1
  'activityType', // 2
  'data', // 3  ← 签到数据（关键未知量）
  'rollcallId', // 4
  'groupSetId', // 5
  'accessCode', // 6
  'action', // 7
  'enableGroupRollcall', // 8
  'createUser', // 9
  'joinCourse', // 10
];

// 「唯一值」表：\x1a + base36(下标+2)
const UNIQUE_KEYS = ['classroom-exam', 'feedback', 'vote'];

const TA = '\x1e'; // 分隔字段
const EA = '\x1f'; // 转义 ~
const NA = '\x1a'; // 布尔 / 唯一值前缀
const RA = '\x10'; // 数字前缀
const TRUE_MARK = '\x1a1';
const FALSE_MARK = '\x1a0';

function toBase36(n) {
  return Math.abs(n).toString(36);
}

function fieldName(token) {
  const index = FIELD_KEYS.findIndex((_, i) => toBase36(i) === token);
  return index >= 0 ? FIELD_KEYS[index] : token;
}

function decodeNumber(text) {
  const body = text.slice(1); // 去掉 \x10
  const parts = body.split('.');
  const nums = parts.map((p) => parseInt(p, 36));
  if (nums.some((n) => Number.isNaN(n))) return text;
  if (nums.length > 1) return `${nums[0]}.${nums[1]}`;
  return nums[0];
}

/** 压缩串 -> 字段对象 */
function parseCompressed(t) {
  const out = {};
  if (!t) return out;
  for (const part of t.split(TA).join('!').split('!').filter(Boolean)) {
    const idx = part.indexOf('~');
    if (idx < 0) continue;
    const key = fieldName(part.slice(0, idx));
    const raw = part.slice(idx + 1);
    let value;
    if (raw.startsWith(NA)) {
      if (raw === TRUE_MARK) value = true;
      else if (raw === FALSE_MARK) value = false;
      else {
        const rest = raw.slice(1);
        const found = UNIQUE_KEYS.find((_, i) => toBase36(i + 2) === rest);
        value = found || raw;
      }
    } else if (raw.startsWith(RA)) {
      value = decodeNumber(raw);
    } else {
      value = raw.split(EA).join('~').split(TA).join('!');
    }
    out[key] = value;
  }
  return out;
}

/** 二维码原文 -> { url, raw, fields } */
function inspect(text) {
  const raw = String(text || '').trim();
  if (!raw) throw new Error('二维码内容为空');
  let url;
  try {
    url = new URL(raw.startsWith('http') ? raw : 'http://lms.tc.cqupt.edu.cn' + raw);
  } catch (e) {
    throw new Error('不是合法 URL，也不是 /j?p=... 相对路径');
  }
  if (url.pathname !== '/j' && url.pathname !== '/scanner-jumper') {
    throw new Error(`路径不是 /j 或 /scanner-jumper（实际 ${url.pathname}），可能不是畅课签到码`);
  }
  // _p 是明文 JSON，p 是压缩串
  const jsonParam = url.searchParams.get('_p');
  if (jsonParam) {
    try {
      const parsed = JSON.parse(jsonParam);
      if (parsed && typeof parsed === 'object' && Object.keys(parsed).length) {
        return { url: raw, encoding: '_p (JSON)', raw: jsonParam, fields: parsed };
      }
    } catch (e) {
      /* 落到压缩串分支 */
    }
  }
  const compressed = url.searchParams.get('p');
  if (!compressed) throw new Error('URL 里既没有 p 也没有 _p 参数');
  return { url: raw, encoding: 'p (压缩串)', raw: compressed, fields: parseCompressed(compressed) };
}

/** 解析 data 的结构。实测畅课的 data 是「10 位 Unix 时间戳 + 32 位十六进制」。 */
function analyzeData(data) {
  const text = String(data == null ? '' : data);
  const m = text.match(/^(\d{10})([0-9a-fA-F]{32})$/);
  if (!m) {
    return {
      recognized: false,
      length: text.length,
      note: '不符合「10位时间戳+32位hex」，可能是别的 activityType 或新版格式',
    };
  }
  const ts = Number(m[1]);
  const issued = new Date(ts * 1000);
  const ageSeconds = Math.floor(Date.now() / 1000) - ts;
  return {
    recognized: true,
    timestamp: ts,
    issuedAtLocal: issued.toLocaleString('zh-CN', { timeZone: 'Asia/Hong_Kong', hour12: false }),
    issuedAtUtc: issued.toISOString(),
    ageSeconds,
    signature: m[2],
    signatureNote: '32 位十六进制（MD5 长度）。实测不是「时间戳/rollcallId/courseId」的直接哈希，推断带服务端密钥',
  };
}

function describe(fields) {
  const lines = [];
  for (const [key, value] of Object.entries(fields)) {
    const shown = typeof value === 'string' ? JSON.stringify(value) : String(value);
    const note = key === 'data' ? '   ← 签到数据' : '';
    lines.push(`  ${key.padEnd(20)} = ${shown}${note}`);
  }
  if (fields.data !== undefined) {
    const info = analyzeData(fields.data);
    lines.push('');
    if (info.recognized) {
      lines.push('  data 结构 = 「10 位时间戳 + 32 位签名」');
      lines.push(`    时间戳   : ${info.timestamp}  (${info.issuedAtLocal} HKT)`);
      lines.push(`    距现在   : ${info.ageSeconds} 秒${info.ageSeconds > 60 ? '  ← 已经比较旧了' : ''}`);
      lines.push(`    签名     : ${info.signature}`);
      lines.push(`    签名说明 : ${info.signatureNote}`);
    } else {
      lines.push(`  data 结构 : 未识别（长度 ${info.length}）—— ${info.note}`);
    }
  }
  return lines.join('\n');
}

function main(argv) {
  const args = argv.slice(2);
  const asJson = args.includes('--json');
  const compareIdx = args.indexOf('--compare');
  const fs = require('fs');

  if (compareIdx >= 0) {
    const files = args.slice(compareIdx + 1).filter((a) => !a.startsWith('--'));
    if (files.length < 1) throw new Error('--compare 需要至少一个文件（每行一个二维码原文）');
    const rows = [];
    for (const file of files) {
      const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
      for (const line of lines) {
        try {
          rows.push(inspect(line));
        } catch (e) {
          rows.push({ url: line, error: e.message });
        }
      }
    }
    if (asJson) {
      console.log(JSON.stringify(rows, null, 2));
      return;
    }
    // 逐字段横向对比，方便看哪个字段随 rollcallId 变、哪个是随机量
    const keys = [...new Set(rows.flatMap((r) => Object.keys(r.fields || {})))];
    const widths = keys.map((k) => Math.max(k.length, 8));
    console.log('共 ' + rows.length + ' 张二维码\n');
    keys.forEach((k, i) => {
      const values = rows.map((r) => {
        const v = r.fields ? r.fields[k] : undefined;
        if (v === undefined) return '(无)';
        const s = typeof v === 'string' ? v : String(v);
        return s.length > 40 ? s.slice(0, 37) + '...' : s;
      });
      const distinct = new Set(values).size;
      console.log(
        k.padEnd(widths[i]) + ' | ' + values.join(' | ') + (distinct > 1 ? '   ← 有变化' : '   ← 全都一样')
      );
    });

    // data 的时间戳部分单独摊开，这是判断"是否按时间刷新"的关键
    const stamps = rows.map((r) => {
      const info = r.fields ? analyzeData(r.fields.data) : { recognized: false };
      return info.recognized ? info : null;
    });
    if (stamps.some(Boolean)) {
      console.log('\ndata 内嵌时间戳：');
      stamps.forEach((s, i) => {
        if (!s) return;
        console.log(`  第${i + 1}张: ${s.timestamp} (${s.issuedAtLocal}) 签名=${s.signature.slice(0, 12)}...`);
      });
      const valid = stamps.filter(Boolean).map((s) => s.timestamp);
      if (valid.length > 1) {
        console.log(`  时间跨度 ${Math.max(...valid) - Math.min(...valid)} 秒`);
      }
    }

    console.log('\n判读提示：');
    console.log('- `rollcallId` 相同但 `data` 不同 → data 含时间/随机量，伪造需知道生成算法');
    console.log('- `data` 与 `rollcallId` 有固定算术/编码关系 → 可能可推导');
    console.log('- 同一场点名所有人扫的是同一张码 → data 相同，说明它只是"这场点名的凭证"');
    return;
  }

  const target = args.filter((a) => !a.startsWith('--'))[0];
  if (!target) {
    console.error('用法: node tools/qr-inspect.js "<二维码原文>"');
    console.error('      node tools/qr-inspect.js --compare samples.txt');
    process.exit(2);
  }
  const result = inspect(target);
  if (asJson) {
    console.log(JSON.stringify(result, null, 2));
    return;
  }
  console.log('编码方式: ' + result.encoding);
  console.log('原始串  : ' + JSON.stringify(result.raw));
  console.log('解析字段:');
  console.log(describe(result.fields));
  if (result.fields.rollcallId !== undefined) {
    console.log('\n签到会提交到: PUT /api/rollcall/' + result.fields.rollcallId + '/answer_qr_rollcall');
  }
}

if (require.main === module) {
  try {
    main(process.argv);
  } catch (e) {
    console.error('解析失败: ' + e.message);
    process.exit(1);
  }
}

module.exports = { inspect, parseCompressed, FIELD_KEYS, UNIQUE_KEYS };
