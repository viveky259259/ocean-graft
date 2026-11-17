import { describe, expect, it } from "vitest";
import { createApp } from "../src/app.js";

function post(app: ReturnType<typeof createApp>, body: unknown) {
  return app.request("/notes", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

describe("notes api", () => {
  it("creates and fetches a note", async () => {
    const app = createApp();
    const created = await post(app, { title: "First", body: "hello" });
    expect(created.status).toBe(201);
    const note = await created.json();
    const fetched = await app.request(`/notes/${note.id}`);
    expect(await fetched.json()).toMatchObject({ title: "First", body: "hello" });
  });

  it("lists notes newest first", async () => {
    const app = createApp();
    await post(app, { title: "Older" });
    await post(app, { title: "Newer" });
    const page = await (await app.request("/notes")).json();
    expect(page.total).toBe(2);
    expect(page.items.map((n: { title: string }) => n.title)).toEqual(["Newer", "Older"]);
  });

  it("deletes a note", async () => {
    const app = createApp();
    const note = await (await post(app, { title: "Temporary" })).json();
    expect((await app.request(`/notes/${note.id}`, { method: "DELETE" })).status).toBe(204);
    expect((await app.request(`/notes/${note.id}`)).status).toBe(404);
  });

  it("renders the home page", async () => {
    const app = createApp();
    await post(app, { title: "Shown on the page" });
    const html = await (await app.request("/")).text();
    expect(html).toContain("Shown on the page");
    expect(html).toContain("1 note");
  });
});
