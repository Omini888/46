use std::env;
use anyhow::Result;
use moka::future::Cache;
use regex::Regex;
use reqwest::Client;
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct AppConfig {
    /// Адрес и порт нашего Rust-сервера на VM с OpenWebUI
    pub listen_addr: String,
    /// Токен авторизации для OpenWebUI (EXTERNAL_DOCUMENT_LOADER_API_KEY)
    pub loader_api_key: String,
    /// Полный URL до VLM на другой VM по IP (например, http://192.168.1.50:8000/v1/chat/completions)
    pub vlm_api_url: String,
    /// API-ключ для VLM-сервера (если нужен)
    pub vlm_api_key: String,
    /// Название модели на VLM-сервере
    pub vlm_model: String,
    /// Минимальный размер встроенного фото в пикселях (отсекает мелкие иконки и линии)
    pub min_image_dim: u32,
    /// Минимальное число непробельных символов, чтобы считать страницу текстовой, а не фото-сканом
    pub min_text_chars_per_page: usize,
    /// Максимальное число одновременных запросов к VLM
    pub max_concurrent_vlm: usize,
    /// Возвращать ли JSON-массив страниц (true) или единый документ со всеми страницами (false)
    pub return_page_array: bool,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();
        Self {
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:5000".to_string()),
            loader_api_key: env::var("LOADER_API_KEY")
                .unwrap_or_else(|_| "my-secret-loader-key-2026".to_string()),
            vlm_api_url: env::var("VLM_API_URL")
                .unwrap_or_else(|_| "http://192.168.1.50:8000/v1/chat/completions".to_string()),
            vlm_api_key: env::var("VLM_API_KEY").unwrap_or_else(|_| "EMPTY".to_string()),
            vlm_model: env::var("VLM_MODEL").unwrap_or_else(|_| "qwen-3.8-27b".to_string()),
            min_image_dim: env::var("MIN_IMAGE_DIM")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(80),
            min_text_chars_per_page: env::var("MIN_TEXT_CHARS_PER_PAGE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(25),
            max_concurrent_vlm: env::var("MAX_CONCURRENT_VLM")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            return_page_array: env::var("RETURN_PAGE_ARRAY")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
        }
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub http_client: Client,
    pub vlm_cache: Cache<String, String>,
    pub vlm_semaphore: Semaphore,
    pub figure_regex: Regex,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        http_client: Client,
        vlm_cache: Cache<String, String>,
    ) -> Result<Self> {
        let vlm_semaphore = Semaphore::new(config.max_concurrent_vlm);
        // Ищем слово "рисунок" (в любом падеже/регистре) или сокращение "рис."
        let figure_regex = Regex::new(r"(?i)\b(рисунок|рисунк[аеуом]|рисунки|рисунков|рис\.)")?;

        Ok(Self {
            config,
            http_client,
            vlm_cache,
            vlm_semaphore,
            figure_regex,
        })
    }
}
