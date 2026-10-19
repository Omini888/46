mod config;
mod pdf_images;
mod processor;
mod vlm;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, put},
    Router,
};
use bytes::Bytes;
use moka::future::Cache;
use reqwest::Client;
use serde_json::json;
use tracing::{error, info, warn};

use crate::config::{AppConfig, AppState};
use crate::processor::{
    process_pdf_document, process_single_image_file, OpenWebUiDocument,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pdf_vlm_loader=info,tower_http=info".into()),
        )
        .init();

    let config = AppConfig::from_env();
    let listen_addr = config.listen_addr.clone();
    let vlm_url = config.vlm_api_url.clone();

    let http_client = Client::builder()
        .timeout(Duration::from_secs(240))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("Не удалось инициализировать HTTP-клиент")?;

    let vlm_cache = Cache::builder()
        .max_capacity(5_000)
        .time_to_live(Duration::from_secs(60 * 60 * 24 * 7))
        .build();

    let state = Arc::new(AppState::new(config, http_client, vlm_cache)?);

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/process", put(handle_process).post(handle_process))
        .layer(DefaultBodyLimit::max(150 * 1024 * 1024))
        .with_state(state);

    let addr: SocketAddr = listen_addr
        .parse()
        .context("Некорректный формат LISTEN_ADDR")?;

    info!("==========================================================");
    info!("🚀 Сервер распознавания PDF/Фото для OpenWebUI запущен!");
    info!("📡 Слушает на VM OpenWebUI: http://{}", addr);
    info!("🤖 Удалённый VLM сервер (GPU VM): {}", vlm_url);
    info!("==========================================================");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "pdf-vlm-loader",
        "vlm_api_url": state.config.vlm_api_url,
        "vlm_model": state.config.vlm_model
    }))
}

async fn handle_process(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. Проверка Bearer ключа от OpenWebUI
    if !state.config.loader_api_key.is_empty() {
        let authorized = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(|token| token.trim() == state.config.loader_api_key)
            .unwrap_or(false);

        if !authorized {
            warn!("Отклонён запрос без верного Bearer-токена");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Unauthorized: invalid Bearer token" })),
            )
                .into_response();
        }
    }

    if body.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Empty request body" })),
        )
            .into_response();
    }

    let raw_filename = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("document.pdf");
    let filename = url_decode(raw_filename);

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_lowercase();

    info!(
        "📥 Получен файл '{}' ({} байт, Content-Type: {})",
        filename,
        body.len(),
        content_type
    );

    let is_pdf = content_type.contains("pdf")
        || filename.to_lowercase().ends_with(".pdf")
        || body.starts_with(b"%PDF-");

    let is_image = content_type.starts_with("image/")
        || filename.to_lowercase().ends_with(".png")
        || filename.to_lowercase().ends_with(".jpg")
        || filename.to_lowercase().ends_with(".jpeg")
        || filename.to_lowercase().ends_with(".webp");

    let result = if is_pdf {
        process_pdf_document(state.clone(), &filename, &content_type, body).await
    } else if is_image {
        process_single_image_file(state.clone(), &filename, &content_type, body).await
    } else {
        let text = String::from_utf8_lossy(&body).to_string();
        Ok(vec![OpenWebUiDocument {
            page_content: format!("{}\n\n[Страница 1 из 1]", text.trim()),
            metadata: json!({
                "source": filename,
                "content_type": content_type,
                "page": 1,
                "total_pages": 1
            }),
        }])
    };

    match result {
        Ok(pages) => {
            if state.config.return_page_array {
                Json(json!(pages)).into_response()
            } else if pages.len() == 1 {
                Json(json!(pages.into_iter().next().unwrap())).into_response()
            } else {
                let total_pages = pages.len();
                let combined_text = pages
                    .into_iter()
                    .map(|p| p.page_content)
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");

                let doc = OpenWebUiDocument {
                    page_content: combined_text,
                    metadata: json!({
                        "source": filename,
                        "content_type": content_type,
                        "total_pages": total_pages
                    }),
                };
                Json(json!(doc)).into_response()
            }
        }
        Err(e) => {
            error!("❌ Ошибка обработки '{}': {:#}", filename, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{:#}", e) })),
            )
                .into_response()
        }
    }
}

fn url_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.as_bytes().iter().copied();

    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(c1), Some(c2)) = (h1, h2) {
                let hex = [c1, c2];
                if let Ok(s) = std::str::from_utf8(&hex) {
                    if let Ok(val) = u8::from_str_radix(s, 16) {
                        bytes.push(val);
                        continue;
                    }
                }
                bytes.push(b'%');
                bytes.push(c1);
                bytes.push(c2);
            } else {
                bytes.push(b'%');
            }
        } else if b == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(b);
        }
    }

    String::from_utf8_lossy(&bytes).to_string()
}
