//! Runtime and OpenAPI response contracts.
//!
//! Rust port of the Python package `drf-api-contracts`: one contract describes
//! both the JSON a handler returns and the OpenAPI documentation of it, so the
//! two can never drift apart. The generated document can be imported in Apidog.
//!
//! ```ignore
//! use api_contracts::{OperationContract, ResponseContract};
//!
//! #[derive(serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
//! struct Widget { id: i64 }
//!
//! let op = OperationContract::builder(ResponseContract::success::<Widget>("Widget")?)
//!     .response("created", ResponseContract::created::<Widget>("WidgetCreated")?)
//!     .response("not_found", ResponseContract::problem(404)?)
//!     .tags(["widgets"])
//!     .build()?;
//!
//! let response = op.respond(&Widget { id: 7 })?;      // {"data": {"id": 7}}
//! let doc = op.schema()?;                             // OpenAPI operation + components
//! ```

mod error;
mod openapi;
mod operation;
mod response;
mod schema;
mod settings;

#[cfg(feature = "axum")]
mod axum_support;

pub use error::{ContractError, Result};
pub use openapi::{OpenApiDocument, OperationDoc};
pub use operation::{OperationBuilder, OperationContract, ResponseSet};
pub use response::{
    error_details, ContractResponse, ErrorTree, PageParams, ProblemArgs, ResponseContract,
};
pub use schema::{merge_components, Components, ResponseSchemaFactory, SchemaDef, SchemaRef};
pub use settings::{
    default_code_value, default_problem_detail, CodeResolver, ContractSettings, ProblemResolver,
    DEFAULT_PROBLEM_TYPE, PROBLEM_JSON,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
