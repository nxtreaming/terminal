# feedback-ingest

Lightweight HTTP service that stores user feedback from browser-use-terminal into Postgres.

## Environment variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | Yes | — | Postgres connection string, e.g. `postgres://user:pass@host/db` |
| `PORT` | No | `8080` | TCP port the server binds to |

## API

### `GET /health`
Returns `{"ok":true}` with HTTP 200. Use as the Railway healthcheck endpoint.

### `POST /feedback`
Inserts a feedback row. All fields except `category` are optional.

```json
{
  "category": "bug|bad_result|good_result|safety_check|other",
  "description": "string or null",
  "include_logs": true,
  "session_id": "string or null",
  "session_events": [<arbitrary JSON>],
  "app_version": "0.1.2",
  "os": "macos",
  "model": "gpt-4o",
  "surface": "tui",
  "install_id": "uuid-string"
}
```

1. Install the Railway CLI and log in:
   ```bash
   npm i -g @railway/cli
   railway login
   ```

2. From the **repo root**, link to your Railway project:
   ```bash
   railway link
   ```

3. Add a Postgres plugin in the Railway dashboard and copy the `DATABASE_URL`.
   Then set it as a service variable (or use a Railway reference variable):
   ```bash
   railway variables set DATABASE_URL="postgres://..."
   ```

4. Deploy from the `feedback-ingest/` subdirectory:
   ```bash
   cd feedback-ingest
   railway up
   ```

   Railway will build the Dockerfile and start the service. The `/health`
   endpoint is used as the healthcheck.

## Local development

```bash
cd feedback-ingest
DATABASE_URL="postgres://postgres:password@localhost/feedback" cargo run
```

## Workspace isolation

This crate is listed under `exclude` in the root `Cargo.toml` so it is never
compiled as part of `cargo build -p browser-use-tui` or any other workspace
command run from the repo root.
