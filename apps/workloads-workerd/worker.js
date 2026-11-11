// Workload comparison app, JavaScript side. Twin of apps/workloads-spared/src/main.rs: same routes, same generated data
// (same LCG and seeds), same JSON results. Inputs are generated once per isolate and cached; each request does the work.
import { DurableObject } from "cloudflare:workers";
import { marked } from "marked";

// ---------- shared deterministic generator (same constants as the Rust twin) ----------

class Lcg {
  constructor(seed) {
    this.s = seed >>> 0;
  }
  next() {
    this.s = (Math.imul(this.s, 1664525) + 1013904223) >>> 0;
    return this.s;
  }
  below(n) {
    return (this.next() >>> 8) % n;
  }
  word() {
    const len = 3 + this.below(8);
    let w = "";
    for (let i = 0; i < len; i++) w += String.fromCharCode(97 + this.below(26));
    return w;
  }
}

function genBytes(n, seed) {
  const r = new Lcg(seed);
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i++) out[i] = r.next() >>> 24;
  return out;
}

const CATS = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"];
const TAGS = ["new", "sale", "hot", "eco", "gift", "bulk"];
const pad2 = (n) => String(n).padStart(2, "0");

function lazy(f) {
  let v;
  return () => (v ??= f());
}

const vocab = lazy(() => {
  const r = new Lcg(7);
  return Array.from({ length: 4000 }, () => r.word());
});

const text = lazy(() => {
  const v = vocab();
  const r = new Lcg(42);
  const parts = [];
  let len = 0;
  while (len < 2_000_000) {
    let t;
    const k = r.below(100);
    if (k === 0) {
      const y = 1990 + r.below(40);
      const m = 1 + r.below(12);
      const d = 1 + r.below(28);
      t = `${y}-${pad2(m)}-${pad2(d)}`;
    } else if (k === 1) {
      const a = r.below(4000);
      const b = r.below(4000);
      t = `${v[a]}@${v[b]}.com`;
    } else if (k === 2) {
      t = "";
      for (let i = 0; i < 3; i++) t += String.fromCharCode(97 + r.below(26));
      t += r.below(100000);
    } else {
      const a = r.below(4000);
      const b = r.below(4000);
      t = v[Math.floor((a * b) / 4000)];
    }
    t += r.below(12) === 0 ? "\n" : " ";
    parts.push(t);
    len += t.length;
  }
  return parts.join("").slice(0, 2_000_000);
});

const hex = (buf) => [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
const sha256hex = async (data) =>
  hex(await crypto.subtle.digest("SHA-256", typeof data === "string" ? new TextEncoder().encode(data) : data));

// ---------- workloads ----------

const shaPayload = lazy(() => genBytes(256 * 1024, 71));
const SHA_ROUNDS = 16;

async function wSha256() {
  const buf = shaPayload().slice();
  let last;
  for (let i = 0; i < SHA_ROUNDS; i++) {
    last = new Uint8Array(await crypto.subtle.digest("SHA-256", buf));
    buf.set(last, 0);
  }
  return { bytes: buf.length, rounds: SHA_ROUNDS, digest: hex(last) };
}

const recordsJson = lazy(() => {
  const v = vocab();
  const r = new Lcg(11);
  const recs = [];
  for (let i = 0; i < 2000; i++) {
    const a = r.below(4000);
    const b = r.below(4000);
    const category = CATS[r.below(8)];
    const price_cents = 100 + r.below(100000);
    const qty = r.below(20);
    const active = r.below(4) !== 0;
    const n = r.below(4);
    const tags = [];
    for (let k = 0; k < n; k++) tags.push(TAGS[r.below(6)]);
    recs.push({ id: i, name: `${v[a]} ${v[b]}`, category, price_cents, qty, active, tags });
  }
  return JSON.stringify(recs);
});

async function wJson(verify) {
  const input = recordsJson();
  const recs = JSON.parse(input);
  const kept = recs.filter((r) => r.active && r.qty > 0);
  const groups = new Map();
  for (const r of kept) {
    let g = groups.get(r.category);
    if (!g) {
      g = { category: r.category, count: 0, units: 0, revenue_cents: 0, on_sale: 0 };
      groups.set(r.category, g);
    }
    g.count++;
    g.units += r.qty;
    g.revenue_cents += r.price_cents * r.qty;
    if (r.tags.includes("sale")) g.on_sale++;
  }
  const sorted = [...groups.values()].sort((a, b) => (a.category < b.category ? -1 : a.category > b.category ? 1 : 0));
  const items = kept.map((r) => ({
    id: r.id,
    name: r.name.toUpperCase(),
    total_cents: r.price_cents * r.qty,
    tags: r.tags.join(","),
  }));
  const out = JSON.stringify({ groups: sorted, items });
  const res = { in_len: input.length, kept: kept.length, groups: sorted, out_len: out.length };
  if (verify) {
    res.in_sha256 = await sha256hex(input);
    res.out_sha256 = await sha256hex(out);
  }
  return res;
}

const markdown = lazy(() => {
  const v = vocab();
  const r = new Lcg(51);
  const w = () => v[r.below(4000)];
  let s = "";
  let sec = 0;
  while (s.length < 50_000) {
    sec++;
    s += `## Section ${sec} ${w()}\n\n`;
    const paras = 1 + r.below(3);
    for (let p = 0; p < paras; p++) {
      const n = 30 + r.below(30);
      const words = [];
      for (let k = 0; k < n; k++) {
        const kind = r.below(20);
        const t = w();
        words.push(
          kind === 0 ? `*${t}*` : kind === 1 ? `**${t}**` : kind === 2 ? `\`${t}\`` : kind === 3 ? `[${t}](https://example.com/${t})` : t,
        );
      }
      s += words.join(" ") + ".\n\n";
    }
    if (r.below(2) === 0) {
      const items = 3 + r.below(5);
      for (let i = 0; i < items; i++) {
        s += "-";
        const n = 4 + r.below(7);
        for (let k = 0; k < n; k++) s += " " + w();
        s += "\n";
      }
      s += "\n";
    }
    if (r.below(3) === 0) {
      const items = 3 + r.below(4);
      for (let i = 0; i < items; i++) {
        s += `${i + 1}.`;
        const n = 4 + r.below(7);
        for (let k = 0; k < n; k++) s += " " + w();
        s += "\n";
      }
      s += "\n";
    }
    if (r.below(4) === 0) {
      s += "```\n";
      const lines = 2 + r.below(4);
      for (let i = 0; i < lines; i++) {
        const a = w();
        const b = w();
        s += `let ${a} = ${b}(${r.below(1000)});\n`;
      }
      s += "```\n\n";
    }
  }
  return s;
});

async function wMarkdown(verify) {
  const md = markdown();
  const html = marked.parse(md);
  const res = { md_len: md.length, html_len: html.length };
  if (verify) {
    const tags = ["<h2>", "<p>", "<ul>", "<ol>", "<li>", "<em>", "<strong>", "<code>", "<pre>", "<a "];
    res.md_sha256 = await sha256hex(md);
    res.tags = Object.fromEntries(tags.map((t) => [t, html.split(t).length - 1]));
    res.html_sha256 = await sha256hex(html);
  }
  return res;
}

const SIEVE_N = 2_000_000;

function wSieve() {
  const n = SIEVE_N;
  const composite = new Uint8Array(n + 1);
  let count = 0;
  let sum = 0;
  for (let i = 2; i <= n; i++) {
    if (!composite[i]) {
      count++;
      sum += i;
      for (let j = i * i; j <= n; j += i) composite[j] = 1;
    }
  }
  return { n, primes: count, sum };
}

const NBODY_STEPS = 200_000;

function nbodyBodies() {
  const PI = Math.PI;
  const SOLAR_MASS = 4 * PI * PI;
  const DAYS = 365.24;
  const b = (x, y, z, vx, vy, vz, m) => ({ x, y, z, vx: vx * DAYS, vy: vy * DAYS, vz: vz * DAYS, mass: m * SOLAR_MASS });
  const v = [
    { x: 0, y: 0, z: 0, vx: 0, vy: 0, vz: 0, mass: SOLAR_MASS },
    b(4.8414314424647209, -1.16032004402742839, -1.03622044471123109e-1, 1.66007664274403694e-3, 7.69901118419740425e-3, -6.90460016972063023e-5, 9.54791938424326609e-4),
    b(8.34336671824457987, 4.12479856412430479, -4.03523417114321381e-1, -2.76742510726862411e-3, 4.99852801234917238e-3, 2.30417297573763929e-5, 2.85885980666130812e-4),
    b(1.2894369562139131e1, -1.51111514016986312e1, -2.23307578892655734e-1, 2.96460137564761618e-3, 2.3784717395948095e-3, -2.96589568540237556e-5, 4.36624404335156298e-5),
    b(1.53796971148509165e1, -2.59193146099879641e1, 1.79258772950371181e-1, 2.68067772490389322e-3, 1.62824170038242295e-3, -9.5159225451971587e-5, 5.15138902046611451e-5),
  ];
  let px = 0, py = 0, pz = 0;
  for (const b of v) {
    px += b.vx * b.mass;
    py += b.vy * b.mass;
    pz += b.vz * b.mass;
  }
  v[0].vx = -px / SOLAR_MASS;
  v[0].vy = -py / SOLAR_MASS;
  v[0].vz = -pz / SOLAR_MASS;
  return v;
}

function energy(v) {
  let e = 0;
  for (let i = 0; i < v.length; i++) {
    const b = v[i];
    e += 0.5 * b.mass * (b.vx * b.vx + b.vy * b.vy + b.vz * b.vz);
    for (let j = i + 1; j < v.length; j++) {
      const c = v[j];
      const dx = b.x - c.x, dy = b.y - c.y, dz = b.z - c.z;
      e -= (b.mass * c.mass) / Math.sqrt(dx * dx + dy * dy + dz * dz);
    }
  }
  return e;
}

function advance(v, dt) {
  const n = v.length;
  for (let i = 0; i < n; i++) {
    const bi = v[i];
    for (let j = i + 1; j < n; j++) {
      const bj = v[j];
      const dx = bi.x - bj.x;
      const dy = bi.y - bj.y;
      const dz = bi.z - bj.z;
      const d2 = dx * dx + dy * dy + dz * dz;
      const mag = dt / (d2 * Math.sqrt(d2));
      bi.vx -= dx * bj.mass * mag;
      bi.vy -= dy * bj.mass * mag;
      bi.vz -= dz * bj.mass * mag;
      bj.vx += dx * bi.mass * mag;
      bj.vy += dy * bi.mass * mag;
      bj.vz += dz * bi.mass * mag;
    }
  }
  for (const b of v) {
    b.x += dt * b.vx;
    b.y += dt * b.vy;
    b.z += dt * b.vz;
  }
}

function wNbody() {
  const v = nbodyBodies();
  const e0 = energy(v);
  for (let i = 0; i < NBODY_STEPS; i++) advance(v, 0.01);
  return { steps: NBODY_STEPS, energy_before: e0.toFixed(9), energy_after: energy(v).toFixed(9) };
}

const IMG = 512;
const image = lazy(() => genBytes(IMG * IMG * 4, 61));

async function wPixels(verify) {
  const src = image();
  const w = IMG, h = IMG;
  const gray = new Uint8ClampedArray(w * h);
  for (let i = 0; i < w * h; i++) {
    gray[i] = (77 * src[4 * i] + 150 * src[4 * i + 1] + 29 * src[4 * i + 2]) >> 8;
  }
  const R = 2;
  const tmp = new Uint8ClampedArray(w * h);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      let s = 0;
      for (let k = -R; k <= R; k++) {
        const xx = Math.min(w - 1, Math.max(0, x + k));
        s += gray[y * w + xx];
      }
      tmp[y * w + x] = (s / 5) | 0;
    }
  }
  const out = new Uint8ClampedArray(w * h * 4);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      let s = 0;
      for (let k = -R; k <= R; k++) {
        const yy = Math.min(h - 1, Math.max(0, y + k));
        s += tmp[yy * w + x];
      }
      const v = (s / 5) | 0;
      const i = 4 * (y * w + x);
      out[i] = v;
      out[i + 1] = v;
      out[i + 2] = v;
      out[i + 3] = 255;
    }
  }
  const res = { bytes: out.length, sample: [out[0], out[4 * 1000], out[4 * 131071], out[out.length - 4]] };
  if (verify) res.out_sha256 = await sha256hex(out);
  return res;
}

const sortInts = lazy(() => {
  const r = new Lcg(91);
  return Array.from({ length: 200_000 }, () => r.next());
});
const sortStrs = lazy(() => {
  const r = new Lcg(92);
  return Array.from({ length: 50_000 }, () => {
    const len = 6 + r.below(9);
    let s = "";
    for (let i = 0; i < len; i++) s += String.fromCharCode(97 + r.below(26));
    return s;
  });
});

async function wSort(verify) {
  const a = sortInts().slice();
  a.sort((x, y) => x - y);
  const s = sortStrs().slice();
  s.sort();
  const res = { ints: [a[0], a[100_000], a[a.length - 1]], strs: [s[0], s[25_000], s[s.length - 1]] };
  if (verify) {
    res.ints_sha256 = await sha256hex(a.join(","));
    res.strs_sha256 = await sha256hex(s.join(","));
  }
  return res;
}

function wWordfreq() {
  const t = text();
  const counts = new Map();
  let tokens = 0;
  for (const w of t.split(/\s+/)) {
    if (!w) continue;
    tokens++;
    counts.set(w, (counts.get(w) ?? 0) + 1);
  }
  const top = [...counts]
    .sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0))
    .slice(0, 10);
  return { text_len: t.length, tokens, distinct: counts.size, top };
}

const tableRows = lazy(() => {
  const v = vocab();
  const r = new Lcg(81);
  return Array.from({ length: 10_000 }, (_, i) => {
    const a = v[r.below(4000)];
    const k = r.below(10);
    const sep = k === 0 ? " & " : k === 1 ? " <b> " : " ";
    const b = v[r.below(4000)];
    const category = CATS[r.below(8)];
    const cents = r.below(1_000_000);
    return { id: i + 1, name: `${a}${sep}${b}`, category, cents };
  });
});

function escapeHtml(s) {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

async function wHtml(verify) {
  const body = tableRows()
    .map(
      (r) =>
        `<tr><td>${r.id}</td><td>${escapeHtml(r.name)}</td><td>${r.category}</td><td class="num">$${Math.floor(r.cents / 100)}.${pad2(r.cents % 100)}</td></tr>`,
    )
    .join("\n");
  const html = `<table>\n<thead><tr><th>ID</th><th>Name</th><th>Category</th><th>Amount</th></tr></thead>\n<tbody>\n${body}\n</tbody>\n</table>\n`;
  const mid = Math.floor(html.length / 2);
  const res = { html_len: html.length, sample: html.slice(mid, mid + 40) };
  if (verify) res.html_sha256 = await sha256hex(html);
  return res;
}

const GZIP_LEN = 1_000_000;
const gzipInput = lazy(() => new TextEncoder().encode(text().slice(0, GZIP_LEN)));

async function wGzip(verify) {
  const input = gzipInput();
  const stream = new Blob([input]).stream().pipeThrough(new CompressionStream("gzip"));
  const out = new Uint8Array(await new Response(stream).arrayBuffer());
  const res = { in_len: input.length, out_len: out.length };
  if (verify) {
    let bin = "";
    for (let i = 0; i < out.length; i++) bin += String.fromCharCode(out[i]);
    res.gzip_b64 = btoa(bin);
  }
  return res;
}

const PATTERNS = [/\b\d{4}-\d{2}-\d{2}\b/g, /[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}/g, /\b[a-z]{3}\d+\b/g, /\b\w*(?:qu|xz|zz)\w*\b/g];

function wRegex() {
  const t = text();
  return { text_len: t.length, counts: PATTERNS.map((re) => (t.match(re) ?? []).length) };
}

const PBKDF2_ITERS = 10_000;

async function wPbkdf2() {
  const enc = new TextEncoder();
  const salt = enc.encode("spared-workloads-salt");
  const keys = [];
  for (let i = 0; i < 4; i++) {
    const key = await crypto.subtle.importKey("raw", enc.encode(`correct horse battery staple ${i}`), "PBKDF2", false, ["deriveBits"]);
    const bits = await crypto.subtle.deriveBits({ name: "PBKDF2", hash: "SHA-256", salt, iterations: PBKDF2_ITERS }, key, 256);
    keys.push(hex(bits));
  }
  return { iterations: PBKDF2_ITERS, keys };
}

// ---------- SQLite workloads, in a SQLite-backed Durable Object as apps/demo-workerd does ----------

const AGG_ROWS = 50_000;
const WRITE_ROWS = 10_000;

export class Db extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(
      "CREATE TABLE IF NOT EXISTS agg(id INTEGER PRIMARY KEY, cat INTEGER NOT NULL, amount INTEGER NOT NULL, label TEXT NOT NULL)",
    );
    this.sql.exec(
      "CREATE TABLE IF NOT EXISTS w(id INTEGER PRIMARY KEY, cat INTEGER NOT NULL, amount INTEGER NOT NULL, label TEXT NOT NULL)",
    );
  }

  seed() {
    const n = this.sql.exec("SELECT count(*) AS n FROM agg").one().n;
    if (n !== AGG_ROWS) {
      const v = vocab();
      const r = new Lcg(21);
      this.ctx.storage.transactionSync(() => {
        this.sql.exec("DELETE FROM agg");
        for (let i = 0; i < AGG_ROWS; i++) {
          const cat = r.below(10);
          const amount = r.below(1_000_000);
          const label = v[r.below(4000)];
          this.sql.exec("INSERT INTO agg(id, cat, amount, label) VALUES (?, ?, ?, ?)", i + 1, cat, amount, label);
        }
      });
    }
    return { rows: this.sql.exec("SELECT count(*) AS n FROM agg").one().n };
  }

  agg() {
    let count = 0, sum = 0, min = Infinity, max = -Infinity;
    const catCount = new Array(10).fill(0);
    const catSum = new Array(10).fill(0);
    const labels = new Set();
    for (const row of this.sql.exec("SELECT cat, amount, label FROM agg")) {
      count++;
      sum += row.amount;
      if (row.amount < min) min = row.amount;
      if (row.amount > max) max = row.amount;
      catCount[row.cat]++;
      catSum[row.cat] += row.amount;
      labels.add(row.label);
    }
    return { count, sum, min, max, mean: Math.floor(sum / count), cat_count: catCount, cat_sum: catSum, distinct_labels: labels.size };
  }

  write() {
    const v = vocab();
    const r = new Lcg(31);
    this.ctx.storage.transactionSync(() => {
      this.sql.exec("DELETE FROM w");
      for (let i = 0; i < WRITE_ROWS; i++) {
        const cat = r.below(10);
        const amount = r.below(1_000_000);
        const label = v[r.below(4000)];
        this.sql.exec("INSERT INTO w(id, cat, amount, label) VALUES (?, ?, ?, ?)", i + 1, cat, amount, label);
      }
    });
    const row = this.sql.exec("SELECT count(*) AS n, sum(amount) AS s FROM w").one();
    return { rows: row.n, sum: row.s };
  }

  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === "/w/sql-seed") return Response.json(this.seed());
    if (path === "/w/sql-agg") return Response.json(this.agg());
    if (path === "/w/sql-write") return Response.json(this.write());
    return new Response("not found\n", { status: 404 });
  }
}

// ---------- routing ----------

const ROUTES = {
  "/w/noop": () => ({ ok: true }),
  "/w/sha256": wSha256,
  "/w/json": wJson,
  "/w/markdown": wMarkdown,
  "/w/sieve": wSieve,
  "/w/nbody": wNbody,
  "/w/pixels": wPixels,
  "/w/sort": wSort,
  "/w/wordfreq": wWordfreq,
  "/w/html": wHtml,
  "/w/gzip": wGzip,
  "/w/regex": wRegex,
  "/w/pbkdf2": wPbkdf2,
};

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname.startsWith("/w/sql-")) {
      const stub = env.DB.get(env.DB.idFromName("main"));
      return stub.fetch(request);
    }
    const route = ROUTES[url.pathname];
    if (!route) return new Response("not found\n", { status: 404 });
    return Response.json(await route(url.searchParams.get("verify") === "1"));
  },
};
