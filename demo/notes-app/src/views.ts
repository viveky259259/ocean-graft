import type { Note } from "./store.js";

const escape = (text: string) =>
  text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

export function renderPage(notes: Note[], total: number): string {
  const session = process.env.SHOAL_SESSION ?? "local";
  const items = notes
    .map(
      (note) => `
      <li>
        <h2>${escape(note.title)}</h2>
        <p>${escape(note.body)}</p>
        <time>${note.createdAt.slice(0, 16).replace("T", " ")}</time>
      </li>`,
    )
    .join("");
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Notes</title>
  <style>
    body { font: 15px/1.5 system-ui, sans-serif; max-width: 640px; margin: 40px auto; padding: 0 16px; color: #1d2330; }
    header { display: flex; align-items: baseline; justify-content: space-between; }
    .session { font: 12px ui-monospace, monospace; background: #e8eefc; color: #2b4ea2; padding: 2px 8px; border-radius: 99px; }
    ul { list-style: none; padding: 0; display: grid; gap: 12px; }
    li { border: 1px solid #dfe3ea; border-radius: 8px; padding: 12px 14px; }
    h2 { font-size: 16px; margin: 0 0 4px; }
    p { margin: 0 0 6px; color: #4a5263; }
    time { font-size: 12px; color: #7b8496; }
  </style>
</head>
<body>
  <header><h1>Notes</h1><span class="session">${escape(session)}</span></header>
  <p>${total} note${total === 1 ? "" : "s"}</p>
  <ul>${items}</ul>
</body>
</html>`;
}
