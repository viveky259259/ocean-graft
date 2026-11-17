# notes-app

A small notes service used as the base repository when trying shoal: Hono on
Node, tested with Vitest, run with tsx.

```bash
npm ci
npm test
npm run typecheck
PORT=3000 npm start
```

Routes: `GET /` (HTML page), `GET /notes?limit=&offset=`, `GET /notes/:id`,
`POST /notes`, `DELETE /notes/:id`, `GET /health`.
