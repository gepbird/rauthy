use crate::ApiKeyAccess;
use rauthy_common::constants::RE_API_KEY;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use validator::Validate;

#[derive(Debug, Serialize, Deserialize, Validate, ToSchema)]
pub struct ApiKeyRequest {
    /// Validation: `^[a-zA-Z0-9_-/]{2,24}$`
    #[validate(regex(path = "RE_API_KEY", code = "^[a-zA-Z0-9_-/]{2,24}$"))]
    pub name: String,
    /// Unix timestamp in seconds in the future (max year 2099)
    #[validate(range(min = 1672527600, max = 4070905200))]
    pub exp: Option<i64>,
    pub access: Vec<ApiKeyAccess>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiKeysResponse {
    pub keys: Vec<ApiKeyResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiKeyResponse {
    pub name: String,
    /// unix timestamp
    pub created: i64,
    /// unix timestamp
    pub expires: Option<i64>,
    pub access: Vec<ApiKeyAccess>,
}
