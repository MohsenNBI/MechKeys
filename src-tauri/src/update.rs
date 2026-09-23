//! Updates, from the releases of this repository.
//!
//! The app asks GitHub whether a newer build is published, and if it is it can
//! fetch the installer for that release into the Downloads folder. That is as
//! far as it goes on purpose: nothing here is ever executed. A build signed by
//! nobody, which is what an indie release usually is, cannot prove to this
//! program that the file it downloaded came from the person who wrote it, so
//! the last click — running the installer — stays a decision the user makes.
//!
//! Two requests leave the machine at most, both plain unauthenticated GETs of
//! one fixed https address, and both only when the button is pressed (or when
//! the automatic check is on and the window is opened).

use serde::Serialize;

/// Where the releases live. Change these two lines and the whole feature moves.
const OWNER: &str = "mohsenNBI";
const REPO: &str = "MechKeys";

/// Page a human can read, offered as the fallback.
pub fn release_page() -> String {
    format!("https://github.com/{OWNER}/{REPO}/releases")
}

fn latest_api() -> String {
    format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest")
}

/// What GitHub said about the newest release, kept on the Rust side so the
/// download step cannot be pointed anywhere the front end chooses.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub notes: String,
    pub published: String,
    pub download_url: String,
    pub file_name: String,
}

/// The same thing as handed to the window: no download URL, because the window
/// has no business naming a file this program is about to write to disk.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Update {
    /// This build is older than the published one.
    pub available: bool,
    pub current: String,
    pub version: String,
    pub page: String,
    pub notes: String,
    pub published: String,
    pub file_name: String,
}

impl Update {
    pub fn from_release(r: &Release, available: bool) -> Update {
        Update {
            available,
            current: env!("CARGO_PKG_VERSION").to_string(),
            version: r.version.clone(),
            page: r.page.clone(),
            notes: r.notes.clone(),
            published: r.published.clone(),
            file_name: r.file_name.clone(),
        }
    }
}

/// Requests the newest release from GitHub. The message in `Err` is meant for
/// the window, so it is in whatever language the window is in and it names what
/// to do about it.
pub fn latest(lang: &str) -> Result<Release, String> {
    let response = minreq::get(latest_api())
        .with_header("User-Agent", user_agent())
        .with_header("Accept", "application/vnd.github+json")
        .with_timeout(8)
        .send()
        .map_err(|e| format!("{}: {e}", m(lang, "Could not reach GitHub", "اتصال به گیت‌هاب برقرار نشد")))?;
    match response.status_code {
        200 => {}
        // The honest reading of a 404 is that nothing has been published yet.
        404 => {
            return Err(m(
                lang,
                "No release has been published on GitHub yet",
                "هنوز هیچ نسخه‌ای در گیت‌هاب منتشر نشده است",
            ))
        }
        // Rate limit, or the repository is private.
        403 | 401 => {
            return Err(m(
                lang,
                "GitHub refused the request (probably a request limit)",
                "گیت‌هاب این درخواست را نپذیرفت (احتمال محدودیت درخواست‌ها)",
            ))
        }
        code => {
            return Err(format!(
                "{} (HTTP {code})",
                m(lang, "GitHub answered unexpectedly", "گیت‌هاب پاسخ غیرمنتظره داد")
            ))
        }
    }
    let body = response.as_str().map_err(|e| {
        format!(
            "{}: {e}",
            m(lang, "Could not read GitHub's answer", "پاسخ گیت‌هاب خوانده نشد")
        )
    })?;
    parse(body, lang)
}

/// One sentence, in the language the window asked for.
fn m(lang: &str, en: &str, fa: &str) -> String {
    if crate::config::lang(lang) == "fa" {
        fa.to_string()
    } else {
        en.to_string()
    }
}

/// GitHub asks every API client to name itself.
fn user_agent() -> String {
    format!("{REPO}/{}", env!("CARGO_PKG_VERSION"))
}

/// Reads one release out of the API answer. Split out from the request so the
/// rules can be tried on a canned answer without a network.
fn parse(body: &str, lang: &str) -> Result<Release, String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        format!(
            "{}: {e}",
            m(lang, "GitHub's answer is not readable", "پاسخ گیت‌هاب قابل خواندن نیست")
        )
    })?;
    let text = |key: &str| -> String {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let tag = text("tag_name");
    let version = tag.trim_start_matches('v').to_string();
    if version.is_empty() {
        return Err(m(
            lang,
            "The published release has no version tag",
            "نسخهٔ منتشرشده بی‌نام است",
        ));
    }
    let draft = value.get("draft").and_then(|v| v.as_bool()).unwrap_or(false);
    if draft {
        return Err(m(
            lang,
            "The newest release is still a draft",
            "آخرین ریلیز هنوز پیش‌نویس است",
        ));
    }
    // A release carries one installer per word size, and the wrong one fails at
    // install time, so anything built for the other size is dropped before the
    // pick. A release with a single unmarked installer still works.
    let theirs = if cfg!(target_pointer_width = "64") { "_x86" } else { "_x64" };
    let asset = value.get("assets").and_then(|a| a.as_array()).and_then(|list| {
        let ours = || list.iter().filter(|a| !asset_name(a).contains(theirs));
        ours()
            .find(|a| asset_name(a).ends_with("-setup.exe"))
            .or_else(|| ours().find(|a| asset_name(a).ends_with(".exe")))
    });
    let (download_url, file_name) = match asset {
        Some(a) => {
            let url = a.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or_default();
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or_default();
            if !url.starts_with("https://") {
                return Err(m(
                    lang,
                    "The download address is not https",
                    "نشانی دریافت امن (https) نیست",
                ));
            }
            (url.to_string(), name.to_string())
        }
        None => (String::new(), String::new()),
    };
    Ok(Release {
        version,
        page: text("html_url"),
        notes: text("body").trim().to_string(),
        // "2026-09-20T11:04:22Z" is a lot of machinery for a date line.
        published: text("published_at").chars().take(10).collect(),
        download_url,
        file_name,
    })
}

/// Whether `tag` names a build newer than `have`. Component wise, so 1.10 is
/// past 1.9, and a leading v in the tag is the same version without it.
pub fn is_newer(tag: &str, have: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(tag), parts(have));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// Writes the installer of `release` into the Downloads folder and returns the
/// path it landed on. Nothing is run: see the module note.
pub fn download(release: &Release, lang: &str) -> Result<std::path::PathBuf, String> {
    if release.download_url.is_empty() {
        return Err(m(
            lang,
            "This release has no installer; take it from the release page",
            "این ریلیز فایل نصبی ندارد؛ از صفحهٔ ریلیز بردارید",
        ));
    }
    let body = minreq::get(&release.download_url)
        .with_header("User-Agent", user_agent())
        .with_timeout(120)
        .send()
        .map_err(|e| format!("{}: {e}", m(lang, "Download failed", "دریافت انجام نشد")))?;
    if body.status_code != 200 {
        return Err(format!(
            "{} (HTTP {})",
            m(lang, "The file server answered unexpectedly", "سرور فایل پاسخ غیرمنتظره داد"),
            body.status_code
        ));
    }
    // Five megabytes of installer is small enough to hold in one go, and it
    // means a half written file can never be mistaken for the real thing.
    let bytes = body.as_bytes();
    if bytes.len() < 100_000 {
        return Err(m(
            lang,
            "The downloaded file is too small to be an installer",
            "فایل دریافتی کوتاه‌تر از آن است که نصبی باشد",
        ));
    }
    let path = crate::config::downloads().join(safe_name(&release.file_name));
    std::fs::write(&path, bytes).map_err(|e| {
        format!(
            "{}: {e}",
            m(lang, "Could not write the file", "نوشتن فایل ممکن نشد")
        )
    })?;
    Ok(path)
}

/// The name a release lists an asset under.
fn asset_name(a: &serde_json::Value) -> &str {
    a.get("name").and_then(|n| n.as_str()).unwrap_or_default()
}

/// The file name as it may appear on disk: nothing that could walk out of the
/// folder it is written to, and always an installer.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(120)
        .collect();
    if cleaned.to_lowercase().ends_with(".exe") && cleaned.len() > 4 {
        cleaned
    } else {
        format!("MechKeys-setup-{}.exe", env!("CARGO_PKG_VERSION"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: &str = env!("CARGO_PKG_VERSION");

    /// One past this build, however the version happens to read, so the
    /// comparisons below do not expire at the next release.
    fn next() -> String {
        let minor: u64 = CURRENT.split('.').nth(1).unwrap_or("0").parse().unwrap_or(0);
        format!("{}.{}.0", CURRENT.split('.').next().unwrap(), minor + 1)
    }

    fn release(tag: &str, assets: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/o/r/releases/tag/{tag}",
            "body":"fixes","published_at":"2026-09-20T11:04:22Z","draft":false,
            "assets":[{assets}]}}"#
        )
    }

    /// An installer asset as a release page would list it.
    fn exe(arch: &str) -> String {
        format!(
            r#"{{"name":"MechKeys_1.4.1_{arch}-setup.exe","browser_download_url":"https://objects.example/{arch}.exe"}}"#
        )
    }
    // The release carries both word sizes, so the tests below have to know
    // which one this build is.
    const MINE: &str = if cfg!(target_pointer_width = "64") { "x64" } else { "x86" };
    const THEIRS: &str = if cfg!(target_pointer_width = "64") { "x86" } else { "x64" };
    const EN: &str = "en";
    const FA: &str = "fa";

    /// Does this sentence actually read as Persian?
    fn is_fa(s: &str) -> bool {
        s.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c))
    }

    #[test]
    fn a_newer_release_is_recognised() {
        let r = parse(&release("v1.4.1", &exe(MINE)).replace('\n', ""), EN).unwrap();
        assert_eq!(r.version, "1.4.1");
        assert_eq!(r.published, "2026-09-20");
        assert!(r.download_url.starts_with("https://"));
    }

    #[test]
    fn this_build_beats_itself_and_older_ones() {
        assert!(!is_newer(CURRENT, CURRENT));
        assert!(!is_newer(&format!("v{CURRENT}"), CURRENT));
        assert!(is_newer(&next(), CURRENT));
        assert!(!is_newer(CURRENT, &next()));
        assert!(!is_newer("", CURRENT));
        // The case a two digit minor actually produces: 1.10 is past 1.9.
        assert!(is_newer("1.10.0", "1.9.7"));
        assert!(!is_newer("1.9.7", "1.10.0"));
        // A version with fewer parts is padded, not judged by length.
        assert!(is_newer("2.0", "1.99.99"));
        assert!(!is_newer("2.0", "2.0.1"));
    }

    #[test]
    fn the_installer_is_preferred_over_the_source_archive() {
        let zip = r#"{"name":"source.zip","browser_download_url":"https://example/s.zip"}"#;
        let mine = exe(MINE);
        let body = release("1.4.1", &format!("{zip}, {mine}")).replace('\n', "");
        let r = parse(&body, EN).unwrap();
        assert!(r.file_name.ends_with("-setup.exe"));
        // With no installer at all the check still works, it just has nothing
        // to offer but the page.
        let only_zip = release("1.4.1", zip).replace('\n', "");
        let r = parse(&only_zip, EN).unwrap();
        assert!(r.download_url.is_empty());
    }

    #[test]
    fn an_installer_for_the_other_word_size_is_never_offered() {
        // Both installers on one release, the foreign one listed first.
        let (mine, theirs) = (exe(MINE), exe(THEIRS));
        let body = release("1.4.1", &format!("{theirs}, {mine}")).replace('\n', "");
        let r = parse(&body, EN).unwrap();
        assert!(r.file_name.contains(MINE), "{}", r.file_name);
        // A release with only the wrong build for this machine is a real
        // release; the check still answers, it just has nothing to hand over.
        let alone = release("1.4.1", &theirs).replace('\n', "");
        let r = parse(&alone, EN).unwrap();
        assert_eq!(r.version, "1.4.1");
        assert!(r.download_url.is_empty(), "{}", r.download_url);
    }

    #[test]
    fn a_release_from_a_wrong_place_is_not_fetched() {
        let plain = r#"{"name":"a.exe","browser_download_url":"http://example/a.exe"}"#;
        let err = parse(&release("1.4.1", plain).replace('\n', ""), EN).unwrap_err();
        assert!(err.contains("https"));
    }

    #[test]
    fn junk_answers_are_explained_not_panicked_on() {
        assert!(parse("not json", EN).is_err());
        assert!(parse(r#"{"tag_name":""}"#, EN).is_err());
        let draft = release("1.4.1", &exe(MINE))
            .replace("\"draft\":false", "\"draft\":true")
            .replace('\n', "");
        assert!(parse(&draft, EN).is_err());
        assert!(parse("{}", EN).is_err());
    }

    #[test]
    fn every_refusal_speaks_the_language_it_was_asked_for() {
        // The same failure, in whichever language the window is reading.
        for lang in [EN, FA] {
            let bad = parse("not json", lang).unwrap_err();
            assert_eq!(is_fa(&bad), lang == FA, "{bad}");
            let insecure = {
                let plain = r#"{"name":"a.exe","browser_download_url":"http://example/a.exe"}"#;
                parse(&release("1.4.1", plain).replace('\n', ""), lang).unwrap_err()
            };
            assert_eq!(is_fa(&insecure), lang == FA, "{insecure}");
            // An unknown language is not a third one: it gets the default.
            assert!(!is_fa(&parse("not json", "xx").unwrap_err()));
        }
        // Nothing the window can be shown is left untranslated.
        let r = Release {
            version: "1.5.0".into(),
            page: String::new(),
            notes: String::new(),
            published: String::new(),
            download_url: String::new(),
            file_name: String::new(),
        };
        assert!(!is_fa(&download(&r, EN).unwrap_err()));
    }

    #[test]
    fn a_file_name_cannot_escape_the_folder() {
        assert_eq!(safe_name("..\\..\\windows\\system32\\evil.exe"), "....windowssystem32evil.exe");
        assert_eq!(safe_name("setup.exe"), "setup.exe");
        // Anything that is not an installer becomes one with a known name.
        let fallback = safe_name("../../x");
        assert!(fallback.starts_with("MechKeys-setup-") && fallback.ends_with(".exe"));
        assert!(safe_name("").ends_with(".exe"));
        assert!(safe_name(&"a".repeat(400)).len() < 60);
    }

    #[test]
    fn the_update_dto_carries_no_download_address() {
        let r = parse(&release("1.4.1", &exe(MINE)).replace('\n', ""), EN).unwrap();
        let json = serde_json::to_string(&Update::from_release(&r, true)).unwrap();
        assert!(!json.contains("objects.example"));
        assert!(json.contains("\"available\":true"));
        assert!(json.contains(&format!("\"current\":\"{CURRENT}\"")));
    }
}
