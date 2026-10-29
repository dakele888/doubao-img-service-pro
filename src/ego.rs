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
    want_raw: bool,
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
        .replace("@@RAW@@", if want_raw { "1" } else { "0" })
        .replace("@@MARK@@", MARK)
        .replace("@@PROMPT@@", &prompt_json)
        .replace("@@PAGE@@", &page_json)
        .replace("@@NAME@@", &name_json)
        .replace("@@IMAGES@@", &refs_json)
        .replace("@@DEADLINE@@", &deadline.to_string())
}

const TPL: &str = r##"
const task = await taskSpace("@@SPACE@@");
const WANT_RAW = "@@RAW@@" === "1";
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
  // 嗅探页面自己的网络响应：豆包前端拉消息/作品列表时带 image_ori_raw（无水印原图档），
  // 提前按图片 hash 缓存起来，生成完直接取，不用刷新页面。
  try {
    await page.evaluate(() => {
      if (window.__mohaRawHook) return;
      window.__mohaRawHook = 1;
      const RAW = (window.__mohaRaw = {});
      function scan(obj) {
        try {
          if (!obj || typeof obj !== "object") return;
          if (Array.isArray(obj)) { obj.forEach(scan); return; }
          for (const k in obj) {
            const v = obj[k];
            if (v && typeof v === "object") {
              const r = v.image_ori_raw || (v.image && v.image.image_ori_raw);
              const u = r && r.url;
              if (u) {
                const m = String(u).match(/rc_gen_image\/([^?\/~]+)/);
                if (m) RAW[m[1]] = String(u);
              }
              scan(v);
            }
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
              if (t.indexOf("image_ori_raw") >= 0 && t.length < 8e6) scan(JSON.parse(t));
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
    // 缩略图是 img + blob:，等它渲染出来；最多等 18 秒
    let after = before;
    for (let k = 0; k < 30; k++) {
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
    async function findRaw(key) {
      return await page.evaluate((k) => {
        const res = { targets: 0, found: null, err: null, cached: false };
        try {
          const RAW = window.__mohaRaw || {};
          if (RAW[k]) { res.found = RAW[k]; res.cached = true; return res; }
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
                  if (v.length < 3000 && v.indexOf("image_raw") >= 0 && v.indexOf("http") === 0 && v.indexOf(key) >= 0) out.push(v);
                } else if (v && typeof v === "object") {
                  walk(v, key, out, depth + 1, seen);
                }
              } catch (e) {}
            }
          }
          const targets = [...document.querySelectorAll("img")].filter((i) => {
            const s = i.currentSrc || i.src || "";
            return s.indexOf(k) >= 0;
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
              walk(f.memoizedProps, k, hits, 0, new Set());
              if (hits.length) res.found = hits[0];
              f = f.return;
              depth++;
            }
          }
        } catch (e) {
          res.err = String((e && e.message) || e);
        }
        return res;
      }, key);
    }
    let upgraded = null;
    let rawDbg = null;
    if (hit && WANT_RAW) {
      const key = (hit.split("?")[0].split("/").pop() || "").split("~")[0];
      if (key) {
        let r = await findRaw(key);
        rawDbg = r;
        upgraded = r && r.found;
        for (let i = 0; i < 2 && !upgraded; i++) {
          try {
            await page.reload();
            await page.waitForTimeout(4200);
          } catch (e) {}
          r = await findRaw(key);
          rawDbg = r;
          upgraded = r && r.found;
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
    else emit({ ok: true, url: finalUrl, thumbUrl: hit, upgraded: !!upgraded, rawDbg: rawDbg, urls: freshUrls, pageLabel: page.label, convUrl: convUrl, renamed: renamed, convHit: !!convHit, refs: REFS.length });
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
