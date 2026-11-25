//! OpenAPI 3.1 document of the service, built with the `api-contracts` crate and committed as
//! `openapi/openapi.json` (`make openapi`; CI fails when the committed file is out of date).
//!
//! Responses follow the API envelope: `{"data": ...}` (plus `meta` where shown) and
//! `{"error": {"code", "message", "details", "request_id"}}`. Database structs that are returned as they are
//! (`BackendView`, `Route`) derive `ToSchema` directly; the other shapes are built with `json!` in the handlers
//! and mirrored here, and the tests check the mirrors against the real values.

#![allow(dead_code)]

use api_contracts::{
    OpenApiDocument, OperationBuilder, OperationContract, ResponseContract, ResponseSet, Result, SchemaDef,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use utoipa::ToSchema;

use crate::{
    admin::API as ADMIN,
    db::{backends::BackendView, claims::Route},
    routing::{RESOLVE_AGENT_PATH, RESOLVE_USER_PATH},
};

// ------------------------------------------------------------------ errors

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorDetail {
    /// Request field or query parameter.
    pub field: String,
    /// Machine-stable reason, e.g. `required`, `unknown_field`, `out_of_range`.
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    /// Machine-stable error code, e.g. `validation_failed`, `not_routed`, `unauthorized`.
    pub code: String,
    pub message: String,
    pub details: Vec<ErrorDetail>,
    /// Same value as the `X-Request-ID` response header.
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

// ------------------------------------------------------------------ requests

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveUserRequest {
    /// Microsoft Entra ID token of the user; its `tid` and `oid` claims give the routing key.
    pub id_token: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveAgentRequest {
    /// Agent UUID (`agent_uid` on the backend).
    pub agent_id: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BackendCreateRequest {
    /// Display name, 1 to 255 characters.
    pub name: String,
    /// The backend's `BACKEND_ID`; empty = not checked.
    pub backend_id: Option<String>,
    /// `scheme://host[:port]` without a path; empty = not routed.
    pub upstream_url: Option<String>,
    /// Default `true`.
    pub is_active: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BackendUpdateRequest {
    pub name: Option<String>,
    pub backend_id: Option<String>,
    /// `""` stops routing the backend.
    pub upstream_url: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobUpdateRequest {
    pub enabled: Option<bool>,
    /// 5 to 31 536 000 seconds.
    pub interval_secs: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ClaimsQuery {
    /// Case-insensitive part of the key.
    pub search: Option<String>,
    /// 1 to 5000, default 500.
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RunsQuery {
    /// Only runs of this job.
    pub job: Option<String>,
    /// 1 to 500, default 50.
    pub limit: Option<i64>,
}

// ------------------------------------------------------------------ response payloads

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Session {
    pub username: String,
}

/// Outcome of rewriting the nginx map (`error` instead of the other fields when it failed).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NginxMapResult {
    pub entries: Option<i64>,
    pub changed: Option<bool>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MutationMeta {
    pub nginx_map: NginxMapResult,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Claim {
    /// `entra:<tenant uuid>:<object uuid>` or the agent UUID.
    pub key: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ClaimsMeta {
    /// Claims matching the filter (the list is cut at `limit`).
    pub total: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MapEntry {
    pub prefix_key: String,
    pub upstream_url: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NginxStatus {
    pub map_file: String,
    pub entries: Vec<MapEntry>,
    pub problems: Vec<String>,
    pub reload_pending: bool,
    /// Last result reported by the nginx watcher; `null` until nginx reports.
    #[schema(value_type = Option<Object>)]
    pub nginx: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ReloadRequested {
    pub reload_requested: bool,
    pub nginx_map: NginxMapResult,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct JobRun {
    pub id: i64,
    pub job: String,
    /// `schedule`, `manual`, `admin` or `startup`.
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
pub struct Job {
    /// `check_users_stream`, `check_agents_stream` or `write_nginx_map`.
    pub name: String,
    pub enabled: bool,
    pub interval_secs: i32,
    pub last_run_at: Option<String>,
    pub updated_at: String,
    /// A run is in progress in this replica.
    pub running: bool,
    pub last_run: Option<JobRun>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RunStarted {
    pub run_id: i64,
    pub job: String,
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
    /// This replica runs the stream sync and the scheduler.
    pub leader: bool,
    pub leader_election: bool,
    pub streams: Vec<StreamCursor>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Conflict {
    pub key: String,
    /// `prefix_key` of every active backend that claims the key.
    pub backends: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ConflictGroup {
    pub total: i64,
    /// At most 200 rows.
    pub items: Vec<Conflict>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Conflicts {
    pub users: ConflictGroup,
    pub agents: ConflictGroup,
}

// ------------------------------------------------------------------ envelopes with meta

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendMutationResponse {
    pub data: BackendView,
    pub meta: MutationMeta,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ClaimListResponse {
    pub data: Vec<Claim>,
    pub meta: ClaimsMeta,
}

// ------------------------------------------------------------------ contracts

fn ok<T: ToSchema + serde::de::DeserializeOwned + 'static>(status: u16, name: &str) -> Result<ResponseContract> {
    ResponseContract::success_with::<T>(name, false, status)
}

fn ok_list<T: ToSchema + serde::de::DeserializeOwned + 'static>(name: &str) -> Result<ResponseContract> {
    ResponseContract::success_with::<T>(name, true, 200)
}

fn raw<T: ToSchema>(status: u16) -> ResponseContract {
    ResponseContract::documented(status, SchemaDef::of::<T>())
}

fn error(status: u16) -> ResponseContract {
    raw::<ErrorResponse>(status)
}

fn errors(statuses: &[(&str, u16)]) -> Vec<(String, ResponseContract)> {
    statuses.iter().map(|(name, status)| (name.to_string(), error(*status))).collect()
}

struct Op {
    method: &'static str,
    path: String,
    id: &'static str,
    summary: &'static str,
    admin: bool,
    contract: OperationContract,
}

fn op(
    method: &'static str,
    path: impl Into<String>,
    id: &'static str,
    summary: &'static str,
    success: ResponseContract,
    problems: &[(&str, u16)],
    build: impl FnOnce(OperationBuilder) -> OperationBuilder,
) -> Result<Op> {
    let path = path.into();
    let admin = path.starts_with(ADMIN);
    let mut all = problems.to_vec();
    if admin {
        all.extend([("unauthorized", 401), ("admin_disabled", 503)]);
    }
    let builder = OperationContract::from_responses(ResponseSet::new(success, Vec::new(), errors(&all))?)
        .tags([if admin { "admin" } else { "routing" }]);
    Ok(Op { method, path, id, summary, admin, contract: build(builder).build()? })
}

fn id_param(b: OperationBuilder) -> OperationBuilder {
    b.path_param("id", "integer", "Backend id")
}

fn name_param(b: OperationBuilder) -> OperationBuilder {
    b.path_param("name", "string", "Job name, e.g. check_users_stream")
}

fn operations() -> Result<Vec<Op>> {
    let none = |b: OperationBuilder| b;
    Ok(vec![
        // ---- routing (public)
        op(
            "post",
            RESOLVE_USER_PATH,
            "resolveUser",
            "Backend of a user (Microsoft ID token)",
            ok::<Route>(200, "ResolvedUser")?,
            &[
                ("validation_failed", 400),
                ("not_routed", 404),
                ("ambiguous_route", 409),
                ("unsupported_media_type", 415),
            ],
            |b| b.request::<ResolveUserRequest>(),
        )?,
        op(
            "post",
            RESOLVE_AGENT_PATH,
            "resolveAgent",
            "Backend of an agent",
            ok::<Route>(200, "ResolvedAgent")?,
            &[
                ("validation_failed", 400),
                ("not_routed", 404),
                ("ambiguous_route", 409),
                ("unsupported_media_type", 415),
            ],
            |b| b.request::<ResolveAgentRequest>(),
        )?,
        // ---- admin
        op(
            "get",
            format!("{ADMIN}/session"),
            "adminSession",
            "Check the admin credentials",
            ok::<Session>(200, "Session")?,
            &[],
            none,
        )?,
        op("get", format!("{ADMIN}/backends"), "listBackends", "List backends", raw::<BackendList>(200), &[], none)?,
        op(
            "post",
            format!("{ADMIN}/backends"),
            "createBackend",
            "Create a backend (prefix_key is generated)",
            raw::<BackendMutationResponse>(201),
            &[("validation_failed", 400)],
            |b| b.request::<BackendCreateRequest>(),
        )?,
        op(
            "get",
            format!("{ADMIN}/backends/{{id}}"),
            "getBackend",
            "One backend",
            raw::<BackendDetail>(200),
            &[("not_found", 404)],
            id_param,
        )?,
        op(
            "patch",
            format!("{ADMIN}/backends/{{id}}"),
            "updateBackend",
            "Update a backend (every field optional)",
            raw::<BackendMutationResponse>(200),
            &[("validation_failed", 400), ("not_found", 404)],
            |b| id_param(b).request::<BackendUpdateRequest>(),
        )?,
        op(
            "delete",
            format!("{ADMIN}/backends/{{id}}"),
            "deleteBackend",
            "Delete a backend with its claims and cursors",
            ResponseContract::no_content(),
            &[("not_found", 404)],
            id_param,
        )?,
        op(
            "get",
            format!("{ADMIN}/backends/{{id}}/users"),
            "listUserClaims",
            "User claims of a backend",
            raw::<ClaimListResponse>(200),
            &[("validation_failed", 400), ("not_found", 404)],
            |b| id_param(b).query::<ClaimsQuery>(),
        )?,
        op(
            "get",
            format!("{ADMIN}/backends/{{id}}/agents"),
            "listAgentClaims",
            "Agent claims of a backend",
            raw::<ClaimListResponse>(200),
            &[("validation_failed", 400), ("not_found", 404)],
            |b| id_param(b).query::<ClaimsQuery>(),
        )?,
        op(
            "get",
            format!("{ADMIN}/nginx"),
            "nginxStatus",
            "nginx map, problems and last reload result",
            ok::<NginxStatus>(200, "Nginx")?,
            &[],
            none,
        )?,
        op(
            "post",
            format!("{ADMIN}/nginx/reload"),
            "reloadNginx",
            "Rewrite the map and make nginx reload",
            ok::<ReloadRequested>(202, "NginxReload")?,
            &[],
            none,
        )?,
        op(
            "get",
            format!("{ADMIN}/jobs"),
            "listJobs",
            "Jobs with their schedule and last run",
            ok_list::<Job>("Jobs")?,
            &[],
            none,
        )?,
        op(
            "get",
            format!("{ADMIN}/jobs/{{name}}"),
            "getJob",
            "One job",
            ok::<Job>(200, "JobDetail")?,
            &[("not_found", 404)],
            name_param,
        )?,
        op(
            "patch",
            format!("{ADMIN}/jobs/{{name}}"),
            "updateJob",
            "Enable/disable a job or change its interval",
            ok::<Job>(200, "JobUpdated")?,
            &[("validation_failed", 400), ("not_found", 404)],
            |b| name_param(b).request::<JobUpdateRequest>(),
        )?,
        op(
            "post",
            format!("{ADMIN}/jobs/{{name}}/run"),
            "runJob",
            "Run a job now (Location: the run)",
            ok::<RunStarted>(202, "RunStarted")?,
            &[("not_found", 404), ("already_running", 409)],
            name_param,
        )?,
        op(
            "get",
            format!("{ADMIN}/runs"),
            "listRuns",
            "Run history, newest first",
            ok_list::<JobRun>("Runs")?,
            &[("validation_failed", 400)],
            |b| b.query::<RunsQuery>(),
        )?,
        op(
            "get",
            format!("{ADMIN}/runs/{{id}}"),
            "getRun",
            "One run",
            ok::<JobRun>(200, "RunDetail")?,
            &[("not_found", 404)],
            |b| b.path_param("id", "integer", "Run id"),
        )?,
        op(
            "get",
            format!("{ADMIN}/sync"),
            "syncStatus",
            "Stream cursors and sync leadership",
            ok::<SyncStatus>(200, "Sync")?,
            &[],
            none,
        )?,
        op(
            "get",
            format!("{ADMIN}/sync/conflicts"),
            "listConflicts",
            "Keys claimed by more than one active backend",
            ok::<Conflicts>(200, "Conflicts")?,
            &[],
            none,
        )?,
    ])
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendList {
    pub data: Vec<BackendView>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendDetail {
    pub data: BackendView,
}

/// The complete OpenAPI 3.1 document.
pub fn document() -> Result<Value> {
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
        "Routes users and agents to their department backend. The routing endpoints are public; the admin API \
         needs the admin account (HTTP Basic). Responses use {\"data\": ...} and \
         {\"error\": {\"code\", \"message\", \"details\", \"request_id\"}}; every response carries X-Request-ID."
    );
    value["components"]["securitySchemes"] =
        json!({ "basicAuth": { "type": "http", "scheme": "basic", "description": "ADMIN_USERNAME / ADMIN_PASSWORD" } });
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::jobs::Schedule;

    const ROUTES: [(&str, &str); 20] = [
        ("post", "/routing/resolve/user"),
        ("post", "/routing/resolve/agent"),
        ("get", "/admin/api/session"),
        ("get", "/admin/api/backends"),
        ("post", "/admin/api/backends"),
        ("get", "/admin/api/backends/{id}"),
        ("patch", "/admin/api/backends/{id}"),
        ("delete", "/admin/api/backends/{id}"),
        ("get", "/admin/api/backends/{id}/users"),
        ("get", "/admin/api/backends/{id}/agents"),
        ("get", "/admin/api/nginx"),
        ("post", "/admin/api/nginx/reload"),
        ("get", "/admin/api/jobs"),
        ("get", "/admin/api/jobs/{name}"),
        ("patch", "/admin/api/jobs/{name}"),
        ("post", "/admin/api/jobs/{name}/run"),
        ("get", "/admin/api/runs"),
        ("get", "/admin/api/runs/{id}"),
        ("get", "/admin/api/sync"),
        ("get", "/admin/api/sync/conflicts"),
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
            assert!(!path.ends_with('/'), "trailing slash in {path}");
        }
        let operations: usize = doc["paths"].as_object().unwrap().values().map(|p| p.as_object().unwrap().len()).sum();
        assert_eq!(operations, ROUTES.len());
    }

    #[test]
    fn every_ref_resolves_and_admin_is_protected() {
        let doc = document().unwrap();
        let mut found = Vec::new();
        refs(&doc, &mut found);
        for target in found {
            let name = target.strip_prefix("#/components/schemas/").unwrap_or_else(|| panic!("bad ref {target}"));
            assert!(doc["components"]["schemas"][name].is_object(), "dangling $ref {target}");
        }
        for (method, path) in ROUTES {
            let protected = !doc["paths"][path][method]["security"].is_null();
            assert_eq!(protected, path.starts_with("/admin/"), "{method} {path}");
        }
        let ids: std::collections::HashSet<_> = operations().unwrap().iter().map(|o| o.id).collect();
        assert_eq!(ids.len(), ROUTES.len());
    }

    /// The mirrors accept what the handlers really send.
    #[test]
    fn mirrors_accept_the_real_values() {
        let schedule = Schedule {
            name: "check_users_stream".into(),
            enabled: true,
            interval_secs: 60,
            last_run_at: None,
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        let mut v = serde_json::to_value(&schedule).unwrap();
        v["running"] = json!(false);
        v["last_run"] = Value::Null;
        serde_json::from_value::<Job>(v).unwrap();
        serde_json::from_value::<NginxMapResult>(json!({"entries": 1, "changed": true})).unwrap();
        serde_json::from_value::<NginxMapResult>(json!({"error": "boom"})).unwrap();
        let body = crate::http::api::ApiError::field("x", "required", "m");
        let v =
            json!({"error": {"code": body.code, "message": body.message, "details": body.details, "request_id": "0"}});
        serde_json::from_value::<ErrorResponse>(v).unwrap();
    }
}
