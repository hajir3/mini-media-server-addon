use crate::routes::AppState;
use std::time::Duration;

// Cached remuxes (src/routes/stream.rs) are named "{hash}_<original filename>",
// so they'd otherwise sit on disk forever once their source torrent is gone.
async fn remove_cached_remuxes(remux_cache_dir: &str, hash: &str) {
    let prefix = format!("{}_", hash.to_lowercase());
    let mut entries = match tokio::fs::read_dir(remux_cache_dir).await {
        Ok(entries) => entries,
        Err(_) => return, // cache dir may not exist yet if nothing's ever been remuxed
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            if let Err(e) = tokio::fs::remove_file(entry.path()).await {
                log::warn!("Failed to remove stale remux cache file {:?}: {}", entry.path(), e);
            } else {
                log::debug!("Removed stale remux cache file {:?}", entry.path());
            }
        }
    }
}

pub fn start_retention_worker(state: AppState) {
    if state.config.retention_days > 0 {
        log::info!("Starting background retention worker (Policy: {} days inactivity before deletion)", state.config.retention_days);
        let retention_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600)); // check every hour
            loop {
                interval.tick().await;
                log::debug!("Running retention policy check...");
                
                let torrents = retention_state.qbit.get_all_torrents().await;
                let watch_times = match retention_state.db.get_torrent_watch_times() {
                    Ok(times) => times,
                    Err(e) => {
                        log::error!("Failed to get watch times for retention check: {}", e);
                        continue;
                    }
                };

                let mut stale_count = 0;

                for torrent in &torrents {
                    let hash = torrent.hash.to_lowercase();
                    let mut delete = false;
                    
                    if let Some(last_watched) = watch_times.get(&hash) {
                        if let Ok(parsed_time) = chrono::NaiveDateTime::parse_from_str(last_watched, "%Y-%m-%d %H:%M:%S") {
                            let now = chrono::Utc::now().naive_utc();
                            let duration = now.signed_duration_since(parsed_time);
                            if duration.num_days() >= retention_state.config.retention_days as i64 {
                                delete = true;
                                log::debug!("Torrent {} is stale (last watched {} days ago). Scheduling deletion.", hash, duration.num_days());
                            }
                        }
                    } else if torrent.added_on > 0 {
                        let now = chrono::Utc::now().timestamp();
                        let duration_seconds = now - torrent.added_on as i64;
                        if duration_seconds >= (retention_state.config.retention_days as i64 * 86400) {
                            delete = true;
                            log::debug!("Torrent {} has no watch history and was added {} days ago. Scheduling deletion.", hash, duration_seconds / 86400);
                        }
                    }

                    if delete {
                        stale_count += 1;
                        if retention_state.qbit.delete_torrent(&hash, true).await {
                            let _ = retention_state.db.delete_watch_history(&hash);
                            remove_cached_remuxes(&retention_state.config.remux_cache_dir, &hash).await;
                        }
                    }
                }
                
                if stale_count > 0 {
                    log::info!("Retention policy check finished: found and deleted {} stale torrent(s).", stale_count);
                } else {
                    log::debug!("Retention policy check finished: no stale torrents found.");
                }
            }
        });
    } else {
        log::info!("Running without retention policy, no auto-deletion of torrents is configured (RETENTION_DAYS)");
    }
}
