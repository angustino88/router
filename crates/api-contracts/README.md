# api-contracts

Response contracts for the LabMango Router.

One contract describes **both** the JSON your handler returns and the OpenAPI
documentation of that JSON, so they cannot drift apart. The generated OpenAPI 3.1
document can be imported into Apidog.

```rust
use api_contracts::{OpenApiDocument, OperationContract, ResponseContract};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, ToSchema)]
struct Widget { id: i64 }

fn main() -> api_contracts::Result<()> {
    let get_widget = OperationContract::builder(ResponseContract::success::<Widget>("Widget")?)
        .response("created", ResponseContract::created::<Widget>("WidgetCreated")?)
        .response("not_found", ResponseContract::problem(404)?)
        .tags(["widgets"])
        .build()?;

    // runtime: {"data": {"id": 7}}
    let response = get_widget.respond(&Widget { id: 7 })?;

    // docs: write openapi.json and import it in Apidog
    let doc = OpenApiDocument::new("Widgets API", "1.0.0")
        .operation("get", "/widgets/{id}", &get_widget)?;
    std::fs::write("openapi.json", doc.to_json_pretty()).unwrap();
    Ok(())
}
```

## Concepts

| Type | Role |
| --- | --- |
| `ResponseContract` | status code + schema + how to build the body (`respond`, `serialize`, `respond_data`, `respond_page`, `respond_problem`) |
| `ResponseSet` | `success` + named alternate successes (2xx/3xx) + named problems (4xx/5xx) |
| `OperationContract` | responses + optional request/query types + tags; `schema()` gives the OpenAPI operation |
| `ResponseSchemaFactory` | builds envelope schemas: `{data}`, `{data: []}`, `{data, meta.pagination}`, `oneOf` |
| `ContractSettings` | envelope keys, code resolver, problem message resolver |
| `OpenApiDocument` | collects operations into a full OpenAPI 3.1 JSON |

Request/query types must derive `serde::Deserialize` + `utoipa::ToSchema`;
payload types also derive `serde::Serialize`.

## axum

```toml
api-contracts = { version = "0.1", features = ["axum"] }
```

`ContractResponse` then implements `IntoResponse`.

## Notes

* The default problem schema (`ProblemDetail`) is the inner error object; `respond_problem` returns it
  wrapped as `{"error": {...}}`. Services that document the full envelope declare their own schema.

## Development

```shell
cargo test
cargo test --features axum
```
