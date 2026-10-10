//! Opt-in update check. The app never phones home by default: it only checks when the
//! SMF_UPDATE_URL environment variable names a JSON manifest, and only when you ask.
//!
//! Manifest: {"version": "0.2.0", "url": "https://.../download", "notes": "optional text"}

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Release {
    pub version: String,
    pub url: String,
    #[serde(default)]
    pub notes: String,
}

/// Parse "1.2.3" (optionally "v1.2.3", with a "-suffix") into numbers; None for anything else.
fn parse_version(v: &str) -> Option<Vec<u64>> {
    let core = v
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()?;
    let parts: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty() && p.len() <= 4)
}

/// True when `candidate` is strictly newer than `current`. Unparseable versions never count as newer.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    let (Some(mut a), Some(mut b)) = (parse_version(current), parse_version(candidate)) else {
        return false;
    };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    b > a
}

pub fn parse_release(text: &str) -> Result<Release, String> {
    let r: Release =
        serde_json::from_str(text).map_err(|e| format!("update manifest is invalid: {e}"))?;
    // The link is shown to the user and opened on click: only web links.
    if !(r.url.starts_with("https://") || r.url.starts_with("http://")) {
        return Err("update manifest has a non-web download link".into());
    }
    Ok(r)
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Failed(String),
}

/// Fetch and evaluate the manifest at `url` against the running version.
pub fn check(url: &str) -> UpdateState {
    let client = match reqwest::blocking::Client::builder()
        .user_agent(concat!("ShowMeFireField/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return UpdateState::Failed(e.to_string()),
    };
    let text = match client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
    {
        Ok(t) => t,
        Err(e) => return UpdateState::Failed(format!("could not reach the update server ({e})")),
    };
    match parse_release(&text) {
        Ok(r) if is_newer(env!("CARGO_PKG_VERSION"), &r.version) => UpdateState::Available(r),
        Ok(_) => UpdateState::UpToDate,
        Err(e) => UpdateState::Failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.9", "0.2.0"));
        assert!(is_newer("0.9.0", "1.0"), "missing parts count as zero");
        assert!(is_newer("1.2.3", "v1.10.0"), "numeric, not lexicographic");
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(
            !is_newer("1.0.0", "1.0.0-beta"),
            "pre-release suffix is ignored, not newer"
        );
        for bad in ["", "latest", "1.x.0", "1..2", "1.2.3.4.5"] {
            assert!(!is_newer("0.1.0", bad), "{bad:?} must never look newer");
            assert!(
                !is_newer(bad, "9.9.9"),
                "{bad:?} current version can't be compared"
            );
        }
    }

    #[test]
    fn manifest_parsing_and_link_safety() {
        let r = parse_release(
            r#"{"version":"0.2.0","url":"https://example.org/dl","notes":"Faster tiles"}"#,
        )
        .unwrap();
        assert_eq!(
            (r.version.as_str(), r.notes.as_str()),
            ("0.2.0", "Faster tiles")
        );
        assert!(
            parse_release(r#"{"version":"0.2.0","url":"https://example.org/dl"}"#)
                .unwrap()
                .notes
                .is_empty()
        );
        assert!(parse_release(r#"{"version":"0.2.0","url":"javascript:alert(1)"}"#).is_err());
        assert!(parse_release(r#"{"version":"0.2.0","url":"file:///C:/x.exe"}"#).is_err());
        assert!(parse_release("{}").is_err());
        assert!(parse_release("nope").is_err());
    }

    #[test]
    fn unreachable_server_is_a_message_not_a_crash() {
        match check("http://127.0.0.1:9/none.json") {
            UpdateState::Failed(m) => assert!(m.contains("could not reach"), "{m}"),
            other => panic!("{other:?}"),
        }
    }
}
