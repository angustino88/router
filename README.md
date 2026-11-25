# LabMango Router

Routes users and agents to the backend of their department when LabMango runs **several department backends**
behind one frontend.

- `POST /routing/resolve/user` (Microsoft ID token) and `POST /routing/resolve/agent` (agent UUID) return the
  backend that serves the caller, with its path prefix `/t/<prefix_key>`.
- nginx proxies `/t/<prefix_key>/...` to that backend: API, WebSockets and agents.
- The Router keeps its routing table in sync with every backend's private `/internal/*` API (snapshot and
  long-poll of the `users` and `agents` streams), following the backend's routing-sync spec.
- `/admin/`: backend registry, nginx status, sync status and conflicts, jobs and run history.
- OpenAPI 3.1: [`openapi/openapi.json`](openapi/openapi.json).

```mermaid
flowchart LR
    FE["Frontend (VITE_API_MODE=router)"] -->|"1. POST /routing/resolve/user"| NG["nginx"]
    FE -->|"2. /t/&lt;prefix&gt;/api/..."| NG
    NG --> RT["Router service"]
    NG -->|"/t/&lt;prefix&gt;/..."| B1["Backend dept A"]
    NG -->|"/t/&lt;prefix&gt;/..."| B2["Backend dept B"]
    RT -->|"GET /internal/* via nginx :8080"| B1
    RT -->|"GET /internal/* via nginx :8080"| B2
    RT --> DB[("PostgreSQL")]
```

A single-department deployment does not need the Router: the frontend runs with `VITE_API_MODE=direct`.

## Quick start

```bash
cp .env.example .env            # ADMIN_PASSWORD, AAD_TENANT_ID, AAD_AUDIENCE, ROUTING_SYNC_TOKEN, CORS_ALLOWED_ORIGINS
make dev-up                     # docker compose -f docker-compose.dev.yml up -d --build
```

Admin: http://localhost:8080/admin/

## API

Paths have no version segment; changes are additive. Every response carries `X-Request-ID`.
Success bodies are `{"data": ...}` (plus `"meta"` where documented); errors are
`{"error": {"code", "message", "details", "request_id"}}`; deletes answer `204`.

| Area | Endpoints | Access |
|---|---|---|
| Routing | `POST /routing/resolve/user`, `POST /routing/resolve/agent` | public |
| Admin | `/admin/api/session`, `backends`, `backends/{id}`, `backends/{id}/users`, `backends/{id}/agents`, `nginx`, `nginx/reload`, `jobs`, `jobs/{name}`, `jobs/{name}/run`, `runs`, `runs/{id}`, `sync`, `sync/conflicts` | admin (HTTP Basic) |
| Documents | `GET /openapi.json` | admin |
| Operations | `GET /healthz`, `GET /readyz`, `GET /metrics` | `/metrics` on the internal network only |

## Repository

| Path | Content |
|---|---|
| `src/` | the Router service, one module per feature (`routing`, `admin`, `jobs`, `nginx`, `db`, `http`, `openapi`) |
| `crates/api-contracts/` | OpenAPI contract crate used by `src/openapi/` |
| `openapi/openapi.json` | generated OpenAPI document (`make openapi`; CI checks it is current) |
| `deploy/nginx/` | `dev.conf`, `prod.conf`, shared `snippets/`, and `entrypoint.sh` (runs nginx, reloads on map changes) |
| `deploy/certs/` | extra CA certificates (`*.crt`) |
| `docker-compose.dev.yml` | development stack: Router, nginx, PostgreSQL |
| `docker-compose.prod.yml` | production stack: Router, nginx (HTTPS); PostgreSQL is external |
| `docs/DEPLOYMENT.md` | configuration, deployment and operations |

## Development

```bash
make help                                                                   # all targets
make check                                                                  # fmt, clippy, tests, OpenAPI check (as in CI)
TEST_DATABASE_URL=postgres://user:pass@localhost:5432/test_db make test-db  # + database tests (DELETES data in that DB)
cargo run -- --help                                                         # serve, migrate, resync, backend, openapi, healthcheck
```
