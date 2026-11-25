# LabMango Router: deployment and operations

## 1. How the parts work together

LabMango has a **frontend**, one **backend** per department, and, when there is more than one backend, the
**Router**. The frontend chooses with `VITE_API_MODE`:

| Mode | Frontend settings | Requests |
|---|---|---|
| `direct` (default) | `VITE_API_URL=https://backend` | frontend → backend; no Router |
| `router` | `VITE_ROUTER_URL=https://router` | frontend → Router nginx `/t/<prefix_key>/` → backend |

```mermaid
sequenceDiagram
    participant FE as Frontend
    participant MS as Microsoft Entra
    participant NG as Router nginx
    participant RT as Router service
    participant BE as Backend (dept A)
    FE->>MS: sign-in (MSAL)
    MS-->>FE: ID token
    FE->>NG: POST /routing/resolve/user {id_token}
    NG->>RT: POST /routing/resolve/user
    RT-->>FE: {"data": {"backend_id": "dept_a", "name": "Dept A", "prefix": "/t/abc123"}}
    FE->>NG: /t/abc123/api/..., wss /t/abc123/ws/...
    NG->>BE: /api/..., /ws/... (prefix removed)
    loop always (leader replica)
        RT->>NG: GET :8080/t/abc123/internal/changes/users?wait=30
        NG->>BE: GET /internal/changes/users
    end
```

1. The user signs in with Microsoft; the frontend sends the ID token to the Router.
2. The Router validates it (signature, audience = the frontend's client ID, expiry), builds the routing key
   `entra:<tid>:<oid>` and looks it up in the claims synced from the backends.
3. The frontend keeps the prefix (localStorage `labmango.routedBackend`) and sends every request, including
   sign-in, token refresh and WebSockets, to `VITE_ROUTER_URL/t/<prefix_key>/...`. nginx removes the prefix and
   proxies to the backend; cookies set by the backend are scoped to `/t/<prefix_key>/`.
4. The backend authenticates and authorizes every request itself: routing is not authorization.

A user is routable once their backend publishes them, i.e. their account has an Entra binding on that backend
(bind with `python manage.py bind_entra_identity` on the backend, or one sign-in there directly). Until then
`resolve/user` answers `404 not_routed`.

Agents enroll and talk to their backend through the same prefix (`https://router/t/<prefix_key>`); the frontend's
agent setup dialog uses it by default.

### 1.1 Settings of each part (router mode)

| Part | Setting | Value |
|---|---|---|
| Frontend | `VITE_API_MODE` / `VITE_ROUTER_URL` | `router` / public URL of the Router nginx |
| Router | `AAD_TENANT_ID`, `AAD_AUDIENCE` | the frontend's `VITE_TENANT_ID` and `VITE_CLIENT_ID` |
| Router | `CORS_ALLOWED_ORIGINS` | the frontend origin |
| Router | sync credential | section 3.1 |
| Router admin page | one backend per department | name, URL (`https://backend-a:8443`), backend_id (= its `BACKEND_ID`) |
| Each backend | `BACKEND_ID` | its logical id, e.g. `dept_a` |
| Each backend | `ROUTING_SYNC_STATIC_TOKEN_HASHES` (static) or `ROUTING_SYNC_AUDIENCE` (Entra) | section 3.1 |
| Each backend | `DJANGO_CSRF_TRUSTED_ORIGINS`, `DJANGO_CORS_ALLOWED_ORIGINS` | the frontend origin |
| Each backend | `ALLOWED_HOSTS` | the host of its URL as registered on the Router |

The backend is never configured with anything about the Router (backend spec AUTH-11).

## 2. Running it

Requirements: Docker Engine and Docker Compose v2.

### Development

```bash
cp .env.example .env
make dev-up            # Router, nginx and PostgreSQL
make dev-logs
```

http://localhost:8080/admin/ (admin account from `.env`). For a full local stack, run two backends on the host
(e.g. ports 8001 and 8002, `BACKEND_ID=dept_a` / `dept_b`, `ALLOWED_HOSTS` containing `host.docker.internal`),
register them as `http://host.docker.internal:8001` and `:8002`, and start the frontend with
`VITE_API_MODE=router` and `VITE_ROUTER_URL=http://localhost:8080`.

### Production

1. Put the TLS certificate in `ssl_certificates/server.pem` and `server.key`, and set the public host name in
   `deploy/nginx/prod.conf` (`router.example.com`).
2. Create `.env` from `.env.example`: a strong `ADMIN_PASSWORD`, `DATABASE_URL`, `AAD_TENANT_ID`,
   `AAD_AUDIENCE`, `CORS_ALLOWED_ORIGINS`, the sync credential.
3. `make prod-up` (`docker compose -f docker-compose.prod.yml up -d --build`).
4. On `/admin/`, add every backend with its URL and backend_id. The "Sync" tab shows their streams within
   seconds, the "Nginx" tab **● OK**.

The Router creates its tables at start-up (`AUTO_MIGRATE=true`). Several replicas are safe
(`docker compose -f docker-compose.prod.yml up -d --scale router=2`): only the leader syncs and runs the jobs;
all replicas serve the API.

The production stack runs the Router with a read-only filesystem, no Linux capabilities, resource limits, log
rotation and JSON logs; nginx starts once the Router is healthy.

## 3. Configuration (`.env`)

After a change: `docker compose -f docker-compose.<dev|prod>.yml up -d` (the containers are recreated).

| Variable | Required | Meaning |
|---|---|---|
| `ADMIN_USERNAME`, `ADMIN_PASSWORD` | for admin | admin account; without both the admin answers `503 admin_disabled` |
| `DATABASE_URL` | yes | `postgres://user:pass@host:5432/db` (dev: built by the compose file) |
| `AAD_TENANT_ID` | yes | Entra tenant of the users (the signing keys URL is derived from it) |
| `AAD_AUDIENCE` | yes | the frontend's client ID |
| `AAD_JWKS_URL` | no | signing keys URL, instead of deriving it |
| `HTTPS_PROXY` | no | outbound proxy for Microsoft endpoints only |
| `CORS_ALLOWED_ORIGINS` | prod | frontend origin(s), comma separated |
| `ROUTING_SYNC_AUTH` | no | `static` (default) or `entra` |
| `ROUTING_SYNC_TOKEN` / `_FILE` | static | one token for every backend |
| `ROUTING_SYNC_TOKENS_DIR` | static | one token per backend: files `<dir>/<prefix_key>` or `<dir>/<backend_id>` |
| `ROUTING_SYNC_ENTRA_TENANT_ID`, `ROUTING_SYNC_CLIENT_ID`, `ROUTING_SYNC_CLIENT_SECRET` / `_FILE` | entra | the Router's Entra app |
| `ROUTING_SYNC_SCOPE_TEMPLATE` | no | default `api://labmango-{backend_id}/.default` |
| `SYNC_LONGPOLL_WAIT` | no | long-poll wait, 0..30 s (default 30) |
| `BACKEND_SYNC_CONNECT_TIMEOUT`, `BACKEND_SYNC_READ_TIMEOUT` | no | 5 s / 60 s; read ≥ wait + 10 s |
| `SYNC_ENABLED` | no | `false`: no stream sync and no scheduled jobs ("Run now" still works) |
| `SYNC_CHECK_INTERVAL` | no | first interval of the `check_*_stream` jobs (default 3600 s) |
| `LEADER_ELECTION` | no | default `true` |
| `NGINX_MAP_DIR`, `NGINX_INTERNAL_BASE_URL` | no | set by the compose files |
| `DB_MAX_CONNECTIONS`, `AAD_JWKS_CACHE_TTL`, `RUST_LOG`, `LOG_FORMAT` | no | tuning |

### 3.1 Sync credential (backend spec section 10.3)

**Static token.** Generate one token per backend and put it in `secrets/sync-tokens/<prefix_key>` (mount the
folder, set `ROUTING_SYNC_TOKENS_DIR`; see the compose files). The backend lists its SHA-256 in
`ROUTING_SYNC_STATIC_TOKEN_HASHES`:

```bash
python -c "import secrets; print(secrets.token_urlsafe(32))"
python -c "import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())" "<token>"
```

Rotation: add the new hash on the backend, replace the file, remove the old hash.

**Entra application token.** Per backend, an API app `api://labmango-<backend_id>` with the application role
`Routing.Sync.Read`, assigned to the Router's app. Set `ROUTING_SYNC_AUTH=entra` and the `ROUTING_SYNC_*`
client variables, and the backend_id of every backend on the admin page. Tokens are cached until five minutes
before they expire and renewed after a `401`.

## 4. Admin page (`/admin/`)

- **Backends:** create (name, URL, backend_id), edit, deactivate, delete (type the `prefix_key` to confirm; its
  claims go with it). The `prefix_key` is generated and never changes. Users / Agents show the synced claims and
  the stream version. A backend whose `/internal/info` reports another backend_id is refused by the sync.
- **Nginx:** the map nginx uses, problems (active backend without URL, map out of date, nginx error) and
  **Reload nginx**. The map is rewritten after every backend change, by the `write_nginx_map` job, and on
  reload; `entrypoint.sh` in the nginx container tests (`nginx -t`) and reloads, keeping the running
  configuration when the test fails.
- **Sync:** leadership, stream cursors (epoch, version), and **conflicts**: keys claimed by more than one active
  backend. They resolve to `409 ambiguous_route` until the assignment is fixed on the backends.
- **Jobs:** enable/disable, interval, **Run now**.

| Job | What it does |
|---|---|
| `check_users_stream`, `check_agents_stream` | safety net per active backend: epoch, version and digest (count and checksum); resync on drift |
| `write_nginx_map` | rewrites the nginx map from the registry (default every 60 s) |

The continuous sync is not a job: the leader runs two long-poll loops per active backend (`users`, `agents`).
Without a cursor a loop downloads `/internal/snapshot/{entity}` and replays `/internal/changes` from the first
page's watermark; then it long-polls `/internal/changes/{entity}?since=&epoch=&wait=30` and applies every page
with its cursor in one transaction. `mode: "reset"` triggers a resync. A stream the backend does not advertise
yet is checked again every minute.

- **Run history:** the last 500 runs per job with trigger (`schedule`, `manual`, `admin`, `startup`), duration and
  result.

## 5. API

The OpenAPI document is [`openapi/openapi.json`](../openapi/openapi.json) (import into Apidog: Import data →
OpenAPI/Swagger). `make openapi` regenerates it; CI fails when it is out of date.

Conventions: no version segment, no trailing slash; `{"data": ...}` / `{"error": {"code", "message", "details",
"request_id"}}`; `X-Request-ID` on every response (a valid inbound one, 32 lowercase hex, is kept); request
bodies are JSON objects and unknown fields are rejected; `201` and `202` carry `Location`; deletes answer `204`.

### 5.1 Routing (public)

```bash
curl -X POST http://localhost:8080/routing/resolve/user \
     -H "Content-Type: application/json" -d '{"id_token": "eyJ0eXAiOi..."}'
curl -X POST http://localhost:8080/routing/resolve/agent \
     -H "Content-Type: application/json" -d '{"agent_id": "bbbbbbbb-1111-4000-8000-000000000001"}'
```

| Status | `error.code` | When |
|---|---|---|
| 200 | | `{"data": {"backend_id", "name", "prefix"}}` |
| 400 | `validation_failed` | missing, blank or unknown field; agent_id not a UUID |
| 400 | `invalid_token` | the ID token is invalid or expired |
| 404 | `not_routed` | no active backend claims the user or agent |
| 409 | `ambiguous_route` | more than one active backend claims it |
| 415 | `unsupported_media_type` | the body is not JSON |

### 5.2 Admin (HTTP Basic)

| Method | Path | Body / query | Result |
|---|---|---|---|
| GET | `/admin/api/session` | | `{"data": {"username"}}` |
| GET | `/admin/api/backends` | | backends |
| POST | `/admin/api/backends` | `name`, `backend_id`, `upstream_url`, `is_active` | `201`, backend + `meta.nginx_map` |
| GET | `/admin/api/backends/{id}` | | backend |
| PATCH | `/admin/api/backends/{id}` | any of the create fields (`upstream_url: ""` stops routing) | backend + `meta.nginx_map` |
| DELETE | `/admin/api/backends/{id}` | | `204` |
| GET | `/admin/api/backends/{id}/users`, `/agents` | `search`, `limit` (1..5000) | claims + `meta.total` |
| GET | `/admin/api/nginx` | | map, problems, last reload |
| POST | `/admin/api/nginx/reload` | | `202` |
| GET | `/admin/api/jobs`, `/admin/api/jobs/{name}` | | jobs with schedule and last run |
| PATCH | `/admin/api/jobs/{name}` | `enabled`, `interval_secs` (5..31 536 000) | job |
| POST | `/admin/api/jobs/{name}/run` | | `202` + `Location: /admin/api/runs/{id}`; `409 already_running` |
| GET | `/admin/api/runs`, `/admin/api/runs/{id}` | `job`, `limit` (1..500) | runs |
| GET | `/admin/api/sync` | | leadership, stream cursors |
| GET | `/admin/api/sync/conflicts` | | conflicting keys per stream |

```bash
curl -u admin:secret -X POST http://localhost:8080/admin/api/backends -H "Content-Type: application/json" \
     -d '{"name": "Dept A", "upstream_url": "https://dept-a.example.com:8443", "backend_id": "dept_a"}'
curl -u admin:secret -X POST http://localhost:8080/admin/api/jobs/check_users_stream/run
```

### 5.3 Backend sync API (consumed)

`GET /internal/info`, `/internal/snapshot/{entity}`, `/internal/changes/{entity}`, `/internal/digest/{entity}` of
each backend, always through nginx's internal listener `http://nginx:8080/t/<prefix_key>/internal/...`. The
Router sends GET only and never follows redirects, checks `backend_id` against the registry, validates every page
before writing it, applies each page with its cursor atomically, compares digest count and checksum, keeps the
last known claims of an unreachable backend, and syncs from one replica only. The public listener answers 404 for
`/t/*/internal/*`; the internal listener proxies GET only and verifies https backend certificates against the
system CAs plus `deploy/certs/*.crt`.

## 6. Operations

| Task | Command |
|---|---|
| Logs | `make prod-logs` |
| Update | `git pull && make prod-up` |
| Reload nginx | admin page, "Nginx" → Reload nginx |
| Full resync | `docker compose -f docker-compose.prod.yml exec router labmango-router resync users` (or `agents`) |
| Backends from the command line | `docker compose -f docker-compose.prod.yml exec router labmango-router backend list` |
| Health | `docker compose ps` (`healthy`); `GET /readyz` checks the database |
| Metrics | Prometheus scrapes `http://router:8000/metrics` from the Docker network |

Metrics: `router_routing_requests_total{entity,outcome}`, `router_sync_pages_applied_total`,
`router_sync_resyncs_total`, `router_sync_errors_total`, `router_jwks_refresh_failures_total`,
`router_sync_leader`, `router_sync_enabled`, `router_backends_active`, `router_backends_routed`,
`router_conflicting_keys{entity}`.

## 7. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `503 admin_disabled` | `ADMIN_USERNAME` / `ADMIN_PASSWORD` missing | set them, recreate the container |
| `400 invalid_token` on every sign-in | Microsoft unreachable (proxy/CA), wrong `AAD_AUDIENCE`, or expired token | log line `ID token rejected`; set `HTTPS_PROXY` / CA certificates |
| `404 not_routed` | the user is not published by any backend (no Entra binding, or not synced yet) | "Sync" tab; bind the user on the backend |
| `409 ambiguous_route` | the user is claimed by two backends | "Sync" tab → Conflicts; fix the assignment on a backend |
| CORS error in the browser | frontend origin not in `CORS_ALLOWED_ORIGINS` | set it; nginx logs the origins at start |
| Sync `HTTP 401` / `403` | wrong token or Entra role | check the token file / hash or the role assignment |
| Sync `reports backend_id "x", the registry expects "y"` | URL points at another backend, or wrong backend_id | fix it on the admin page |
| Sync `HTTP 404` without error code | backend not routed by nginx | "Nginx" tab: set its URL |
| Log `stream not advertised by the backend yet` | the backend does not publish the stream yet | nothing to do; checked every minute |
| "Nginx" tab: no status | nginx does not run `entrypoint.sh` or does not share the `nginx_map` volume | check the compose file |
| "Nginx" tab: ● ERROR | broken configuration; nginx keeps the previous one | read the error on the tab, fix, Reload |
| Log `database not ready` | PostgreSQL not reachable | retried for about a minute; check `DATABASE_URL` |

## 8. Security

- One admin account sent with every request: use HTTPS, and restrict `/admin/` to the internal network:

  ```nginx
  location ^~ /admin/ {
      allow 10.0.0.0/8;
      deny all;
      proxy_pass http://$router_upstream;
      proxy_set_header Host $host;
      proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
      proxy_set_header X-Forwarded-Proto $scheme;
  }
  ```

- Failed admin sign-ins wait 0.5 s and are logged. The admin page keeps the credentials in `sessionStorage`.
- `/internal/*` of the backends is never public; `/metrics` is not exposed by the public listener.
- Secrets come from files or Docker secrets (`*_FILE`), never from the image or the repository. Routing keys are
  never logged at INFO or above.

## 9. Code

- Admin UI: `src/admin/page.html` (embedded in the binary).
- Schema: `src/db/schema.sql`, applied at every start; idempotent statements only.
- A new endpoint: handler in its feature module, route in that module's `router()`, operation in
  `src/openapi/mod.rs`, then `make openapi`.
