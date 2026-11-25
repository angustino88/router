//! OpenAPI 3.1 document of the whole service, built with the `api-contracts` crate.
//!
//! **Documentation only.** This service must keep the wire format of the old Django REST Framework
//! service (`{"detail": "..."}`, `{"<field>": ["..."]}`, bare `{"mappings": [...]}`), so the handlers are
//! not changed and the `{"data": ...}` envelope of `api-contracts` is not used. Every response is declared
//! with `ResponseContract::documented(status, schema)` and describes what the handler really returns.
//!
//! The document is available as:
//!
//! * `tenant-resolver openapi > openapi.json` (no database needed), and
//! * `GET /openapi.json` (admin credentials required).
//!
//! Import either one into Apidog (Import data -> OpenAPI/Swagger, or "Import from URL").
//!
//! Response bodies that are plain database structs (`Mapping`, `BackendView`, `DeleteCounts`) derive
//! `ToSchema` directly, so they cannot drift from the real JSON. The other shapes are built with
//! `json!` in the handlers and are mirrored here; the tests below check that the mirrors still
//! deserialize the real structs and that every `$ref` resolves.

#![allow(dead_code)]

use std::collections::BTreeMap;

use api_contracts::{
    OpenApiDocument, OperationContract, ResponseContract, ResponseSchemaFactory, ResponseSet, Result as ContractResult,
    SchemaDef,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use utoipa::ToSchema;

use crate::{
    db::{
        backends::{BackendView, DeleteCounts},
        routing::Mapping,
    },
    routing::{AGENT_ID_PATH, ID_TOKEN_PATH},
};

// ------------------------------------------------------------------ errors (DRF shapes)

/// DRF error body: `{"detail": "..."}`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Detail {
    pub detail: String,
}

/// DRF field errors: `{"<field>": ["message", ...]}`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FieldErrors(pub BTreeMap<String, Vec<String>>);

// ------------------------------------------------------------------ requests

/// Optional admin credentials in the body (alternative to HTTP Basic auth).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Credentials {
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct IdTokenRequest {
    /// Microsoft (Azure AD) ID token of the user; its `tid` and `oid` claims give the routing key.
    pub id_token: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AgentIdRequest {
    /// Agent UUID (`agent_uid` on the core backend).
    pub agent_id: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BackendCreateRequest {
    /// Display name, 1 to 255 characters.
    pub name: String,
    /// `scheme://host[:port]` without path; empty = not routed by nginx.
    pub upstream_url: Option<String>,
    /// The backend's `BACKEND_ID`; checked against `GET /internal/info` and used for the Entra token audience.
    pub backend_id: Option<String>,
    /// Default `true`.
    pub is_active: Option<bool>,
    /// Default `0`.
    pub computer_sync_version: Option<i32>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BackendUpdateRequest {
    pub name: Option<String>,
    /// `""` removes the URL.
    pub upstream_url: Option<String>,
    /// `""` removes the expected backend id (no identity check).
    pub backend_id: Option<String>,
    pub is_active: Option<bool>,
    pub computer_sync_version: Option<i32>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ScheduleUpdateRequest {
    pub enabled: Option<bool>,
    /// 5 to 31 536 000 seconds.
    pub interval_secs: Option<i32>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Query of the user / agent mapping lists.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MappingsQuery {
    /// Case-insensitive text found in the id or in the name/sku.
    pub search: Option<String>,
    /// `true`, `false` or `all` (default).
    pub active: Option<String>,
    /// 1 to 5000, default 500.
    pub limit: Option<i64>,
}

/// Query of the run history.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RunsQuery {
    /// Only this task name.
    pub task: Option<String>,
    /// 1 to 500, default 50.
    pub limit: Option<i64>,
}

// ------------------------------------------------------------------ responses

#[derive(Debug, Serialize, ToSchema)]
pub struct MappingsResponse {
    pub mappings: Vec<Mapping>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct LoginOk {
    pub ok: bool,
    pub username: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendList {
    pub backends: Vec<BackendView>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SkippedKey {
    pub prefix_key: String,
    pub reason: String,
}

/// Result of regenerating the nginx map (`error` is set instead of the other fields when it failed).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NginxMapResult {
    pub entries: Option<i64>,
    pub changed: Option<bool>,
    pub skipped: Option<Vec<SkippedKey>>,
    pub file: Option<String>,
    pub error: Option<String>,
}

/// A backend (all `BackendView` fields) plus the nginx map result.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BackendWithMap {
    pub id: i64,
    pub prefix_key: String,
    pub name: String,
    pub is_active: bool,
    pub computer_sync_version: i32,
    pub upstream_url: String,
    pub backend_id: String,
    pub created: String,
    pub modified: String,
    pub users_active: i64,
    pub agents_active: i64,
    pub users_version: Option<i64>,
    pub agents_version: Option<i64>,
    pub nginx_map: NginxMapResult,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteResult {
    pub deleted: DeleteCounts,
    pub nginx_map: NginxMapResult,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserMapping {
    pub id: i64,
    /// Routing key `entra:<tenant UUID>:<object UUID>`.
    pub user_id: String,
    /// Legacy column, no longer filled by the sync.
    pub display_name: String,
    pub is_active: bool,
    pub created: String,
    pub modified: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AgentMapping {
    pub id: i64,
    pub agent_id: String,
    pub sku: String,
    pub is_active: bool,
    pub created: String,
    pub modified: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserMappingList {
    /// Rows matching the filter.
    pub total: i64,
    /// Rows returned (limited by `limit`).
    pub count: i64,
    pub items: Vec<UserMapping>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AgentMappingList {
    pub total: i64,
    pub count: i64,
    pub items: Vec<AgentMapping>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NginxEntry {
    pub prefix_key: String,
    pub target: String,
    /// `git` (static backends.map) or `database` (generated map).
    pub source: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NginxStatus {
    pub git_file: String,
    pub generated_file: String,
    pub entries: Vec<NginxEntry>,
    pub problems: Vec<String>,
    pub reload_pending: bool,
    /// Last result written by the nginx watcher script; `null` until nginx reports.
    #[schema(value_type = Option<Object>)]
    pub nginx_status: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ReloadRequested {
    pub reload_requested: bool,
    pub nginx_map: NginxMapResult,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TaskRun {
    pub id: i64,
    pub task: String,
    /// `schedule`, `manual`, `admin`, `force` or `startup`.
    pub trigger: String,
    /// `running`, `succeeded` or `failed`.
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    #[schema(value_type = Option<Object>)]
    pub result: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ScheduleOut {
    pub name: String,
    pub enabled: bool,
    pub interval_secs: i32,
    pub last_run_at: Option<String>,
    pub updated_at: String,
    /// A run of this task is in progress in this process.
    pub running: bool,
    pub last_run: Option<TaskRun>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ScheduleList {
    pub scheduler_enabled: bool,
    /// This replica holds the sync leadership (runs the scheduler and the stream sync).
    pub leader: bool,
    pub schedules: Vec<ScheduleOut>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StreamCursor {
    pub backend: i64,
    pub prefix_key: String,
    /// `users` or `agents`.
    pub entity: String,
    pub epoch: String,
    pub version: i64,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SyncStatus {
    pub sync_enabled: bool,
    pub leader: bool,
    pub leader_election: bool,
    pub streams: Vec<StreamCursor>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Conflict {
    /// Routing key (`entra:<tid>:<oid>` or agent UUID).
    pub key: String,
    /// `prefix_key` of every active backend that claims it.
    pub backends: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ConflictGroup {
    pub total: i64,
    /// At most 200 rows.
    pub items: Vec<Conflict>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ConflictList {
    /// `reject` (conflicting keys resolve to 409) or `all` (every claiming backend is returned).
    pub policy: String,
    pub users: ConflictGroup,
    pub agents: ConflictGroup,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RunStarted {
    pub run_id: i64,
    pub task: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RunList {
    pub runs: Vec<TaskRun>,
}

// ------------------------------------------------------------------ contracts

type Named = Vec<(String, ResponseContract)>;

/// Success (or any documented) response with a JSON body of type `T`.
fn body<T: ToSchema>(status: u16) -> ResponseContract {
    ResponseContract::documented(status, SchemaDef::of::<T>())
}

/// DRF `{"detail": "..."}` error.
fn error(status: u16) -> ResponseContract {
    body::<Detail>(status)
}

/// 400 is either `{"detail": ...}` (bad JSON, bad token) or `{"<field>": [...]}` (field errors).
fn bad_request() -> ContractResult<ResponseContract> {
    let schema = ResponseSchemaFactory::variants(
        "BadRequestError",
        vec![SchemaDef::of::<Detail>(), SchemaDef::of::<FieldErrors>()],
    )?;
    Ok(ResponseContract::documented(400, schema))
}

fn named(items: Vec<(&str, ResponseContract)>) -> Named {
    items.into_iter().map(|(name, contract)| (name.to_string(), contract)).collect()
}

/// Responses of an admin operation: 401 / 503 are always possible.
fn admin_set(success: ResponseContract, problems: Vec<(&str, ResponseContract)>) -> ContractResult<ResponseSet> {
    let mut all = vec![("unauthorized", error(401)), ("admin_disabled", error(503))];
    all.extend(problems);
    ResponseSet::new(success, Vec::new(), named(all))
}

struct Op {
    method: &'static str,
    path: String,
    id: &'static str,
    summary: &'static str,
    admin: bool,
    contract: OperationContract,
}

fn operations() -> ContractResult<Vec<Op>> {
    const ADMIN: &str = "/api/v1/admin";
    let bad = bad_request()?;
    let id_param = |b: api_contracts::OperationBuilder| b.path_param("id", "integer", "Backend id");
    let name_param = |b: api_contracts::OperationBuilder| {
        b.path_param("name", "string", "Schedule name, e.g. sync_all_backend_users")
    };
    let mut ops = Vec::new();

    // ---- public routing API
    ops.push(Op {
        method: "post",
        path: ID_TOKEN_PATH.to_string(),
        id: "routingByIdToken",
        summary: "Backends of a user (Microsoft ID token)",
        admin: false,
        contract: OperationContract::from_responses(ResponseSet::new(
            body::<MappingsResponse>(200),
            Vec::new(),
            named(vec![
                ("bad_request", bad.clone()),
                ("not_found", error(404)),
                ("conflict", error(409)),
                ("unsupported_media_type", error(415)),
            ]),
        )?)
        .request::<IdTokenRequest>()
        .tags(["routing"])
        .build()?,
    });
    ops.push(Op {
        method: "post",
        path: AGENT_ID_PATH.to_string(),
        id: "routingByAgentId",
        summary: "Backends of an agent",
        admin: false,
        contract: OperationContract::from_responses(ResponseSet::new(
            body::<MappingsResponse>(200),
            Vec::new(),
            named(vec![
                ("bad_request", bad.clone()),
                ("not_found", error(404)),
                ("conflict", error(409)),
                ("unsupported_media_type", error(415)),
            ]),
        )?)
        .request::<AgentIdRequest>()
        .tags(["routing"])
        .build()?,
    });

    // ---- admin API
    ops.push(Op {
        method: "post",
        path: format!("{ADMIN}/login"),
        id: "adminLogin",
        summary: "Check the admin credentials",
        admin: true,
        contract: OperationContract::from_responses(admin_set(
            body::<LoginOk>(200),
            vec![("bad_request", bad.clone())],
        )?)
        .request::<Credentials>()
        .tags(["admin"])
        .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/backends"),
        id: "adminListBackends",
        summary: "List backends",
        admin: true,
        contract: OperationContract::from_responses(admin_set(body::<BackendList>(200), vec![])?)
            .tags(["admin"])
            .build()?,
    });
    ops.push(Op {
        method: "post",
        path: format!("{ADMIN}/backends"),
        id: "adminCreateBackend",
        summary: "Create a backend (prefix_key is generated)",
        admin: true,
        contract: OperationContract::from_responses(admin_set(
            body::<BackendWithMap>(201),
            vec![("bad_request", bad.clone())],
        )?)
        .request::<BackendCreateRequest>()
        .tags(["admin"])
        .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/backends/{{id}}"),
        id: "adminGetBackend",
        summary: "Get one backend",
        admin: true,
        contract: id_param(OperationContract::from_responses(admin_set(
            body::<BackendView>(200),
            vec![("not_found", error(404))],
        )?))
        .tags(["admin"])
        .build()?,
    });
    // The router accepts PUT as an alias of PATCH.
    for (method, id, summary) in [
        ("patch", "adminUpdateBackend", "Update a backend"),
        ("put", "adminReplaceBackend", "Update a backend (alias of PATCH, fields are still optional)"),
    ] {
        ops.push(Op {
            method,
            path: format!("{ADMIN}/backends/{{id}}"),
            id,
            summary,
            admin: true,
            contract: id_param(OperationContract::from_responses(admin_set(
                body::<BackendWithMap>(200),
                vec![("bad_request", bad.clone()), ("not_found", error(404))],
            )?))
            .request::<BackendUpdateRequest>()
            .tags(["admin"])
            .build()?,
        });
    }
    ops.push(Op {
        method: "delete",
        path: format!("{ADMIN}/backends/{{id}}"),
        id: "adminDeleteBackend",
        summary: "Delete a backend and its user/agent mappings",
        admin: true,
        contract: id_param(OperationContract::from_responses(admin_set(
            body::<DeleteResult>(200),
            vec![("not_found", error(404))],
        )?))
        .tags(["admin"])
        .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/backends/{{id}}/users"),
        id: "adminListBackendUsers",
        summary: "User mappings of a backend",
        admin: true,
        contract: id_param(OperationContract::from_responses(admin_set(
            body::<UserMappingList>(200),
            vec![("not_found", error(404))],
        )?))
        .query::<MappingsQuery>()
        .tags(["admin"])
        .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/backends/{{id}}/agents"),
        id: "adminListBackendAgents",
        summary: "Agent mappings of a backend",
        admin: true,
        contract: id_param(OperationContract::from_responses(admin_set(
            body::<AgentMappingList>(200),
            vec![("not_found", error(404))],
        )?))
        .query::<MappingsQuery>()
        .tags(["admin"])
        .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/nginx"),
        id: "adminNginxStatus",
        summary: "Nginx routing table, problems and last reload result",
        admin: true,
        contract: OperationContract::from_responses(admin_set(body::<NginxStatus>(200), vec![])?)
            .tags(["admin"])
            .build()?,
    });
    ops.push(Op {
        method: "post",
        path: format!("{ADMIN}/nginx/reload"),
        id: "adminNginxReload",
        summary: "Regenerate the map and force nginx to reload",
        admin: true,
        contract: OperationContract::from_responses(admin_set(
            body::<ReloadRequested>(202),
            vec![("server_error", error(500))],
        )?)
        .tags(["admin"])
        .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/schedules"),
        id: "adminListSchedules",
        summary: "Schedules and their last run",
        admin: true,
        contract: OperationContract::from_responses(admin_set(body::<ScheduleList>(200), vec![])?)
            .tags(["admin"])
            .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/schedules/{{name}}"),
        id: "adminGetSchedule",
        summary: "One schedule and its last run",
        admin: true,
        contract: name_param(OperationContract::from_responses(admin_set(
            body::<ScheduleOut>(200),
            vec![("not_found", error(404))],
        )?))
        .tags(["admin"])
        .build()?,
    });
    for (method, id, summary) in [
        ("patch", "adminUpdateSchedule", "Enable/disable a schedule or change its interval"),
        ("put", "adminReplaceSchedule", "Same as PATCH (alias)"),
    ] {
        ops.push(Op {
            method,
            path: format!("{ADMIN}/schedules/{{name}}"),
            id,
            summary,
            admin: true,
            contract: name_param(OperationContract::from_responses(admin_set(
                body::<ScheduleOut>(200),
                vec![("bad_request", bad.clone()), ("not_found", error(404))],
            )?))
            .request::<ScheduleUpdateRequest>()
            .tags(["admin"])
            .build()?,
        });
    }
    ops.push(Op {
        method: "post",
        path: format!("{ADMIN}/schedules/{{name}}/run"),
        id: "adminRunSchedule",
        summary: "Run a task now",
        admin: true,
        contract: name_param(OperationContract::from_responses(admin_set(
            body::<RunStarted>(202),
            vec![("not_found", error(404)), ("already_running", error(409))],
        )?))
        .tags(["admin"])
        .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/task-runs"),
        id: "adminListRuns",
        summary: "Run history",
        admin: true,
        contract: OperationContract::from_responses(admin_set(body::<RunList>(200), vec![])?)
            .query::<RunsQuery>()
            .tags(["admin"])
            .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/sync"),
        id: "adminSyncStatus",
        summary: "Routing stream cursors and sync leadership",
        admin: true,
        contract: OperationContract::from_responses(admin_set(body::<SyncStatus>(200), vec![])?)
            .tags(["admin"])
            .build()?,
    });
    ops.push(Op {
        method: "get",
        path: format!("{ADMIN}/conflicts"),
        id: "adminListConflicts",
        summary: "Keys claimed by more than one active backend",
        admin: true,
        contract: OperationContract::from_responses(admin_set(body::<ConflictList>(200), vec![])?)
            .tags(["admin"])
            .build()?,
    });
    Ok(ops)
}

/// The complete OpenAPI 3.1 document.
pub fn document() -> ContractResult<Value> {
    let mut doc = OpenApiDocument::new("LabMango Router", env!("CARGO_PKG_VERSION"));
    for op in operations()? {
        let mut extra = Map::new();
        extra.insert("operationId".into(), json!(op.id));
        extra.insert("summary".into(), json!(op.summary));
        if op.admin {
            extra.insert("security".into(), json!([{ "basicAuth": [] }]));
        }
        doc = doc.operation_with(op.method, &op.path, &op.contract, extra)?;
    }
    let mut value = doc.to_value();
    value["info"]["description"] = json!(
        "Routes users and agents to their department backend. Routing endpoints are public; \
         through nginx they are served under /routing/... instead of /api/v1/routing/... \
         A key claimed by several backends answers 409 (ROUTING_CONFLICT_POLICY=reject). \
         Admin endpoints need the admin account (HTTP Basic, or username/password in the JSON body). \
         Error bodies keep the Django REST Framework shapes."
    );
    value["components"]["securitySchemes"] = json!({
        "basicAuth": { "type": "http", "scheme": "basic", "description": "ADMIN_USERNAME / ADMIN_PASSWORD from .env" }
    });
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::jobs::Schedule;

    const ROUTES: [(&str, &str); 21] = [
        ("post", "/api/v1/routing/auth/backend-info/by-id-token/"),
        ("post", "/api/v1/routing/auth/backend-info/by-agent-id/"),
        ("post", "/api/v1/admin/login"),
        ("get", "/api/v1/admin/backends"),
        ("post", "/api/v1/admin/backends"),
        ("get", "/api/v1/admin/backends/{id}"),
        ("patch", "/api/v1/admin/backends/{id}"),
        ("put", "/api/v1/admin/backends/{id}"),
        ("delete", "/api/v1/admin/backends/{id}"),
        ("get", "/api/v1/admin/backends/{id}/users"),
        ("get", "/api/v1/admin/backends/{id}/agents"),
        ("get", "/api/v1/admin/nginx"),
        ("post", "/api/v1/admin/nginx/reload"),
        ("get", "/api/v1/admin/schedules"),
        ("get", "/api/v1/admin/schedules/{name}"),
        ("patch", "/api/v1/admin/schedules/{name}"),
        ("put", "/api/v1/admin/schedules/{name}"),
        ("post", "/api/v1/admin/schedules/{name}/run"),
        ("get", "/api/v1/admin/task-runs"),
        ("get", "/api/v1/admin/sync"),
        ("get", "/api/v1/admin/conflicts"),
    ];

    fn refs(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    match (k.as_str(), x.as_str()) {
                        ("$ref", Some(target)) => out.push(target.to_string()),
                        _ => refs(x, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|x| refs(x, out)),
            _ => {}
        }
    }

    #[test]
    fn document_lists_every_route() {
        let doc = document().unwrap();
        assert_eq!(doc["openapi"], "3.1.0");
        for (method, path) in ROUTES {
            assert!(doc["paths"][path][method].is_object(), "missing {method} {path}");
        }
        let operations: usize = doc["paths"].as_object().unwrap().values().map(|p| p.as_object().unwrap().len()).sum();
        assert_eq!(operations, ROUTES.len());
    }

    #[test]
    fn every_ref_resolves_to_a_component() {
        let doc = document().unwrap();
        let mut found = Vec::new();
        refs(&doc, &mut found);
        assert!(!found.is_empty());
        for target in found {
            let name = target.strip_prefix("#/components/schemas/").unwrap_or_else(|| panic!("bad ref {target}"));
            assert!(doc["components"]["schemas"][name].is_object(), "dangling $ref {target}");
        }
    }

    #[test]
    fn operation_ids_are_unique_and_admin_is_protected() {
        let ops = operations().unwrap();
        let mut ids: Vec<_> = ops.iter().map(|o| o.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ops.len());

        let doc = document().unwrap();
        for (method, path) in ROUTES {
            let protected = !doc["paths"][path][method]["security"].is_null();
            assert_eq!(protected, path.contains("/admin/"), "{method} {path}");
        }
    }

    #[test]
    fn path_parameters_are_declared() {
        let doc = document().unwrap();
        let params = doc["paths"]["/api/v1/admin/backends/{id}/users"]["get"]["parameters"].as_array().unwrap();
        assert!(params.iter().any(|p| p["name"] == "id" && p["in"] == "path"));
        assert!(params.iter().any(|p| p["name"] == "search" && p["in"] == "query"));
        let params = doc["paths"]["/api/v1/admin/schedules/{name}"]["patch"]["parameters"].as_array().unwrap();
        assert!(params.iter().any(|p| p["name"] == "name" && p["in"] == "path"));
    }

    /// The mirrors must accept what the handlers really send.
    #[test]
    fn dtos_accept_the_real_structs() {
        let schedule = Schedule {
            name: "sync_all_backend_users".into(),
            enabled: true,
            interval_secs: 60,
            last_run_at: None,
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        let mut v = serde_json::to_value(&schedule).unwrap();
        v["running"] = json!(false);
        v["last_run"] = Value::Null;
        serde_json::from_value::<ScheduleOut>(v).unwrap();

        let backend = BackendView {
            id: 1,
            prefix_key: "abc".into(),
            name: "B".into(),
            is_active: true,
            computer_sync_version: 0,
            upstream_url: String::new(),
            backend_id: "dept_a".into(),
            created: "2026-01-01T00:00:00Z".into(),
            modified: "2026-01-01T00:00:00Z".into(),
            users_active: 0,
            agents_active: 0,
            users_version: Some(3),
            agents_version: None,
        };
        let mut v = serde_json::to_value(&backend).unwrap();
        v["nginx_map"] = json!({"entries": 1, "changed": true, "skipped": [], "file": "/x"});
        serde_json::from_value::<BackendWithMap>(v).unwrap();
        let v = json!({"nginx_map": {"error": "boom"}});
        serde_json::from_value::<NginxMapResult>(v["nginx_map"].clone()).unwrap();
    }
}
