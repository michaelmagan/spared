import { DurableObject } from "cloudflare:workers";
import PAGE from "page.html";

function escape(s) {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export class Items extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(
      "CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, text TEXT NOT NULL, created_at INTEGER)",
    );
    const n = this.sql.exec("SELECT count(*) AS n FROM items").one().n;
    if (n === 0) {
      ctx.storage.transactionSync(() => {
        for (let i = 1; i <= 100; i++) {
          this.sql.exec(
            "INSERT INTO items(text, created_at) VALUES (?, ?)",
            `Seed item ${i}`,
            1700000000 + i,
          );
        }
      });
    }
  }

  latest() {
    return this.sql
      .exec("SELECT id, text, created_at FROM items ORDER BY id DESC LIMIT 20")
      .toArray();
  }

  async fetch(request) {
    const url = new URL(request.url);
    const method = request.method;
    if (method === "GET" && url.pathname === "/") {
      let list = "";
      for (const it of this.latest()) {
        list += `<li><span>${escape(it.text)}</span><time>${it.created_at}</time></li>\n`;
      }
      return new Response(PAGE.replace("<!--ITEMS-->\n", () => list), {
        headers: { "content-type": "text/html; charset=utf-8" },
      });
    }
    if (url.pathname === "/api/items") {
      if (method === "GET") {
        return Response.json(this.latest());
      }
      if (method === "POST") {
        let body;
        try {
          body = await request.json();
          if (typeof body.text !== "string") throw new Error("text");
        } catch {
          return new Response("bad json\n", { status: 400 });
        }
        const created_at = Math.floor(Date.now() / 1000);
        const id = this.sql
          .exec(
            "INSERT INTO items(text, created_at) VALUES (?, ?) RETURNING id",
            body.text,
            created_at,
          )
          .one().id;
        return Response.json({ id, text: body.text, created_at }, { status: 201 });
      }
    }
    return new Response("not found\n", { status: 404 });
  }
}

export default {
  async fetch(request, env) {
    const id = new URL(request.url).searchParams.get("id") ?? "main";
    const stub = env.ITEMS.get(env.ITEMS.idFromName(id));
    return stub.fetch(request);
  },
};
