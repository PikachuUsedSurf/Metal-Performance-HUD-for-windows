//! Downloads the "Developer Disk Image" (DDI).
//!
//! The DDI is a small disk image made by Apple that ships inside Xcode. When it is
//! mounted on an iPhone or iPad, the device shows the Settings → Developer menu
//! (which is where the Graphics HUD switch lives) and turns on the developer tools
//! we use to launch games.
//!
//! We don't have Xcode on Windows, so we download the same files from a public
//! GitHub mirror that the open-source tool pymobiledevice3 also uses. Downloads are
//! cached in `%LOCALAPPDATA%\iOS HUD Helper` so they only happen once.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

use crate::worker::Reporter;

/// Where the image files are downloaded from.
const MIRROR: &str = "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main";

/// iOS versions that have a classic (iOS 16 and older) image on the mirror.
const CLASSIC_VERSIONS: &[(u32, u32)] = &[
    (11, 4),
    (12, 0), (12, 1), (12, 2), (12, 3), (12, 4),
    (13, 0), (13, 1), (13, 2), (13, 3), (13, 4), (13, 5), (13, 6), (13, 7),
    (14, 0), (14, 1), (14, 2), (14, 3), (14, 4), (14, 5), (14, 6), (14, 7), (14, 8),
    (15, 0), (15, 1), (15, 2), (15, 3), (15, 4), (15, 5), (15, 6), (15, 7), (15, 8),
    (16, 0), (16, 1), (16, 2), (16, 3), (16, 4), (16, 5), (16, 6), (16, 7),
];

/// Image for iOS 16 and older: one shared image per iOS version, plus Apple's signature.
pub struct ClassicImage {
    pub image: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Image for iOS 17 and newer: one image for every device. Apple's servers
/// "personalize" (sign) it for your specific device right before it is mounted.
pub struct PersonalizedImage {
    pub image: Vec<u8>,
    pub build_manifest: Vec<u8>,
    pub trust_cache: Vec<u8>,
}

/// The newest packaging of the image (used from iOS 27), installed as a "cryptex".
pub struct CryptexImage {
    pub image: Vec<u8>,
    pub build_manifest: Vec<u8>,
    pub trust_cache: Vec<u8>,
    pub info: Vec<u8>,
    pub root_hash: Vec<u8>,
}

/// Picks the closest classic image for an iOS version, e.g. iOS 15.3.1 → "15.3".
pub fn pick_classic_version(major: u32, minor: u32) -> Option<(u32, u32)> {
    CLASSIC_VERSIONS
        .iter()
        .copied()
        .filter(|&(ma, mi)| ma == major && mi <= minor)
        .max()
        // If there's no exact match (e.g. a brand-new minor version), use the newest one for that major.
        .or_else(|| CLASSIC_VERSIONS.iter().copied().filter(|&(ma, _)| ma == major).max())
}

fn cache_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("iOS HUD Helper")
        .join("DeveloperDiskImages")
}

pub async fn classic(major: u32, minor: u32, rep: &Reporter) -> Result<ClassicImage> {
    let (ma, mi) = pick_classic_version(major, minor)
        .ok_or_else(|| anyhow!("No Developer Disk Image is available for iOS {major}.{minor}."))?;
    let version = format!("{ma}.{mi}");
    let dir = cache_dir().join(&version);
    let remote = format!("{MIRROR}/DeveloperDiskImages/{version}");

    let image = cached_or_download(
        &dir.join("DeveloperDiskImage.dmg"),
        &format!("{remote}/DeveloperDiskImage.dmg"),
        &format!("Downloading developer image for iOS {version}"),
        rep,
    )
    .await?;
    let signature = cached_or_download(
        &dir.join("DeveloperDiskImage.dmg.signature"),
        &format!("{remote}/DeveloperDiskImage.dmg.signature"),
        "Downloading image signature",
        rep,
    )
    .await?;
    Ok(ClassicImage { image, signature })
}

pub async fn personalized(rep: &Reporter) -> Result<PersonalizedImage> {
    let dir = cache_dir().join("Xcode_iOS_DDI_Personalized");
    let remote = format!("{MIRROR}/PersonalizedImages/Xcode_iOS_DDI_Personalized");
    let fresh = refresh_manifest(&dir, &remote, rep).await?;

    let image = fetch(&dir, &remote, "Image.dmg", fresh, "Downloading developer image", rep).await?;
    let trust_cache = fetch(&dir, &remote, "Image.dmg.trustcache", fresh, "Downloading trust cache", rep).await?;
    let build_manifest = tokio::fs::read(dir.join("BuildManifest.plist")).await?;
    Ok(PersonalizedImage { image, build_manifest, trust_cache })
}

pub async fn cryptex(rep: &Reporter) -> Result<CryptexImage> {
    let dir = cache_dir().join("Xcode_iOS_DDI_Cryptex");
    let remote = format!("{MIRROR}/PersonalizedImages/Xcode_iOS_DDI_Cryptex");
    let fresh = refresh_manifest(&dir, &remote, rep).await?;

    let image = fetch(&dir, &remote, "Image.dmg", fresh, "Downloading developer image", rep).await?;
    let trust_cache = fetch(&dir, &remote, "Image.dmg.trustcache", fresh, "Downloading trust cache", rep).await?;
    let info = fetch(&dir, &remote, "Image.dmg.cryptex_info", fresh, "Downloading image info", rep).await?;
    let root_hash = fetch(&dir, &remote, "Image.dmg.root_hash", fresh, "Downloading image hash", rep).await?;
    let build_manifest = tokio::fs::read(dir.join("BuildManifest.plist")).await?;
    Ok(CryptexImage { image, build_manifest, trust_cache, info, root_hash })
}

/// Downloads the small BuildManifest.plist and compares it with the cached copy.
/// Returns `true` when it changed, meaning the big image must be downloaded again.
/// If we're offline but have a cached copy, we just keep using the cache.
async fn refresh_manifest(dir: &std::path::Path, remote: &str, rep: &Reporter) -> Result<bool> {
    let local = dir.join("BuildManifest.plist");
    let cached = tokio::fs::read(&local).await.ok();
    match download(&format!("{remote}/BuildManifest.plist"), "Checking for the latest developer image", rep).await {
        Ok(latest) => {
            let changed = cached.as_deref() != Some(latest.as_slice());
            if changed {
                tokio::fs::create_dir_all(dir).await?;
                tokio::fs::write(&local, &latest).await?;
            }
            Ok(changed)
        }
        Err(e) if cached.is_some() => {
            rep.log(format!("Couldn't check for a newer image ({e:#}). Using the saved copy."));
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

async fn fetch(
    dir: &std::path::Path,
    remote: &str,
    name: &str,
    force: bool,
    label: &str,
    rep: &Reporter,
) -> Result<Vec<u8>> {
    let path = dir.join(name);
    if force {
        let _ = tokio::fs::remove_file(&path).await;
    }
    cached_or_download(&path, &format!("{remote}/{name}"), label, rep).await
}

async fn cached_or_download(path: &std::path::Path, url: &str, label: &str, rep: &Reporter) -> Result<Vec<u8>> {
    if let Ok(bytes) = tokio::fs::read(path).await
        && !bytes.is_empty()
    {
        return Ok(bytes);
    }
    let bytes = download(url, label, rep).await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Write to a temporary name first so a half-finished download is never mistaken for a good one.
    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(bytes)
}

async fn download(url: &str, label: &str, rep: &Reporter) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("ios-hud-helper/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Couldn't connect to download {url}. Check your internet connection."))?;
    if !response.status().is_success() {
        bail!("Download failed ({}) for {url}", response.status());
    }
    let total = response.content_length();
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if let Some(total) = total.filter(|t| *t > 0) {
            rep.progress(label, bytes.len() as f32 / total as f32);
        }
    }
    rep.clear_progress();
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_closest_classic_image() {
        assert_eq!(pick_classic_version(15, 3), Some((15, 3)));
        assert_eq!(pick_classic_version(16, 7), Some((16, 7)));
        assert_eq!(pick_classic_version(16, 9), Some((16, 7))); // newer minor than the mirror has
        assert_eq!(pick_classic_version(11, 0), Some((11, 4))); // only 11.4 exists
        assert_eq!(pick_classic_version(17, 0), None); // iOS 17+ uses the personalized image
    }

    /// Downloads the real files (about 25 MB). Run with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn downloads_images() {
        let (rep, _rx) = Reporter::for_tests();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let classic = classic(16, 4, &rep).await.unwrap();
            assert!(classic.image.len() > 1_000_000 && classic.signature.len() == 128);

            let p = personalized(&rep).await.unwrap();
            let manifest: plist::Dictionary = plist::from_bytes(&p.build_manifest).unwrap();
            assert!(manifest.contains_key("BuildIdentities"));
            assert!(p.image.len() > 1_000_000 && !p.trust_cache.is_empty());

            let c = cryptex(&rep).await.unwrap();
            let manifest: plist::Dictionary = plist::from_bytes(&c.build_manifest).unwrap();
            idevice::tss::select_cryptex_build_identity(&manifest).expect("cryptex identity");
            assert!(!c.info.is_empty() && !c.root_hash.is_empty());

            // Second time round everything comes from the cache.
            let again = personalized(&rep).await.unwrap();
            assert_eq!(again.image.len(), p.image.len());
        });
    }
}
