use std::collections::{BTreeMap, HashSet};

use serde_json::{json, Map, Value};

use crate::error::{ContractError, Result};
use crate::operation::OperationContract;
use crate::schema::{merge_components, Components, SchemaDef, SchemaRef};
use crate::settings::http_phrase;

/// Result of [`OperationContract::schema`]: an OpenAPI operation object and the
/// `components.schemas` entries it references.
#[derive(Debug, Clone, PartialEq)]
pub struct OperationDoc {
    pub operation: Value,
    pub components: Components,
}

pub(crate) fn response_object(status: u16, schema: &SchemaDef) -> Value {
    let description = http_phrase(status);
    match schema.json() {
        Some(s) => json!({
            "description": description,
            "content": { "application/json": { "schema": s } }
        }),
        None => json!({ "description": description }),
    }
}

/// Expand the properties of an object schema into OpenAPI `in: query` parameters
/// (what drf-spectacular does when a serializer is passed as `parameters`).
pub(crate) fn query_parameters(schema: &SchemaDef) -> Result<Vec<Value>> {
    let SchemaRef::Component(name) = &schema.root else {
        return Err(ContractError::type_err("query schema must be an object type"));
    };
    let component = schema
        .components
        .get(name)
        .ok_or_else(|| ContractError::type_err(format!("query schema component not found: {name}")))?;
    let properties = component
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| ContractError::type_err("query schema must be an object with properties"))?;
    let required: HashSet<&str> = component
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Ok(properties
        .iter()
        .map(|(key, value)| {
            json!({
                "name": key,
                "in": "query",
                "required": required.contains(key.as_str()),
                "schema": value
            })
        })
        .collect())
}

/// Collects operations into one OpenAPI 3.1 document you can import into Apidog.
///
/// ```ignore
/// let doc = OpenApiDocument::new("Widgets API", "1.0.0")
///     .operation("get", "/widgets/{id}", &get_widget)?
///     .operation("post", "/widgets", &create_widget)?;
/// std::fs::write("openapi.json", doc.to_json_pretty())?;
/// ```
#[derive(Debug, Clone)]
pub struct OpenApiDocument {
    title: String,
    version: String,
    paths: BTreeMap<String, Map<String, Value>>,
    components: Components,
}

impl OpenApiDocument {
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self { title: title.into(), version: version.into(), paths: BTreeMap::new(), components: Components::new() }
    }

    pub fn operation(self, method: &str, path: &str, contract: &OperationContract) -> Result<Self> {
        self.operation_with(method, path, contract, Map::new())
    }

    /// Same as [`operation`](Self::operation) with extra operation fields
    /// (`summary`, `operationId`, `description`, `tags` override...).
    pub fn operation_with(
        mut self,
        method: &str,
        path: &str,
        contract: &OperationContract,
        extra: Map<String, Value>,
    ) -> Result<Self> {
        let method = method.to_ascii_lowercase();
        if !["get", "put", "post", "delete", "options", "head", "patch", "trace"].contains(&method.as_str()) {
            return Err(ContractError::value(format!("unsupported HTTP method: {method}")));
        }
        let doc = contract.schema_with(extra)?;
        merge_components(&mut self.components, &doc.components)?;
        let item = self.paths.entry(path.to_string()).or_default();
        if item.insert(method.clone(), doc.operation).is_some() {
            return Err(ContractError::value(format!("operation already defined: {method} {path}")));
        }
        Ok(self)
    }

    pub fn to_value(&self) -> Value {
        let paths: Map<String, Value> =
            self.paths.iter().map(|(path, item)| (path.clone(), Value::Object(item.clone()))).collect();
        let schemas: Map<String, Value> = self.components.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        json!({
            "openapi": "3.1.0",
            "info": { "title": self.title, "version": self.version },
            "paths": paths,
            "components": { "schemas": schemas }
        })
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(&self.to_value()).unwrap_or_default()
    }
}
