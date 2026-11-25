# LabMango Router: deployment and operations guide

The Router (Rust, formerly the Django `tenant_resolver_service`) replaces the whole old stack: routing API, Django
admin, Celery worker, Celery beat and Redis. It runs as two containers (Router service + nginx) plus PostgreSQL.
A backend added on the admin page (with its URL) is picked up by nginx within seconds: no file edit, manual reload
or CI/CD run is needed.

---

## 1. Deployment modes: with or without the Router

LabMango has three parts: the **frontend** (`beauty`, React), one **backend** per department (`lab-mango`,
Django) and, when there is more than one backend, the **Router** (this repository). The frontend chooses with one
switch, `VITE_API_MODE`:

| Mode | When | Frontend settings | Request path |
|---|---|---|---|
| **direct** (default) | one department, one backend | `VITE_API_MODE=direct`, `VITE_API_URL=https://backend` | frontend → backend |
| **router** | several departments, one backend each | `VITE_API_MODE=router`, `VITE_ROUTER_URL=https://router` | frontend → Router nginx `/t/<prefix_key>/` → backend |

In direct mode the Router is not deployed at all. In router mode:

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
    RT-->>FE: {"mappings": [{"backend_name": "Dept A", "prefix": "/t/abc123"}]}
    FE->>NG: /t/abc123/api/aad-login, /t/abc123/api/..., wss /t/abc123/ws/...
    NG->>BE: /api/..., /ws/... (prefix removed)
    loop always (leader replica)
        RT->>NG: GET :8080/t/abc123/internal/changes/users?wait=30
        NG->>BE: GET /internal/changes/users
    end
```

1. The user signs in with Microsoft. The frontend sends the ID token to the Router.
2. The Router validates the token (signature, audience = the frontend's client ID, expiry), builds the routing key
   `entra:<tid>:<oid>` and looks it up in the claims it synced from the backends. It answers the backend prefix.
3. The frontend stores the prefix (localStorage `labmango.routedBackend`) and sends every request, including
   `aad-login`, the refresh token and WebSockets, to `VITE_ROUTER_URL/t/<prefix_key>/...`. nginx strips the
   prefix and proxies to the backend. Cookies set by the backend are scoped to `/t/<prefix_key>/`.
4. The backend authenticates and authorizes every request itself: routing is not authorization (spec P1).

> **A user is routable only after their backend publishes them.** A backend publishes a user in its `users`
> stream once the user's account has an Entra binding (backend spec C3). Users who never signed in on their
> backend have none yet: bind them on the backend first (`python manage.py bind_entra_identity ...`, or one
> sign-in directly on that backend), otherwise the Router answers 404 and the frontend shows "Your account is
> not assigned to a lab yet".

### 1.1 What each part must be configured with (router mode)

| Part | Setting | Value |
|---|---|---|
| Frontend | `VITE_API_MODE` | `router` |
| Frontend | `VITE_ROUTER_URL` | public URL of the Router nginx, e.g. `https://router.example.com` |
| Frontend | `VITE_CLIENT_ID`, `VITE_TENANT_ID`, `VITE_REDIRECT_URI` | unchanged |
| Router | `AAD_AUDIENCE` | the frontend's `VITE_CLIENT_ID` |
| Router | `CORS_ALLOWED_ORIGINS` | the frontend origin, e.g. `https://labmango.example.com` |
| Router | `ROUTING_SYNC_AUTH` + token(s) | see section 5 |
| Router admin page | one backend per department | name, URL (`https://backend-a:8443`), `backend_id` (= the backend's `BACKEND_ID`) |
| Each backend | `BACKEND_ID` | its logical id, e.g. `dept_a` (must match the Router registry) |
| Each backend | `ROUTING_SYNC_STATIC_TOKEN_HASHES` (Method B) | SHA-256 hex of the Router's token for this backend |
| Each backend | `ROUTING_SYNC_AUDIENCE`, `ROUTING_SYNC_ENTRA_TENANT_IDS` (Method A) | its own audience and tenant |
| Each backend | `DJANGO_CSRF_TRUSTED_ORIGINS`, `DJANGO_CORS_ALLOWED_ORIGINS` | the frontend origin (as in direct mode) |
| Each backend | `ALLOWED_HOSTS` | the host the Router's nginx sends (production: the host of the backend URL; local: `host.docker.internal`) |

The backend needs no setting that names the Router (spec AUTH-11): it only knows its own id, its token digest
(or its Entra audience) and the frontend origin.

### 1.2 Agents

In router mode the agent setup dialog of the frontend uses the user's backend prefix on the Router as the enroll
URL (`https://router/t/<prefix_key>`), unless `VITE_AGENT_URL` overrides it. Agents then reach their backend
through the Router's nginx. `POST /routing/resolve/agent` resolves an agent UUID to its
backend once the backend publishes it in its `agents` stream.

---

## 2. Architecture

```mermaid
flowchart LR
    U["Users / agents / admin"] --> NG["nginx"]
    NG -->|"/routing/... (resolve backend)"| TR["Router service (Rust)"]
    NG -->|"/admin/"| TR
    NG -->|"/t/&lt;prefix&gt;/... (business API)"| BE["Department backends"]
    TR -->|"read / write"| DB[("PostgreSQL")]
    TR -.->|"token keys (JWKS)"| MS["Microsoft"]
    TR -->|"routing sync GET /internal/* (nginx :8080)"| BE
    TR -->|"generates the map from the backend URLs"| MAP[("nginx map")]
    MAP -->|"reload on change"| NG
```

| Component | Replaces | Job |
|---|---|---|
| Routing API | `tenant/api.py` | `resolve/user`, `resolve/agent`: the backend a user/agent belongs to |
| `/admin/` page + admin API | Django admin | create / edit / delete backends, view mappings, sync status, conflicts, schedules |
| Stream sync | Celery periodic sync | snapshot + continuous long-poll of every backend's `users` and `agents` streams |
| Scheduler | Celery beat | safety-net checks (digest), nginx map; schedules stored in the database, edited on the admin page |
| Run history | Celery result backend (Redis) | table `rust_task_run`, "Run history" tab |
| nginx routing table | hand-edited `backends.map` | backend URLs stored in the database, the Router writes the map file, nginx reloads |
| Leader election | – | only one Router replica syncs (PostgreSQL advisory lock); all replicas serve the API |

---

## 3. Repository layout

```
.
├── README.md
├── Cargo.toml, Cargo.lock, rustfmt.toml   ← Rust workspace
├── Makefile                               ← common commands (make help)
├── .github/workflows/ci.yml               ← CI
├── Dockerfile, .dockerignore              ← image of the Router service
├── docker-compose.dev.yml                 ← development (Router, nginx, PostgreSQL)
├── docker-compose.prod.yml                ← production (external PostgreSQL, HTTPS)
├── crates/api-contracts/                  ← OpenAPI contract crate (workspace member)
├── .env.example                           ← configuration template, copy to .env
├── docs/DEPLOYMENT.md                     ← this file
├── deploy/                                ← everything needed to run, no code
│   ├── certs/                             ← extra CA certificates (*.crt)
│   └── nginx/
│       ├── watch-maps.sh                  ← runs nginx, reloads it when a map changes, writes CORS/CA files
│       ├── dev/          nginx.conf, tenants/_proxy.conf, tenants/backends.map (static, normally empty)
│       └── prod/         nginx.conf, tenants/_proxy.conf, tenants/backends.map (static, normally empty)
└── src/                                   ← Rust code, one folder per feature
    ├── main.rs        CLI commands, start-up
    ├── app.rs         wires everything and runs the server, scheduler and stream sync
    ├── cli.rs         maintenance commands (sync-users, backend list…)
    ├── settings.rs    environment variables (.env)
    ├── state.rs       state shared by the handlers
    ├── outbound.rs    HTTP client for Microsoft endpoints (proxy, CA bundle)
    ├── metrics.rs     Prometheus metrics
    ├── routing/       the public API resolve/user, resolve/agent
    │   └── aad.rs     Microsoft ID-token validation (cached keys)
    ├── admin/         page /admin/ and API /admin/api/*
    │   ├── auth.rs        sign-in (account from .env)
    │   ├── backends.rs    backend CRUD, mappings
    │   ├── nginx.rs       nginx status, forced reload
    │   ├── jobs.rs        schedules, run history
    │   ├── sync.rs        stream cursors, leadership, conflicts
    │   ├── fields.rs      request field parsing and validation
    │   └── page.html      admin UI
    ├── jobs/          background work (replaces Celery)
    │   ├── mod.rs         scheduler + job runner + run history
    │   ├── sync.rs        routing sync protocol (snapshot, changes, info, digest)
    │   ├── stream.rs      long-poll workers, one per backend and stream
    │   ├── credentials.rs sync credentials (static tokens, Entra client credentials)
    │   └── leader.rs      leader election
    ├── nginx/         nginx map files
    ├── openapi/       OpenAPI 3.1 document built with the api-contracts crate
    ├── db/            PostgreSQL
    │   ├── schema.sql     create / upgrade tables (runs at every start)
    │   ├── routing.rs     lookups of the routing API
    │   ├── backends.rs    table tenant_backend
    │   ├── mappings.rs    user/agent claims, stream cursors, conflicts
    │   └── jobs.rs        schedules and run history
    └── http/          server assembly, middleware, request parsing and error bodies
```

`crates/api-contracts/` holds the OpenAPI contract crate (Rust port of `drf-api-contracts`), a member of the
Cargo workspace.

---

## 4. Local run

Requirements: Docker Engine + Docker Compose v2. The first build downloads Rust crates (Internet needed once).

```bash
cp .env.example .env
# edit .env: at least ADMIN_PASSWORD and ROUTING_SYNC_TOKEN
make dev-up        # docker compose -f docker-compose.dev.yml up -d --build
make dev-logs
```

| Open | Expected |
|---|---|
| http://localhost:8080/healthz | `ok` (nginx) |
| http://localhost:8080/admin/ | admin sign-in page |

### 4.1 Full local stack: frontend + Router + two backends

1. Run two backends on the host, e.g. ports 8001 and 8002, each with its own database and
   `BACKEND_ID=dept_a` / `dept_b`, `ALLOWED_HOSTS` containing `host.docker.internal`, and
   `ROUTING_SYNC_STATIC_TOKEN_HASHES=<sha256 of the token>`:

   ```bash
   python -c "import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())" "$ROUTING_SYNC_TOKEN"
   ```

2. Start the Router (above) with the same `ROUTING_SYNC_TOKEN` and `CORS_ALLOWED_ORIGINS=http://localhost:3000`.
3. On http://localhost:8080/admin/ add two backends: URL `http://host.docker.internal:8001`, backend_id `dept_a`,
   and `http://host.docker.internal:8002`, `dept_b`. Within seconds the "Sync" tab shows their streams.
4. Start the frontend with `VITE_API_MODE=router` and `VITE_ROUTER_URL=http://localhost:8080`
   (and the Router's `AAD_AUDIENCE` = `VITE_CLIENT_ID`).

---

## 5. Configuration (`.env`)

Every setting lives in `.env`. **After a change run `docker compose -f docker-compose.<env>.yml up -d`**: Compose sees the new values and
recreates the container (if not, `docker compose up -d --force-recreate router`).

| Variable | Required | Meaning |
|---|---|---|
| `ADMIN_USERNAME`, `ADMIN_PASSWORD` | yes (for admin) | admin account. Without both, the admin is disabled (API answers 503) |
| `DATABASE_URL` | yes | `postgres://user:pass@host:5432/db` |
| `POSTGRES_USER/PASSWORD/DB` | dev | database of the `postgres` service of `docker-compose.dev.yml` (which sets `DATABASE_URL` itself) |
| `AAD_AUDIENCE`, `AAD_JWKS_URL` | no | ID-token audience (= frontend client ID) and Microsoft keys |
| `HTTPS_PROXY` | no | outbound proxy for Microsoft endpoints |
| `CORS_ALLOWED_ORIGINS` | production | frontend origin(s), comma separated; nginx answers CORS for them |
| `ROUTING_CONFLICT_POLICY` | no | `reject` (default): a key claimed by several backends answers 409; `all`: list every backend |
| `NGINX_INTERNAL_BASE_URL` | no | where the sync reaches the backends: `http://nginx:8080` (internal listener) |
| `NGINX_INTERNAL_HOST_HEADER` | no | Host header of sync requests to nginx (default `localhost`) |
| `BACKEND_SYNC_CONNECT_TIMEOUT`, `BACKEND_SYNC_READ_TIMEOUT` | no | sync timeouts (5 s / 60 s; read ≥ long-poll wait + 10 s) |
| `SYNC_LONGPOLL_WAIT` | no | long-poll wait of `/internal/changes` (default 30 s, max 30) |
| `ROUTING_SYNC_AUTH` | no | `static` (default) or `entra` |
| `ROUTING_SYNC_TOKEN` / `ROUTING_SYNC_TOKEN_FILE` | static | one bearer token for every backend |
| `ROUTING_SYNC_TOKENS_DIR` | static | one token per backend: files `<dir>/<prefix_key>` or `<dir>/<backend_id>` (win over the shared token) |
| `ROUTING_SYNC_ENTRA_TENANT_ID`, `ROUTING_SYNC_CLIENT_ID`, `ROUTING_SYNC_CLIENT_SECRET(_FILE)` | entra | the Router's Entra app (client credentials) |
| `ROUTING_SYNC_SCOPE_TEMPLATE` | no | default `api://labmango-{backend_id}/.default` |
| `SYNC_ENABLED` | no | `false`: no long-poll and no scheduled runs ("Run now" still works) |
| `LEADER_ELECTION` | no | default `true`: only one replica syncs |
| `USER_SYNC_INTERVAL`, `AGENT_SYNC_INTERVAL` | no | first interval (seconds) of the safety-net checks, **only used once**, when the database has no schedule |
| `HOST_PORT` | no | public port of the local nginx (default 8080) |
| `DB_MAX_CONNECTIONS`, `AAD_JWKS_CACHE_TTL`, `RUST_LOG`, `LOG_FORMAT` | no | tuning, see `.env.example` |

### 5.1 Sync credentials (backend spec section 10.3)

**Method B, static token (simple).** Generate one random token per backend (at least 256 bits):

```bash
python -c "import secrets; print(secrets.token_urlsafe(32))"
```

Put it in `secrets/sync-tokens/<prefix_key>` (mount the folder and set `ROUTING_SYNC_TOKENS_DIR`, see the compose
files), and put its SHA-256 into that backend's `ROUTING_SYNC_STATIC_TOKEN_HASHES`. A single shared
`ROUTING_SYNC_TOKEN` also works but the spec recommends one token per backend (AUTH-2). Rotation: add the new
hash on the backend, replace the file on the Router, remove the old hash.

**Method A, Entra application token (preferred by the spec).** Per backend, register an API app with App ID URI
`api://labmango-<backend_id>` and the application role `Routing.Sync.Read`; assign the role to the Router's app.
Set `ROUTING_SYNC_AUTH=entra` and the `ROUTING_SYNC_ENTRA_TENANT_ID` / `CLIENT_ID` / `CLIENT_SECRET_FILE`
variables, and the `backend_id` of every backend on the admin page (the token audience is derived from it).
Tokens are cached until five minutes before they expire and refreshed after a `401`.

---

## 6. Admin page (`/admin/`)

### "Backend" tab
- **Add a backend:** name, **backend URL** (`http://host:port` or `https://host:port`) and **backend_id** (the
  backend's `BACKEND_ID`), then Create. The `prefix_key` is generated (10 characters a-z0-9) as in the Django
  admin. Within seconds `https://<domain>/t/<prefix_key>/...` is proxied to that URL.
- **backend_id:** the sync refuses a backend whose `GET /internal/info` reports another id (protects against a
  wrong URL, spec CNS-2). Empty = not checked. Required with Entra sync authentication.
- **Edit:** name, URL, backend_id, Active. `prefix_key` cannot change. An empty URL stops nginx routing it.
- **Deactivate** a backend: nginx stops routing it and the sync skips it.
- **Delete:** type the `prefix_key` to confirm. **All its user/agent mappings are deleted too** (Django CASCADE).
- **Mapping:** read-only list of the user/agent claims synced from the backend, with search and active filter.
- Columns **Users / Agents (stream v.)**: active claims and the stream version applied so far.

### "Nginx" tab: how nginx is updated
**The database is the only source.** The Router writes the nginx map (`backends.admin.map`, in the Docker volume
`nginx_admin`) from the **active** backends that have a URL. The file is rewritten:
1. **immediately** after a backend is created, edited or deleted;
2. **periodically** by the `render_nginx_map` job (default 60 s, "Jobs" tab), only when its content changes;
3. on **Force reload nginx**: the file is rewritten **and** nginx reloads even when nothing changed.

The nginx container runs `deploy/nginx/watch-maps.sh`. Every few seconds it:
- runs `nginx -t` when a map changed or a forced reload was requested;
- reloads with `nginx -s reload` when the test passes, otherwise **keeps the running configuration**;
- writes the outcome to a status file shown on the tab (● OK / ● ERROR, with the `nginx -t` output) together
  with the whole routing table and any problem found (backend without URL, stale generated file…).

> `deploy/nginx/<env>/tenants/backends.map` is a **static file, normally empty** (nginx needs it to exist). A
> backend listed there cannot get a URL on the admin page.

### "Sync" tab
- Whether this replica is the sync leader.
- Stream cursors: epoch and version of every backend's `users` and `agents` stream.
- **Conflicts:** keys claimed by more than one active backend. With `ROUTING_CONFLICT_POLICY=reject` the routing
  API answers 409 for them, so nobody is sent to the wrong department (spec CNS-6). Fix the assignment on the
  backends; the next sync removes the conflict.

### "Jobs" tab (replaces Celery beat)
Each task has **Enabled** and **Interval (seconds)** (minimum 5), saved immediately, and **Run now** (409 when it
is already running).

| Task | Replaces | What it does |
|---|---|---|
| `sync_all_backend_users` | `tenant.tasks.sync_all_backend_users` | safety net for the `users` stream of every active backend: epoch, version and digest (count **and** checksum); resync on drift |
| `sync_all_backend_agents` | `tenant.tasks.sync_all_backend_agents` | same for the `agents` stream |
| `render_nginx_map` | (new) | rewrites the nginx map from the database (default 60 s) |

The main sync does **not** run on a schedule. It runs continuously (`src/jobs/stream.rs`) on the leader replica:
each active backend has two loops (`users`, `agents`). The first time (no cursor) a loop downloads
`/internal/snapshot/{entity}` page by page and replays `/internal/changes` from the first page's watermark, then
long-polls `/internal/changes/{entity}?since=<version>&epoch=<epoch>&wait=30`, applying every page together
with its cursor in one transaction. A `mode: "reset"` answer starts a full resync. Cursors live in
`rust_sync_cursor`. A backend that does not advertise a stream in `/internal/info` yet (spec PUB-9) is checked
again every minute; this is not an error.

Stored data: keys only. Users are `entra:<tid>:<oid>` (column `user_id`), agents are the agent UUID (column
`agent_id`). `resolve/user` builds the key from the `tid` and `oid` claims of the ID token.

On the very first start the two sync schedules get their interval from, in order:
1. the old Celery beat schedule (`django_celery_beat_*` tables) when the Django database is reused;
2. `USER_SYNC_INTERVAL` / `AGENT_SYNC_INTERVAL` from `.env`;
3. 300 s / 60 s.

Unlike Celery, a task **never overlaps itself**: if the previous run is still going, the next one waits.

### "Run history" tab
The last 500 runs of each task: trigger (`schedule`, `manual`, `admin`, `force`, `startup`), duration and the
full JSON result. One failing backend does not fail the run: it is reported with `"status": "failed"` and the
error. Sync results also count conflicting keys.

---

## 7. API

Through nginx: `http://<host>:8080` (local) or `https://<domain>` (production).

### 7.0 Endpoint overview

Paths carry no version segment; changes are additive. The service and nginx use the same paths.

| Area | Paths | Access |
|---|---|---|
| Routing | `POST /routing/resolve/user`, `POST /routing/resolve/agent` | public |
| Admin UI | `/admin/` | admin account |
| Admin API | `/admin/api/...` (section 7.2) | admin account |
| OpenAPI | `GET /openapi.json` (also committed as `openapi/openapi.json`) | admin account |
| Operations | `GET /healthz`, `GET /readyz`, `GET /metrics` | `/metrics` internal network only |

### 7.1 Routing API (no sign-in)

```bash
# User: Microsoft ID token
curl -X POST http://localhost:8080/routing/resolve/user \
     -H "Content-Type: application/json" -d '{"id_token": "eyJ0eXAiOi..."}'

# Agent: agent UUID
curl -X POST http://localhost:8080/routing/resolve/agent \
     -H "Content-Type: application/json" -d '{"agent_id": "bbbbbbbb-1111-4000-8000-000000000001"}'
```

**Deprecated aliases.** The paths of the original service, `/routing/auth/backend-info/by-id-token/` and
`/routing/auth/backend-info/by-agent-id/` (with or without the trailing slash, and under `/api/v1/`), still
work with the same request and response, and add `Deprecation: true` and
`Link: </routing/resolve/user>; rel="successor-version"` (or `.../agent`). Move clients to the new paths; the
aliases will be removed in a later release.

| Result | Body |
|---|---|
| 200 | `{"mappings": [{"backend_name": "Backend 1", "prefix": "/t/<prefix_key>"}]}` |
| 400 missing field | `{"agent_id": ["This field is required."]}` |
| 400 bad/expired token | `{"detail": "Microsoft Token is invalid or expired"}` |
| 404 | `{"detail": "No backend access found for this user."}` (or `... this agent.`) |
| 409 | `{"detail": "This user is assigned to more than one backend. ..."}` (policy `reject`) |

### 7.2 Admin API (username + password)

Send the account **either** as HTTP Basic auth (`curl -u admin:password ...`) **or** in the JSON body
(`{"username": "admin", "password": "...", ...}`). GET requests use Basic auth.

| Method | Path | Body / parameters | Result |
|---|---|---|---|
| POST | `/admin/api/login` | – | 200 when the account is right |
| GET | `/admin/api/backends` | – | list of backends |
| POST | `/admin/api/backends` | `name` (required), `upstream_url`, `backend_id`, `is_active`, `computer_sync_version` | 201 + backend + map result (`nginx_map`) |
| GET | `/admin/api/backends/{id}` | – | one backend |
| PATCH | `/admin/api/backends/{id}` | `name`, `upstream_url` (`""` = none), `backend_id`, `is_active`, `computer_sync_version` (all optional) | the backend after the change |
| DELETE | `/admin/api/backends/{id}` | – | `{"deleted": {"backend": 1, "users": n, "agents": m}}` |
| GET | `/admin/api/backends/{id}/users` | `?search=&active=true/false&limit=` | user claims |
| GET | `/admin/api/backends/{id}/agents` | same | agent claims |
| GET | `/admin/api/nginx` | – | routing table, problems, last reload result |
| POST | `/admin/api/nginx/reload` | – | 202: map rewritten and nginx reload forced |
| GET | `/admin/api/sync` | – | stream cursors, leadership |
| GET | `/admin/api/sync/conflicts` | – | keys claimed by more than one backend |
| GET | `/admin/api/jobs` | – | jobs with their schedule and last run |
| GET | `/admin/api/jobs/{name}` | – | one job |
| PATCH | `/admin/api/jobs/{name}` | `enabled`, `interval_secs` | the job after the change |
| POST | `/admin/api/jobs/{name}/run` | – | 202 `{"run_id": ...}`, 409 when already running |
| GET | `/admin/api/runs` | `?task=&limit=` | run history |

Examples:

```bash
# Create a backend with its URL and backend_id (password in the body)
curl -X POST http://localhost:8080/admin/api/backends \
     -H "Content-Type: application/json" \
     -d '{"username":"admin","password":"secret","name":"Dept A","upstream_url":"https://dept-a.example.com:8443","backend_id":"dept_a"}'

# Force an nginx reload
curl -u admin:secret -X POST http://localhost:8080/admin/api/nginx/reload

# Deactivate backend id=3
curl -u admin:secret -X PATCH http://localhost:8080/admin/api/backends/3 \
     -H "Content-Type: application/json" -d '{"is_active": false}'

# Run the agent stream check now
curl -u admin:secret -X POST http://localhost:8080/admin/api/jobs/sync_all_backend_agents/run
```

Wrong account: `401 {"detail": "Invalid username or password."}`. No account in `.env`: `503`.

### 7.3 OpenAPI for Apidog

The OpenAPI 3.1 document is generated from the `api-contracts` crate (`src/openapi/mod.rs`) and committed as
**`openapi/openapi.json`**. Use that file for Apidog or client generation.

```bash
make openapi          # regenerate openapi/openapi.json after an API change, then commit it
make openapi-check    # fails when the committed file is out of date (CI runs the same check)
curl -u admin:secret http://localhost:8080/openapi.json -o openapi.json   # from a running service
```

In Apidog: **Import data → OpenAPI/Swagger**, then pick `openapi/openapi.json`. Errors are `{"detail": ...}` or
`{"<field>": [...]}`; the deprecated routing aliases are marked `deprecated: true`. The test
`document_lists_every_route` fails when a route is missing from the document.

### 7.4 Backend sync API used by the Router

Consumed, not served: `GET /internal/info`, `/internal/snapshot/{entity}`, `/internal/changes/{entity}`,
`/internal/digest/{entity}` of each backend (backend repository, `docs/agent-platform/routing-sync.md`), always
through nginx's internal listener: `http://nginx:8080/t/<prefix_key>/internal/...`. The Router:

- sends `GET` only, no body, never follows redirects (GET-1, GET-6), a fresh `X-Request-ID` per request;
- checks `backend_id` of `/internal/info` against the registry (CNS-2) and syncs only advertised streams;
- validates every page before writing it, and applies a page together with its cursor atomically (CNS-3..5);
- compares the digest count and checksum (DIG-1) and resyncs on drift (CNS-8);
- keeps the last known claims of an unreachable backend (CNS-7);
- runs the sync on one replica only (CNS-9).

The public nginx listener answers 404 for `/t/*/internal/*` (AUTH-6); the internal listener proxies only `GET`
and, in production, verifies the backend certificate against the system CAs plus `deploy/certs/*.crt` (AUTH-4).

---

## 8. Production

### 8.1 Server preparation
1. Install Docker + Compose v2 and copy the repository to the server.
2. Put the HTTPS certificate in `ssl_certificates/` (`another-host.com.pem` and `.key`, or edit the names and the
   `server_name` in `deploy/nginx/prod/nginx.conf`). The folder is not in git.
3. Create `.env` from `.env.example`:
   - `ADMIN_PASSWORD`: a strong password;
   - `DATABASE_URL`: the production PostgreSQL;
   - `CORS_ALLOWED_ORIGINS`: the frontend origin;
   - `AAD_AUDIENCE`: the frontend's client ID;
   - the sync credential (section 5.1);
   - remove the `POSTGRES_*` lines (production does not run PostgreSQL in Docker).
4. Keep `deploy/nginx/prod/tenants/backends.map` **empty** (comments only); the backend URLs are set on the
   admin page.

### 8.2 Reusing the Django database
Point `DATABASE_URL` at the old database. At start-up the Router:
- **reuses** `tenant_backend`, `tenant_usertenantresolver`, `tenant_agenttenantresolver` (backends and mappings
  are kept);
- creates `rust_periodic_task`, `rust_task_run`, `rust_sync_cursor` and adds the columns `upstream_url` and
  `backend_id` (default empty) to `tenant_backend`; Django ignores them;
- reads the old Celery beat schedule, if any, to create the sync schedules;
- **does not touch** any other Django table (`auth_*`, `django_*`…).

Back up first:

```bash
pg_dump "$DATABASE_URL" > backup_before_router.sql
```

### 8.3 Switch-over
```bash
# stop the old Django stack (in the old repository)
docker compose -f docker-compose.prod.yml down

# start the Router (in this repository)
docker compose -f docker-compose.prod.yml up -d --build
docker compose -f docker-compose.prod.yml logs -f router
```

The log shows `schedule created` (first start), `sync leadership acquired`, `scheduler started` and
`router listening`.

### 8.4 Checks after the switch
1. Open `https://<domain>/admin/`, sign in and check the backend list. **Set the URL and backend_id of every
   backend.** Take the address from the old Django `backends.map`: the line `qlporzk7ct  another-host.com:8443;`
   becomes the URL `https://another-host.com:8443` (the old production always used https). Without a URL nginx
   answers 404 for that backend.
2. The "Nginx" tab shows **● OK**. The "Sync" tab lists the streams of every backend. On "Jobs", click
   **Run now** for each sync job and check `failed: 0` in "Run history".
3. Call `POST /routing/resolve/agent` with a real agent UUID: 200.
4. Sign in from the frontend in router mode (`resolve/user` flow).

### 8.5 Rolling back to Django
The Router drops no Django table or column (it only adds columns with defaults). Stop it and start the old Django
stack on the same database. The `django_celery_beat_*` tables still hold the old schedule.

---

## 9. Daily operations

| Task | Command (add `-f docker-compose.prod.yml` or `-f docker-compose.dev.yml`, or use `make prod-*` / `make dev-*`) |
|---|---|
| Logs | `docker compose logs -f router` |
| Restart | `docker compose restart router` |
| Update | `git pull && docker compose up -d --build` |
| Change the admin password | edit `.env` → `docker compose up -d` |
| Add / edit / delete a backend | admin page (nginx reloads by itself) |
| Force an nginx reload | admin page, "Nginx" tab, **Force reload nginx** |
| Did nginx reload? | `docker compose logs -f nginx` (lines `[watch-maps]`) |
| Full resync from the command line | `docker compose exec router tenant-resolver sync-users` (or `sync-agents`) |
| Backends from the command line | `docker compose exec router tenant-resolver backend list` |
| Health | `docker compose ps` (STATUS `healthy`); `GET /readyz` also checks the database |
| Metrics | Prometheus scrapes `http://router:8000/metrics` from the Docker network (blocked on the public listener) |

Several replicas are safe: only the leader syncs and runs the schedules (the claim of a scheduled run is atomic as
well); every replica serves the routing API and the admin page. "Run now" runs on the replica that received it.

---

## 10. Troubleshooting

| Symptom | Usual cause | Fix |
|---|---|---|
| Admin says "Admin is disabled…" (503) | `ADMIN_USERNAME` or `ADMIN_PASSWORD` missing | add them to `.env`, `docker compose up -d` |
| Right password, still 401 | `.env` changed but the container was not recreated | `docker compose up -d --force-recreate router` |
| `resolve/user` always "Microsoft Token is invalid or expired" | Microsoft unreachable (proxy/CA), `AAD_AUDIENCE` is not the frontend client ID, or expired token | look for `JWKS refresh failed` in the log; set `HTTPS_PROXY` if needed |
| Frontend: "Your account is not assigned to a lab yet" | the user's backend has not published the user (no Entra binding, or stream not synced yet) | "Sync" tab of the Router; bind the user on the backend (`manage.py bind_entra_identity`, see section 1) |
| Frontend: CORS error on `/routing/...` | frontend origin not in `CORS_ALLOWED_ORIGINS` | set it, `docker compose up -d` (nginx logs the origins at start) |
| `409` from resolve/user | the user is claimed by two backends | "Sync" tab → Conflicts; remove the duplicate assignment on one backend |
| Sync: `HTTP 401` / `403` | wrong token or Entra role | check the token file/hash or the app role assignment |
| Sync: `reports backend_id "x", the registry expects "y"` | the URL points at another backend, or a typo in backend_id | fix the URL or backend_id on the admin page |
| Sync: `HTTP 404` without error code | backend not routed by nginx, or wrong `NGINX_INTERNAL_BASE_URL` | "Nginx" tab; it must be `http://nginx:8080` |
| Log: `stream not advertised by the backend yet` | the backend does not publish that stream yet (spec PUB-9) | nothing to do on the Router; it checks every minute |
| Admin says "... declared in the static backends.map" when setting a URL | the static file still has that backend | remove the line from `backends.map`, wait a few seconds, save again |
| "Nginx" tab: "No status received from nginx yet" | nginx does not run `watch-maps.sh` or does not share the `nginx_admin` volume | check `command` and `volumes` of nginx in the compose file |
| "Nginx" tab: ● ERROR `nginx -t failed` | broken map or configuration; nginx keeps the old one | read the error on the tab, fix, Force reload |
| Schedules do not run | `SYNC_ENABLED=false`, schedule disabled, or this replica is not the leader | "Jobs" and "Sync" tabs |
| Log: `database not ready` | PostgreSQL not ready or wrong `DATABASE_URL` | retried 30 times (about a minute); check URL, user, password |

---

## 11. Security (minimum)

- The admin uses **one account** from `.env`, sent with every request. **Use HTTPS** in production (configured in
  `deploy/nginx/prod/nginx.conf`).
- The admin page keeps the credentials in `sessionStorage` (gone when the tab closes); "Sign out" clears them.
- Every failed sign-in waits 0.5 s and logs `admin login failed`.
- Restrict `/admin/` (page and API) to the internal network, e.g. in the production nginx:

  ```nginx
  location ^~ /admin/ {
      allow 10.0.0.0/8;
      deny all;
      proxy_pass http://$tenant_svc;
      proxy_set_header Host $host;
      proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
      proxy_set_header X-Forwarded-Proto $scheme;
  }
  ```

- The backends' `/internal/*` sync API is never public (404 on the public listener) and is reached only over the
  unpublished port 8080.
- Sync tokens are files or Docker secrets (`*_FILE` variables), never in the image or the repository. Routing keys
  are never logged at INFO or above.

---

## 12. Working on the code

```bash
cargo build                    # compile
cargo test                     # unit tests
TEST_DATABASE_URL=postgres://user:pass@localhost:5432/test_db cargo test   # + database tests (DELETES the data in that DB)
cargo run -- serve             # run directly, reads .env from the current directory
cargo run -- --help            # commands: serve, migrate, sync-users, sync-agents, backend, openapi …
```

- Admin UI: `src/admin/page.html`, embedded into the binary (rebuild after a change).
- New table or column: `src/db/schema.sql`, only with `CREATE ... IF NOT EXISTS` / `ADD COLUMN IF NOT EXISTS`
  (the file runs at every start).
- New feature: its own folder under `src/` (handlers in `admin/` or `routing/`, queries in `db/`), routes
  registered in its `mod.rs`, and the OpenAPI operation in `src/openapi/mod.rs`.
