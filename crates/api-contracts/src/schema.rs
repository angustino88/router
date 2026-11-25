use std::collections::BTreeMap;

use serde_json::{json, Value};
use utoipa::ToSchema;

use crate::error::{ContractError, Result};

/// OpenAPI `components.schemas` (name -> JSON schema).
pub type Components = BTreeMap<String, Value>;

/// Merge `other` into `into`. Same name with a different body is a conflict.
pub fn merge_components(into: &mut Components, other: &Components) -> Result<()> {
    for (name, schema) in other {
        match into.get(name) {
            Some(existing) if existing != schema => {
                return Err(ContractError::value(format!(
                    "conflicting OpenAPI schemas registered under the same component name: {name}"
                )));
            }
            Some(_) => {}
            None => {
                into.insert(name.clone(), schema.clone());
            }
        }
    }
    Ok(())
}

/// Where a response body schema points to.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaRef {
    /// No body (e.g. 204).
    None,
    /// `$ref: #/components/schemas/<name>`
    Component(String),
    /// `{type: array, items: <inner>}`
    Array(Box<SchemaRef>),
}

impl SchemaRef {
    pub fn to_json(&self) -> Option<Value> {
        match self {
            Self::None => None,
            Self::Component(name) => Some(json!({ "$ref": format!("#/components/schemas/{name}") })),
            Self::Array(inner) => inner.to_json().map(|items| json!({ "type": "array", "items": items })),
        }
    }
}

/// Counterpart of "a serializer (class or instance)" in the Python package:
/// a reference to a schema plus every component that reference needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaDef {
    pub root: SchemaRef,
    pub components: Components,
}

impl SchemaDef {
    /// No body.
    pub fn none() -> Self {
        Self { root: SchemaRef::None, components: Components::new() }
    }

    /// Schema of a Rust type deriving `utoipa::ToSchema` (like passing a serializer class).
    pub fn of<T: ToSchema>() -> Self {
        let name = T::name().into_owned();
        let mut list: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::Schema>)> =
            vec![(name.clone(), T::schema())];
        T::schemas(&mut list);
        let mut components = Components::new();
        for (n, s) in list {
            components.insert(n, serde_json::to_value(&s).unwrap_or(Value::Null));
        }
        Self { root: SchemaRef::Component(name), components }
    }

    /// Array of `T` (like `Serializer(many=True)`).
    pub fn array_of<T: ToSchema>() -> Self {
        let inner = Self::of::<T>();
        Self { root: SchemaRef::Array(Box::new(inner.root)), components: inner.components }
    }

    /// A hand-written component schema.
    pub fn component(name: impl Into<String>, schema: Value) -> Self {
        let name = name.into();
        let mut components = Components::new();
        components.insert(name.clone(), schema);
        Self { root: SchemaRef::Component(name), components }
    }

    /// Default problem schema (`ProblemDetailSerializer`).
    ///
    /// NOTE: kept exactly like the Python package. At runtime `respond_problem`
    /// wraps this object in `{"error": {...}}`, but the documented schema is the
    /// flat object. See README "Known quirks".
    pub fn problem_detail() -> Self {
        Self::component(
            "ProblemDetail",
            json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string" },
                    "message": { "type": "string" },
                    "details": { "type": "array", "items": { "type": "object", "additionalProperties": {} } },
                    "request_id": { "type": "string" }
                },
                "required": ["code", "message", "details", "request_id"]
            }),
        )
    }

    /// `$ref` / `array` JSON for this schema, `None` when there is no body.
    pub fn json(&self) -> Option<Value> {
        self.root.to_json()
    }
}

/// Factory for consistent OpenAPI response schemas (envelopes).
pub struct ResponseSchemaFactory;

impl ResponseSchemaFactory {
    /// `{ "data": <T> }` or `{ "data": [<T>] }`.
    pub fn success<T: ToSchema>(name: &str, many: bool) -> Result<SchemaDef> {
        Self::validate_name(name)?;
        let payload = SchemaDef::of::<T>();
        let kind = if many { "List" } else { "Detail" };
        let component = format!("{name}{}{kind}Response", Self::component_suffix::<T>());
        let data = if many { SchemaRef::Array(Box::new(payload.root.clone())) } else { payload.root.clone() };
        let mut def = SchemaDef::component(
            component,
            json!({
                "type": "object",
                "properties": { "data": data.to_json() },
                "required": ["data"]
            }),
        );
        merge_components(&mut def.components, &payload.components)?;
        Ok(def)
    }

    /// `{ "data": [<T>], "meta": { "pagination": {...} } }`.
    pub fn paginated<T: ToSchema>(name: &str) -> Result<SchemaDef> {
        Self::validate_name(name)?;
        let payload = SchemaDef::of::<T>();
        let prefix = format!("{name}{}", Self::component_suffix::<T>());

        let pagination = SchemaDef::component(
            format!("{prefix}Pagination"),
            json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer", "minimum": 0 },
                    "page": { "type": "integer", "minimum": 1 },
                    "page_size": { "type": "integer", "minimum": 1 },
                    "next": { "type": ["string", "null"] },
                    "previous": { "type": ["string", "null"] }
                },
                "required": ["count", "page", "page_size", "next", "previous"]
            }),
        );
        let meta = SchemaDef::component(
            format!("{prefix}PaginationMeta"),
            json!({
                "type": "object",
                "properties": { "pagination": pagination.json() },
                "required": ["pagination"]
            }),
        );
        let mut def = SchemaDef::component(
            format!("{prefix}PaginatedResponse"),
            json!({
                "type": "object",
                "properties": {
                    "data": SchemaRef::Array(Box::new(payload.root.clone())).to_json(),
                    "meta": meta.json()
                },
                "required": ["data", "meta"]
            }),
        );
        merge_components(&mut def.components, &pagination.components)?;
        merge_components(&mut def.components, &meta.components)?;
        merge_components(&mut def.components, &payload.components)?;
        Ok(def)
    }

    /// `{ "data": {} }`.
    pub fn empty(name: &str) -> Result<SchemaDef> {
        Self::validate_name(name)?;
        Ok(SchemaDef::component(
            format!("{name}EmptyResponse"),
            json!({
                "type": "object",
                "properties": { "data": { "type": "object", "additionalProperties": {}, "readOnly": true } },
                "required": ["data"]
            }),
        ))
    }

    /// `oneOf` over several schemas, registered as component `component_name`
    /// (drf-spectacular `PolymorphicProxySerializer`).
    pub fn variants(component_name: &str, schemas: Vec<SchemaDef>) -> Result<SchemaDef> {
        Self::validate_name(component_name)?;
        if schemas.is_empty() {
            return Err(ContractError::value("schemas must contain at least one serializer"));
        }
        let one_of: Vec<Value> = schemas.iter().filter_map(SchemaDef::json).collect();
        let mut def = SchemaDef::component(component_name, json!({ "oneOf": one_of }));
        for s in &schemas {
            merge_components(&mut def.components, &s.components)?;
        }
        Ok(def)
    }

    /// Either a plain list envelope or a paginated envelope.
    pub fn list_variants<T: ToSchema>(name: &str) -> Result<SchemaDef> {
        Self::variants(
            &format!("{name}ResponseVariant"),
            vec![Self::success::<T>(name, true)?, Self::paginated::<T>(name)?],
        )
    }

    pub fn polymorphic(component_name: &str, schemas: Vec<SchemaDef>) -> Result<SchemaDef> {
        Self::variants(component_name, schemas)
    }

    /// Responses map for one schema (Python returned an `extend_schema` decorator).
    pub fn document(schema: SchemaDef, status_code: u16) -> Vec<(u16, SchemaDef)> {
        vec![(status_code, schema)]
    }

    /// `document` for `Serializer(many=True)`.
    pub fn document_list<T: ToSchema>(status_code: u16) -> Vec<(u16, SchemaDef)> {
        Self::document(SchemaDef::array_of::<T>(), status_code)
    }

    pub(crate) fn validate_name(name: &str) -> Result<()> {
        if name.trim().is_empty() {
            return Err(ContractError::value("name must be a non-empty string"));
        }
        Ok(())
    }

    /// `<ModuleTitle><TypeName>` taken from the Rust type path, so two payload
    /// types with the same name in different modules get different components.
    pub(crate) fn component_suffix<T: ?Sized>() -> String {
        let full = std::any::type_name::<T>();
        let base = full.split('<').next().unwrap_or(full);
        let mut parts = base.rsplit("::");
        let type_name = parts.next().unwrap_or(base);
        let module = parts.next().unwrap_or("");
        let type_name = type_name.strip_suffix("Serializer").unwrap_or(type_name);
        format!("{}{}", py_title(module).replace('_', ""), type_name)
    }
}

/// Python `str.title()`.
pub(crate) fn py_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_alpha = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if prev_alpha {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alpha = true;
        } else {
            out.push(c);
            prev_alpha = false;
        }
    }
    out
}
