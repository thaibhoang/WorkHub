# WorkHub UI

React 19 + Vite + Tailwind v4 + shadcn/ui. Talks to the Rust API from `wh serve`.

```bash
npm install
npm run dev     # http://localhost:5173, proxies /api to http://127.0.0.1:7070 (override with WH_API)
npm run build   # web/dist, served by `wh serve` at /
```

Pages: Runs, Run detail (gate review, resume, timeline, transcripts), Inbox, Dashboard, Factory.
Live updates come from the SSE feed at `/api/events/stream` (`src/lib/live.tsx`).
