use dotenvy::dotenv;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub public_url: String,
    pub jackett_url: String,
    pub jackett_api_key: String,
    pub jackett_search_type: String, // "imdb" or "text"
    pub qbittorrent_url: String,
    pub qbittorrent_username: String,
    pub qbittorrent_password: String,
    pub download_path: String,
    pub retention_days: u64,
    pub remux_cache_dir: String,
}

impl Config {
    pub fn load() -> Self {
        // Load .env if it exists, silently ignore if it doesn't
        let _ = dotenv();

        Self {
            port: env::var("PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .expect("PORT must be a number"),
            public_url: get_req_env("PUBLIC_URL"),
            jackett_url: get_req_env("JACKETT_URL"),
            jackett_api_key: get_req_env("JACKETT_API_KEY"),
            jackett_search_type: get_req_env("JACKETT_SEARCH_TYPE"),
            qbittorrent_url: get_req_env("QBITTORRENT_URL"),
            qbittorrent_username: get_req_env("QBITTORRENT_USERNAME"),
            qbittorrent_password: get_req_env("QBITTORRENT_PASSWORD"),
            download_path: get_req_env("DOWNLOAD_PATH"),
            retention_days: env::var("RETENTION_DAYS")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .unwrap_or(0),
            // Where audio-incompatible files get remuxed to (video copy, audio ->
            // AAC) so they're playable in browsers. Must be a writable path,
            // separate from DOWNLOAD_PATH which is mounted read-only.
            remux_cache_dir: env::var("REMUX_CACHE_DIR")
                .unwrap_or_else(|_| "./remux-cache".to_string()),
        }
    }
}

fn get_req_env(name: &str) -> String {
    env::var(name)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| panic!("Environment variable {} is required", name))
}
