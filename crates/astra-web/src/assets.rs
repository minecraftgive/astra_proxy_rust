use axum::{
    body::Body,
    http::{header, StatusCode, Uri},
    response::Response,
};
include!(concat!(env!("OUT_DIR"), "/assets.rs"));

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    }
    let found = ASSETS
        .iter()
        .find(|(name, _, _)| *name == path)
        .or_else(|| {
            if path.is_empty() || !path.rsplit('/').next().unwrap_or("").contains('.') {
                ASSETS.iter().find(|(name, _, _)| *name == "index.html")
            } else {
                None
            }
        });
    match found {
        Some((name,bytes,mime))=>Response::builder().header(header::CONTENT_TYPE,*mime)
            .header(header::CACHE_CONTROL,if *name=="index.html" {"no-cache"} else {"public, max-age=31536000, immutable"})
            .body(Body::from(*bytes)).unwrap(),
        None if ASSETS.is_empty()=>Response::builder().status(StatusCode::SERVICE_UNAVAILABLE).header(header::CONTENT_TYPE,"text/plain; charset=utf-8").body(Body::from("Web assets are not built. Run npm ci && npm run build in apps/web, then rebuild astra-web.")).unwrap(),
        None=>Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
    }
}
