//! Uniform extractor wrappers (SPEC-API-005 F-33, REQ-API-409/410).
//!
//! `ApiJson` / `ApiQuery` / `ApiPath` wrap the built-in axum `Json` / `Query` / `Path`
//! extractors but convert their rejections into [`ApiError`], so a malformed request body,
//! query string, or path parameter produces the documented `{code, message}` JSON error body
//! (REQ-API-074) instead of Axum's default `text/plain` rejection.
//!
//! Implemented via `#[derive(FromRequest)]` (bodies) / `#[derive(FromRequestParts)]`
//! (query/path) with `rejection = ApiError` — NO new dependency (D2/D10): `axum-extra`'s
//! `WithRejection` is deliberately NOT used.

use axum::extract::{FromRequest, FromRequestParts};

use super::ApiError;

// @MX:ANCHOR: [AUTO] ApiJson/ApiQuery/ApiPath — uniform extractor rejection funnel (high fan_in)
// @MX:REASON: fan_in >= 3: every /v1 handler routes its body/query/path rejections through these
//             wrappers so ALL malformed-input responses carry the uniform {code, message} JSON body
//             (REQ-API-074/409/410). The `rejection(ApiError)` attribute requires
//             ApiError: From<{Json,Query,Path}Rejection> (src/api/mod.rs).
// @MX:SPEC: SPEC-API-005 REQ-API-409 REQ-API-410

/// JSON body extractor whose rejection is [`ApiError`] (uniform `{code, message}` body).
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

/// Query-string extractor whose rejection is [`ApiError`].
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct ApiQuery<T>(pub T);

/// Path-parameter extractor whose rejection is [`ApiError`].
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);
