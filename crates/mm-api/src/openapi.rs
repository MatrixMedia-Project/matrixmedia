//! OpenAPI document assembly — spec generated from the code.
//!
//! Top-level document metadata (info, servers, tags, security schemes) is
//! ported from the hand-written `contracts/api/mm_api_v1.yaml` header; the
//! paths and component schemas are accumulated from the per-module
//! `OpenApiRouter` fragments, so a route or DTO change in the code is the
//! only way the generated spec changes.
//!
//! Conversion is incremental (see
//! `WorkingDirectory/docs/improvements/02-api-contract-typegen.md`): only the
//! client streams/recordings module (`crate::client`) is converted so far.
//! Until every module is converted, the generated document is emitted to
//! `contracts/api/generated/mm_api_gen.yaml` (via the `mm-openapi` bin) and
//! coexists with the hand-written `contracts/api/mm_api_v1.yaml`.

use utoipa::{Modify, OpenApi};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "MatrixMedia API",
        description = "MatrixMedia is an independent media streaming service for the Matrix \
ecosystem. It installs as a sidecar next to any Matrix homeserver and provides live \
audio/video streaming, VoD, and screen sharing.\n\n\
GENERATED SPEC — produced from the Axum handlers and serde DTOs in `crates/mm-api` by \
`cargo run -p mm-api --bin mm-openapi`. Do not edit by hand.\n\n\
Coverage: client streams/recordings module only so far; remaining modules are converted \
incrementally (the hand-written `mm_api_v1.yaml` still documents the rest).\n\n\
**Authentication:**\n\
- Client API endpoints require a Bearer MM JWT obtained via `/auth/token`.\n\
- Admin API endpoints require a Bearer shared secret configured at deployment.\n\
- The Appservice callback requires a Bearer `hs_token` issued by the homeserver.\n\n\
**Error envelope:**\n\
All non-2xx responses use a standard JSON envelope with `error` (machine-readable code), \
`message` (human-readable), and optional `retry_after_ms`.",
        license(name = "Apache-2.0", identifier = "Apache-2.0"),
    ),
    servers(
        (url = "http://localhost:6167", description = "Local development -- Client & Widget API"),
        (url = "http://localhost:6168", description = "Local development -- Admin API (localhost-only in production)"),
    ),
    tags(
        (name = "auth", description = "Authentication -- exchange Matrix OpenID tokens for MM JWTs"),
        (name = "streams", description = "Stream lifecycle -- create, join, leave, end, resume, query"),
        (name = "recordings", description = "Recording start/stop, listing, retrieval, and deletion"),
    ),
    modifiers(&SecuritySchemes),
)]
struct ApiDoc;

/// Registers the three bearer security schemes (ported from
/// `mm_api_v1.yaml` `components.securitySchemes`).
struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "mm_jwt",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "MM session JWT obtained via `POST /auth/token`. HS256-signed, \
                         15-minute TTL. Claims include `iss: \"matrixmedia\"`, \
                         `aud: \"mm-api\"`, `sub: \"<matrix_user_id>\"`.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "admin_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Shared secret configured via `MM_ADMIN_TOKEN` env var. Admin API \
                         listens on a separate port (default 6168) and is localhost-only \
                         by default.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "hs_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Homeserver token (`hs_token`) from the appservice registration \
                         YAML. The homeserver sends this when pushing transaction batches.",
                    ))
                    .build(),
            ),
        );
    }
}

/// Assemble the full generated OpenAPI document: top-level metadata plus the
/// per-module path/schema fragments nested under their mount prefixes.
pub fn api_doc() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
        // Client streams/recordings module — mounted at /_mm/client/v1 in
        // `crate::client_router`. Further modules merge here as they convert.
        .nest("/_mm/client/v1", crate::client::openapi_fragment())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_doc_contains_client_module_paths() {
        let doc = api_doc();
        let paths = &doc.paths.paths;
        assert!(paths.contains_key("/_mm/client/v1/streams"));
        assert!(paths.contains_key("/_mm/client/v1/streams/{id}/resume"));
        assert!(paths.contains_key("/_mm/client/v1/recordings/{recording_id}"));
        // Schemas accumulated from the module fragment.
        let components = doc.components.as_ref().expect("components");
        assert!(components.schemas.contains_key("StreamResponse"));
        assert!(components.schemas.contains_key("ErrorResponse"));
        // Security schemes ported from the hand-written header.
        assert!(components.security_schemes.contains_key("mm_jwt"));
    }

    #[test]
    fn test_api_doc_serializes_to_yaml() {
        let yaml = api_doc().to_yaml().expect("to_yaml");
        assert!(yaml.starts_with("openapi:"));
    }
}
