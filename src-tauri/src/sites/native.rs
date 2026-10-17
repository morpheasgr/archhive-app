//! Native stream resolvers for sites without their own adapter, tried before
//! plain yt-dlp:
//!
//! - KVS tubes (Hoes, Wankoz, FapCat, …): `get_file` links only work inside
//!   the page's session — opened from the player they 403 ("stream expired")
//!   or redirect to a placeholder (`video_error.mp4`). Resolved in-session
//!   to the CDN URL, which plays without cookies.
//! - XXXBP: the player asks `api.xxxbp.tv/hls` for its sources; yt-dlp has
//!   no extractor for it.
//! - HQPorner: the video sits in a `mydaddy.cc` player iframe listing MP4s
//!   per quality; yt-dlp has no extractor for it.
//! - XVIDEOS / XNXX: the page hands its player the sources in script calls
//!   (`html5player.setVideoHLS(…)`, `setVideoUrlHigh(…)`). yt-dlp on Android
//!   fails on these sites ("I/O operation on closed file").

use crate::error::{AppError, AppResult};
use crate::sites::SiteContext;

/// A stream URL for `page_url`, or `None` when no native resolver applies.
pub async fn resolve_stream(
    ctx: &SiteContext,
    page_url: &str,
    site_id: &str,
) -> Option<AppResult<String>> {
    let host = host_of(page_url);
    if host_is(&host, "xxxbp.tv") {
        return Some(xxxbp_best(page_url).await);
    }
    if host_is(&host, "hqporner.com") {
        return Some(hqporner_best(page_url).await);
    }
    if is_xvideos_family(page_url) {
        return Some(xvideos_best(Some(ctx), page_url).await);
    }
    match crate::sites::extractors::thothub::resolve_kvs_stream(ctx, page_url, site_id, &host).await
    {
        Ok(url) => Some(Ok(url)),
        // A KVS page whose video is private / a placeholder: say so rather
        // than let yt-dlp hand the player the same placeholder.
        Err(e) if is_kvs_verdict(&e.to_string()) => Some(Err(e)),
        Err(e) => {
            tracing::info!("[native] {page_url}: not resolved as KVS: {e}");
            None
        }
    }
}

/// A directly downloadable media URL for pages yt-dlp has no extractor for
/// (no session needed). `None` when no native resolver applies.
pub async fn resolve_download(page_url: &str) -> Option<AppResult<String>> {
    let host = host_of(page_url);
    if host_is(&host, "xxxbp.tv") {
        return Some(xxxbp_best(page_url).await);
    }
    if host_is(&host, "hqporner.com") {
        return Some(hqporner_best(page_url).await);
    }
    if is_xvideos_family(page_url) {
        return Some(xvideos_best(None, page_url).await);
    }
    None
}

/// XVIDEOS / XNXX video pages (their adapters also resolve natively).
pub(crate) fn is_xvideos_family(page_url: &str) -> bool {
    let host = host_of(page_url);
    host_is(&host, "xvideos.com") || host_is(&host, "xnxx.com")
}

/// The best source an XVIDEOS / XNXX page gives its player: the HLS
/// playlist (all qualities up to 1080p), else the MP4s. They play and
/// download without cookies or a Referer. With `ctx` the page is fetched
/// with the site's cookies (logged-in / age-verified sessions).
pub(crate) async fn xvideos_best(ctx: Option<&SiteContext>, page_url: &str) -> AppResult<String> {
    let site_id = if host_is(&host_of(page_url), "xnxx.com") {
        "xnxx"
    } else {
        "xvideos"
    };
    let html = match ctx {
        Some(ctx) => ctx.fetch_html(page_url, site_id).await?,
        None => {
            let client = reqwest::Client::builder()
                .user_agent(crate::sites::BROWSER_UA)
                .timeout(std::time::Duration::from_secs(30))
                .build()?;
            crate::sites::throttle::acquire(page_url).await?;
            client.get(page_url).send().await?.text().await?
        }
    };
    xvideos_sources(&html).ok_or_else(|| {
        AppError::Site(format!(
            "No video sources on this {} page — it may have been removed.",
            if site_id == "xnxx" { "XNXX" } else { "XVIDEOS" }
        ))
    })
}

/// `setVideoHLS`, else `setVideoUrlHigh`, else `setVideoUrlLow`.
fn xvideos_sources(html: &str) -> Option<String> {
    ["setVideoHLS", "setVideoUrlHigh", "setVideoUrlLow"]
        .iter()
        .find_map(|call| {
            let start = html.find(&format!("html5player.{call}('"))? + call.len() + 14;
            let len = html[start..].find('\'')?;
            let url = &html[start..start + len];
            url.starts_with("http").then(|| url.to_string())
        })
}

/// `resolve_kvs_stream` errors that are about the video itself, not about
/// the page being something other than a reachable KVS page.
pub(crate) fn is_kvs_verdict(err: &str) -> bool {
    // Exact phrases from `resolve_kvs_stream`; its "No player on this page —
    // … may be private" error means "not a KVS page" and must fall through.
    err.contains("This is a private") || err.contains("served its placeholder clip")
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
        })
        .unwrap_or_default()
}

fn host_is(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// `https://xxxbp.tv/video/<id>/<slug>` (also `/embed/<id>`).
fn xxxbp_video_id(page_url: &str) -> Option<String> {
    let path = url::Url::parse(page_url).ok()?.path().to_string();
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    match parts.next()? {
        "video" | "embed" => parts
            .next()
            .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_string),
        _ => None,
    }
}

/// Highest-quality progressive MP4 from `POST api.xxxbp.tv/hls`, else the
/// HLS master playlist.
async fn xxxbp_best(page_url: &str) -> AppResult<String> {
    let id = xxxbp_video_id(page_url)
        .ok_or_else(|| AppError::InvalidInput(format!("not an XXXBP video URL: {page_url}")))?;
    let client = reqwest::Client::builder()
        .user_agent(crate::sites::BROWSER_UA)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    crate::sites::throttle::acquire("https://api.xxxbp.tv/hls").await?;
    let resp = client
        .post("https://api.xxxbp.tv/hls")
        .header("Origin", "https://xxxbp.tv")
        .header("Referer", page_url)
        .header("Accept", "application/json")
        .form(&[("id", id.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AppError::Site(format!(
            "XXXBP returned HTTP {} for the video sources",
            resp.status()
        )));
    }
    let body: serde_json::Value = resp.json().await?;
    pick_xxxbp(&body).ok_or_else(|| {
        AppError::Site("XXXBP returned no playable sources — the video may be removed.".into())
    })
}

/// HQPorner page → `//mydaddy.cc/video/<id>/` iframe → best `<source>`.
async fn hqporner_best(page_url: &str) -> AppResult<String> {
    use std::sync::LazyLock;
    static IFRAME: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"src="((?:https?:)?//mydaddy\.cc/video/[^"]+)""#).expect("iframe")
    });
    let client = reqwest::Client::builder()
        .user_agent(crate::sites::BROWSER_UA)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    crate::sites::throttle::acquire(page_url).await?;
    let html = client.get(page_url).send().await?.text().await?;
    let player = IFRAME
        .captures(&html)
        .map(|c| absolutize_scheme(&c[1]))
        .ok_or_else(|| {
            AppError::Site("No player on this HQPorner page — the video may be removed.".into())
        })?;
    let body = client
        .get(&player)
        .header("Referer", page_url)
        .send()
        .await?
        .text()
        .await?;
    // The player lists every quality, but often only some files exist:
    // take the best one that answers.
    for candidate in mydaddy_sources(&body) {
        let ok = client
            .get(&candidate)
            .header("Range", "bytes=0-0")
            .header("Referer", &player)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if ok {
            return Ok(candidate);
        }
    }
    Err(AppError::Site(
        "HQPorner's player lists no playable video — it may be removed.".into(),
    ))
}

/// `<source src=\"//cdn/…/720.mp4\" title=\"720p HD\" …>` (inside a JS
/// string, so the quotes may be escaped) → highest quality.
#[cfg(test)]
fn pick_mydaddy(body: &str) -> Option<String> {
    mydaddy_sources(body).into_iter().next()
}

/// All `<source>` MP4s, best quality first.
fn mydaddy_sources(body: &str) -> Vec<String> {
    use std::sync::LazyLock;
    static SOURCE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"<source src=\\?"([^"\\]+\.mp4)\\?"\s+title=\\?"(\d+)p"#)
            .expect("source")
    });
    let mut found: Vec<(u32, String)> = SOURCE
        .captures_iter(body)
        .filter_map(|c| Some((c[2].parse::<u32>().ok()?, absolutize_scheme(&c[1]))))
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.dedup_by(|a, b| a.1 == b.1);
    found.into_iter().map(|(_, u)| u).collect()
}

fn absolutize_scheme(u: &str) -> String {
    if u.starts_with("//") {
        format!("https:{u}")
    } else {
        u.to_string()
    }
}

fn pick_xxxbp(body: &serde_json::Value) -> Option<String> {
    let height = |t: &serde_json::Value| {
        t.get("title")
            .and_then(|v| v.as_str())
            .and_then(|s| s.trim_end_matches('p').parse::<u32>().ok())
            .unwrap_or(0)
    };
    body.get("mp4")
        .and_then(|v| v.as_array())
        .and_then(|list| {
            list.iter()
                .filter(|e| e.get("src").and_then(|s| s.as_str()).is_some())
                .max_by_key(|e| height(e))
        })
        .and_then(|e| e.get("src")?.as_str().map(str::to_string))
        .or_else(|| {
            body.get("master")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mydaddy_sources() {
        let body = r#"document.write('<video><source src=\"//s60.bigcdn.cc/pubs/x/360.mp4\" title=\"360p\" type=\"video/mp4\" /><source src=\"//s60.bigcdn.cc/pubs/x/1080.mp4\" title=\"1080p Full HD\" type=\"video/mp4\" /><source src=\"//s60.bigcdn.cc/pubs/x/720.mp4\" title=\"720p HD\" /></video>');"#;
        assert_eq!(
            pick_mydaddy(body).as_deref(),
            Some("https://s60.bigcdn.cc/pubs/x/1080.mp4")
        );
        assert_eq!(pick_mydaddy("<p>none</p>"), None);
    }

    #[test]
    fn kvs_verdicts_vs_not_kvs() {
        assert!(is_kvs_verdict(
            "This is a private Hoes video — only logged-in members can watch it."
        ));
        assert!(is_kvs_verdict(
            "wankoz.com served its placeholder clip instead of the video"
        ));
        // Non-KVS pages must fall through to yt-dlp.
        assert!(!is_kvs_verdict(
            "No player on this redtube.com page — the video may be private or need you to be \
             logged in (Settings → Cookies → redtube.com)."
        ));
    }

    #[test]
    fn xxxbp_ids_and_source_pick() {
        assert_eq!(
            xxxbp_video_id("https://xxxbp.tv/video/488547/ikeepthemcuming-blowjob").as_deref(),
            Some("488547")
        );
        assert_eq!(
            xxxbp_video_id("https://xxxbp.tv/embed/12").as_deref(),
            Some("12")
        );
        assert_eq!(xxxbp_video_id("https://xxxbp.tv/model/x"), None);
        let body = serde_json::json!({
            "master": "https://cdn.test/multi/master.m3u8",
            "mp4": [
                {"src": "https://cdn.test/a_480p.mp4", "title": "480p"},
                {"src": "https://cdn.test/a_1080p.mp4", "title": "1080p"},
                {"src": "https://cdn.test/a_720p.mp4", "title": "720p"}
            ]
        });
        assert_eq!(
            pick_xxxbp(&body).as_deref(),
            Some("https://cdn.test/a_1080p.mp4")
        );
        let hls_only = serde_json::json!({"master": "https://cdn.test/m.m3u8", "mp4": []});
        assert_eq!(
            pick_xxxbp(&hls_only).as_deref(),
            Some("https://cdn.test/m.m3u8")
        );
    }

    #[test]
    fn xvideos_sources_prefer_hls_then_mp4() {
        let page = "html5player.setVideoTitle('t');\n\
            html5player.setVideoUrlLow('https://mp4.example/sd.mp4?s=1');\n\
            html5player.setVideoUrlHigh('https://mp4.example/hd.mp4?s=1');\n\
            html5player.setVideoHLS('https://hls.example/a==,1/x/hls.m3u8');";
        assert_eq!(
            xvideos_sources(page).as_deref(),
            Some("https://hls.example/a==,1/x/hls.m3u8")
        );
        let mp4_only = "html5player.setVideoUrlHigh('https://mp4.example/hd.mp4');";
        assert_eq!(
            xvideos_sources(mp4_only).as_deref(),
            Some("https://mp4.example/hd.mp4")
        );
        assert_eq!(xvideos_sources("<html>removed</html>"), None);
        assert!(is_xvideos_family("https://www.xvideos.com/video.abc/x"));
        assert!(is_xvideos_family("https://www.xnxx.com/video-abc/x"));
        assert!(!is_xvideos_family("https://www.xvideos.com.evil.example/x"));
    }
}
