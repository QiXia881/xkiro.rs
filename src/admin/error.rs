//! Admin API 错误类型定义

use std::fmt;

use axum::http::StatusCode;

use super::types::AdminErrorResponse;

/// Admin 服务错误类型
#[derive(Debug)]
pub enum AdminServiceError {
    /// 凭据不存在
    NotFound { id: u64 },

    /// 资源不存在
    ResourceNotFound(String),

    /// 上游服务调用失败（网络、API 错误等）
    UpstreamError(String),

    /// 内部状态错误
    InternalError(String),

    /// 凭据无效（验证失败）
    InvalidCredential(String),

    /// 无效请求
    InvalidRequest(String),
}

impl fmt::Display for AdminServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminServiceError::NotFound { id } => {
                write!(f, "凭据不存在: {}", id)
            }
            AdminServiceError::ResourceNotFound(msg) => write!(f, "{}", msg),
            AdminServiceError::UpstreamError(msg) => write!(f, "上游服务错误: {}", msg),
            AdminServiceError::InternalError(msg) => write!(f, "内部错误: {}", msg),
            AdminServiceError::InvalidCredential(msg) => write!(f, "凭据无效: {}", msg),
            AdminServiceError::InvalidRequest(msg) => write!(f, "无效请求: {}", msg),
        }
    }
}

impl std::error::Error for AdminServiceError {}

impl AdminServiceError {
    /// 获取对应的 HTTP 状态码
    pub fn status_code(&self) -> StatusCode {
        match self {
            AdminServiceError::NotFound { .. } => StatusCode::NOT_FOUND,
            AdminServiceError::ResourceNotFound(_) => StatusCode::NOT_FOUND,
            AdminServiceError::UpstreamError(_) => StatusCode::BAD_GATEWAY,
            AdminServiceError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminServiceError::InvalidCredential(_) => StatusCode::BAD_REQUEST,
            AdminServiceError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        }
    }

    /// 转换为 API 错误响应体（不含 HTTP 状态码）
    ///
    /// 注意：命名刻意区别于 axum `IntoResponse::into_response`——后者返回完整
    /// `Response`（含状态码），此方法只产出 JSON body，由 `IntoResponse` impl 组合状态码。
    pub fn to_error_body(&self) -> AdminErrorResponse {
        match self {
            AdminServiceError::NotFound { .. } => AdminErrorResponse::not_found(self.to_string()),
            AdminServiceError::ResourceNotFound(_) => {
                AdminErrorResponse::not_found(self.to_string())
            }
            AdminServiceError::UpstreamError(_) => AdminErrorResponse::api_error(self.to_string()),
            AdminServiceError::InternalError(_) => {
                AdminErrorResponse::internal_error(self.to_string())
            }
            AdminServiceError::InvalidCredential(_) => {
                AdminErrorResponse::invalid_request(self.to_string())
            }
            AdminServiceError::InvalidRequest(_) => {
                AdminErrorResponse::invalid_request(self.to_string())
            }
        }
    }
}

impl axum::response::IntoResponse for AdminServiceError {
    /// 统一把 Admin 错误渲染为 `(status_code, Json(body))`。
    ///
    /// 消除 ~64 处 handler 里 `(e.status_code(), Json(e.to_error_body())).into_response()`
    /// 的重复错误臂，收敛到单一渲染点，避免状态码/body 组合方式漂移。
    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();
        (status, axum::Json(self.to_error_body())).into_response()
    }
}
