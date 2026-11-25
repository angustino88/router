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
make dev-up                     # docker compose -f docker-compose.dev.yml up -d --build
# http://localhost:8080/admin/
```

| Path | Content |
|---|---|
| `src/` | the Router service, one folder per feature (`routing`, `admin`, `jobs`, `nginx`, `db`, `http`, `openapi`) |
| `crates/api-contracts/` | OpenAPI contract crate used by `src/openapi/` (Cargo workspace member) |
| `deploy/nginx/{dev,prod}/` | nginx configuration per environment; `deploy/nginx/watch-maps.sh` reloads nginx on map changes |
| `deploy/certs/` | extra CA certificates (`*.crt`) |
| `docker-compose.dev.yml` | development stack: Router, nginx, PostgreSQL |
| `docker-compose.prod.yml` | production stack: Router, nginx (HTTPS); external PostgreSQL |
| `docs/DEPLOYMENT.md` | deployment and operations guide |
| `.github/workflows/ci.yml` | CI: fmt, clippy, tests with PostgreSQL, nginx/compose validation, image build |

Full guide (frontend/backend wiring, configuration, API, production, operations):
**[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)**

## Development

```bash
make help                                                                   # all targets
make check                                                                  # fmt, clippy, tests (as in CI)
TEST_DATABASE_URL=postgres://user:pass@localhost:5432/test_db make test-db  # + database tests (DELETES data in that DB)
cargo run -- --help                                                         # serve, migrate, sync-users, sync-agents, backend, openapi
```

## Operations at a glance

| Endpoint | Purpose |
|---|---|
| `GET /healthz` | liveness (process answers) |
| `GET /readyz` | readiness (database answers) |
| `GET /metrics` | Prometheus metrics; blocked on the public nginx listener, scrape `router:8000` internally |
| `X-Request-ID` | accepted or generated on every request, returned in the response and logged |
