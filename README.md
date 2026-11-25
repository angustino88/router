# LabMango Router

Routes users and agents to the backend of their department when LabMango runs **several department backends**
behind one frontend.

- Public routing API: `POST /routing/auth/backend-info/by-id-token/` (users, Microsoft ID token) and
  `POST /routing/auth/backend-info/by-agent-id/` (agents) answer the backend prefix `/t/<prefix_key>`.
- nginx proxies `/t/<prefix_key>/...` to that backend (API, WebSockets, agents).
- Continuous routing sync from every backend's private `/internal/*` API (snapshot + long-poll of the `users`
  and `agents` streams, per the backend's [routing sync spec](https://github.com/angustino88/lab-mango/blob/main/docs/agent-platform/routing-sync.md)).
- Admin page `/admin/`: backends and their URLs (nginx reloads by itself), sync status, conflicts, schedules.
- OpenAPI document for Apidog: `tenant-resolver openapi` or `GET /openapi.json`.

```mermaid
flowchart LR
    FE["Frontend (VITE_API_MODE=router)"] -->|"1. by-id-token"| NG["nginx (Router)"]
    FE -->|"2. /t/&lt;prefix&gt;/api/..."| NG
    NG --> RT["Router service (Rust)"]
    NG -->|"/t/&lt;prefix&gt;/..."| B1["Backend dept A"]
    NG -->|"/t/&lt;prefix&gt;/..."| B2["Backend dept B"]
    RT -->|"GET /internal/* via nginx :8080"| B1
    RT -->|"GET /internal/* via nginx :8080"| B2
    RT --> DB[("PostgreSQL")]
```

A single-department deployment does not need the Router: run the frontend with `VITE_API_MODE=direct` against the
backend.

## Quick start (local)

```bash
cp .env.example .env            # edit ADMIN_PASSWORD, ROUTING_SYNC_TOKEN, CORS_ALLOWED_ORIGINS…
docker compose --profile db up -d --build
# http://localhost:8080/admin/
```

| Folder | Content |
|---|---|
| `src/` | Rust code, one folder per feature (`routing`, `admin`, `jobs`, `nginx`, `db`, `http`, `openapi`) |
| `api-contracts-rs/` | the `api-contracts` crate (OpenAPI contracts), used by `src/openapi/` |
| `deploy/` | nginx configuration (local, production), the nginx reload watcher, extra CA certificates |
| `docs/` | deployment and operations guide |

Full guide (frontend/backend wiring, configuration, API, production, operations):
**[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)**

## Development

```bash
cargo build
cargo test                                                                  # unit tests
TEST_DATABASE_URL=postgres://user:pass@localhost:5432/test_db cargo test    # + database tests (DELETES data in that DB)
cargo run -- --help                                                         # serve, migrate, sync-users, sync-agents, backend, openapi
```
