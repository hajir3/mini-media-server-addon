use axum::{extract::{Path, Query, State, Request}, response::{IntoResponse, Response}};
use axum::http::header::RANGE;
use std::collections::HashMap;
use tower_http::services::ServeFile;
use tower::ServiceExt;
use tokio::process::Command;
use axum::body::Body;
use serde_json::Value;
use super::AppState;

#[derive(Default)]
struct MediaCompat {
    has_video: bool,
    video_ok: bool,
    has_audio: bool,
    audio_ok: bool,
}

fn is_compatible_video_codec(codec: &str) -> bool {
    matches!(codec, "h264" | "hevc")
}

// Browsers' native <video> element only decodes a handful of audio codecs.
// Most torrent releases (WEB-DL/BluRay especially) carry AC3/E-AC3/DTS/TrueHD,
// which play video with silently-dropped audio -- no error, just no sound.
fn is_compatible_audio_codec(codec: &str) -> bool {
    matches!(codec, "aac" | "mp3" | "opus" | "vorbis" | "flac")
}

async fn probe_compatibility(path: &std::path::Path) -> MediaCompat {
    log::info!("Probing media file: {:?}", path);
    let mut cmd = Command::new("ffprobe");
    cmd.arg("-v")
       .arg("error")
       .arg("-show_entries")
       .arg("stream=codec_name,codec_type")
       .arg("-of")
       .arg("json")
       .arg(path);

    let mut compat = MediaCompat::default();

    match cmd.output().await {
        Ok(output) => {
            if !output.status.success() {
                log::warn!("ffprobe returned error status: {}", output.status);
                let err_str = String::from_utf8_lossy(&output.stderr);
                log::warn!("ffprobe stderr: {}", err_str);
                return compat;
            }
            if let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) {
                if let Some(streams) = json.get("streams").and_then(|s| s.as_array()) {
                    for stream in streams {
                        let codec_type = stream.get("codec_type").and_then(|s| s.as_str());
                        let codec_name = stream.get("codec_name").and_then(|s| s.as_str());

                        match (codec_type, codec_name) {
                            (Some("video"), Some(name)) => {
                                compat.has_video = true;
                                log::info!("Found video codec: {}", name);
                                if is_compatible_video_codec(name) {
                                    compat.video_ok = true;
                                }
                            }
                            (Some("audio"), Some(name)) => {
                                compat.has_audio = true;
                                log::info!("Found audio codec: {}", name);
                                if is_compatible_audio_codec(name) {
                                    compat.audio_ok = true;
                                }
                            }
                            _ => {}
                        }
                    }
                } else {
                    log::warn!("ffprobe JSON output missing 'streams' array");
                }
            } else {
                log::warn!("Failed to parse ffprobe JSON output");
            }
        }
        Err(e) => {
            log::error!("Failed to execute ffprobe: {}", e);
        }
    }

    log::info!(
        "Probe result: has_video={}, video_ok={}, has_audio={}, audio_ok={}",
        compat.has_video, compat.video_ok, compat.has_audio, compat.audio_ok
    );
    compat
}

async fn is_file_fully_downloaded(state: &AppState, info_hash: &str, file_path: &str) -> bool {
    let files = state.qbit.get_torrent_files(info_hash).await;
    files.iter().any(|f| f.name == file_path && f.progress >= 1.0)
}

fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect()
}

fn remux_cache_path(state: &AppState, info_hash: &str, file_path: &str, ext: &str) -> std::path::PathBuf {
    let filename = format!("{}_{}.{}", info_hash.to_lowercase(), sanitize_for_filename(file_path), ext);
    std::path::Path::new(&state.config.remux_cache_dir).join(filename)
}

// Remuxes in the background: video is stream-copied (no re-encode, fast) and
// audio is transcoded to AAC. Written to a .tmp path and renamed into place
// atomically on success, so a concurrent request can never see a
// half-written file. Only called once per (info_hash, file_path) at a time --
// callers check/set `active_remuxes` first.
async fn spawn_background_remux(state: AppState, source: std::path::PathBuf, dest: std::path::PathBuf, guard_key: String) {
    let tmp_dest = dest.with_extension(format!(
        "{}.tmp",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("mp4")
    ));

    log::info!("Starting background audio remux: {:?} -> {:?}", source, dest);

    let mkdir_result = match dest.parent() {
        Some(parent) => tokio::fs::create_dir_all(parent).await,
        None => Ok(()),
    };
    if let Err(e) = mkdir_result {
        log::error!("Failed to create remux cache dir for {:?}: {}", dest, e);
        state.active_remuxes.lock().unwrap().remove(&guard_key);
        return;
    }

    let result = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i").arg(&source)
        .arg("-map").arg("0:v:0")
        .arg("-map").arg("0:a:0")
        .arg("-c:v").arg("copy")
        .arg("-c:a").arg("aac")
        .arg(&tmp_dest)
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            if let Err(e) = tokio::fs::rename(&tmp_dest, &dest).await {
                log::error!("Remux succeeded but rename failed for {:?}: {}", dest, e);
                let _ = tokio::fs::remove_file(&tmp_dest).await;
            } else {
                log::info!("Audio remux finished: {:?}", dest);
            }
        }
        Ok(output) => {
            log::warn!(
                "ffmpeg remux failed for {:?}: {}",
                source,
                String::from_utf8_lossy(&output.stderr)
            );
            let _ = tokio::fs::remove_file(&tmp_dest).await;
        }
        Err(e) => {
            log::error!("Failed to execute ffmpeg for remux of {:?}: {}", source, e);
        }
    }

    state.active_remuxes.lock().unwrap().remove(&guard_key);
}

pub async fn stream_file(
    State(state): State<AppState>,
    Path((api_key, info_hash)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    if !state.db.validate_api_key(&api_key) {
        return axum::response::Response::builder()
            .status(401)
            .body(Body::from("Unauthorized"))
            .unwrap();
    }

    let file_path = params.get("filePath").unwrap_or(&String::new()).clone();
    let full_path = std::path::Path::new(&state.config.download_path).join(&file_path);

    log::info!("Stream request for info_hash: {}, filePath: {}", info_hash, file_path);

    if !full_path.starts_with(&state.config.download_path) {
        log::warn!("Security violation attempt or invalid path: {:?}", full_path);
        return axum::response::Response::builder()
            .status(404)
            .body(Body::from("Not Found"))
            .unwrap();
    }

    if !full_path.exists() {
        log::warn!("File does not exist (yet): {:?}", full_path);
        // We should still proceed, ServeFile might handle it or we wait. But ffprobe will definitely fail.
    }

    let ext = full_path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    let range_header = req.headers().get(RANGE).and_then(|h| h.to_str().ok()).unwrap_or("none").to_string();
    log::info!("File extension: '{}', Range header: {}", ext, range_header);

    let mut serve_path = full_path.clone();

    if ext != "mp4" && ext != "webm" && full_path.exists() {
        let compat = probe_compatibility(&full_path).await;

        if compat.has_video && compat.video_ok && compat.has_audio && !compat.audio_ok {
            let cache_ext = if ext.is_empty() { "mp4".to_string() } else { ext.clone() };
            let cache_path = remux_cache_path(&state, &info_hash, &file_path, &cache_ext);

            if cache_path.exists() {
                log::info!("Serving previously-remuxed audio-fixed file: {:?}", cache_path);
                serve_path = cache_path;
            } else {
                let guard_key = cache_path.to_string_lossy().to_string();
                let already_running = {
                    let mut active = state.active_remuxes.lock().unwrap();
                    !active.insert(guard_key.clone())
                };

                if already_running {
                    log::info!("Audio remux already in progress for {:?}; serving without audio for now.", full_path);
                } else if is_file_fully_downloaded(&state, &info_hash, &file_path).await {
                    log::info!("Incompatible audio codec detected on a fully-downloaded file; starting background remux.");
                    tokio::spawn(spawn_background_remux(state.clone(), full_path.clone(), cache_path, guard_key));
                } else {
                    log::info!("Incompatible audio codec detected but file is still downloading; will remux once complete. Serving without audio for now.");
                    state.active_remuxes.lock().unwrap().remove(&guard_key);
                }
            }
        } else if compat.has_video && compat.video_ok {
            log::info!("File contains a compatible video format (H.264/HEVC) and compatible (or no) audio.");
        } else {
            log::info!("File does not contain a compatible video format.");
        }
    }

    log::info!("Serving file directly via ServeFile: {:?}", serve_path);
    ServeFile::new(serve_path).oneshot(req).await.unwrap().into_response()
}
