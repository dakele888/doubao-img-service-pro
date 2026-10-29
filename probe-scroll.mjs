const task = await taskSpace("豆包视频抓取调试");
const pages = await task.pages();
const page = pages[0];
if (!page) { console.log("无页面"); process.exit(0); }

await page.goto("https://www.doubao.com/chat/", { waitUntil: "domcontentloaded" });
await page.waitForTimeout(3000);
await page.evaluate(() => {
  for (const el of Array.from(document.querySelectorAll("a,div,span"))) {
    if (el.children.length !== 0) continue;
    if ((el.innerText || "").trim() !== "合租厨房宵夜互动") continue;
    if (el.getBoundingClientRect().width <= 0) continue;
    (el.closest("a") || el.closest("[role=button]") || el.parentElement).click();
    return;
  }
});
await page.waitForTimeout(4500);

const before = await page.evaluate(() => {
  const boxes = Array.from(document.querySelectorAll("div"))
    .filter(function (el) {
      const st = getComputedStyle(el);
      return el.scrollHeight > el.clientHeight + 80 && /auto|scroll/.test(st.overflowY);
    })
    .map(function (el) {
      return { cls: String(el.className).slice(0, 40), top: Math.round(el.scrollTop), h: el.scrollHeight, ch: el.clientHeight };
    })
    .sort(function (a, b) { return b.h - a.h; });
  return { count: boxes.length, top3: boxes.slice(0, 3), win: { y: window.scrollY, h: document.body.scrollHeight } };
});
console.log("滚动前:", JSON.stringify(before, null, 2));

// 跑和脚本里一样的滚动逻辑
await page.evaluate(() => {
  const boxes = Array.from(document.querySelectorAll("div")).filter(function (el) {
    const st = getComputedStyle(el);
    return el.scrollHeight > el.clientHeight + 80 && /auto|scroll/.test(st.overflowY);
  });
  boxes.sort(function (a, b) { return b.scrollHeight - a.scrollHeight; });
  if (boxes[0]) boxes[0].scrollTop = boxes[0].scrollHeight;
  window.scrollTo(0, document.body.scrollHeight);
});
await page.waitForTimeout(1200);

const after = await page.evaluate(() => {
  const boxes = Array.from(document.querySelectorAll("div"))
    .filter(function (el) {
      const st = getComputedStyle(el);
      return el.scrollHeight > el.clientHeight + 80 && /auto|scroll/.test(st.overflowY);
    })
    .map(function (el) {
      return { cls: String(el.className).slice(0, 40), top: Math.round(el.scrollTop), h: el.scrollHeight, ch: el.clientHeight };
    })
    .sort(function (a, b) { return b.h - a.h; });
  return { top3: boxes.slice(0, 3) };
});
console.log("滚动后:", JSON.stringify(after, null, 2));
