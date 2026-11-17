import { serve } from "@hono/node-server";
import { createApp } from "./app.js";
import { NoteStore } from "./store.js";

const store = new NoteStore();
if (process.env.SEED !== "0") {
  store.create({ title: "Welcome", body: "This is a small notes service." });
  store.create({ title: "Groceries", body: "Oat milk, coffee, lemons" });
  store.create({ title: "Ideas", body: "Try running four agents at once" });
}

const port = Number(process.env.PORT ?? 3000);
serve({ fetch: createApp(store).fetch, port }, (info) => {
  console.log(`notes-app listening on http://localhost:${info.port}`);
});
