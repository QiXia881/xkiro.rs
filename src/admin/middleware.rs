//! Admin API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use parking_lot::RwLock;

use super::service::AdminService;
use super::types::AdminErrorResponse;
use crate::common::auth;
use crate::model::config::CompressionConfig;

/// Admin API 共享状态
#[derive(Clone)]
pub struct AdminState {
    /// Admin API 密钥
    pub admin_api_key: Arc<RwLock<String>>,
    /// Admin 服务
    pub service: Arc<AdminService>,
    /// 共享压缩配置（运行时可修改）
    pub compression_config: Arc<RwLock<CompressionConfig>>,
}

impl AdminState {
    pub fn new(
        admin_api_key: Arc<RwLock<String>>,
        service: AdminService,
        compression_config: Arc<RwLock<CompressionConfig>>,
    ) -> Self {
        Self {
            admin_api_key,
            service: Arc::new(service),
            compression_config,
        }
    }
}

/// Admin API 认证中间件
pub async fn admin_auth_middleware(
    State(state): State<AdminState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let api_key = auth::extract_api_key(&request);

    match api_key {
        Some(key) if auth::constant_time_eq(&key, &state.admin_api_key.read()) => {
            next.run(request).await
        }
        _ => {
            let error = AdminErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}
