//! Swagger UI for `/api/v1/openapi.json`, served from the binary itself so
//! the docs work offline. The UI is `swagger-ui-dist`, vendored under
//! `assets/swagger-ui/` (Apache-2.0; its license files are served next to
//! it). It's embedded and served by hand rather than through
//! `utoipa-swagger-ui`, whose axum integration pulls in a second,
//! incompatible axum version (0.8) next to the workspace's 0.7.

use axum::http::header;
use axum::response::{Html, IntoResponse};

/// Bump together with the files in `assets/swagger-ui/`. Part of the asset
/// URLs, so a browser never reuses a cached copy of an older version.
pub const SWAGGER_UI_VERSION: &str = "5.33.0";

const BUNDLE_JS: &[u8] = include_bytes!("../assets/swagger-ui/swagger-ui-bundle.js");
const CSS: &[u8] = include_bytes!("../assets/swagger-ui/swagger-ui.css");
const LICENSE: &str = include_str!("../assets/swagger-ui/LICENSE");
const NOTICE: &str = include_str!("../assets/swagger-ui/NOTICE");
const THIRD_PARTY: &str = include_str!("../assets/swagger-ui/swagger-ui-bundle.js.LICENSE.txt");

/// Versioned asset URLs never change content, so browsers may cache them for good.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

pub async fn swagger_ui() -> Html<String> {
    let base = format!("/api/v1/docs/{SWAGGER_UI_VERSION}");
    Html(format!(
        r##"<!DOCTYPE html>
<html>
<head>
  <title>fluxolap API</title>
  <meta charset="utf-8"/>
  <link rel="stylesheet" href="{base}/swagger-ui.css">
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="{base}/swagger-ui-bundle.js"></script>
  <script>
    window.onload = () => {{
      window.ui = SwaggerUIBundle({{
        url: "/api/v1/openapi.json",
        dom_id: "#swagger-ui",
        presets: [SwaggerUIBundle.presets.apis],
      }});
    }};
  </script>
</body>
</html>"##
    ))
}

/// A vendored Swagger UI file, by name. Unknown names are the router's 404.
pub async fn asset(axum::extract::Path((version, file)): axum::extract::Path<(String, String)>) -> impl IntoResponse {
    if version != SWAGGER_UI_VERSION {
        return Err(crate::error::ApiError::route_not_found());
    }
    let (content_type, body): (&str, &[u8]) = match file.as_str() {
        "swagger-ui-bundle.js" => ("text/javascript; charset=utf-8", BUNDLE_JS),
        "swagger-ui.css" => ("text/css; charset=utf-8", CSS),
        "LICENSE" => ("text/plain; charset=utf-8", LICENSE.as_bytes()),
        "NOTICE" => ("text/plain; charset=utf-8", NOTICE.as_bytes()),
        "THIRD-PARTY-LICENSES" => ("text/plain; charset=utf-8", THIRD_PARTY.as_bytes()),
        _ => return Err(crate::error::ApiError::route_not_found()),
    };
    Ok(([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, IMMUTABLE)], body))
}
