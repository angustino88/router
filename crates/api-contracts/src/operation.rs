use std::collections::HashSet;

use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use utoipa::ToSchema;

use crate::error::{ContractError, Result};
use crate::openapi::{query_parameters, response_object, OperationDoc};
use crate::response::{validator_for, ContractResponse, ProblemArgs, ResponseContract, Validator};
use crate::schema::{merge_components, Components, SchemaDef};

/// Identifier check for response names: a letter or `_`, then letters, digits or `_`.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_alphabetic() => chars.all(|c| c == '_' || c.is_alphanumeric()),
        _ => false,
    }
}

/// All responses of one operation: the primary `success`, named extra
/// successes (`alternates`, 2xx/3xx) and named problems (4xx/5xx).
/// Order of declaration is preserved.
#[derive(Debug, Clone)]
pub struct ResponseSet {
    pub success: ResponseContract,
    pub alternates: Vec<(String, ResponseContract)>,
    pub problems: Vec<(String, ResponseContract)>,
}

impl ResponseSet {
    pub fn new(
        success: ResponseContract,
        alternates: Vec<(String, ResponseContract)>,
        problems: Vec<(String, ResponseContract)>,
    ) -> Result<Self> {
        for (name, _) in &alternates {
            if name == "success" || !is_identifier(name) {
                return Err(ContractError::value("alternate response names must be identifiers other than 'success'"));
            }
        }
        Self::validate_success("success", &success)?;
        for (name, contract) in &alternates {
            Self::validate_success(name, contract)?;
        }
        for (name, contract) in &problems {
            if contract.status_code < 400 {
                return Err(ContractError::value(format!("problem response must use a 4xx or 5xx status: {name}")));
            }
        }
        Ok(Self { success, alternates, problems })
    }

    fn validate_success(name: &str, contract: &ResponseContract) -> Result<()> {
        if contract.status_code >= 400 {
            return Err(ContractError::value(format!("success response must use a 2xx or 3xx status: {name}")));
        }
        Ok(())
    }

    /// Look up an alternate or problem by name.
    pub fn find(&self, name: &str) -> Option<&ResponseContract> {
        self.alternates.iter().chain(self.problems.iter()).find(|(n, _)| n == name).map(|(_, c)| c)
    }

    pub fn get(&self, name: &str) -> Result<&ResponseContract> {
        if name == "success" {
            return Ok(&self.success);
        }
        self.find(name).ok_or_else(|| ContractError::value(format!("response contract is not configured: {name}")))
    }

    /// `success` plus the named responses.
    pub fn document(&self, names: &[&str]) -> Result<Vec<(u16, SchemaDef)>> {
        let mut contracts = vec![&self.success];
        for name in names {
            contracts.push(self.get(name)?);
        }
        ResponseContract::document_many(&contracts)
    }

    /// Every configured response; errors on duplicate status codes.
    pub fn document_configured(&self) -> Result<Vec<(u16, SchemaDef)>> {
        let contracts: Vec<&ResponseContract> = std::iter::once(&self.success)
            .chain(self.alternates.iter().map(|(_, c)| c))
            .chain(self.problems.iter().map(|(_, c)| c))
            .collect();
        ResponseContract::document_many(&contracts)
    }
}

/// Insert or replace (keeping position) a named problem, like `dict[name] = value`.
fn set_problem(problems: &mut Vec<(String, ResponseContract)>, name: &str, contract: ResponseContract) {
    match problems.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = contract,
        None => problems.push((name.to_string(), contract)),
    }
}

/// Request/response contract of one API operation. Build it with
/// [`OperationContract::builder`].
#[derive(Clone)]
pub struct OperationContract {
    pub responses: ResponseSet,
    pub request: Option<SchemaDef>,
    pub query: Option<SchemaDef>,
    pub tags: Vec<String>,
    /// Ready-made OpenAPI `in: path` parameters (see [`OperationBuilder::path_param`]).
    pub path_params: Vec<Value>,
    request_validator: Option<Validator>,
    query_validator: Option<Validator>,
}

impl std::fmt::Debug for OperationContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationContract")
            .field("responses", &self.responses)
            .field("request", &self.request.as_ref().map(|s| &s.root))
            .field("query", &self.query.as_ref().map(|s| &s.root))
            .field("tags", &self.tags)
            .finish_non_exhaustive()
    }
}

/// Builder of an [`OperationContract`].
pub struct OperationBuilder {
    success: Option<ResponseContract>,
    response_set: Option<ResponseSet>,
    named: Vec<(String, ResponseContract)>,
    request: Option<(SchemaDef, Validator)>,
    query: Option<(SchemaDef, Validator)>,
    tags: Vec<String>,
    path_params: Vec<Value>,
}

impl OperationBuilder {
    /// Path parameter of the route, e.g. `.path_param("id", "integer", "Backend id")` for
    /// `/backends/{id}`. Always `required: true`.
    pub fn path_param(mut self, name: impl Into<String>, schema_type: &str, description: impl Into<String>) -> Self {
        self.path_params.push(json!({
            "name": name.into(),
            "in": "path",
            "required": true,
            "description": description.into(),
            "schema": { "type": schema_type }
        }));
        self
    }

    /// Named response (`created`, `not_found`, ...). 2xx/3xx become alternates,
    /// 4xx/5xx become problems.
    pub fn response(mut self, name: impl Into<String>, contract: ResponseContract) -> Self {
        self.named.push((name.into(), contract));
        self
    }

    /// Request body type. Adds `bad_request` (400) and `validation_error` (422)
    /// problems unless already configured.
    pub fn request<T: ToSchema + DeserializeOwned + 'static>(mut self) -> Self {
        self.request = Some((SchemaDef::of::<T>(), validator_for::<T>(false)));
        self
    }

    /// Query parameters type (an object schema). Adds `bad_request` (400).
    pub fn query<T: ToSchema + DeserializeOwned + 'static>(mut self) -> Self {
        self.query = Some((SchemaDef::of::<T>(), validator_for::<T>(false)));
        self
    }

    pub fn tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    pub fn build(self) -> Result<OperationContract> {
        let (request, request_validator) = match self.request {
            Some((s, v)) => (Some(s), Some(v)),
            None => (None, None),
        };
        let (query, query_validator) = match self.query {
            Some((s, v)) => (Some(s), Some(v)),
            None => (None, None),
        };

        let responses = if let Some(set) = self.response_set {
            if !self.named.is_empty() {
                return Err(ContractError::value("use either responses or success and named responses, not both"));
            }
            set
        } else {
            let success = self.success.ok_or_else(|| ContractError::type_err("success response must be configured"))?;
            let mut alternates = Vec::new();
            let mut problems = Vec::new();
            for (name, contract) in self.named {
                if contract.status_code < 400 {
                    alternates.push((name, contract));
                } else {
                    problems.push((name, contract));
                }
            }
            let mut configured: HashSet<u16> = problems.iter().map(|(_, c)| c.status_code).collect();
            if request.is_some() && !configured.contains(&400) {
                set_problem(&mut problems, "bad_request", ResponseContract::bad_request());
                configured.insert(400);
            }
            if request.is_some() && !configured.contains(&422) {
                set_problem(&mut problems, "validation_error", ResponseContract::problem(422)?);
                configured.insert(422);
            }
            if (query.is_some() || success.is_paginated) && !configured.contains(&400) {
                set_problem(&mut problems, "bad_request", ResponseContract::bad_request());
            }
            ResponseSet::new(success, alternates, problems)?
        };

        Ok(OperationContract {
            responses,
            request,
            query,
            tags: self.tags,
            path_params: self.path_params,
            request_validator,
            query_validator,
        })
    }
}

impl OperationContract {
    pub fn builder(success: ResponseContract) -> OperationBuilder {
        OperationBuilder {
            success: Some(success),
            response_set: None,
            named: Vec::new(),
            request: None,
            query: None,
            tags: Vec::new(),
            path_params: Vec::new(),
        }
    }

    /// Start from an explicit [`ResponseSet`].
    pub fn from_responses(responses: ResponseSet) -> OperationBuilder {
        OperationBuilder {
            success: None,
            response_set: Some(responses),
            named: Vec::new(),
            request: None,
            query: None,
            tags: Vec::new(),
            path_params: Vec::new(),
        }
    }

    // ---- OpenAPI ------------------------------------------------------------------

    /// OpenAPI operation object + the components it needs.
    pub fn schema(&self) -> Result<OperationDoc> {
        self.schema_with(Map::new())
    }

    /// Like [`schema`](Self::schema) with extra operation fields (`summary`,
    /// `operationId`, `tags`, ...). Extra fields win over contract values, so an
    /// explicit `tags` overrides the contract tags.
    pub fn schema_with(&self, extra: Map<String, Value>) -> Result<OperationDoc> {
        let mut operation = Map::new();
        let mut components = Components::new();

        if !self.tags.is_empty() {
            operation.insert("tags".into(), json!(self.tags));
        }
        if let Some(request) = &self.request {
            merge_components(&mut components, &request.components)?;
            operation.insert(
                "requestBody".into(),
                json!({
                    "required": true,
                    "content": { "application/json": { "schema": request.json().unwrap_or(Value::Null) } }
                }),
            );
        }
        let mut parameters: Vec<Value> = self.path_params.clone();
        if let Some(query) = &self.query {
            merge_components(&mut components, &query.components)?;
            parameters.extend(query_parameters(query)?);
        }
        if !parameters.is_empty() {
            operation.insert("parameters".into(), Value::Array(parameters));
        }
        let mut responses = Map::new();
        for (status, schema) in self.responses.document_configured()? {
            merge_components(&mut components, &schema.components)?;
            responses.insert(status.to_string(), response_object(status, &schema));
        }
        operation.insert("responses".into(), Value::Object(responses));

        for (key, value) in extra {
            operation.insert(key, value);
        }
        Ok(OperationDoc { operation: Value::Object(operation), components })
    }

    // ---- runtime ------------------------------------------------------------------

    /// Serialize `instance` into the primary success response.
    pub fn respond<S: serde::Serialize + ?Sized>(&self, instance: &S) -> Result<ContractResponse> {
        self.responses.success.serialize(instance)
    }

    pub fn respond_data(&self, data: Value) -> Result<ContractResponse> {
        self.responses.success.respond_data(data)
    }

    pub fn respond_problem(&self, name: &str, args: ProblemArgs) -> Result<ContractResponse> {
        self.responses.get(name)?.respond_problem(args)
    }

    /// Check a request body against the declared request type.
    /// (In axum prefer `Json<T>`; this is for dynamic payloads.)
    pub fn validate_request(&self, data: &Value) -> Result<()> {
        match &self.request_validator {
            Some(validate) => validate(data).map_err(ContractError::Validation),
            None => Err(ContractError::type_err("validate_request() requires a request schema")),
        }
    }

    /// Check query parameters (as a JSON object) against the declared query type.
    pub fn validate_query(&self, data: &Value) -> Result<()> {
        match &self.query_validator {
            Some(validate) => validate(data).map_err(ContractError::Validation),
            None => Err(ContractError::type_err("validate_query() requires a query schema")),
        }
    }
}
