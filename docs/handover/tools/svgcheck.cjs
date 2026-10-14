#!/usr/bin/env node
// svgcheck: render an SVG the way GitHub shows it (an <img>), at desktop and
// phone width, cut zoom tiles, and lint the layout.
//
// Usage, from the repository root:
//   node docs/handover/tools/svgcheck.cjs [--out DIR] docs/handover/diagrams/*.svg
// Needs Node 18+ and Playwright with Chromium (npm install playwright, then
// npx playwright install chromium). Output: PNGs in DIR (default:
// docs/handover/.render/, which .gitignore keeps out of git) and a JSON report
// on stdout. Look at the PNGs: the lint finds overlaps and overflow, not
// everything that reads badly.
//
// Lint (errors must be fixed; warnings must be looked at in the image):
//   E parse        the file is not well-formed XML
//   E forbidden    <script>, <foreignObject>, external <image>, @import or @font-face
//   E viewbox      no viewBox, or no width/height on the root
//   E outside      text (or part of it) is outside the viewBox
//   E overlap      two text elements overlap
//   W small-font   text drawn smaller than 13 px at the viewBox's own scale
//   W crosses-box  text crosses the edge of a rect (it should be inside or outside)
//   W line-thru    a line or path passes through a text's box
//   W background   the first drawn element is not a full-size opaque rect
//   W wide-font    with a wider font (DejaVu Sans) text overlaps or leaves its box
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright');

const args = process.argv.slice(2);
let outDir = null;
const files = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--out') outDir = args[++i];
  else files.push(args[i]);
}
if (!files.length) {
  console.error('usage: svgcheck.cjs [--out DIR] file.svg ...');
  process.exit(2);
}

const LINT = (opts) => {
  const res = { errors: [], warnings: [] };
  const svg = document.querySelector('svg');
  if (!svg) { res.errors.push({ code: 'parse', msg: 'no <svg> root' }); return res; }
  const vb = svg.viewBox && svg.viewBox.baseVal;
  if (!vb || !vb.width) { res.errors.push({ code: 'viewbox', msg: 'root has no viewBox' }); return res; }
  if (!svg.getAttribute('width') || !svg.getAttribute('height'))
    res.errors.push({ code: 'viewbox', msg: 'root needs width and height attributes (GitHub sizes the image from them)' });
  // Lay the SVG out at 1 CSS px per viewBox unit.
  svg.setAttribute('width', vb.width);
  svg.setAttribute('height', vb.height);
  svg.style.display = 'block';
  const R = svg.getBoundingClientRect();
  const box = (el) => {
    const r = el.getBoundingClientRect();
    return { x: r.left - R.left, y: r.top - R.top, w: r.width, h: r.height };
  };
  const label = (el) => (el.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 60);
  const inter = (a, b, pad = 0) =>
    a.x + pad < b.x + b.w && b.x + pad < a.x + a.w && a.y + pad < b.y + b.h && b.y + pad < a.y + a.h;
  const contains = (o, i, tol = 0.5) =>
    i.x >= o.x - tol && i.y >= o.y - tol && i.x + i.w <= o.x + o.w + tol && i.y + i.h <= o.y + o.h + tol;
  const tag = opts.wide ? 'wide-font' : null;
  const push = (kind, code, msg) => {
    if (tag) res.warnings.push({ code: tag, msg: code + ': ' + msg });
    else res[kind].push({ code, msg });
  };

  if (!opts.wide) {
    for (const t of ['script', 'foreignObject']) {
      if (svg.querySelector(t)) res.errors.push({ code: 'forbidden', msg: '<' + t + '> is not allowed' });
    }
    for (const im of svg.querySelectorAll('image')) {
      const h = im.getAttribute('href') || im.getAttribute('xlink:href') || '';
      if (!h.startsWith('data:')) res.errors.push({ code: 'forbidden', msg: 'external <image> ' + h });
    }
    for (const st of svg.querySelectorAll('style')) {
      if (/@import|@font-face|url\(\s*['"]?http/i.test(st.textContent))
        res.errors.push({ code: 'forbidden', msg: '<style> loads something external' });
    }
    const first = [...svg.children].find((c) => !['defs', 'title', 'desc', 'style', 'metadata'].includes(c.tagName));
    let okBg = false;
    if (first && first.tagName === 'rect') {
      const b = box(first);
      const fill = getComputedStyle(first).fill;
      okBg = b.x <= 0.5 && b.y <= 0.5 && b.w >= vb.width - 1 && b.h >= vb.height - 1 && fill && fill !== 'none' && !/rgba\(.*,\s*0\)/.test(fill);
    }
    if (!okBg) res.warnings.push({ code: 'background', msg: 'first element should be a full-size opaque background rect (GitHub dark mode shows a transparent SVG on black)' });
  }

  const texts = [...svg.querySelectorAll('text')].filter((t) => label(t).length && getComputedStyle(t).display !== 'none');
  const tb = texts.map((t) => ({ el: t, b: box(t), s: label(t) }));
  for (const { el, b, s } of tb) {
    if (b.x < -0.5 || b.y < -0.5 || b.x + b.w > vb.width + 0.5 || b.y + b.h > vb.height + 0.5)
      push('errors', 'outside', `"${s}" at (${b.x.toFixed(0)},${b.y.toFixed(0)}) ${b.w.toFixed(0)}x${b.h.toFixed(0)} leaves the ${vb.width}x${vb.height} viewBox`);
    if (!opts.wide) {
      const fsz = parseFloat(getComputedStyle(el).fontSize) * (el.getScreenCTM() ? Math.abs(el.getScreenCTM().a) : 1);
      if (fsz < 12.95) res.warnings.push({ code: 'small-font', msg: `"${s}" is ${fsz.toFixed(1)} px` });
    }
  }
  for (let i = 0; i < tb.length; i++)
    for (let j = i + 1; j < tb.length; j++)
      if (inter(tb[i].b, tb[j].b, 1)) push('errors', 'overlap', `"${tb[i].s}" overlaps "${tb[j].s}"`);

  const rects = [...svg.querySelectorAll('rect')].filter((r) => {
    const b = box(r);
    return !(b.w >= vb.width - 1 && b.h >= vb.height - 1) && b.w > 2 && b.h > 2;
  }).map((r) => ({ el: r, b: box(r) }));
  for (const { b, s } of tb)
    for (const r of rects)
      if (inter(b, r.b, 1) && !contains(r.b, b) && !contains(b, r.b))
        push('warnings', 'crosses-box', `"${s}" crosses the edge of a ${r.b.w.toFixed(0)}x${r.b.h.toFixed(0)} rect at (${r.b.x.toFixed(0)},${r.b.y.toFixed(0)})`);

  if (!opts.wide) {
    const shapes = [...svg.querySelectorAll('line, path, polyline')];
    for (const sh of shapes) {
      const cs = getComputedStyle(sh);
      if (cs.stroke === 'none' || parseFloat(cs.strokeWidth) === 0) continue;
      if (sh.closest('marker') || sh.closest('defs')) continue;
      let len = 0;
      try { len = sh.getTotalLength(); } catch (e) { continue; }
      if (!len) continue;
      const m = sh.getScreenCTM();
      const hits = new Set();
      for (let d = 0; d <= len; d += 1.5) {
        const p = sh.getPointAtLength(d);
        const x = m.a * p.x + m.c * p.y + m.e - R.left;
        const y = m.b * p.x + m.d * p.y + m.f - R.top;
        for (let k = 0; k < tb.length; k++) {
          const b = tb[k].b;
          if (x > b.x + 1.5 && x < b.x + b.w - 1.5 && y > b.y + 2 && y < b.y + b.h - 2) hits.add(k);
        }
      }
      for (const k of hits) res.warnings.push({ code: 'line-thru', msg: `a <${sh.tagName}> passes through "${tb[k].s}"` });
    }
  }
  res.viewBox = [vb.width, vb.height];
  res.textCount = tb.length;
  return res;
};

(async () => {
  const browser = await chromium.launch();
  const report = [];
  for (const f of files) {
    const abs = path.resolve(f);
    const name = path.basename(abs, '.svg');
    const dir = outDir ? path.resolve(outDir) : path.join(path.dirname(abs), '..', '.render');
    fs.mkdirSync(dir, { recursive: true });
    const src = fs.readFileSync(abs, 'utf8');
    const entry = { file: f, errors: [], warnings: [], png: {} };
    // Well-formedness.
    const page = await browser.newPage();
    const parseErr = await page.evaluate((s) => {
      const d = new DOMParser().parseFromString(s, 'image/svg+xml');
      const e = d.querySelector('parsererror');
      return e ? e.textContent.slice(0, 300) : null;
    }, src);
    if (parseErr) {
      entry.errors.push({ code: 'parse', msg: parseErr });
      report.push(entry);
      await page.close();
      continue;
    }
    // Lint with the declared fonts, then with a wider one.
    const body = src.replace(/<\?xml[^>]*>/, '');
    await page.setContent(`<!doctype html><html><head><style>html,body{margin:0;padding:0;background:#fff}</style></head><body>${body}</body></html>`);
    const lint = await page.evaluate(LINT, { wide: false });
    await page.setContent(`<!doctype html><html><head><style>html,body{margin:0;padding:0;background:#fff} svg text, svg tspan{font-family:'DejaVu Sans' !important}</style></head><body>${body}</body></html>`);
    const wide = await page.evaluate(LINT, { wide: true });
    entry.errors.push(...lint.errors);
    entry.warnings.push(...lint.warnings);
    // Only report wide-font problems that the normal font does not already have.
    const seen = new Set(lint.errors.concat(lint.warnings).map((x) => x.msg));
    for (const w of wide.warnings) if (!seen.has(w.msg.replace(/^[a-z-]+: /, ''))) entry.warnings.push(w);
    entry.viewBox = lint.viewBox;
    entry.textCount = lint.textCount;
    await page.close();
    if (!lint.viewBox) { report.push(entry); continue; }
    const [W, H] = lint.viewBox;
    const url = 'data:image/svg+xml;base64,' + Buffer.from(src).toString('base64');
    // Desktop: the image at its own width (GitHub's column is ~830 px), on GitHub's dark page too.
    for (const [suffix, bg] of [['desktop', '#ffffff'], ['desktop-dark', '#0d1117']]) {
      const p = await browser.newPage({ viewport: { width: Math.ceil(W) + 32, height: 400 }, deviceScaleFactor: 2 });
      await p.setContent(`<!doctype html><html><body style="margin:16px;background:${bg}"><img id="i" src="${url}" style="max-width:100%;display:block"></body></html>`);
      await p.waitForFunction(() => document.getElementById('i').complete);
      if (!(await p.evaluate(() => document.getElementById('i').naturalWidth))) throw new Error('image did not load: ' + abs);
      const out = path.join(dir, `${name}.${suffix}.png`);
      await (await p.$('#i')).screenshot({ path: out });
      entry.png[suffix] = out;
      await p.close();
    }
    // Phone: 390 px screen, 16 px gutters, as GitHub's mobile view scales the image down.
    {
      const p = await browser.newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 3 });
      await p.setContent(`<!doctype html><html><body style="margin:16px;background:#fff"><img id="i" src="${url}" style="max-width:100%;display:block"></body></html>`);
      await p.waitForFunction(() => document.getElementById('i').complete);
      if (!(await p.evaluate(() => document.getElementById('i').naturalWidth))) throw new Error('image did not load: ' + abs);
      const out = path.join(dir, `${name}.phone.png`);
      await (await p.$('#i')).screenshot({ path: out });
      entry.png.phone = out;
      await p.close();
    }
    // Zoom tiles: 400x400 CSS px of the image at 3x, to read small parts closely.
    {
      const p = await browser.newPage({ viewport: { width: Math.ceil(W), height: Math.ceil(H) }, deviceScaleFactor: 3 });
      await p.setContent(`<!doctype html><html><body style="margin:0;background:#fff"><img id="i" src="${url}" width="${W}" height="${H}" style="display:block"></body></html>`);
      await p.waitForFunction(() => document.getElementById('i').complete);
      if (!(await p.evaluate(() => document.getElementById('i').naturalWidth))) throw new Error('image did not load: ' + abs);
      entry.png.tiles = [];
      const T = 400;
      for (let y = 0, r = 0; y < H; y += T - 40, r++) {
        for (let x = 0, c = 0; x < W; x += T - 40, c++) {
          const clip = { x, y, width: Math.min(T, W - x), height: Math.min(T, H - y) };
          if (clip.width < 40 || clip.height < 40) continue;
          const out = path.join(dir, `${name}.tile-r${r}c${c}.png`);
          await p.screenshot({ path: out, clip });
          entry.png.tiles.push(out);
          if (x + T >= W) break;
        }
        if (y + T >= H) break;
      }
      await p.close();
    }
    report.push(entry);
  }
  await browser.close();
  const summary = report.map((e) => ({
    file: e.file,
    viewBox: e.viewBox,
    errors: e.errors,
    warnings: e.warnings,
    png: e.png,
    verdict: e.errors.length ? 'FIX ERRORS' : e.warnings.length ? 'LOOK AT WARNINGS IN THE IMAGE' : 'LINT CLEAN (still look at the image)',
  }));
  console.log(JSON.stringify(summary, null, 2));
  process.exit(report.some((e) => e.errors.length) ? 1 : 0);
})();
