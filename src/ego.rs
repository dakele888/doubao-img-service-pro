// 生成 ego-browser 要执行的脚本，并带着超时去跑它。
use crate::util::CREATE_NO_WINDOW;
use serde_json::Value;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub const MARK: &str = "__MOHA_IMG_RESULT__";
pub const SPACE: &str = "moha-doubao-img";

/// 参考图：(文件名, MIME, base64)
pub type RefImage = (String, String, String);

pub fn build_script(
    prompt: &str,
    page_label: &str,
    conv_name: &str,
    gen_timeout_ms: u64,
    refs: &[RefImage],
    want_tier: &str,
) -> String {
    let prompt_json = serde_json::to_string(prompt).unwrap_or_else(|_| "\"\"".into());
    let page_json = serde_json::to_string(page_label).unwrap_or_else(|_| "\"p2\"".into());
    let name_json = serde_json::to_string(conv_name).unwrap_or_else(|_| "\"生图专用\"".into());
    let deadline = gen_timeout_ms.saturating_sub(60_000);

    let refs_json = serde_json::to_string(
        &refs
            .iter()
            .map(|(name, mime, data)| serde_json::json!({ "name": name, "mime": mime, "b64": data }))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into());

    TPL.replace("@@SPACE@@", SPACE)
        .replace("@@TIER@@", want_tier)
        .replace("@@MARK@@", MARK)
        .replace("@@PROMPT@@", &prompt_json)
        .replace("@@PAGE@@", &page_json)
        .replace("@@NAME@@", &name_json)
        .replace("@@IMAGES@@", &refs_json)
        .replace("@@DEADLINE@@", &deadline.to_string())
}

/// 视频中转脚本：和生图同一套思路（占住对话页 → 切视频模式 → 发提示词 → 嗅探视频地址）
pub fn build_video_script(
    prompt: &str,
    page_label: &str,
    conv_name: &str,
    timeout_ms: u64,
    refs: &[RefImage],
) -> String {
    let prompt_json = serde_json::to_string(prompt).unwrap_or_else(|_| "\"\"".into());
    let page_json = serde_json::to_string(page_label).unwrap_or_else(|_| "\"p2\"".into());
    let name_json = serde_json::to_string(conv_name).unwrap_or_else(|_| "\"视频专用\"".into());
    let deadline = timeout_ms.saturating_sub(60_000);

    let refs_json = serde_json::to_string(
        &refs
            .iter()
            .map(|(name, mime, data)| serde_json::json!({ "name": name, "mime": mime, "b64": data }))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into());

    VIDEO_TPL
        .replace("@@SPACE@@", SPACE)
        .replace("@@MARK@@", MARK)
        .replace("@@PROMPT@@", &prompt_json)
        .replace("@@PAGE@@", &page_json)
        .replace("@@NAME@@", &name_json)
        .replace("@@IMAGES@@", &refs_json)
        .replace("@@DEADLINE@@", &deadline.to_string())
}

const TPL: &str = r##"
// 空间归属可能被 ego 判给用户（调试/手动操作后会这样），拿不到就按错误里的 id 接管回来
const task = await (async () => {
  try { return await taskSpace("@@SPACE@@"); }
  catch (e) {
    const m = String((e && e.message) || e).match(/belongs to the user \(id (\d+)\)/);
    if (!m) throw e;
    return await claimTaskSpace(Number(m[1]));
  }
})();
// 接口返回的图片档位："ori"=网页「下载原图」同档（带水印）、"raw"=无水印原图、"thumb"=列表缩略图
const WANT_TIER = "@@TIER@@";
const PROMPT = @@PROMPT@@;
const PAGE_LABEL = @@PAGE@@;
const CONV_NAME = @@NAME@@;
const REFS = @@IMAGES@@;

const pages = await task.pages();
let page = pages.find((p) => p.label === PAGE_LABEL) || pages[0];
function emit(obj) { console.log("@@MARK@@" + JSON.stringify(obj)); }

async function ensureChat() {
  const u = await page.url();
  if (u.indexOf("doubao.com") < 0) {
    await page.goto("https://www.doubao.com/chat/", { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(2800);
  }
}

function listConvs() {
  return page.evaluate(() => {
    const seen = {};
    const out = [];
    document.querySelectorAll("a[href*='/chat/']").forEach((a) => {
      const tail = (a.getAttribute("href") || "").split("/chat/")[1] || "";
      const id = tail.split("?")[0].split("/")[0];
      if (!id || !/^[0-9]+$/.test(id) || seen[id]) return;
      seen[id] = 1;
      out.push({ id: id, name: String(a.innerText || "").replace(/[\s]+/g, " ").trim() });
    });
    return out;
  });
}

async function renameConv(id, name) {
  try {
    await page.evaluate((cid) => {
      const a = document.querySelector("a[href*='/chat/" + cid + "']");
      if (a) a.scrollIntoView({ block: "center" });
    }, id);
    await page.waitForTimeout(400);
    try { await page.hover("css:[data-conversation-id='" + id + "']"); } catch (e) {}
    await page.waitForTimeout(500);
    await page.click("css:[data-conversation-id='" + id + "'] button[aria-label='更多']");
    await page.waitForTimeout(800);
    const okMenu = await page.evaluate(() => {
      const t = [...document.querySelectorAll("div,span")].find((e) => e.children.length === 0 && (e.innerText || "").trim() === "重命名");
      if (!t) return false;
      t.click();
      return true;
    });
    if (!okMenu) return false;
    await page.waitForTimeout(900);
    await page.click("css:input.semi-input");
    await page.keyboard.press("Control+A");
    await page.keyboard.insertText(name);
    await page.waitForTimeout(250);
    const okBtn = await page.evaluate(() => {
      const b = [...document.querySelectorAll("button,div,span")].find((e) => e.children.length === 0 && (e.innerText || "").trim() === "确定");
      if (!b) return false;
      (b.closest("button") || b).click();
      return true;
    });
    if (!okBtn) return false;
    await page.waitForTimeout(1200);
    return (await listConvs()).some((c) => c.name === name);
  } catch (e) {
    return false;
  }
}

try { await ensureChat(); } catch (e) {}

// 未登录豆包时立刻报错，别让人干等超时
const __wall = await page.evaluate(() => ({
  url: location.href,
  hasInput: !!document.querySelector(".tiptap"),
  text: (document.body.innerText || "").slice(0, 3000),
}));
const __needLogin = ((/扫码登录|手机号登录|立即登录|登录后即可|验证码登录/.test(__wall.text) && !__wall.hasInput)
  || /login|passport|sso/i.test(__wall.url));

async function main() {
  let needRename = false;
  let convHit = null;
  // 嗅探页面自己的网络响应：豆包前端拉消息/作品列表时带全套图片档位，
  // 提前按图片 hash 分档缓存（raw=无水印原图，ori=网页「下载原图」同档带水印），生成完直接取。
  try {
    await page.evaluate(() => {
      if (window.__mohaRawHook) return;
      window.__mohaRawHook = 1;
      const T = (window.__mohaTiers = window.__mohaTiers || { raw: {}, ori: {}, thumb: {} });
      // 按 URL 里的 CDN 模板分档：image_raw=无水印原图、cthumb=缩略图、cgen=网页「下载原图」同档
      function putUrl(u) {
        try {
          const s = String(u || "");
          const m = s.match(/rc_gen_image\/([^?\/~]+)/);
          if (!m) return;
          let tier = null;
          if (s.indexOf("image_raw") >= 0) tier = "raw";
          else if (s.indexOf("cthumb") >= 0) tier = "thumb";
          else if (s.indexOf("cgen") >= 0) tier = "ori";
          if (tier) T[tier][m[1]] = s;
        } catch (e) {}
      }
      function scan(obj) {
        try {
          if (!obj || typeof obj !== "object") return;
          if (Array.isArray(obj)) { obj.forEach(scan); return; }
          for (const k in obj) {
            const v = obj[k];
            if (typeof v === "string") putUrl(v);
            else if (v && typeof v === "object") scan(v);
          }
        } catch (e) {}
      }
      if (window.fetch) {
        const of_ = window.fetch;
        window.fetch = function () {
          const p = of_.apply(this, arguments);
          try {
            p.then((res) => {
              try {
                const ct = (res.headers && res.headers.get("content-type")) || "";
                if (ct.indexOf("json") >= 0) res.clone().json().then(scan).catch(() => {});
              } catch (e) {}
            }).catch(() => {});
          } catch (e) {}
          return p;
        };
      }
      const oo = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function (m, u) { this.__mohaUrl = u; return oo.apply(this, arguments); };
      const os = XMLHttpRequest.prototype.send;
      XMLHttpRequest.prototype.send = function () {
        try {
          this.addEventListener("load", () => {
            try {
              const t = this.responseText || "";
              if (t.indexOf("image_ori") >= 0 && t.length < 8e6) scan(JSON.parse(t));
            } catch (e) {}
          });
        } catch (e) {}
        return os.apply(this, arguments);
      };
    });
  } catch (e) {}
  try {
    const convs = await listConvs();
    convHit = convs.find((c) => c.name === CONV_NAME) || null;
    if (convHit) {
      const cur = await page.url();
      if (cur.indexOf(convHit.id) < 0) {
        await page.goto("https://www.doubao.com/chat/" + convHit.id, { waitUntil: "domcontentloaded" });
        await page.waitForTimeout(2600);
      }
    } else {
      const cur = await page.url();
      const tail = (cur.split("/chat/")[1] || "").split("?")[0].split("/")[0];
      if (tail) {
        await page.goto("https://www.doubao.com/chat/", { waitUntil: "domcontentloaded" });
        await page.waitForTimeout(2600);
      }
      needRename = true;
    }
  } catch (e) {}

  async function inImageMode() {
    return await page.evaluate(() => {
      const t = document.body.innerText || "";
      return t.indexOf("Seedream") >= 0 || /比例\s*(自动|[0-9]+:[0-9]+)/.test(t);
    });
  }

  async function enterImageMode() {
    if (await inImageMode()) return true;
    // 页面可能刚刷新完（豆包会话被回收会掉回首页），第一次点击常常点空。
    // 重试三轮，每轮点一次 + 等 3.6 秒。
    for (let round = 0; round < 3; round++) {
      try { await page.click("text=图像生成"); } catch (e) {
        try { await page.click("loc=role:button[name='图像生成']"); } catch (e2) {}
      }
      for (let i = 0; i < 12; i++) {
        await page.waitForTimeout(300);
        if (await inImageMode()) return true;
      }
      await page.waitForTimeout(1200);
    }
    return false;
  }

  function collect() {
    return page.evaluate(() => {
      const seen = {};
      const out = [];
      [...document.querySelectorAll("img")].forEach((i) => {
        const s = i.currentSrc || i.src || "";
        if (!/^https?:/.test(s) || s.indexOf("rc_gen_image") < 0) return;
        if (seen[s]) return;
        seen[s] = 1;
        out.push(s);
      });
      return out;
    });
  }

  // 豆包的参考图只能靠「粘贴」进输入框，没有可用的文件选择器。
  // 这里构造一个合成的 paste 事件，把图片当成剪贴板里的文件交给编辑器。
  // 输入框里的参考图缩略图：短边 40~120px 的 blob 图
  function attachCount() {
    return page.evaluate(() =>
      [...document.querySelectorAll("img")]
        .filter((i) => /^blob:/.test(i.src || ""))
        .filter((i) => {
          const r = i.getBoundingClientRect();
          return r.width > 30 && r.width < 130;
        }).length
    );
  }

  // 每次请求都从干净状态开始：上一轮若没发出去，缩略图会留在输入框里
  async function clearStale() {
    const n = await attachCount();
    if (n === 0) return 0;
    await page.reload();
    await page.waitForTimeout(3800);
    return n;
  }

  function blobCount() {
    return page.evaluate(() =>
      [...document.querySelectorAll("img")].filter((i) => /^(blob:|data:image\/)/.test(i.src || "")).length
    );
  }

  async function attachRefs() {
    if (!REFS.length) return { ok: true, n: 0 };
    const before = await attachCount();
    let dispatched = null;
    for (let i = 0; i < REFS.length; i++) {
      try { await page.click("css:.tiptap"); } catch (e) {}
      await page.waitForTimeout(300);
      dispatched = await page.evaluate((one) => {
        const bin = atob(one.b64);
        const arr = new Uint8Array(bin.length);
        for (let k = 0; k < bin.length; k++) arr[k] = bin.charCodeAt(k);
        const file = new File([arr], one.name || "ref.png", { type: one.mime || "image/png" });
        const dt = new DataTransfer();
        dt.items.add(file);
        const ed = document.querySelector(".tiptap");
        if (!ed) return { ok: false, why: "no-editor" };
        ed.focus();
        const ret = ed.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
        return { ok: true, ret: ret, size: file.size, type: file.type };
      }, REFS[i]);
      if (!dispatched || !dispatched.ok) {
        return { ok: false, error: "找不到输入框，参考图没法粘贴" };
      }
      await page.waitForTimeout(1800);
    }
    // 缩略图是 img + blob:，等它渲染出来；张数越多给的时间越长（每张 8 轮 × 600ms，最少 18 秒）
    let after = before;
    const rounds = Math.max(30, REFS.length * 8);
    for (let k = 0; k < rounds; k++) {
      after = await attachCount();
      if (after >= before + REFS.length) return { ok: true, n: REFS.length };
      await page.waitForTimeout(600);
    }
    return {
      ok: false,
      error:
        "参考图没出现在输入框（粘贴前 " + before + " 张，之后 " + after + " 张；派发结果 " +
        JSON.stringify(dispatched) + "）",
    };
  }

  let renamed = null;
  try {
    const stale = await clearStale();
    if (stale) console.log("清掉输入框里残留的 " + stale + " 张参考图");
    if (!(await enterImageMode())) {
      const st = await page.evaluate(() => ({
        url: location.href,
        hasBtn: [...document.querySelectorAll("button,[role=button]")].some((b) => (b.innerText || "").indexOf("图像生成") >= 0),
        tail: (document.body.innerText || "").slice(-200),
      }));
      emit({ ok: false, error: "进不到图像生成模式", state: st });
      return;
    }

    const before = {};
    (await collect()).forEach((s) => (before[s] = 1));

    const rf = await attachRefs();
    if (!rf.ok) { emit({ ok: false, error: rf.error }); return; }

    await page.click("css:.tiptap");
    await page.waitForTimeout(300);
    await page.keyboard.insertText(PROMPT);
    await page.waitForTimeout(500);
    await page.press("css:.tiptap", "Enter");

    let hit = null;
    let freshUrls = [];
    const deadline = Date.now() + @@DEADLINE@@;
    while (Date.now() < deadline) {
      await page.waitForTimeout(2500);
      const fresh = (await collect()).filter((s) => !before[s]);
      if (fresh.length) { hit = fresh[fresh.length - 1]; freshUrls = fresh; break; }
    }

    // 列表里给的是 384x216 缩略图（CDN 模板 cthumb），签名绑定整条 URL，改参数只会 403。
    // 原图（image_raw 模板）的签名地址藏在 React 组件数据里：顺着 img 的 fiber 往上找。
    // 刚生成的图数据常常还没带 raw，等不到就刷新一次页面再找。
    // 按档位找同一张图的签名地址：先查嗅探缓存，再顺着 img 的 fiber 往上找。
    // tier="raw"→image_raw 模板（无水印原图）；"thumb"→cthumb 模板（缩略图）；
    // "ori"→cgen 模板（网页「下载原图」同档，带水印）。
    async function findTier(key, tier) {
      const tpl = tier === "raw" ? "image_raw" : (tier === "thumb" ? "cthumb" : "cgen");
      return await page.evaluate((arg) => {
        const res = { targets: 0, found: null, err: null, cached: false };
        try {
          const T = ((window.__mohaTiers || {})[arg.tier]) || {};
          if (T[arg.key]) { res.found = T[arg.key]; res.cached = true; return res; }
          // 防止 fiber 对象图太大拖垮执行：限制访问节点数
          let budget = 200000;
          function walk(obj, key, out, depth, seen) {
            if (budget <= 0) return;
            budget--;
            if (!obj || typeof obj !== "object" || depth > 12 || out.length >= 4) return;
            seen.add(obj);
            for (const kk in obj) {
              if (budget <= 0) return;
              try {
                const v = obj[kk];
                if (typeof v === "string") {
                  if (v.length < 3000 && v.indexOf(arg.tpl) >= 0 && v.indexOf("http") === 0 && v.indexOf(key) >= 0) out.push(v);
                } else if (v && typeof v === "object") {
                  walk(v, key, out, depth + 1, seen);
                }
              } catch (e) {}
            }
          }
          const targets = [...document.querySelectorAll("img")].filter((i) => {
            const s = i.currentSrc || i.src || "";
            return s.indexOf(arg.key) >= 0;
          });
          res.targets = targets.length;
          for (let ti = targets.length - 1; ti >= 0; ti--) {
            const target = targets[ti];
            if (res.found || budget <= 0) break;
            const fk = Object.keys(target).find((x) => x.indexOf("__reactFiber$") === 0);
            if (!fk) continue;
            let f = target[fk];
            let depth = 0;
            while (f && depth < 60 && !res.found && budget > 0) {
              const hits = [];
              walk(f.memoizedProps, arg.key, hits, 0, new Set());
              if (hits.length) res.found = hits[0];
              f = f.return;
              depth++;
            }
          }
        } catch (e) {
          res.err = String((e && e.message) || e);
        }
        return res;
      }, { key: key, tier: tier, tpl: tpl });
    }
    // 档位选择：thumb = 强制换成 cthumb 缩略图（列表地址有时被页面渲染成 cgen 大图）；
    // ori = 默认（同网页「下载原图」：全分辨率，带豆包AI生成水印）；raw = 无水印原图。
    // 要的档找不到就退另一档（总比缩略图强），全失败才用列表原始地址。
    let upgraded = null;
    let tier = null;
    let rawDbg = null;
    if (hit) {
      const key = (hit.split("?")[0].split("/").pop() || "").split("~")[0];
      if (key) {
        if (WANT_TIER === "thumb") {
          if (hit.indexOf("cthumb") >= 0) {
            tier = "thumb";
          } else {
            const r = await findTier(key, "thumb");
            rawDbg = r;
            if (r && r.found) { upgraded = r.found; tier = "thumb"; }
          }
        } else {
          const want = WANT_TIER === "raw" ? "raw" : "ori";
          const alt = WANT_TIER === "raw" ? "ori" : "raw";
          let r = await findTier(key, want);
          rawDbg = r;
          if (r && r.found) { upgraded = r.found; tier = want; }
          for (let i = 0; i < 2 && !upgraded; i++) {
            try {
              await page.reload();
              await page.waitForTimeout(4200);
            } catch (e) {}
            r = await findTier(key, want);
            rawDbg = r;
            if (r && r.found) { upgraded = r.found; tier = want; }
          }
          if (!upgraded) {
            r = await findTier(key, alt);
            if (r && r.found) { upgraded = r.found; tier = alt; }
          }
        }
      }
    }
    const finalUrl = upgraded || hit;

    const convUrl = await page.url();
    if (needRename && hit) {
      const tail = (convUrl.split("/chat/")[1] || "").split("?")[0].split("/")[0];
      if (tail) {
        renamed = await renameConv(tail, CONV_NAME);
        if (!renamed) {
          try {
            await page.goto(convUrl, { waitUntil: "domcontentloaded" });
            await page.waitForTimeout(2600);
            renamed = await renameConv(tail, CONV_NAME);
          } catch (e) {}
        }
      }
    }

    if (!hit) emit({ ok: false, error: "等待超时，没抓到新生成的图片" });
    else emit({ ok: true, url: finalUrl, thumbUrl: hit, upgraded: !!upgraded, tier: tier || "thumb", rawDbg: rawDbg, urls: freshUrls, pageLabel: page.label, convUrl: convUrl, renamed: renamed, convHit: !!convHit, refs: REFS.length });
  } catch (e) {
    emit({ ok: false, error: "脚本异常：" + String((e && e.message) || e) });
  }
}

if (__needLogin) {
  emit({ ok: false, code: "NOT_LOGGED_IN", error: "豆包还没登录：请在 ego 浏览器里打开 doubao.com 扫码登录，登录后重试", convUrl: __wall.url });
} else {
  await main();
}
"##;

// 视频中转脚本：和生图同一套路（占住对话页 → 切视频模式 → 发提示词 → 嗅探视频地址）。
// 豆包的视频地址藏在页面 video 元素和接口响应里，抓法比图片宽一些。
const VIDEO_TPL: &str = r##"
// 空间归属可能被 ego 判给用户（调试/手动操作后会这样），拿不到就按错误里的 id 接管回来
const task = await (async () => {
  try { return await taskSpace("@@SPACE@@"); }
  catch (e) {
    const m = String((e && e.message) || e).match(/belongs to the user \(id (\d+)\)/);
    if (!m) throw e;
    return await claimTaskSpace(Number(m[1]));
  }
})();
const PROMPT = @@PROMPT@@;
const PAGE_LABEL = @@PAGE@@;
const CONV_NAME = @@NAME@@;
const REFS = @@IMAGES@@;

const pages = await task.pages();
let page = pages.find((p) => p.label === PAGE_LABEL) || pages[0];
function emit(obj) { console.log("@@MARK@@" + JSON.stringify(obj)); }

async function ensureChat() {
  const u = await page.url();
  if (u.indexOf("doubao.com") < 0) {
    await page.goto("https://www.doubao.com/chat/", { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(2800);
  }
}

function listConvs() {
  return page.evaluate(() => {
    const seen = {};
    const out = [];
    document.querySelectorAll("a[href*='/chat/']").forEach((a) => {
      const tail = (a.getAttribute("href") || "").split("/chat/")[1] || "";
      const id = tail.split("?")[0].split("/")[0];
      if (!id || !/^[0-9]+$/.test(id) || seen[id]) return;
      seen[id] = 1;
      out.push({ id: id, name: String(a.innerText || "").replace(/[\s]+/g, " ").trim() });
    });
    return out;
  });
}

// 嗅探视频地址：页面 video 元素 + fetch/XHR 响应里带 .mp4 或视频 CDN 的字符串
async function installHook() {
  try {
    await page.evaluate(() => {
      if (window.__mohaVidHook) return;
      window.__mohaVidHook = 1;
      const V = (window.__mohaVideos = window.__mohaVideos || {});
      const RE = /\.mp4(\?|$)|vlabvod|video[-_]?sign|rc_gen_video|videogen|vod\./i;
      function put(u) {
        try {
          const s = String(u || "");
          if (s.length < 20 || s.length > 4000) return;
          if (!/^https?:/.test(s)) return;
          if (!RE.test(s)) return;
          const base = s.split("?")[0];
          const key = base.split("/").pop() || base.slice(-40);
          V[key] = s;
        } catch (e) {}
      }
      function scan(o, d) {
        try {
          if (!o || typeof o !== "object" || d > 8) return;
          if (Array.isArray(o)) { o.forEach((x) => scan(x, d + 1)); return; }
          for (const k in o) {
            const v = o[k];
            if (typeof v === "string") put(v);
            else if (v && typeof v === "object") scan(v, d + 1);
          }
        } catch (e) {}
      }
      if (window.fetch) {
        const of_ = window.fetch;
        window.fetch = function () {
          const p = of_.apply(this, arguments);
          try {
            p.then((res) => {
              try {
                const ct = (res.headers && res.headers.get("content-type")) || "";
                if (ct.indexOf("json") >= 0) res.clone().json().then((j) => scan(j, 0)).catch(() => {});
              } catch (e) {}
            }).catch(() => {});
          } catch (e) {}
          return p;
        };
      }
      const oo = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function (m, u) { this.__mohaUrl = u; return oo.apply(this, arguments); };
      const os = XMLHttpRequest.prototype.send;
      XMLHttpRequest.prototype.send = function () {
        try {
          this.addEventListener("load", () => {
            try {
              const t = this.responseText || "";
              if (t.length < 8e6 && (t.indexOf(".mp4") >= 0 || t.indexOf("video") >= 0)) scan(JSON.parse(t), 0);
            } catch (e) {}
          });
        } catch (e) {}
        return os.apply(this, arguments);
      };
    });
  } catch (e) {}
}

function videoUrls() {
  return page.evaluate(() => {
    const out = [];
    try {
      [...document.querySelectorAll("video")].forEach((v) => {
        const s = v.currentSrc || v.src || "";
        if (/^https?:/.test(s)) out.push(s);
        [...v.querySelectorAll("source")].forEach((x) => {
          const q = x.src || "";
          if (/^https?:/.test(q)) out.push(q);
        });
      });
    } catch (e) {}
    const T = window.__mohaVideos || {};
    for (const k in T) out.push(T[k]);
    return [...new Set(out)];
  });
}

// 视频模式同样支持参考图（首帧），粘贴方式和生图一样
function attachCount() {
  return page.evaluate(() =>
    [...document.querySelectorAll("img")]
      .filter((i) => /^blob:/.test(i.src || ""))
      .filter((i) => {
        const r = i.getBoundingClientRect();
        return r.width > 30 && r.width < 130;
      }).length
  );
}

async function attachRefs() {
  if (!REFS.length) return { ok: true, n: 0 };
  const before = await attachCount();
  for (let i = 0; i < REFS.length; i++) {
    try { await page.click("css:.tiptap"); } catch (e) {}
    await page.waitForTimeout(300);
    const r = await page.evaluate((one) => {
      try {
        const bin = atob(one.b64);
        const arr = new Uint8Array(bin.length);
        for (let k = 0; k < bin.length; k++) arr[k] = bin.charCodeAt(k);
        const file = new File([arr], one.name || "ref.png", { type: one.mime || "image/png" });
        const dt = new DataTransfer();
        dt.items.add(file);
        const ed = document.querySelector(".tiptap");
        if (!ed) return { ok: false, why: "no-editor" };
        ed.focus();
        ed.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
        return { ok: true };
      } catch (e) {
        return { ok: false, why: String((e && e.message) || e) };
      }
    }, REFS[i]);
    if (!r || !r.ok) return { ok: false, error: "参考图粘贴失败：" + JSON.stringify(r) };
    await page.waitForTimeout(1800);
  }
  let after = before;
  const rounds = Math.max(30, REFS.length * 8);
  for (let k = 0; k < rounds; k++) {
    after = await attachCount();
    if (after >= before + REFS.length) return { ok: true, n: REFS.length };
    await page.waitForTimeout(600);
  }
  return { ok: false, error: "参考图没出现在输入框（" + before + "→" + after + "）" };
}

// 切视频模式：豆包入口文案可能是「视频生成 / 生成视频 / AI视频」，逐个精确点一遍
async function enterVideoMode() {
  const tried = [];
  for (let round = 0; round < 3; round++) {
    const clicked = await page.evaluate(() => {
      const want = ["视频生成", "生成视频", "AI视频", "AI 视频"];
      const nodes = [...document.querySelectorAll("button,[role=button],div,span")];
      for (const w of want) {
        for (const n of nodes) {
          if (n.children.length !== 0) continue;
          const t = (n.innerText || "").trim();
          if (t !== w) continue;
          const r = n.getBoundingClientRect();
          if (r.width <= 0 || r.height <= 0) continue;
          (n.closest("button") || n).click();
          return w;
        }
      }
      return null;
    });
    if (clicked) {
      tried.push(clicked);
      await page.waitForTimeout(3200);
      return { ok: true, clicked: tried };
    }
    await page.waitForTimeout(1500);
  }
  return { ok: false, clicked: tried };
}

// 首次用视频生成会弹「安全确认」（素材授权告知），必须点「确认」，否则流程卡住。
// 弹窗可能在切模式时出现，也可能在提交时才出现，两处都查一遍。
async function dismissSafetyDialog() {
  for (let i = 0; i < 8; i++) {
    const r = await page.evaluate(() => {
      const t = (document.body.innerText || "").slice(-2500);
      if (!/安全确认|充分授权|豆包使用规范/.test(t)) return "none";
      const nodes = [...document.querySelectorAll("button,[role=button],div,span")];
      for (const n of nodes) {
        if (n.children.length !== 0) continue;
        const s = (n.innerText || "").trim();
        if (s !== "确认" && s !== "确定" && s !== "同意") continue;
        const rc = n.getBoundingClientRect();
        if (rc.width <= 0 || rc.height <= 0) continue;
        (n.closest("button") || n).click();
        return "clicked";
      }
      return "no-button";
    });
    if (r === "clicked") {
      await page.waitForTimeout(700);
      return true;
    }
    if (r === "none") return false;
    await page.waitForTimeout(700);
  }
  return false;
}

// 按请求里的 model 切豆包视频模型。
// 豆包可选：Seedance 2.5（5 倍消耗）/ Seedance 2.0（2 倍）/ Seedance 2.0 Fast / Seedance 2.0 Mini
// 默认 Mini：没传 model、或者模型名认不出，都回落到 Mini（日常生成、消耗最低）。
async function pickModel() {
  const mm = PROMPT.match(/模型\s*([A-Za-z0-9._\- ]{2,40})/);
  const want = mm ? mm[1].trim().toLowerCase() : "";
  let target = "Seedance 2.0 Mini";
  if (want.indexOf("fast") >= 0) target = "Seedance 2.0 Fast";
  else if (want.indexOf("2.5") >= 0) target = "Seedance 2.5";
  else if (want.indexOf("mini") >= 0) target = "Seedance 2.0 Mini";
  else if (want.indexOf("seedance") >= 0 || want.indexOf("2.0") >= 0 || want.indexOf("sd2") >= 0) target = "Seedance 2.0";

  const cur = await page.evaluate(() => {
    for (const n of [...document.querySelectorAll("span,div")]) {
      if (n.children.length !== 0) continue;
      const t = (n.innerText || "").trim();
      if (/^Seedance/.test(t) && n.getBoundingClientRect().width > 0) return t;
    }
    return "";
  });
  if (cur === target) return target;

  const opened = await page.evaluate(() => {
    for (const n of [...document.querySelectorAll("span,div,button")]) {
      if (n.children.length !== 0) continue;
      if (!/^Seedance/.test((n.innerText || "").trim())) continue;
      if (n.getBoundingClientRect().width <= 0) continue;
      const t = n.closest("button") || n.parentElement || n;
      for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {
        t.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, view: window }));
      }
      return true;
    }
    return false;
  });
  if (!opened) return "";
  await page.waitForTimeout(900);

  const ok = await page.evaluate((t) => {
    for (const n of [...document.querySelectorAll("button,div,span")]) {
      if (n.children.length !== 0) continue;
      if ((n.innerText || "").trim() !== t) continue;
      if (n.getBoundingClientRect().width <= 0) continue;
      (n.closest("button") || n.parentElement || n).click();
      return true;
    }
    return false;
  }, target);
  await page.waitForTimeout(600);
  try { await page.keyboard.press("Escape"); } catch (e) {}
  await page.waitForTimeout(300);
  return ok ? target : "";
}

// 豆包视频生成是两段式：发完提示词先回一张「视频生成参数确认」卡片（列出模型/时长/比例），
// 点「确认」才真正开始生成。有时不弹这张卡（直接生成），所以做成"有就点、没有就跳过"。
// 返回 clicked / found-no-button / none
async function tryConfirmVideoParams() {
  return await page.evaluate(() => {
    const t = (document.body.innerText || "").slice(-3000);
    if (!/视频生成参数确认|确认后我再开始生成|请先确认以下参数/.test(t)) return "none";
    for (const n of [...document.querySelectorAll("button,[role=button],div,span")]) {
      if (n.children.length !== 0) continue;
      const s = (n.innerText || "").trim();
      if (s !== "确认" && s !== "确认生成" && s !== "开始生成" && s !== "继续" && s !== "好的") continue;
      if (n.getBoundingClientRect().width <= 0) continue;
      (n.closest("button") || n).click();
      return "clicked:" + s;
    }
    return "found-no-button";
  });
}

// 设置画面参数：输入框上方有「自动 · Ns」控件，点开是「比例 + 时长」面板。
// 比例是 7 个按钮（自动/3:4/4:3/9:16/16:9/1:1/21:9），时长是 4~15s 的自定义滑杆。
// 参数从提示词前缀里取（服务端已拼成「（画面比例 9:16，分辨率 720p，时长 12 秒）…」）。
// 任何一步失败都不阻断生成，只写进返回值。
async function setVideoParams() {
  const res = { model: "", panel: false, ratio: "", duration: 0 };
  // 先按请求切模型（认不出就保持原样），再设比例和时长
  res.model = await pickModel();
  const am = PROMPT.match(/画面比例\s*([0-9]+:[0-9]+)/);
  const aspect = am ? am[1] : "";
  const dm = PROMPT.match(/时长\s*([0-9]+)\s*秒/);
  const seconds = dm ? parseInt(dm[1], 10) : 0;

  // 1) 打开参数面板
  const opened = await page.evaluate(() => {
    const nodes = [...document.querySelectorAll("span,div,button")];
    for (const n of nodes) {
      if (n.children.length !== 0) continue;
      const t = (n.innerText || "").trim();
      if (!/^自动\s*·\s*[0-9]+\s*s$/.test(t) && t !== "自动") continue;
      const r = n.getBoundingClientRect();
      if (r.width <= 0) continue;
      const target = n.closest("button") || n.parentElement || n;
      for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {
        target.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, view: window }));
      }
      return t;
    }
    return null;
  });
  if (!opened) return res;
  await page.waitForTimeout(900);
  res.panel = await page.evaluate(() => /比例[\s\S]{0,60}时长/.test(document.body.innerText || ""));
  if (!res.panel) return res;

  // 2) 选比例
  if (aspect) {
    const ok = await page.evaluate((a) => {
      for (const b of [...document.querySelectorAll("button")]) {
        if ((b.innerText || "").trim() !== a) continue;
        if (b.getBoundingClientRect().width <= 0) continue;
        b.click();
        return true;
      }
      return false;
    }, aspect);
    if (ok) {
      res.ratio = aspect;
      await page.waitForTimeout(500);
    }
  }

  // 3) 拖时长滑杆（4~15s）
  if (seconds) {
    const pt = await page.evaluate((s) => {
      const lbl = [...document.querySelectorAll("*")].find(
        (el) => el.children.length === 0 && (el.innerText || "").trim() === "时长"
      );
      if (!lbl) return null;
      let box = lbl.parentElement;
      for (let i = 0; i < 4 && box; i++) {
        const t = box.innerText || "";
        if (t.indexOf("4s") >= 0 && t.indexOf("15s") >= 0) break;
        box = box.parentElement;
      }
      if (!box) return null;
      const cands = [...box.querySelectorAll("*")]
        .map((n) => ({ n: n, r: n.getBoundingClientRect() }))
        .filter((x) => x.r.width > 150 && x.r.height > 2 && x.r.height < 26 && !(x.n.innerText || "").trim())
        .sort((a, b) => b.r.y - a.r.y);
      if (!cands.length) return null;
      const r = cands[0].r;
      const ratio = Math.max(0, Math.min(1, (s - 4) / 11));
      return { x: Math.round(r.x + r.width * ratio), y: Math.round(r.y + r.height / 2) };
    }, seconds);
    if (pt) {
      await page.mouse.click(pt.x, pt.y);
      res.duration = seconds;
      await page.waitForTimeout(400);
    }
  }

  // 4) 收起面板（实测 Esc 不一定有效，还开着就点空白区域）
  for (let i = 0; i < 4; i++) {
    const open = await page.evaluate(() => /比例[\s\S]{0,40}时长/.test((document.body.innerText || "").slice(-700)));
    if (!open) break;
    try { await page.keyboard.press("Escape"); } catch (e) {}
    await page.waitForTimeout(450);
    const still = await page.evaluate(() => /比例[\s\S]{0,40}时长/.test((document.body.innerText || "").slice(-700)));
    if (!still) break;
    try { await page.mouse.click(420, 260); } catch (e) {}
    await page.waitForTimeout(450);
  }
  return res;
}

try { await ensureChat(); } catch (e) {}

// 未登录直接报错，别等十分钟
const __wall = await page.evaluate(() => ({
  url: location.href,
  hasInput: !!document.querySelector(".tiptap"),
  text: (document.body.innerText || "").slice(0, 3000),
}));
const __needLogin = ((/扫码登录|手机号登录|立即登录|登录后即可|验证码登录/.test(__wall.text) && !__wall.hasInput)
  || /login|passport|sso/i.test(__wall.url));

async function main() {
  // 找到「视频专用」会话就进去，找不到就用当前页
  let convHit = null;
  let convUrl = "";
  try {
    const convs = await listConvs();
    convHit = convs.find((c) => c.name === CONV_NAME) || null;
    if (convHit) {
      const cur = await page.url();
      if (cur.indexOf(convHit.id) < 0) {
        await page.goto("https://www.doubao.com/chat/" + convHit.id, { waitUntil: "domcontentloaded" });
        await page.waitForTimeout(2600);
      }
    }
  } catch (e) {}
  try { convUrl = await page.url(); } catch (e) {}

  await installHook();

  const mode = await enterVideoMode();
  // 切到视频模式后可能立刻弹安全确认，先处理掉，不然输入框被遮罩挡住
  const safetyAfterMode = await dismissSafetyDialog();
  // 按请求里的比例/时长去点豆包的参数面板（失败不影响生成）
  const params = await setVideoParams();

  // 记录发之前已有的视频地址，之后只认新增的
  const before = {};
  (await videoUrls()).forEach((s) => (before[s] = 1));

  const rf = await attachRefs();
  if (!rf.ok) { emit({ ok: false, error: rf.error }); return; }

  let confirmState = "none";
  try {
    await page.click("css:.tiptap");
    await page.waitForTimeout(300);
    await page.keyboard.insertText(PROMPT);
    await page.waitForTimeout(600);
    await page.press("css:.tiptap", "Enter");
    // 提交时也可能才弹安全确认
    await dismissSafetyDialog();
    // 豆包先回一张「视频生成参数确认」卡，点确认才真正开始生成
    await page.waitForTimeout(2200);
    confirmState = await tryConfirmVideoParams();
  } catch (e) {
    emit({ ok: false, error: "输入提示词失败：" + String((e && e.message) || e) });
    return;
  }

  const deadline = Date.now() + @@DEADLINE@@;
  let hit = null;
  let freshUrls = [];
  while (Date.now() < deadline) {
    await page.waitForTimeout(3000);
    // 参数确认卡可能晚几秒才出现，没点中就再试
    if (confirmState.indexOf("clicked") !== 0) {
      const c = await tryConfirmVideoParams();
      if (c !== "none") confirmState = c;
    }
    const list = await videoUrls();
    const fresh = list.filter((s) => !before[s]);
    if (fresh.length) {
      hit = fresh[fresh.length - 1];
      freshUrls = fresh;
      break;
    }
  }

  const tail = await page.evaluate(() => (document.body.innerText || "").slice(-400));
  if (!hit) {
    emit({
      ok: false,
      error: "等待超时，没抓到视频地址",
      mode: mode,
      convUrl: convUrl,
      pageTail: tail,
      params: params,
      confirm: confirmState,
    });
    return;
  }
  emit({
    ok: true,
    url: hit,
    urls: freshUrls,
    pageLabel: page.label,
    convUrl: convUrl,
    convHit: !!convHit,
    mode: mode,
    refs: REFS.length,
    pageTail: tail,
      params: params,
      confirm: confirmState,
  });
}

if (__needLogin) {
  emit({ ok: false, code: "NOT_LOGGED_IN", error: "豆包还没登录：请在 ego 浏览器里打开 doubao.com 扫码登录，登录后重试", convUrl: __wall.url });
} else {
  await main();
}
"##;

pub fn run(bin: &str, script: &str, timeout: Duration) -> Result<Value, String> {
    let mut child = Command::new(bin)
        .arg("nodejs")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("启动 ego-browser 失败：{}", e))?;

    let pid = child.id();
    if let Some(mut si) = child.stdin.take() {
        let body = script.as_bytes().to_vec();
        thread::spawn(move || {
            let _ = si.write_all(&body);
            let _ = si.flush();
        });
    }

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let r = child.wait_with_output();
        let _ = tx.send(r);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            parse(&stdout, &stderr)
        }
        Ok(Err(e)) => Err(format!("ego-browser 执行出错：{}", e)),
        Err(_) => {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
            Err("ego-browser 执行超时".into())
        }
    }
}

fn parse(stdout: &str, stderr: &str) -> Result<Value, String> {
    let all = format!("{}\n{}", stdout, stderr);
    for line in all.lines().rev() {
        if let Some(pos) = line.find(MARK) {
            let js = &line[pos + MARK.len()..];
            if let Ok(v) = serde_json::from_str::<Value>(js.trim()) {
                return Ok(v);
            }
        }
    }
    let tail: String = all.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect();
    Err(format!("脚本没有返回结果：{}", tail.trim()))
}
