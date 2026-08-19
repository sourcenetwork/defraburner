//! burner-dashboard: the embedded operational dashboard's static shell and
//! assets (Phase 5, rebuilt in the console round, D23/D24/D25 to the
//! imported DNA design). No build step: every CSS/JS/font file under
//! `assets/` is embedded verbatim via `include_str!`/`include_bytes!` and
//! served as-is -- zero runtime file reads, zero external network
//! requests, zero external product names outside `design/dna/` (the
//! reference material, never shipped).
//!
//! The shell needs no auth (it carries no data, only chrome). The data
//! APIs (`/admin/api/overview`, `/admin/api/stream`, and the whole
//! `/admin/*` control surface) live in `burner-gateway` itself, which
//! already owns the cluster/policy state this crate deliberately does not
//! depend on; see `gateway::build`'s doc comment in that crate for the
//! mount site.
//!
//! D17b/D24: no remote font import. Newsreader, IBM Plex Sans, and
//! JetBrains Mono are self-hosted latin-subset variable woff2 files under
//! `assets/fonts/`, declared by `app.css`'s own `@font-face` rules
//! pointing at `/dashboard/assets/fonts/*` -- never Google Fonts or any
//! other remote origin, so the dashboard renders identically offline.
//!
//! One wildcard route (`/dashboard/assets/{*path}`) claims the entire
//! asset prefix, matched against a fixed table of known files inside
//! `asset` (private: an internal handler, not part of this crate's
//! public API). This is deliberate, not merely convenient: `gateway::build`
//! `.merge()`s this crate's router into the same flat router as the
//! tenant-routing fallback (`route_to_tenant`), so any path this crate
//! does *not* claim falls through to that fallback, which checks for a
//! bearer token before it ever gets to say "not found" -- a confusing 401
//! instead of a clean 404. Claiming the whole prefix means an unknown or
//! removed asset (D24 deleted `sora-var.woff2`/`inter-var.woff2`) 404s
//! from *this* router's own table lookup, honestly, rather than being
//! swallowed by tenant auth.

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");

const TOKENS_CSS: &str = include_str!("../assets/tokens.css");
const APP_CSS: &str = include_str!("../assets/app.css");

const CORE_JS: &str = include_str!("../assets/core.js");
const CHARTS_JS: &str = include_str!("../assets/charts.js");
const MAIN_JS: &str = include_str!("../assets/main.js");
const MESH_PANEL_JS: &str = include_str!("../assets/mesh-panel.js");
const VIEW_OVERVIEW_JS: &str = include_str!("../assets/view-overview.js");
const VIEW_CELLS_JS: &str = include_str!("../assets/view-cells.js");
const VIEW_TENANTS_JS: &str = include_str!("../assets/view-tenants.js");
const VIEW_AUTOSCALER_JS: &str = include_str!("../assets/view-autoscaler.js");
const VIEW_MESH_JS: &str = include_str!("../assets/view-mesh.js");
const VIEW_CONSOLE_JS: &str = include_str!("../assets/view-console.js");
const VIEW_CONSOLE_SEED_JS: &str = include_str!("../assets/view-console-seed.js");
const VIEW_TRAFFIC_GEN_JS: &str = include_str!("../assets/view-traffic-gen.js");

const FONT_NEWSREADER: &[u8] = include_bytes!("../assets/fonts/newsreader-var.woff2");
const FONT_IBM_PLEX_SANS: &[u8] = include_bytes!("../assets/fonts/ibm-plex-sans-var.woff2");
const FONT_JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/jetbrains-mono-var.woff2");

const CSS_TYPE: &str = "text/css; charset=utf-8";
const JS_TYPE: &str = "text/javascript; charset=utf-8";
const FONT_TYPE: &str = "font/woff2";

/// Every asset is embedded at compile time and can only change by
/// rebuilding the binary, so a stale cache is never a correctness risk --
/// but with no content hash in the URL to bust it, `no-cache` (revalidate
/// every time, not "never cache") is the safe default for the shell,
/// CSS, and JS. Fonts alone get a long-lived immutable cache: their
/// filenames already carry an implicit version (a font swap is a rename,
/// per `FONTS-LICENSE.md`), so there is nothing to go stale.
const NO_CACHE: &str = "no-cache";
const FONT_CACHE: &str = "public, max-age=31536000, immutable";

/// Builds the dashboard's static routes: `GET /dashboard` (the shell) and
/// every asset under `/dashboard/assets/*`. Every handler here is a plain
/// `async fn` with no `State` extractor, so this is generic over the
/// caller's own state type `S` and merges directly into any stateful
/// `Router<S>` (see `burner_gateway::gateway::build`, the mount site).
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/dashboard", get(shell))
        .route("/dashboard/assets/{*path}", get(asset))
}

async fn shell() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE)),
        ],
        DASHBOARD_HTML,
    )
}

/// Looks `path` (everything after `/dashboard/assets/`) up in the fixed
/// table of embedded files, or 404s. See the module doc comment for why
/// this is one wildcard route over a table, not one `.route()` per file.
async fn asset(Path(path): Path<String>) -> Response {
    let (content_type, cache_control, body): (&'static str, &'static str, &'static [u8]) =
        match path.as_str() {
            "tokens.css" => (CSS_TYPE, NO_CACHE, TOKENS_CSS.as_bytes()),
            "app.css" => (CSS_TYPE, NO_CACHE, APP_CSS.as_bytes()),
            "core.js" => (JS_TYPE, NO_CACHE, CORE_JS.as_bytes()),
            "charts.js" => (JS_TYPE, NO_CACHE, CHARTS_JS.as_bytes()),
            "main.js" => (JS_TYPE, NO_CACHE, MAIN_JS.as_bytes()),
            "mesh-panel.js" => (JS_TYPE, NO_CACHE, MESH_PANEL_JS.as_bytes()),
            "view-overview.js" => (JS_TYPE, NO_CACHE, VIEW_OVERVIEW_JS.as_bytes()),
            "view-cells.js" => (JS_TYPE, NO_CACHE, VIEW_CELLS_JS.as_bytes()),
            "view-tenants.js" => (JS_TYPE, NO_CACHE, VIEW_TENANTS_JS.as_bytes()),
            "view-autoscaler.js" => (JS_TYPE, NO_CACHE, VIEW_AUTOSCALER_JS.as_bytes()),
            "view-mesh.js" => (JS_TYPE, NO_CACHE, VIEW_MESH_JS.as_bytes()),
            "view-console.js" => (JS_TYPE, NO_CACHE, VIEW_CONSOLE_JS.as_bytes()),
            "view-console-seed.js" => (JS_TYPE, NO_CACHE, VIEW_CONSOLE_SEED_JS.as_bytes()),
            "view-traffic-gen.js" => (JS_TYPE, NO_CACHE, VIEW_TRAFFIC_GEN_JS.as_bytes()),
            "fonts/newsreader-var.woff2" => (FONT_TYPE, FONT_CACHE, FONT_NEWSREADER),
            "fonts/ibm-plex-sans-var.woff2" => (FONT_TYPE, FONT_CACHE, FONT_IBM_PLEX_SANS),
            "fonts/jetbrains-mono-var.woff2" => (FONT_TYPE, FONT_CACHE, FONT_JETBRAINS_MONO),
            _ => return (StatusCode::NOT_FOUND, "dashboard asset not found").into_response(),
        };
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(cache_control),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_string(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn shell_serves_html_naming_defraburner_and_the_honest_no_data_marker() {
        let response = shell().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let html = body_string(response).await;
        assert!(
            html.contains("DEFRABURNER"),
            "shell should carry the wordmark"
        );
        assert!(
            html.to_lowercase().contains("no data yet"),
            "shell should render honest empty-state placeholders, not a fabricated number"
        );
        assert!(
            html.contains(r#"data-theme="dark""#),
            "dark must be the default theme (D24)"
        );
        assert!(
            !html.contains("@font-face"),
            "the @font-face rules belong in app.css, not the shell"
        );
    }

    #[tokio::test]
    async fn every_known_asset_serves_with_its_content_type_and_is_non_empty() {
        let cases: &[(&str, &str)] = &[
            ("tokens.css", CSS_TYPE),
            ("app.css", CSS_TYPE),
            ("core.js", JS_TYPE),
            ("charts.js", JS_TYPE),
            ("main.js", JS_TYPE),
            ("mesh-panel.js", JS_TYPE),
            ("view-overview.js", JS_TYPE),
            ("view-cells.js", JS_TYPE),
            ("view-tenants.js", JS_TYPE),
            ("view-autoscaler.js", JS_TYPE),
            ("view-mesh.js", JS_TYPE),
            ("view-console.js", JS_TYPE),
            ("view-console-seed.js", JS_TYPE),
            ("view-traffic-gen.js", JS_TYPE),
            ("fonts/newsreader-var.woff2", FONT_TYPE),
            ("fonts/ibm-plex-sans-var.woff2", FONT_TYPE),
            ("fonts/jetbrains-mono-var.woff2", FONT_TYPE),
        ];
        for (path, expected_type) in cases {
            let response = asset(Path((*path).to_string())).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "asset '{path}' should serve"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                *expected_type,
                "asset '{path}' content-type"
            );
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(!bytes.is_empty(), "asset '{path}' should be non-empty");
        }
    }

    #[tokio::test]
    async fn fonts_carry_an_immutable_long_lived_cache_header() {
        let response = asset(Path("fonts/newsreader-var.woff2".to_string())).await;
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            FONT_CACHE
        );
    }

    #[tokio::test]
    async fn css_and_js_are_not_cached_across_a_binary_upgrade() {
        let response = asset(Path("app.css".to_string())).await;
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            NO_CACHE
        );
    }

    /// D24: deleted fonts must 404 from this router's own table, not fall
    /// through to the gateway's tenant-auth fallback (which would 401 on
    /// a missing bearer token -- see the module doc comment).
    #[tokio::test]
    async fn a_removed_or_unknown_asset_404s_cleanly() {
        for path in [
            "sora-var.woff2",
            "fonts/sora-var.woff2",
            "fonts/inter-var.woff2",
            "does-not-exist.js",
        ] {
            let response = asset(Path(path.to_string())).await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "path '{path}' should 404"
            );
        }
    }

    #[test]
    fn no_embedded_css_declares_a_remote_font_import() {
        // D17b/D24: an embedded operational dashboard must render offline.
        for (name, css) in [("tokens.css", TOKENS_CSS), ("app.css", APP_CSS)] {
            assert!(
                !css.contains("fonts.googleapis.com") && !css.contains("@import"),
                "{name} must not import a remote font"
            );
        }
    }

    #[test]
    fn app_css_declares_font_face_for_all_three_self_hosted_fonts() {
        for family in ["Newsreader", "IBM Plex Sans", "JetBrains Mono"] {
            assert!(
                APP_CSS.contains(family),
                "app.css should declare @font-face for {family}"
            );
        }
        assert!(APP_CSS.contains("newsreader-var.woff2"));
    }

    #[test]
    fn dashboard_html_makes_no_external_requests() {
        // CSP-clean offline: no http(s) references to anything other than
        // this same origin's own /dashboard/assets/* paths.
        for line in DASHBOARD_HTML.lines() {
            if line.contains("http://") || line.contains("https://") {
                panic!("dashboard.html references an external URL: {line}");
            }
        }
    }

    #[test]
    fn dashboard_html_references_every_script_it_ships() {
        for path in [
            "core.js",
            "charts.js",
            "main.js",
            "mesh-panel.js",
            "view-overview.js",
            "view-cells.js",
            "view-tenants.js",
            "view-autoscaler.js",
            "view-mesh.js",
            "view-console.js",
            "view-console-seed.js",
            "view-traffic-gen.js",
        ] {
            assert!(
                DASHBOARD_HTML.contains(path),
                "dashboard.html should load {path}"
            );
        }
    }

    /// Visual pass, TESTS requirement: the mesh panel ships (its
    /// container id in the shell, the marker helper and the panel's own
    /// script in the embedded JS) so the panel can never silently go
    /// missing from a build.
    #[test]
    fn the_mesh_panel_and_marker_helper_ship() {
        assert!(
            DASHBOARD_HTML.contains(r#"id="overview-mesh-panel""#),
            "the shell should carry the mesh panel's container"
        );
        assert!(
            CORE_JS.contains("function markerFor"),
            "core.js should define the shared entity-marker helper"
        );
        assert!(
            MESH_PANEL_JS.contains("classifyPair") && MESH_PANEL_JS.contains("mesh-edge-missing"),
            "mesh-panel.js should classify real edges (live/missing/unknown), not draw an idealized mesh"
        );
    }

    /// Visual pass, TESTS requirement (source-level half; the live half
    /// -- does `/admin/api/overview` actually carry these fields against
    /// a real running gateway -- is
    /// `console_coverage::overview_payload_carries_every_field_the_mesh_panel_needs`,
    /// since this crate has no dependency on burner-gateway to call it
    /// directly). This much is still worth asserting here: it catches
    /// the panel silently regressing to read a field it no longer
    /// requests, which the live test alone would not distinguish from
    /// "never needed it".
    #[test]
    fn mesh_panel_javascript_reads_the_fields_its_live_counterpart_test_verifies_the_backend_sends()
    {
        for field in [
            "cell_details",
            "connected_peers",
            "static_peer_outcomes",
            "peer_id",
        ] {
            assert!(
                MESH_PANEL_JS.contains(field),
                "mesh-panel.js should read overview.{field}"
            );
        }
    }
}
