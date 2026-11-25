//! Port of `tests/test_contracts.py` plus a few extra cases.

use api_contracts::{
    default_code_value, ContractError, ContractSettings, ErrorTree, OpenApiDocument, OperationContract, PageParams,
    ProblemArgs, ResponseContract, ResponseSchemaFactory, SchemaDef, SchemaRef,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct Widget {
    id: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct WidgetQuery {
    search: Option<String>,
    limit: i64,
}

fn no_results() -> Vec<Value> {
    Vec::new()
}

#[test]
fn success_contract_validates_and_preserves_materialized_payload() {
    let contract = ResponseContract::success::<Widget>("Widget").unwrap();

    let response = contract.respond_data(json!({"id": 7})).unwrap();
    assert_eq!(response.body, Some(json!({"data": {"id": 7}})));

    let err = contract.respond_data(json!({})).unwrap_err();
    assert!(matches!(err, ContractError::Validation(_)));
}

#[test]
fn named_response_factories_use_the_expected_http_statuses() {
    assert_eq!(ResponseContract::ok::<Widget>("Widget").unwrap().status_code, 200);
    assert_eq!(ResponseContract::created::<Widget>("Widget").unwrap().status_code, 201);
    assert_eq!(ResponseContract::accepted::<Widget>("Widget").unwrap().status_code, 202);
    assert_eq!(ResponseContract::no_content().status_code, 204);
    assert_eq!(ResponseContract::bad_request().status_code, 400);
    assert_eq!(ResponseContract::not_found().status_code, 404);
}

#[test]
fn polymorphic_contract_wraps_multiple_success_shapes() {
    let contract = ResponseContract::polymorphic(
        "WidgetAnalytics",
        vec![SchemaDef::of::<Widget>(), SchemaDef::array_of::<Widget>()],
        200,
    )
    .unwrap();

    assert_eq!(contract.status_code, 200);
    assert_eq!(contract.schema.root, SchemaRef::Component("WidgetAnalytics".into()));
    let one_of = contract.schema.components["WidgetAnalytics"]["oneOf"].as_array().unwrap();
    assert_eq!(one_of.len(), 2);
}

#[test]
fn paginated_contract_owns_runtime_and_schema_shape() {
    let contract = ResponseContract::paginated::<Widget>("Widget").unwrap();

    let response = contract
        .respond_page(
            1,
            &[json!({"id": 7})],
            PageParams { page: 1, page_size: Some(25), next: Some("/widgets/?page=2".into()), ..Default::default() },
        )
        .unwrap();

    assert_eq!(
        response.body,
        Some(json!({
            "data": [{"id": 7}],
            "meta": {
                "pagination": {
                    "count": 1,
                    "page": 1,
                    "page_size": 25,
                    "next": "/widgets/?page=2",
                    "previous": null
                }
            }
        }))
    );
    assert_eq!(contract.schema, ResponseSchemaFactory::paginated::<Widget>("Widget").unwrap());
}

#[test]
fn application_settings_customize_wire_conventions() {
    let settings =
        ContractSettings { success_key: "result".into(), pagination_meta_key: "metadata".into(), ..Default::default() };
    let contract = ResponseContract::paginated::<Widget>("WidgetCustom").unwrap().with_settings(settings);

    let response = contract
        .respond_page(1, &[json!({"id": 7})], PageParams { page_size: Some(10), ..Default::default() })
        .unwrap();

    let body = response.body.unwrap();
    assert_eq!(body["result"], json!([{"id": 7}]));
    assert_eq!(body["metadata"]["pagination"]["page_size"], json!(10));
}

#[test]
fn paginated_contract_rejects_ambiguous_materialized_data() {
    let contract = ResponseContract::paginated::<Widget>("Widget").unwrap();

    let err = contract.respond_data(json!({"total_count": 1, "results": [{"id": 7}]})).unwrap_err();
    assert!(matches!(&err, ContractError::Type(m) if m.contains("respond_page() with explicit page metadata")));
}

#[test]
fn paginated_contract_supports_empty_page_with_positive_page_size() {
    let contract = ResponseContract::paginated::<Widget>("Widget").unwrap();

    let body = contract.respond_page(0, &no_results(), PageParams::default()).unwrap().body.unwrap();

    assert_eq!(body["data"], json!([]));
    assert_eq!(body["meta"]["pagination"]["page_size"], json!(1));
}

#[test]
fn paginated_contract_validates_metadata() {
    let contract = ResponseContract::paginated::<Widget>("Widget").unwrap();
    let items = [json!({"id": 7})];

    let err = contract.respond_page(-1, &items, PageParams::default()).unwrap_err();
    assert_eq!(err, ContractError::Value("total_count must be non-negative".into()));

    let err = contract.respond_page(1, &items, PageParams { page: 0, ..Default::default() }).unwrap_err();
    assert_eq!(err, ContractError::Value("page must be at least 1".into()));

    let err = contract.respond_page(1, &items, PageParams { page_size: Some(0), ..Default::default() }).unwrap_err();
    assert_eq!(err, ContractError::Value("page_size must be positive".into()));
}

#[test]
fn operation_contract_combines_documentation_and_runtime_behavior() {
    let operation = OperationContract::builder(ResponseContract::success::<Widget>("WidgetOperation").unwrap())
        .response("not_found", ResponseContract::problem(404).unwrap())
        .tags(["widgets", "operations"])
        .build()
        .unwrap();

    let statuses: Vec<u16> = operation.responses.document_configured().unwrap().iter().map(|(s, _)| *s).collect();
    assert_eq!(statuses, vec![200, 404]);
    assert_eq!(operation.respond(&Widget { id: 7 }).unwrap().body, Some(json!({"data": {"id": 7}})));
    assert_eq!(operation.respond_problem("not_found", ProblemArgs::new()).unwrap().status, 404);

    let doc = operation.schema().unwrap();
    assert_eq!(doc.operation["tags"], json!(["widgets", "operations"]));
}

#[test]
fn explicit_schema_tags_override_contract_defaults() {
    let operation = OperationContract::builder(ResponseContract::success::<Widget>("WidgetOverride").unwrap())
        .tags(["default"])
        .build()
        .unwrap();

    let mut extra = Map::new();
    extra.insert("tags".into(), json!(["explicit"]));
    let doc = operation.schema_with(extra).unwrap();

    assert_eq!(doc.operation["tags"], json!(["explicit"]));
}

#[test]
fn operation_contract_accepts_named_success_responses_directly() {
    let operation = OperationContract::builder(ResponseContract::success::<Widget>("Widget").unwrap())
        .response("created", ResponseContract::created::<Widget>("WidgetCreated").unwrap())
        .response("not_found", ResponseContract::problem(404).unwrap())
        .build()
        .unwrap();

    let statuses: Vec<u16> = operation.responses.document_configured().unwrap().iter().map(|(s, _)| *s).collect();
    assert_eq!(statuses, vec![200, 201, 404]);
    let created = operation.responses.find("created").unwrap();
    assert_eq!(created.respond_data(json!({"id": 7})).unwrap().status, 201);
}

#[test]
fn operation_contract_rejects_invalid_or_duplicate_named_responses() {
    let operation = OperationContract::builder(ResponseContract::ok::<Widget>("Widget").unwrap())
        .response("same_status", ResponseContract::ok::<Widget>("WidgetAgain").unwrap())
        .build()
        .unwrap();

    let err = operation.responses.document_configured().unwrap_err();
    assert!(matches!(&err, ContractError::Value(m) if m.contains("duplicate response status code")));
}

// ---- extra coverage -----------------------------------------------------------------

#[test]
fn request_and_query_add_default_problem_responses() {
    let operation = OperationContract::builder(ResponseContract::paginated::<Widget>("WidgetList").unwrap())
        .request::<Widget>()
        .query::<WidgetQuery>()
        .build()
        .unwrap();

    let statuses: Vec<u16> = operation.responses.document_configured().unwrap().iter().map(|(s, _)| *s).collect();
    assert_eq!(statuses, vec![200, 400, 422]);

    let doc = operation.schema().unwrap();
    let params = doc.operation["parameters"].as_array().unwrap();
    let limit = params.iter().find(|p| p["name"] == "limit").unwrap();
    assert_eq!(limit["in"], json!("query"));
    assert_eq!(limit["required"], json!(true));
    assert!(doc.operation["requestBody"]["content"]["application/json"]["schema"]["$ref"].is_string());

    assert!(operation.validate_request(&json!({"id": 1})).is_ok());
    assert!(operation.validate_request(&json!({})).is_err());
}

#[test]
fn problem_response_envelope_and_details() {
    let contract = ResponseContract::bad_request();
    let errors = ErrorTree::from(json!({
        "name": ["required"],
        "items": [{"qty": "must be positive"}]
    }));
    let response = contract
        .respond_problem(ProblemArgs::new().code("ValidationFailed").errors(errors).request_id("req-1"))
        .unwrap();

    let body = response.body.unwrap();
    assert_eq!(body["error"]["code"], json!("validation_failed"));
    assert_eq!(body["error"]["message"], json!("One or more fields are invalid."));
    assert_eq!(body["error"]["request_id"], json!("req-1"));
    assert_eq!(
        body["error"]["details"],
        json!([
            {"field": "items[0].qty", "code": "invalid", "message": "must be positive"},
            {"field": "name", "code": "invalid", "message": "required"}
        ])
    );
}

#[test]
fn problem_defaults() {
    let body = ResponseContract::not_found().respond_problem(ProblemArgs::new()).unwrap().body.unwrap();
    assert_eq!(body["error"]["code"], json!("api_error"));
    assert_eq!(body["error"]["message"], json!("Not Found"));
    assert_eq!(body["error"]["request_id"], json!("unknown"));
}

#[test]
fn code_values_are_snake_cased() {
    assert_eq!(default_code_value("NotFound"), "not_found");
    assert_eq!(default_code_value("not-found"), "not_found");
    assert_eq!(default_code_value("  Invalid Query!! "), "invalid_query");
    assert_eq!(default_code_value("HTTPError"), "httperror");
    assert_eq!(default_code_value("v2Beta"), "v2_beta");
}

#[test]
fn openapi_document_collects_operations_and_components() {
    let get = OperationContract::builder(ResponseContract::success::<Widget>("Widget").unwrap())
        .response("not_found", ResponseContract::not_found())
        .tags(["widgets"])
        .build()
        .unwrap();
    let doc = OpenApiDocument::new("Widgets", "1.0.0").operation("GET", "/widgets/{id}", &get).unwrap().to_value();

    assert_eq!(doc["openapi"], json!("3.1.0"));
    let op = &doc["paths"]["/widgets/{id}"]["get"];
    assert_eq!(op["responses"]["200"]["description"], json!("OK"));
    assert!(op["responses"]["404"]["content"]["application/json"]["schema"]["$ref"].is_string());
    assert!(doc["components"]["schemas"]["ProblemDetail"].is_object());
    assert!(doc["components"]["schemas"]["Widget"].is_object());
}

#[test]
fn path_params_are_listed_before_query_params() {
    let operation = OperationContract::builder(ResponseContract::success::<Widget>("Widget").unwrap())
        .path_param("id", "integer", "Widget id")
        .query::<WidgetQuery>()
        .build()
        .unwrap();

    let doc = operation.schema().unwrap();
    let params = doc.operation["parameters"].as_array().unwrap();
    assert_eq!(params[0]["name"], json!("id"));
    assert_eq!(params[0]["in"], json!("path"));
    assert_eq!(params[0]["required"], json!(true));
    assert!(params.iter().any(|p| p["name"] == "limit" && p["in"] == "query"));
}
