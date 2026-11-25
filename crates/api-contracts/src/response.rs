use std::collections::HashSet;
use std::sync::Arc;

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Map, Value};
use utoipa::ToSchema;

use crate::error::{ContractError, Result};
use crate::schema::{ResponseSchemaFactory, SchemaDef};
use crate::settings::ContractSettings;

/// Type-erased "is this JSON a valid `T`?" check (Python: `serializer.is_valid`).
pub(crate) type Validator = Arc<dyn Fn(&Value) -> std::result::Result<(), String> + Send + Sync>;

pub(crate) fn validator_for<T: DeserializeOwned + 'static>(many: bool) -> Validator {
    Arc::new(move |value: &Value| {
        if many {
            let items = value.as_array().ok_or_else(|| "expected a list of items".to_string())?;
            for (i, item) in items.iter().enumerate() {
                serde_json::from_value::<T>(item.clone()).map_err(|e| format!("item {i}: {e}"))?;
            }
            Ok(())
        } else {
            serde_json::from_value::<T>(value.clone()).map(|_| ()).map_err(|e| e.to_string())
        }
    })
}

/// Framework-agnostic HTTP response produced by a contract.
/// Enable the `axum` feature to return it directly from handlers.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// JSON body; `None` for 204.
    pub body: Option<Value>,
}

impl ContractResponse {
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Pagination arguments of [`ResponseContract::respond_page`].
#[derive(Debug, Clone, PartialEq)]
pub struct PageParams {
    pub page: i64,
    /// `None` => number of items on this page (at least 1).
    pub page_size: Option<i64>,
    pub next: Option<String>,
    pub previous: Option<String>,
}

impl Default for PageParams {
    fn default() -> Self {
        Self { page: 1, page_size: None, next: None, previous: None }
    }
}

/// Arguments of [`ResponseContract::respond_problem`].
#[derive(Debug, Clone, Default)]
pub struct ProblemArgs {
    pub code: Option<String>,
    pub detail: Option<Value>,
    pub errors: Option<ErrorTree>,
    pub request_id: Option<String>,
}

impl ProblemArgs {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn detail(mut self, detail: impl Into<Value>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    pub fn errors(mut self, errors: impl Into<ErrorTree>) -> Self {
        self.errors = Some(errors.into());
        self
    }
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }
}

/// Nested validation errors, shaped like DRF's `serializer.errors`.
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorTree {
    None,
    /// DRF `ErrorDetail` (message + code).
    Detail {
        code: String,
        message: String,
    },
    Map(Vec<(String, ErrorTree)>),
    List(Vec<ErrorTree>),
    /// Plain text; reported with code `invalid`.
    Other(String),
}

impl From<&str> for ErrorTree {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

impl From<Value> for ErrorTree {
    fn from(v: Value) -> Self {
        match v {
            Value::Null => Self::None,
            Value::Object(map) => Self::Map(map.into_iter().map(|(k, v)| (k, v.into())).collect()),
            Value::Array(items) => Self::List(items.into_iter().map(Into::into).collect()),
            // DRF error strings are `ErrorDetail`s (they carry a code), default "invalid".
            Value::String(message) => Self::Detail { code: "invalid".to_string(), message },
            other => Self::Other(other.to_string()),
        }
    }
}

#[derive(Clone)]
enum Payload {
    /// No payload type: `serialize()` is not available, `respond_data` skips validation.
    None,
    Typed(Validator),
    /// `EmptyPayloadSerializer`.
    Empty,
}

/// One documented-and-runtime response: status code + schema + how to build the body.
#[derive(Clone)]
pub struct ResponseContract {
    pub status_code: u16,
    pub schema: SchemaDef,
    pub many: bool,
    pub is_paginated: bool,
    pub settings: ContractSettings,
    payload: Payload,
}

impl std::fmt::Debug for ResponseContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseContract")
            .field("status_code", &self.status_code)
            .field("schema", &self.schema.root)
            .field("many", &self.many)
            .field("is_paginated", &self.is_paginated)
            .finish_non_exhaustive()
    }
}

impl ResponseContract {
    fn raw(status_code: u16, schema: SchemaDef, payload: Payload, many: bool, is_paginated: bool) -> Self {
        Self { status_code, schema, many, is_paginated, settings: ContractSettings::default(), payload }
    }

    /// Replace the wire conventions (Python: `settings=` argument).
    pub fn with_settings(mut self, settings: ContractSettings) -> Self {
        self.settings = settings;
        self
    }

    // ---- success ------------------------------------------------------------------

    pub fn success_with<T>(name: &str, many: bool, status_code: u16) -> Result<Self>
    where
        T: ToSchema + DeserializeOwned + 'static,
    {
        Self::validate_success_status(status_code)?;
        let schema = ResponseSchemaFactory::success::<T>(name, many)?;
        Ok(Self::raw(status_code, schema, Payload::Typed(validator_for::<T>(many)), many, false))
    }

    pub fn success<T: ToSchema + DeserializeOwned + 'static>(name: &str) -> Result<Self> {
        Self::success_with::<T>(name, false, 200)
    }

    pub fn ok<T: ToSchema + DeserializeOwned + 'static>(name: &str) -> Result<Self> {
        Self::success_with::<T>(name, false, 200)
    }

    pub fn ok_list<T: ToSchema + DeserializeOwned + 'static>(name: &str) -> Result<Self> {
        Self::success_with::<T>(name, true, 200)
    }

    pub fn created<T: ToSchema + DeserializeOwned + 'static>(name: &str) -> Result<Self> {
        Self::success_with::<T>(name, false, 201)
    }

    pub fn accepted<T: ToSchema + DeserializeOwned + 'static>(name: &str) -> Result<Self> {
        Self::success_with::<T>(name, false, 202)
    }

    pub fn paginated_with<T>(name: &str, status_code: u16) -> Result<Self>
    where
        T: ToSchema + DeserializeOwned + 'static,
    {
        Self::validate_success_status(status_code)?;
        let schema = ResponseSchemaFactory::paginated::<T>(name)?;
        Ok(Self::raw(status_code, schema, Payload::Typed(validator_for::<T>(true)), true, true))
    }

    pub fn paginated<T: ToSchema + DeserializeOwned + 'static>(name: &str) -> Result<Self> {
        Self::paginated_with::<T>(name, 200)
    }

    pub fn empty(name: &str) -> Result<Self> {
        Self::empty_with(name, 200)
    }

    pub fn empty_with(name: &str, status_code: u16) -> Result<Self> {
        Self::validate_success_status(status_code)?;
        Ok(Self::raw(status_code, ResponseSchemaFactory::empty(name)?, Payload::Empty, false, false))
    }

    pub fn no_content() -> Self {
        Self::documented(204, SchemaDef::none())
    }

    // ---- documentation-only -------------------------------------------------------

    /// Document a schema without a payload type (200).
    pub fn from_schema(schema: SchemaDef) -> Self {
        Self::documented(200, schema)
    }

    pub fn documented(status_code: u16, schema: SchemaDef) -> Self {
        Self::raw(status_code, schema, Payload::None, false, false)
    }

    pub fn polymorphic(component_name: &str, variants: Vec<SchemaDef>, status_code: u16) -> Result<Self> {
        Ok(Self::documented(status_code, ResponseSchemaFactory::polymorphic(component_name, variants)?))
    }

    // ---- problems -----------------------------------------------------------------

    pub fn problem(status_code: u16) -> Result<Self> {
        Self::problem_with(status_code, SchemaDef::problem_detail())
    }

    pub fn problem_with(status_code: u16, schema: SchemaDef) -> Result<Self> {
        if status_code < 400 {
            return Err(ContractError::value("problem responses require a 4xx or 5xx status code"));
        }
        Ok(Self::documented(status_code, schema))
    }

    fn known_problem(status_code: u16) -> Self {
        Self::documented(status_code, SchemaDef::problem_detail())
    }

    pub fn bad_request() -> Self {
        Self::known_problem(400)
    }
    pub fn unauthorized() -> Self {
        Self::known_problem(401)
    }
    pub fn forbidden() -> Self {
        Self::known_problem(403)
    }
    pub fn not_found() -> Self {
        Self::known_problem(404)
    }
    pub fn conflict() -> Self {
        Self::known_problem(409)
    }

    fn validate_success_status(status_code: u16) -> Result<()> {
        if !(200..400).contains(&status_code) {
            return Err(ContractError::value("success responses require a 2xx or 3xx status code"));
        }
        Ok(())
    }

    // ---- documentation ------------------------------------------------------------

    pub fn document(&self) -> (u16, SchemaDef) {
        (self.status_code, self.schema.clone())
    }

    /// Several contracts -> `[(status, schema)]`; a duplicated status is an error.
    pub fn document_many(contracts: &[&ResponseContract]) -> Result<Vec<(u16, SchemaDef)>> {
        let mut seen = HashSet::new();
        let mut documented = Vec::with_capacity(contracts.len());
        for contract in contracts {
            if !seen.insert(contract.status_code) {
                return Err(ContractError::value(format!("duplicate response status code: {}", contract.status_code)));
            }
            documented.push(contract.document());
        }
        Ok(documented)
    }

    // ---- runtime ------------------------------------------------------------------

    /// Wrap already materialised data: `{ <success_key>: data }`.
    pub fn respond(&self, data: Value) -> Result<ContractResponse> {
        if self.is_paginated {
            return Err(ContractError::type_err("paginated contracts require respond_page()"));
        }
        // 204 must not carry a body (the Python version leaves that to Django).
        let body = if self.status_code == 204 {
            None
        } else {
            let mut map = Map::new();
            map.insert(self.settings.success_key.clone(), data);
            Some(Value::Object(map))
        };
        Ok(ContractResponse { status: self.status_code, headers: Vec::new(), body })
    }

    /// Serialize a Rust value (struct / `Vec`) and wrap it.
    pub fn serialize<S: Serialize + ?Sized>(&self, instance: &S) -> Result<ContractResponse> {
        if self.is_paginated {
            return Err(ContractError::type_err("paginated contracts require respond_page()"));
        }
        let data = match &self.payload {
            Payload::None => {
                return Err(ContractError::type_err(
                    "serialize() requires a success contract with a payload serializer",
                ))
            }
            Payload::Empty => json!({}),
            Payload::Typed(_) => {
                let data = serde_json::to_value(instance)
                    .map_err(|e| ContractError::type_err(format!("cannot serialize instance: {e}")))?;
                if self.many != data.is_array() {
                    return Err(ContractError::type_err(if self.many {
                        "this contract is declared with many=true: pass a list/slice"
                    } else {
                        "this contract is declared with many=false: pass a single item"
                    }));
                }
                data
            }
        };
        self.respond(data)
    }

    /// Validate `data` against the payload type, then wrap the original data.
    pub fn respond_data(&self, data: Value) -> Result<ContractResponse> {
        if self.is_paginated {
            return Err(ContractError::type_err(
                "paginated contracts require respond_page() with explicit page metadata",
            ));
        }
        match &self.payload {
            Payload::Typed(validate) => validate(&data).map_err(ContractError::Validation)?,
            Payload::Empty if !data.is_object() => {
                return Err(ContractError::Validation("expected an object".to_string()))
            }
            _ => {}
        }
        self.respond(data)
    }

    /// Paginated envelope: `{ data: [...], meta: { pagination: {...} } }`.
    pub fn respond_page<R: Serialize + ?Sized>(
        &self,
        total_count: i64,
        results: &R,
        params: PageParams,
    ) -> Result<ContractResponse> {
        if !self.is_paginated || matches!(self.payload, Payload::None) {
            return Err(ContractError::type_err("respond_page() requires a paginated success contract"));
        }
        if total_count < 0 {
            return Err(ContractError::value("total_count must be non-negative"));
        }
        if params.page < 1 {
            return Err(ContractError::value("page must be at least 1"));
        }
        let data = serde_json::to_value(results)
            .map_err(|e| ContractError::type_err(format!("cannot serialize results: {e}")))?;
        let items =
            data.as_array().ok_or_else(|| ContractError::type_err("respond_page() requires results to be a list"))?;
        let page_size = match params.page_size {
            Some(size) => size,
            None => items.len().max(1) as i64,
        };
        if page_size < 1 {
            return Err(ContractError::value("page_size must be positive"));
        }
        let mut body = Map::new();
        body.insert(self.settings.success_key.clone(), data);
        body.insert(
            self.settings.pagination_meta_key.clone(),
            json!({
                "pagination": {
                    "count": total_count,
                    "page": params.page,
                    "page_size": page_size,
                    "next": params.next,
                    "previous": params.previous,
                }
            }),
        );
        Ok(ContractResponse { status: self.status_code, headers: Vec::new(), body: Some(Value::Object(body)) })
    }

    /// `{ "error": { code, message, details, request_id } }`.
    pub fn respond_problem(&self, args: ProblemArgs) -> Result<ContractResponse> {
        if self.status_code < 400 {
            return Err(ContractError::value("problem responses require a 4xx or 5xx status code"));
        }
        let code = args.code.as_deref().filter(|c| !c.is_empty());
        let error_code = match code {
            Some(c) => (self.settings.code_resolver)(c),
            None => "api_error".to_string(),
        };
        let message = (self.settings.problem_detail_resolver)(args.detail.as_ref(), code, self.status_code);
        let details = match &args.errors {
            Some(tree) => error_details(tree, ""),
            None => Vec::new(),
        };
        Ok(ContractResponse {
            status: self.status_code,
            headers: Vec::new(),
            body: Some(json!({
                "error": {
                    "code": error_code,
                    "message": message,
                    "details": details,
                    "request_id": args.request_id.unwrap_or_else(|| "unknown".to_string()),
                }
            })),
        })
    }
}

fn field_value(prefix: &str) -> Value {
    if prefix.is_empty() {
        Value::Null
    } else {
        Value::String(prefix.to_string())
    }
}

fn tree_text(tree: &ErrorTree) -> String {
    match tree {
        ErrorTree::Other(s) => s.clone(),
        ErrorTree::Detail { message, .. } => message.clone(),
        ErrorTree::None => "None".to_string(),
        ErrorTree::Map(_) | ErrorTree::List(_) => format!("{tree:?}"),
    }
}

/// Flatten nested errors into `[{field, code, message}]` (`field` is a dotted /
/// indexed path such as `items[0].name`, or `null` at the top level).
pub fn error_details(errors: &ErrorTree, prefix: &str) -> Vec<Value> {
    match errors {
        ErrorTree::None => Vec::new(),
        ErrorTree::Detail { code, message } => {
            vec![json!({ "field": field_value(prefix), "code": code, "message": message })]
        }
        ErrorTree::Other(text) => {
            vec![json!({ "field": field_value(prefix), "code": "invalid", "message": text })]
        }
        ErrorTree::Map(items) => {
            // A ready-made `{field, code, message}` mapping is passed through.
            let lookup = |key: &str| items.iter().find(|(k, _)| k == key).map(|(_, v)| v);
            if items.len() == 3 {
                if let (Some(f), Some(c), Some(m)) = (lookup("field"), lookup("code"), lookup("message")) {
                    return vec![json!({
                        "field": tree_text(f),
                        "code": tree_text(c),
                        "message": tree_text(m),
                    })];
                }
            }
            let mut details = Vec::new();
            for (field, value) in items {
                let path = if prefix.is_empty() { field.clone() } else { format!("{prefix}.{field}") };
                details.extend(error_details(value, &path));
            }
            details
        }
        ErrorTree::List(items) => {
            let mut details = Vec::new();
            for (index, value) in items.iter().enumerate() {
                if !prefix.is_empty() && matches!(value, ErrorTree::Detail { .. }) {
                    details.extend(error_details(value, prefix));
                } else {
                    let path = if prefix.is_empty() { index.to_string() } else { format!("{prefix}[{index}]") };
                    details.extend(error_details(value, &path));
                }
            }
            details
        }
    }
}
