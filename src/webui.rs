use axum::{
    body::Body,
    extract::Path,
    http::{header, Response, StatusCode},
    response::IntoResponse,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$AW_WEBUI_DIR"]
struct Assets;

pub async fn index() -> impl IntoResponse {
    serve_file("index.html")
}

pub async fn static_file(Path(path): Path<String>) -> impl IntoResponse {
    serve_file(&path)
}

pub async fn css_file(Path(path): Path<String>) -> impl IntoResponse {
    serve_file(&format!("css/{}", path))
}

pub async fn js_file(Path(path): Path<String>) -> impl IntoResponse {
    serve_file(&format!("js/{}", path))
}

pub async fn fonts_file(Path(path): Path<String>) -> impl IntoResponse {
    serve_file(&format!("fonts/{}", path))
}

pub async fn favicon() -> impl IntoResponse {
    serve_file("favicon.ico")
}

pub async fn logo() -> impl IntoResponse {
    serve_file("logo.png")
}

pub async fn manifest() -> impl IntoResponse {
    serve_file("manifest.json")
}

pub async fn dark_css() -> impl IntoResponse {
    serve_file("dark.css")
}

fn serve_file(path: &str) -> Response<Body> {
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data.to_vec()))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not found"))
            .unwrap(),
    }
}
