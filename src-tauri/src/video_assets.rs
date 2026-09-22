use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{access::AccessOperation, models::Workspace, video_production};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoAssetSource {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) source_type: String,
    pub(crate) categories: Vec<String>,
    pub(crate) free_scope: String,
    pub(crate) commercial_use: String,
    pub(crate) attribution: String,
    pub(crate) account_required: bool,
    pub(crate) automation_mode: String,
    pub(crate) preferred: bool,
    pub(crate) homepage: String,
    pub(crate) license_reference: String,
    pub(crate) notes: String,
    pub(crate) github_stars: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoAssetLicenseInput {
    pub(crate) provider_id: String,
    pub(crate) asset_kind: String,
    pub(crate) project_relative_path: String,
    pub(crate) source_url: String,
    #[serde(default)]
    pub(crate) creator: Option<String>,
    pub(crate) license_id: String,
    #[serde(default)]
    pub(crate) license_url: Option<String>,
    pub(crate) attribution_required: bool,
    #[serde(default)]
    pub(crate) attribution_text: Option<String>,
    #[serde(default)]
    pub(crate) notes: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoAssetLicenseRecord {
    pub(crate) id: String,
    pub(crate) provider_id: String,
    pub(crate) provider_name: String,
    pub(crate) asset_kind: String,
    pub(crate) project_relative_path: String,
    pub(crate) source_url: String,
    #[serde(default)]
    pub(crate) creator: Option<String>,
    pub(crate) license_id: String,
    #[serde(default)]
    pub(crate) license_url: Option<String>,
    pub(crate) attribution_required: bool,
    #[serde(default)]
    pub(crate) attribution_text: Option<String>,
    #[serde(default)]
    pub(crate) notes: Option<String>,
    pub(crate) retrieved_at: u64,
}

macro_rules! source {
    (
        $id:expr,
        $name:expr,
        $source_type:expr,
        $categories:expr,
        $free_scope:expr,
        $commercial_use:expr,
        $attribution:expr,
        $account_required:expr,
        $automation_mode:expr,
        $preferred:expr,
        $homepage:expr,
        $license_reference:expr,
        $notes:expr,
        $github_stars:expr $(,)?
    ) => {
        VideoAssetSource {
            id: $id.to_string(),
            name: $name.to_string(),
            source_type: $source_type.to_string(),
            categories: $categories
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            free_scope: $free_scope.to_string(),
            commercial_use: $commercial_use.to_string(),
            attribution: $attribution.to_string(),
            account_required: $account_required,
            automation_mode: $automation_mode.to_string(),
            preferred: $preferred,
            homepage: $homepage.to_string(),
            license_reference: $license_reference.to_string(),
            notes: $notes.to_string(),
            github_stars: $github_stars,
        }
    };
}

pub(crate) fn registry() -> Vec<VideoAssetSource> {
    vec![
        source!(
            "repotunnel-native",
            "RepoTunnel Native Scenes",
            "native-engine",
            &["diagrams", "motion-graphics", "text", "arrows", "tutorials"],
            "Always local and free.",
            "Allowed.",
            "None.",
            false,
            "native",
            true,
            "local://repotunnel/video",
            "RepoTunnel project code",
            "Preferred first for technical explainers because it is deterministic, fast, project-owned, and avoids external licensing.",
            None,
        ),
        source!(
            "lottiefiles-free",
            "LottieFiles Free Animations",
            "asset-source",
            &["lottie", "animated-icons", "ui-motion", "data-visualization"],
            "Free animations only under the Lottie Simple License.",
            "Allowed for free animations.",
            "Not required for Lottie Simple License assets; creator credit is encouraged.",
            false,
            "browser-or-documented-download",
            true,
            "https://lottiefiles.com/",
            "https://lottiefiles.com/page/license",
            "Never scrape the library or use premium assets without the applicable license. Record the individual animation URL.",
            None,
        ),
        source!(
            "mixkit-free",
            "Mixkit Free",
            "asset-source",
            &["stock-video", "music", "sound-effects", "video-templates", "transitions"],
            "Use only items explicitly carrying a Mixkit Free License.",
            "Allowed for Free License items.",
            "Not required for Free License items.",
            false,
            "browser-manual",
            true,
            "https://mixkit.co/",
            "https://mixkit.co/license/",
            "Reject Restricted License items for commercial tutorial output unless the intended use clearly fits the restriction.",
            None,
        ),
        source!(
            "pexels",
            "Pexels",
            "asset-source",
            &["stock-video", "stock-images", "b-roll"],
            "Photos and videos are free under the Pexels License.",
            "Allowed subject to Pexels restrictions.",
            "Not required.",
            false,
            "documented-api-or-browser",
            true,
            "https://www.pexels.com/",
            "https://www.pexels.com/license/",
            "Useful for real-world B-roll. Do not imply endorsement or reuse unaltered content as standalone stock.",
            None,
        ),
        source!(
            "pixabay",
            "Pixabay",
            "asset-source",
            &["stock-video", "images", "illustrations", "music", "sound-effects"],
            "Free under the Pixabay Content License.",
            "Allowed subject to prohibited uses.",
            "Not required.",
            false,
            "documented-api-or-browser",
            true,
            "https://pixabay.com/",
            "https://pixabay.com/service/license-summary/",
            "Good broad fallback source. Record the asset page because trademarks, people, and other rights may still matter.",
            None,
        ),
        source!(
            "coverr",
            "Coverr",
            "asset-source",
            &["stock-video", "background-video", "music", "loops"],
            "Free content under the Coverr license.",
            "Allowed for commercial and non-commercial projects.",
            "Not required.",
            false,
            "browser-manual",
            true,
            "https://coverr.co/",
            "https://coverr.co/license",
            "Strong source for clean looping backgrounds and B-roll. Do not build a competing stock service from the library.",
            None,
        ),
        source!(
            "openverse",
            "Openverse",
            "asset-search",
            &["images", "audio", "creative-commons", "public-domain"],
            "Searches openly licensed/public-domain media; license varies per result.",
            "Depends on the selected asset license.",
            "Depends on the selected asset license.",
            false,
            "documented-api",
            true,
            "https://openverse.org/",
            "https://openverse.org/about/",
            "Use as a discovery layer only; verify and record the originating asset license before use.",
            None,
        ),
        source!(
            "wikimedia-commons",
            "Wikimedia Commons",
            "asset-source",
            &["images", "video", "audio", "diagrams", "public-domain"],
            "Openly licensed/public-domain media; license varies per file.",
            "Usually allowed under the file's stated license.",
            "Often required; follow the individual file license.",
            false,
            "documented-api-or-browser",
            true,
            "https://commons.wikimedia.org/",
            "https://commons.wikimedia.org/wiki/Commons:Reusing_content_outside_Wikimedia",
            "Excellent for diagrams, historical media, logos with care, and public-domain material. Always record the file-page license.",
            None,
        ),
        source!(
            "poly-haven",
            "Poly Haven",
            "asset-source",
            &["3d-models", "textures", "hdri", "backgrounds"],
            "Assets are CC0.",
            "Allowed.",
            "Not required.",
            false,
            "public-api-or-browser",
            true,
            "https://polyhaven.com/",
            "https://polyhaven.com/license",
            "Best default external source for clean CC0 3D models, textures and HDRIs. Use the public API rather than scraping.",
            None,
        ),
        source!(
            "undraw",
            "unDraw",
            "asset-source",
            &["svg", "illustrations", "concept-scenes"],
            "Illustrations are free for commercial and personal use.",
            "Allowed.",
            "Not required.",
            false,
            "browser-manual-only",
            true,
            "https://undraw.co/",
            "https://undraw.co/license",
            "Useful for clean explainer illustrations, but its license restricts integrations/scraping. RepoTunnel must not automate library ingestion.",
            None,
        ),
        source!(
            "drawkit-free",
            "DrawKit Free",
            "asset-source",
            &["svg", "illustrations", "icons", "concept-scenes"],
            "Use free assets under the current DrawKit license.",
            "Allowed for licensed free assets.",
            "Not required under the current license.",
            false,
            "browser-manual-only",
            true,
            "https://www.drawkit.com/",
            "https://www.drawkit.com/license",
            "Good modern illustrations. Re-check the license at retrieval time because terms can change and redistribution is restricted.",
            None,
        ),
        source!(
            "storyset-free",
            "Storyset Free",
            "asset-source",
            &["animated-illustrations", "svg", "gif", "mp4", "concept-scenes"],
            "Free with attribution under Storyset terms.",
            "Allowed subject to Storyset terms.",
            "Required for free use.",
            false,
            "browser-manual-only",
            false,
            "https://storyset.com/",
            "https://storyset.com/terms",
            "Very useful for animated explainers, but automated downloading is forbidden by current terms. AI may use it only through deliberate browser/manual selection.",
            None,
        ),
        source!(
            "lordicon-free",
            "Lordicon Free",
            "asset-source",
            &["animated-icons", "lottie", "gif", "mp4"],
            "Free tier currently exposes thousands of icons.",
            "Allowed for free-tier icons.",
            "Required for free use.",
            true,
            "browser-or-free-api-with-attribution",
            false,
            "https://lordicon.com/",
            "https://lordicon.com/docs/license/free",
            "Use only free icons and automatically retain required video/description credit metadata.",
            None,
        ),
        source!(
            "iconscout-free-lottie",
            "IconScout Free Lottie",
            "asset-source",
            &["lottie", "animated-icons", "gif", "mp4", "3d-icons"],
            "Free Lottie collection exists; some promotions/freebies rotate.",
            "Allowed for assets marked free under their license.",
            "Free Lottie usage generally requires attribution.",
            true,
            "browser-manual",
            false,
            "https://iconscout.com/free-lottie-animations",
            "https://iconscout.com/free-lottie-animations",
            "Never assume an asset remains free because a premium asset can temporarily appear as a daily freebie. Save the license at download time.",
            None,
        ),
        source!(
            "flaticon-free",
            "Flaticon Free",
            "asset-source",
            &["icons", "animated-icons", "svg", "png"],
            "Free assets are available with attribution.",
            "Allowed for free licensed assets.",
            "Required for free use.",
            true,
            "browser-manual-only",
            false,
            "https://www.flaticon.com/",
            "https://www.flaticon.com/terms-of-use",
            "Good fallback when open icon sets are insufficient. Prefer Lucide/Tabler first because they avoid attribution bookkeeping.",
            None,
        ),
        source!(
            "adobe-stock-free",
            "Adobe Stock Free Collection",
            "asset-source",
            &["motion-graphics", "mogrt", "titles", "transitions", "templates"],
            "Use only assets explicitly marked FREE in Adobe Stock's Free collection.",
            "Depends on the Adobe Stock asset license.",
            "Follow the asset license.",
            true,
            "browser-manual-only",
            false,
            "https://stock.adobe.com/free",
            "https://stock.adobe.com/license-terms",
            "Useful when a free motion template materially improves a scene. Never fall through to paid Stock assets.",
            None,
        ),
        source!(
            "mixamo",
            "Adobe Mixamo",
            "asset-source",
            &["3d-character-animation", "rigging", "gestures", "walk-cycles"],
            "Free with an Adobe ID; no Creative Cloud subscription required.",
            "Royalty-free use is allowed for films and other projects.",
            "Not normally required.",
            true,
            "browser-manual-only",
            true,
            "https://www.mixamo.com/",
            "https://helpx.adobe.com/creative-cloud/faq/mixamo-faq.html",
            "Excellent for a character pointing, walking, presenting or gesturing in an occasional 3D explainer scene.",
            None,
        ),
        source!(
            "sketchfab-free",
            "Sketchfab Free Downloadable Models",
            "asset-source",
            &["3d-models", "animated-3d", "objects"],
            "Free downloadable models exist under Creative Commons licenses; other models may be paid.",
            "Depends on the selected Creative Commons license.",
            "Depends on the selected model license.",
            true,
            "browser-manual",
            false,
            "https://sketchfab.com/features/free-3d-models",
            "https://sketchfab.com/features/free-3d-models",
            "Filter to free/downloadable models and record the exact model license. Prefer CC0 when several models are suitable.",
            None,
        ),
        source!(
            "blendswap-free",
            "BlendSwap Free Library",
            "asset-source",
            &["blender", "3d-models", "rigged-models", "materials"],
            "Free community assets; free accounts have download limits.",
            "Depends on each Creative Commons/general asset license.",
            "Depends on the selected asset license.",
            true,
            "browser-manual",
            false,
            "https://blendswap.com/",
            "https://blendswap.com/about",
            "Useful when a native Blender .blend asset saves significant modeling time. Prefer CC0/CC BY and reject non-commercial licenses for commercial output.",
            None,
        ),
        source!(
            "quaternius",
            "Quaternius",
            "asset-source",
            &["3d-models", "characters", "animations", "low-poly"],
            "Assets are free under the current Quaternius Asset License.",
            "Allowed for personal, educational and commercial projects.",
            "Not required.",
            false,
            "browser-manual",
            true,
            "https://quaternius.com/",
            "https://quaternius.com/license.html",
            "Strong source for lightweight reusable 3D assets and animation packs.",
            None,
        ),
        source!(
            "freesound",
            "Freesound",
            "asset-source",
            &["sound-effects", "ambience", "audio"],
            "Free sounds under per-item CC0, CC BY, or CC BY-NC licenses.",
            "Depends on the selected sound license.",
            "Depends on the selected sound license.",
            true,
            "documented-api-or-browser",
            true,
            "https://freesound.org/",
            "https://freesound.org/help/faq/",
            "Prefer CC0/CC BY. Reject CC BY-NC automatically when the video is commercial or monetized.",
            None,
        ),
        source!(
            "canva-free",
            "Canva Free",
            "creation-site",
            &["templates", "text-animation", "slides", "simple-motion"],
            "Free plan exists, but many individual assets/features are Pro.",
            "Only for elements/templates licensed for the user's free account and intended use.",
            "Depends on the selected content license.",
            true,
            "browser-manual-only",
            false,
            "https://www.canva.com/",
            "https://www.canva.com/policies/content-license-agreement/",
            "Optional convenience tool only. Native RepoTunnel scenes remain preferred; AI must never select Pro-only content and then require payment.",
            None,
        ),
        source!(
            "mermaid",
            "Mermaid",
            "open-source-engine",
            &["flowcharts", "sequence-diagrams", "architecture", "technical-diagrams"],
            "Open source.",
            "Allowed under MIT.",
            "None.",
            false,
            "local-adapter",
            true,
            "https://github.com/mermaid-js/mermaid",
            "MIT",
            "Excellent for MCP vs API, proxies, request flows, sequence diagrams and architecture explainers. Render static SVG then animate/reveal it with RepoTunnel.",
            Some(90_300),
        ),
        source!(
            "excalidraw",
            "Excalidraw",
            "open-source-engine",
            &["hand-drawn-diagrams", "architecture", "whiteboard-style"],
            "Open source.",
            "Allowed under MIT.",
            "None.",
            false,
            "local-adapter",
            true,
            "https://github.com/excalidraw/excalidraw",
            "MIT",
            "Use when a hand-drawn educational whiteboard style explains a concept better than polished boxes.",
            Some(132_500),
        ),
        source!(
            "threejs",
            "three.js",
            "open-source-engine",
            &["3d", "webgl", "animated-objects", "technical-visualization"],
            "Open source.",
            "Allowed under MIT.",
            "None.",
            false,
            "local-adapter",
            true,
            "https://github.com/mrdoob/three.js",
            "MIT",
            "Preferred programmatic 3D engine for lightweight scenes before opening Blender.",
            Some(115_700),
        ),
        source!(
            "manim",
            "Manim Community",
            "open-source-engine",
            &["math", "algorithms", "concept-animation", "graphs", "technical-explainers"],
            "Open source.",
            "Allowed under MIT.",
            "None.",
            false,
            "optional-local-adapter",
            true,
            "https://github.com/ManimCommunity/manim",
            "MIT",
            "High-value optional adapter for algorithmic and mathematical explanations. Do not make it a mandatory dependency.",
            Some(40_900),
        ),
        source!(
            "lottie-web",
            "lottie-web",
            "open-source-engine",
            &["lottie-rendering", "vector-animation", "animated-icons"],
            "Open source.",
            "Allowed under its repository license.",
            "None for the engine; animation assets retain their own licenses.",
            false,
            "local-adapter",
            true,
            "https://github.com/airbnb/lottie-web",
            "Repository license",
            "Use for deterministic rendering of licensed Lottie JSON/dotLottie assets into tutorial scenes.",
            Some(32_100),
        ),
        source!(
            "gsap",
            "GSAP",
            "open-source-engine",
            &["motion-graphics", "svg-animation", "text-animation", "transitions"],
            "GSAP states the full toolset is free, including commercial use, under its standard license.",
            "Allowed under the current GSAP standard license.",
            "None.",
            false,
            "optional-local-adapter",
            true,
            "https://github.com/greensock/GSAP",
            "https://gsap.com/standard-license/",
            "Useful for advanced timing/morphing when RepoTunnel's native scene renderer is insufficient.",
            Some(28_500),
        ),
        source!(
            "lucide",
            "Lucide",
            "open-source-assets",
            &["svg-icons", "ui-icons", "technical-icons"],
            "Open source.",
            "Allowed under ISC.",
            "None.",
            false,
            "local-package",
            true,
            "https://github.com/lucide-icons/lucide",
            "ISC",
            "Preferred icon source before attribution-based icon websites.",
            Some(24_600),
        ),
        source!(
            "tabler-icons",
            "Tabler Icons",
            "open-source-assets",
            &["svg-icons", "ui-icons", "technical-icons"],
            "Open source.",
            "Allowed under MIT.",
            "None.",
            false,
            "local-package",
            true,
            "https://github.com/tabler/tabler-icons",
            "MIT",
            "More than 6,000 SVG icons; excellent fallback when Lucide lacks a needed concept.",
            Some(21_700),
        ),
        source!(
            "blender",
            "Blender",
            "open-source-engine",
            &["3d", "animation", "compositing", "motion-tracking", "video-editing"],
            "Free and open source.",
            "Allowed under GPL for the application; produced artwork is not automatically GPL.",
            "None.",
            false,
            "optional-desktop-adapter",
            false,
            "https://github.com/blender/blender",
            "GPL-3.0",
            "Heavy optional adapter for scenes that genuinely need advanced 3D, physics or compositing. Never open it for a simple diagram.",
            Some(20_400),
        ),
    ]
}

pub(crate) fn get_source(id: &str) -> Option<VideoAssetSource> {
    registry().into_iter().find(|source| source.id == id)
}

fn now_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_millis();
    Ok(u64::try_from(millis).unwrap_or(u64::MAX))
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if dash && !output.is_empty() {
                output.push('-');
            }
            output.push(ch.to_ascii_lowercase());
            dash = false;
        } else {
            dash = true;
        }
        if output.len() >= 64 {
            break;
        }
    }
    if output.is_empty() {
        "asset".to_string()
    } else {
        output.trim_matches('-').to_string()
    }
}

fn validate_https(url: &str, label: &str) -> Result<String, String> {
    let parsed = Url::parse(url).map_err(|_| format!("{label} is not a valid URL."))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(format!("{label} must use HTTPS."));
    }
    Ok(parsed.to_string())
}

fn validate_asset_kind(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 48 {
        return Err("Asset kind must be 1 to 48 characters.".to_string());
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err("Asset kind contains unsupported characters.".to_string());
    }
    Ok(value.to_string())
}

fn ensure_project_owned_file(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    relative_path: &str,
) -> Result<(), String> {
    let relative = relative_path.trim().replace('\\', "/");
    let prefix = format!("{}/", project.relative_path);
    if !relative.starts_with(&prefix) {
        return Err("Licensed asset path must belong to the selected Video Project.".to_string());
    }
    let path = video_production::resolve_project_path(
        workspace,
        project,
        &relative,
        AccessOperation::Read,
        true,
    )?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect licensed Video Project asset: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Licensed asset must be a regular project-owned file.".to_string());
    }
    Ok(())
}

pub(crate) fn record_license(
    workspace: &Workspace,
    project_id: &str,
    input: VideoAssetLicenseInput,
) -> Result<VideoAssetLicenseRecord, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let provider = get_source(input.provider_id.trim())
        .ok_or_else(|| "Unknown Video Production asset provider.".to_string())?;
    ensure_project_owned_file(workspace, &project, &input.project_relative_path)?;

    let source_url = validate_https(&input.source_url, "Asset source URL")?;
    let license_url = input
        .license_url
        .as_deref()
        .map(|value| validate_https(value, "Asset license URL"))
        .transpose()?;
    let license_id = input.license_id.trim();
    if license_id.is_empty() || license_id.len() > 160 {
        return Err("Asset license identifier must be 1 to 160 characters.".to_string());
    }
    if input.attribution_required
        && input
            .attribution_text
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err("Attribution text is required for an attribution-required asset.".to_string());
    }

    let retrieved_at = now_millis()?;
    let record = VideoAssetLicenseRecord {
        id: format!(
            "license-{:x}-{}",
            retrieved_at,
            slug(&input.project_relative_path)
        ),
        provider_id: provider.id,
        provider_name: provider.name,
        asset_kind: validate_asset_kind(&input.asset_kind)?,
        project_relative_path: input.project_relative_path.trim().replace('\\', "/"),
        source_url,
        creator: input
            .creator
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        license_id: license_id.to_string(),
        license_url,
        attribution_required: input.attribution_required,
        attribution_text: input
            .attribution_text
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        notes: input
            .notes
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        retrieved_at,
    };

    let prefix = format!("{}/", project.relative_path);
    let license_relative = format!("licenses/{}.json", record.id);
    let full_relative = format!("{prefix}{license_relative}");
    let path = video_production::resolve_project_path(
        workspace,
        &project,
        &full_relative,
        AccessOperation::Write,
        false,
    )?;
    let data = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("Could not serialize asset license record: {error}"))?;
    fs::write(&path, data)
        .map_err(|error| format!("Could not save asset license record: {error}"))?;
    video_production::register_asset(
        workspace,
        project_id,
        "license-record",
        &license_relative,
        Some(&format!("{} · {}", record.provider_name, record.license_id)),
    )?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{
        access::AccessOperation,
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
        video_production,
    };

    use super::{
        get_source, record_license, registry, validate_asset_kind, validate_https,
        VideoAssetLicenseInput,
    };

    fn temp_workspace() -> (tempfile::TempDir, Workspace) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "video-assets-test".to_string(),
            name: "Video assets test".to_string(),
            path: root.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    #[test]
    fn registry_contains_free_first_tutorial_sources() {
        let sources = registry();
        assert!(sources.len() >= 30);
        for id in [
            "repotunnel-native",
            "lottiefiles-free",
            "mixkit-free",
            "pexels",
            "pixabay",
            "poly-haven",
            "mermaid",
            "excalidraw",
            "threejs",
            "manim",
            "lucide",
            "tabler-icons",
        ] {
            assert!(sources.iter().any(|source| source.id == id), "{id}");
        }
    }

    #[test]
    fn high_star_engines_are_explicit_not_remote_code_execution() {
        for id in [
            "mermaid",
            "excalidraw",
            "threejs",
            "manim",
            "lottie-web",
            "gsap",
        ] {
            let source = get_source(id).unwrap();
            assert!(source.github_stars.unwrap_or_default() >= 20_000);
            assert!(source.automation_mode.contains("adapter"));
        }
    }

    #[test]
    fn license_metadata_rejects_unsafe_values() {
        assert!(validate_https("https://example.com/asset", "source").is_ok());
        assert!(validate_https("http://example.com/asset", "source").is_err());
        assert!(validate_https("file:///tmp/a", "source").is_err());
        assert!(validate_asset_kind("animated-icon").is_ok());
        assert!(validate_asset_kind("../../video").is_err());
    }

    #[test]
    fn license_record_is_project_owned_and_requires_attribution_when_needed() {
        let (_root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "Licensed explainer",
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let asset_relative = format!("{}/assets/images/example.svg", project.relative_path);
        let asset_path = video_production::resolve_project_path(
            &workspace,
            &project,
            &asset_relative,
            AccessOperation::Write,
            false,
        )
        .unwrap();
        fs::write(
            &asset_path,
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        )
        .unwrap();

        let input = VideoAssetLicenseInput {
            provider_id: "wikimedia-commons".to_string(),
            asset_kind: "svg".to_string(),
            project_relative_path: asset_relative.clone(),
            source_url: "https://commons.wikimedia.org/wiki/File:Example.svg".to_string(),
            creator: Some("Example creator".to_string()),
            license_id: "CC-BY-4.0".to_string(),
            license_url: Some("https://creativecommons.org/licenses/by/4.0/".to_string()),
            attribution_required: true,
            attribution_text: Some("Example creator — CC BY 4.0".to_string()),
            notes: Some("Tutorial diagram".to_string()),
        };
        let record = record_license(&workspace, &project.id, input).unwrap();
        assert_eq!(record.provider_id, "wikimedia-commons");
        assert_eq!(record.project_relative_path, asset_relative);
        assert!(record.attribution_required);

        let license_path =
            video_production::project_root(&workspace, &project, AccessOperation::Read)
                .unwrap()
                .join("licenses")
                .join(format!("{}.json", record.id));
        assert!(license_path.is_file());
        let persisted = fs::read_to_string(license_path).unwrap();
        assert!(persisted.contains("CC-BY-4.0"));
        assert!(persisted.contains("Example creator"));

        let missing_attribution = VideoAssetLicenseInput {
            provider_id: "storyset-free".to_string(),
            asset_kind: "svg".to_string(),
            project_relative_path: asset_relative,
            source_url: "https://storyset.com/illustration/example".to_string(),
            creator: None,
            license_id: "Storyset free terms".to_string(),
            license_url: Some("https://storyset.com/terms".to_string()),
            attribution_required: true,
            attribution_text: None,
            notes: None,
        };
        assert!(record_license(&workspace, &project.id, missing_attribution).is_err());
    }
}
