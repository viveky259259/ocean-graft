import { Hono } from "hono";
import { NoteStore } from "./store.js";
import { renderPage } from "./views.js";

function intParam(value: string | undefined): number | undefined {
  if (value === undefined) return undefined;
  const parsed = Number.parseInt(value, 10);
  return Number.isNaN(parsed) ? undefined : parsed;
}

export function createApp(store = new NoteStore()) {
  const app = new Hono();

  app.get("/health", (c) => c.json({ ok: true }));

  app.get("/", (c) => {
    const page = store.list();
    return c.html(renderPage(page.items, page.total));
  });

  app.get("/notes", (c) => {
    const limit = intParam(c.req.query("limit"));
    const offset = intParam(c.req.query("offset"));
    return c.json(store.list({ limit, offset }));
  });

  app.get("/notes/:id", (c) => {
    const note = store.get(Number(c.req.param("id")));
    return note ? c.json(note) : c.json({ error: "not found" }, 404);
  });

  app.post("/notes", async (c) => {
    const body = await c.req.json<{ title?: unknown; body?: unknown }>();
    const note = store.create({
      title: String(body.title ?? "Untitled"),
      body: body.body === undefined ? "" : String(body.body),
    });
    return c.json(note, 201);
  });

  app.delete("/notes/:id", (c) => {
    const deleted = store.delete(Number(c.req.param("id")));
    return deleted ? c.body(null, 204) : c.json({ error: "not found" }, 404);
  });

  return app;
}
