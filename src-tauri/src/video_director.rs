use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    access::AccessOperation,
    models::Workspace,
    video_production::{self, VideoProductionProject},
};

const DIRECTOR_VERSION: u32 = 1;
const ACTION_LIBRARY_VERSION: u32 = 1;
const DIRECTOR_FILE: &str = "story/director.json";
const ANIMATIC_FILE: &str = "story/animatic/plan.json";
const CACHE_FILE: &str = "story/cache/index.json";
const NARRATIVE_QA_FILE: &str = "qa/narrative-plan.json";
const MAX_CHARACTERS: usize = 64;
const MAX_LOCATIONS: usize = 64;
const MAX_PROPS: usize = 256;
const MAX_SHOTS: usize = 2_000;
const MAX_ACTORS_PER_SHOT: usize = 24;
const MAX_SHOT_SECONDS: f64 = 180.0;
const MAX_TOTAL_SECONDS: f64 = 8.0 * 60.0 * 60.0;

const SIMPLE_2D_ACTIONS: &[&str] = &[
    "idle",
    "breathe",
    "talk-neutral",
    "talk-angry",
    "talk-happy",
    "point",
    "explain",
    "wave",
    "look-left",
    "look-right",
    "turn",
    "sit",
    "stand",
    "shocked",
    "sad",
    "laugh",
];

const MOVEMENT_ACTIONS: &[&str] = &["walk", "run"];

const COMPLEX_INTERACTION_ACTIONS: &[&str] = &[
    "kneel",
    "dig",
    "lift",
    "carry",
    "give",
    "receive",
    "hug",
    "push",
    "pull",
    "open-door",
    "pick-up",
    "drop",
];

const TARGET_REQUIRED_ACTIONS: &[&str] = &[
    "dig",
    "lift",
    "carry",
    "give",
    "receive",
    "hug",
    "push",
    "pull",
    "open-door",
    "pick-up",
    "drop",
];

const TWO_HAND_ACTIONS: &[&str] = &["dig", "lift", "carry", "push", "pull"];

const COMPLEX_CAMERA_MOVES: &[&str] = &["tracking", "dolly", "orbit", "crane", "handheld"];

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryCharacter {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) visual_description: String,
    #[serde(default = "default_rig_profile")]
    pub(crate) rig_profile: String,
    #[serde(default)]
    pub(crate) costume: String,
    #[serde(default)]
    pub(crate) continuity_tags: Vec<String>,
    #[serde(default)]
    pub(crate) asset_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryLocation {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) variants: Vec<String>,
    #[serde(default)]
    pub(crate) entrance_anchors: Vec<String>,
    #[serde(default)]
    pub(crate) interaction_anchors: Vec<String>,
    #[serde(default)]
    pub(crate) camera_anchors: Vec<String>,
    #[serde(default)]
    pub(crate) walkable_areas: Vec<String>,
    #[serde(default)]
    pub(crate) asset_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryProp {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) owner_character_id: Option<String>,
    #[serde(default = "default_prop_handling")]
    pub(crate) handling: String,
    #[serde(default)]
    pub(crate) asset_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryVoiceCastEntry {
    pub(crate) character_id: String,
    pub(crate) provider: String,
    pub(crate) voice: String,
    pub(crate) language: String,
    #[serde(default)]
    pub(crate) voice_model_path: Option<String>,
    #[serde(default)]
    pub(crate) rate: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryCamera {
    #[serde(default = "default_shot_type")]
    pub(crate) shot_type: String,
    #[serde(default = "default_camera_movement")]
    pub(crate) movement: String,
    #[serde(default = "default_camera_angle")]
    pub(crate) angle: String,
    #[serde(default)]
    pub(crate) framing: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryActorBeat {
    pub(crate) character_id: String,
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) target_id: Option<String>,
    #[serde(default)]
    pub(crate) start_anchor: Option<String>,
    #[serde(default)]
    pub(crate) end_anchor: Option<String>,
    #[serde(default)]
    pub(crate) emotion: String,
    #[serde(default)]
    pub(crate) look_target: Option<String>,
    #[serde(default)]
    pub(crate) dialogue: Option<String>,
    #[serde(default = "default_lip_sync")]
    pub(crate) lip_sync: bool,
    #[serde(default)]
    pub(crate) hand_target: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryShotInput {
    pub(crate) id: String,
    pub(crate) scene_id: String,
    pub(crate) order: u32,
    pub(crate) duration_seconds: f64,
    pub(crate) location_id: String,
    #[serde(default)]
    pub(crate) location_variant: Option<String>,
    #[serde(default)]
    pub(crate) camera: StoryCamera,
    #[serde(default)]
    pub(crate) actors: Vec<StoryActorBeat>,
    #[serde(default)]
    pub(crate) props: Vec<String>,
    #[serde(default)]
    pub(crate) ambience: Vec<String>,
    #[serde(default)]
    pub(crate) foley: Vec<String>,
    #[serde(default)]
    pub(crate) requested_engine: Option<String>,
    #[serde(default)]
    pub(crate) transition: Option<String>,
    #[serde(default)]
    pub(crate) notes: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryDirectorInput {
    #[serde(default = "default_story_language")]
    pub(crate) language: String,
    #[serde(default = "default_visual_style")]
    pub(crate) visual_style: String,
    #[serde(default)]
    pub(crate) characters: Vec<StoryCharacter>,
    #[serde(default)]
    pub(crate) locations: Vec<StoryLocation>,
    #[serde(default)]
    pub(crate) props: Vec<StoryProp>,
    #[serde(default)]
    pub(crate) voice_cast: Vec<StoryVoiceCastEntry>,
    #[serde(default)]
    pub(crate) shots: Vec<StoryShotInput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryCompiledShot {
    #[serde(flatten)]
    pub(crate) shot: StoryShotInput,
    pub(crate) selected_engine: String,
    pub(crate) engine_reason: String,
    pub(crate) render_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryDirectorPlan {
    pub(crate) version: u32,
    pub(crate) action_library_version: u32,
    pub(crate) project_id: String,
    pub(crate) language: String,
    pub(crate) visual_style: String,
    pub(crate) characters: Vec<StoryCharacter>,
    pub(crate) locations: Vec<StoryLocation>,
    pub(crate) props: Vec<StoryProp>,
    pub(crate) voice_cast: Vec<StoryVoiceCastEntry>,
    pub(crate) shots: Vec<StoryCompiledShot>,
    pub(crate) content_hash: String,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryAnimaticShot {
    pub(crate) shot_id: String,
    pub(crate) scene_id: String,
    pub(crate) order: u32,
    pub(crate) start_seconds: f64,
    pub(crate) end_seconds: f64,
    pub(crate) duration_seconds: f64,
    pub(crate) selected_engine: String,
    pub(crate) render_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryShotCacheEntry {
    pub(crate) shot_id: String,
    pub(crate) render_key: String,
    pub(crate) selected_engine: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) output_path: Option<String>,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryRenderQueue {
    pub(crate) version: u32,
    pub(crate) project_id: String,
    pub(crate) content_hash: String,
    pub(crate) changed_shot_ids: Vec<String>,
    pub(crate) reusable_shot_ids: Vec<String>,
    pub(crate) entries: Vec<StoryShotCacheEntry>,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryShotRenderInput {
    pub(crate) shot_id: String,
    pub(crate) render_key: String,
    pub(crate) output_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryAnimaticPlan {
    pub(crate) version: u32,
    pub(crate) project_id: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) total_duration_seconds: f64,
    pub(crate) content_hash: String,
    pub(crate) shots: Vec<StoryAnimaticShot>,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrativeQaIssue {
    pub(crate) severity: String,
    pub(crate) code: String,
    #[serde(default)]
    pub(crate) shot_id: Option<String>,
    #[serde(default)]
    pub(crate) character_id: Option<String>,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrativeQaMetrics {
    pub(crate) shot_count: usize,
    pub(crate) character_count: usize,
    pub(crate) location_count: usize,
    pub(crate) prop_count: usize,
    pub(crate) voiced_character_count: usize,
    pub(crate) camera_shot_type_count: usize,
    pub(crate) idle_actor_ratio: f64,
    pub(crate) complex_interaction_count: usize,
    pub(crate) total_duration_seconds: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrativeQaReport {
    pub(crate) version: u32,
    pub(crate) project_id: String,
    pub(crate) passed: bool,
    pub(crate) issues: Vec<NarrativeQaIssue>,
    pub(crate) metrics: NarrativeQaMetrics,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryActionCapability {
    pub(crate) action: String,
    pub(crate) complexity: String,
    pub(crate) requires_target: bool,
    pub(crate) requires_movement_anchors: bool,
    pub(crate) requires_hand_target: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryEngineCapability {
    pub(crate) id: String,
    pub(crate) available: bool,
    pub(crate) executable: Option<String>,
    pub(crate) best_for: String,
    pub(crate) notes: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryProductionCapabilities {
    pub(crate) actions: Vec<StoryActionCapability>,
    pub(crate) engines: Vec<StoryEngineCapability>,
    pub(crate) animatic_width: u32,
    pub(crate) animatic_height: u32,
    pub(crate) animatic_fps: u32,
}

fn default_rig_profile() -> String {
    "human".to_string()
}

fn default_prop_handling() -> String {
    "one-hand".to_string()
}

fn default_shot_type() -> String {
    "medium".to_string()
}

fn default_camera_movement() -> String {
    "static".to_string()
}

fn default_camera_angle() -> String {
    "eye-level".to_string()
}

impl Default for StoryCamera {
    fn default() -> Self {
        Self {
            shot_type: default_shot_type(),
            movement: default_camera_movement(),
            angle: default_camera_angle(),
            framing: String::new(),
        }
    }
}

fn default_lip_sync() -> bool {
    true
}

fn default_story_language() -> String {
    "en".to_string()
}

fn default_visual_style() -> String {
    "2d".to_string()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn safe_id(value: &str, label: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > 80
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(format!(
            "{label} must be 1..80 ASCII letters, digits, hyphen, underscore, or dot."
        ));
    }
    Ok(trimmed.to_string())
}

fn bounded_line(value: &str, label: &str, max: usize) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.chars().count() > max || trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(format!(
            "{label} must be one line and at most {max} characters."
        ));
    }
    Ok(trimmed.to_string())
}

fn bounded_text(value: &str, label: &str, max: usize) -> Result<String, String> {
    if value.chars().count() > max {
        return Err(format!("{label} exceeds the {max} character limit."));
    }
    Ok(value.trim().to_string())
}

fn normalized_style(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "2d" | "cutout" | "cutout-2d" => Ok("2d".to_string()),
        "2.5d" | "2_5d" | "2-5d" => Ok("2.5d".to_string()),
        "3d" => Ok("3d".to_string()),
        "mixed" | "hybrid" => Ok("mixed".to_string()),
        _ => Err("Story visualStyle must be 2d, 2.5d, 3d, or mixed.".to_string()),
    }
}

fn normalized_engine(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "godot" | "godot-2d" => Some("godot-2d"),
        "blender-grease-pencil" | "grease-pencil" => Some("blender-grease-pencil"),
        "blender-2.5d" | "blender-2_5d" | "blender-2-5d" => Some("blender-2.5d"),
        "blender-3d" | "blender" => Some("blender-3d"),
        "opentoonz" => Some("opentoonz"),
        "synfig" => Some("synfig"),
        "native-motion" | "native" => Some("native-motion"),
        _ => None,
    }
}

fn known_action(action: &str) -> bool {
    SIMPLE_2D_ACTIONS.contains(&action)
        || MOVEMENT_ACTIONS.contains(&action)
        || COMPLEX_INTERACTION_ACTIONS.contains(&action)
}

fn action_is_complex(action: &str) -> bool {
    COMPLEX_INTERACTION_ACTIONS.contains(&action) || !known_action(action)
}

fn action_requires_target(action: &str) -> bool {
    TARGET_REQUIRED_ACTIONS.contains(&action)
}

fn action_requires_movement(action: &str) -> bool {
    MOVEMENT_ACTIONS.contains(&action)
}

fn action_requires_hand_target(action: &str) -> bool {
    TWO_HAND_ACTIONS.contains(&action)
        || matches!(
            action,
            "carry" | "give" | "receive" | "open-door" | "pick-up" | "drop"
        )
}

fn validate_asset_path(
    workspace: &Workspace,
    project: &VideoProductionProject,
    relative: Option<&str>,
    label: &str,
) -> Result<Option<String>, String> {
    let Some(relative) = relative.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let resolved = video_production::resolve_project_path(
        workspace,
        project,
        relative,
        AccessOperation::Read,
        true,
    )?;
    if !resolved.is_file() {
        return Err(format!(
            "{label} must reference a regular file inside the Video Project."
        ));
    }
    Ok(Some(relative.to_string()))
}

fn validate_unique_lines(
    values: &mut [String],
    label: &str,
    max_items: usize,
) -> Result<(), String> {
    if values.len() > max_items {
        return Err(format!("{label} is limited to {max_items} entries."));
    }
    let mut seen = HashSet::new();
    for value in values.iter_mut() {
        let normalized = bounded_line(value, label, 120)?;
        if normalized.is_empty() {
            return Err(format!("{label} cannot contain empty entries."));
        }
        if !seen.insert(normalized.clone()) {
            return Err(format!("{label} contains duplicate entry '{normalized}'."));
        }
        *value = normalized;
    }
    Ok(())
}

fn validate_character(
    workspace: &Workspace,
    project: &VideoProductionProject,
    character: &mut StoryCharacter,
) -> Result<(), String> {
    character.id = safe_id(&character.id, "Character ID")?;
    character.name = bounded_line(&character.name, "Character name", 120)?;
    if character.name.is_empty() {
        return Err("Character name cannot be empty.".to_string());
    }
    character.role = bounded_line(&character.role, "Character role", 120)?;
    character.visual_description = bounded_text(
        &character.visual_description,
        "Character visual description",
        4_000,
    )?;
    character.rig_profile = bounded_line(&character.rig_profile, "Character rig profile", 80)?;
    character.costume = bounded_text(&character.costume, "Character costume", 2_000)?;
    validate_unique_lines(
        &mut character.continuity_tags,
        "Character continuity tags",
        64,
    )?;
    character.asset_path = validate_asset_path(
        workspace,
        project,
        character.asset_path.as_deref(),
        "Character asset",
    )?;
    Ok(())
}

fn validate_location(
    workspace: &Workspace,
    project: &VideoProductionProject,
    location: &mut StoryLocation,
) -> Result<(), String> {
    location.id = safe_id(&location.id, "Location ID")?;
    location.name = bounded_line(&location.name, "Location name", 120)?;
    if location.name.is_empty() {
        return Err("Location name cannot be empty.".to_string());
    }
    location.description = bounded_text(&location.description, "Location description", 6_000)?;
    validate_unique_lines(&mut location.variants, "Location variants", 64)?;
    validate_unique_lines(
        &mut location.entrance_anchors,
        "Location entrance anchors",
        128,
    )?;
    validate_unique_lines(
        &mut location.interaction_anchors,
        "Location interaction anchors",
        256,
    )?;
    validate_unique_lines(&mut location.camera_anchors, "Location camera anchors", 128)?;
    validate_unique_lines(&mut location.walkable_areas, "Location walkable areas", 128)?;
    location.asset_path = validate_asset_path(
        workspace,
        project,
        location.asset_path.as_deref(),
        "Location asset",
    )?;
    Ok(())
}

fn validate_prop(
    workspace: &Workspace,
    project: &VideoProductionProject,
    prop: &mut StoryProp,
    character_ids: &HashSet<String>,
) -> Result<(), String> {
    prop.id = safe_id(&prop.id, "Prop ID")?;
    prop.name = bounded_line(&prop.name, "Prop name", 120)?;
    if prop.name.is_empty() {
        return Err("Prop name cannot be empty.".to_string());
    }
    prop.description = bounded_text(&prop.description, "Prop description", 2_000)?;
    prop.handling = bounded_line(&prop.handling, "Prop handling", 80)?;
    if let Some(owner) = prop.owner_character_id.as_deref() {
        let owner = safe_id(owner, "Prop owner character ID")?;
        if !character_ids.contains(&owner) {
            return Err(format!(
                "Prop '{}' references unknown owner character '{owner}'.",
                prop.id
            ));
        }
        prop.owner_character_id = Some(owner);
    }
    prop.asset_path =
        validate_asset_path(workspace, project, prop.asset_path.as_deref(), "Prop asset")?;
    Ok(())
}

fn validate_voice_cast(
    entry: &mut StoryVoiceCastEntry,
    character_ids: &HashSet<String>,
) -> Result<(), String> {
    entry.character_id = safe_id(&entry.character_id, "Voice-cast character ID")?;
    if !character_ids.contains(&entry.character_id) {
        return Err(format!(
            "Voice cast references unknown character '{}'.",
            entry.character_id
        ));
    }
    entry.provider = bounded_line(&entry.provider, "Voice provider", 80)?;
    entry.voice = bounded_line(&entry.voice, "Voice ID", 160)?;
    entry.language = bounded_line(&entry.language, "Voice language", 64)?;
    if entry.provider.is_empty() || entry.voice.is_empty() || entry.language.is_empty() {
        return Err("Voice cast requires provider, voice, and language.".to_string());
    }
    if let Some(rate) = entry.rate {
        if !rate.is_finite() || !(0.5..=2.0).contains(&rate) {
            return Err("Voice cast rate must be between 0.5 and 2.0.".to_string());
        }
    }
    if let Some(path) = entry.voice_model_path.as_mut() {
        *path = bounded_line(path, "Voice model path", 4096)?;
    }
    Ok(())
}

fn validate_actor_beat(
    beat: &mut StoryActorBeat,
    character_ids: &HashSet<String>,
    prop_ids: &HashSet<String>,
) -> Result<(), String> {
    beat.character_id = safe_id(&beat.character_id, "Shot character ID")?;
    if !character_ids.contains(&beat.character_id) {
        return Err(format!(
            "Shot actor references unknown character '{}'.",
            beat.character_id
        ));
    }
    beat.action = bounded_line(&beat.action, "Actor action", 80)?
        .trim()
        .to_ascii_lowercase()
        .replace(['_', ' '], "-");
    if beat.action.is_empty() {
        beat.action = "idle".to_string();
    }
    beat.emotion = bounded_line(&beat.emotion, "Actor emotion", 80)?;
    if let Some(target) = beat.target_id.as_mut() {
        *target = safe_id(target, "Action target ID")?;
        if !character_ids.contains(target) && !prop_ids.contains(target) {
            return Err(format!(
                "Action '{}' for '{}' references unknown target '{}'.",
                beat.action, beat.character_id, target
            ));
        }
    }
    if let Some(value) = beat.start_anchor.as_mut() {
        *value = bounded_line(value, "Actor start anchor", 120)?;
    }
    if let Some(value) = beat.end_anchor.as_mut() {
        *value = bounded_line(value, "Actor end anchor", 120)?;
    }
    if let Some(value) = beat.look_target.as_mut() {
        *value = bounded_line(value, "Actor look target", 120)?;
    }
    if let Some(value) = beat.hand_target.as_mut() {
        *value = bounded_line(value, "Actor hand target", 120)?;
    }
    if let Some(dialogue) = beat.dialogue.as_mut() {
        *dialogue = bounded_text(dialogue, "Actor dialogue", 12_000)?;
    }
    Ok(())
}

fn validate_shot(
    shot: &mut StoryShotInput,
    character_ids: &HashSet<String>,
    location_map: &HashMap<String, StoryLocation>,
    prop_ids: &HashSet<String>,
) -> Result<(), String> {
    shot.id = safe_id(&shot.id, "Shot ID")?;
    shot.scene_id = safe_id(&shot.scene_id, "Shot scene ID")?;
    if !shot.duration_seconds.is_finite()
        || !(0.25..=MAX_SHOT_SECONDS).contains(&shot.duration_seconds)
    {
        return Err(format!(
            "Shot '{}' duration must be between 0.25 and {MAX_SHOT_SECONDS:.0} seconds.",
            shot.id
        ));
    }
    shot.location_id = safe_id(&shot.location_id, "Shot location ID")?;
    let location = location_map.get(&shot.location_id).ok_or_else(|| {
        format!(
            "Shot '{}' references unknown location '{}'.",
            shot.id, shot.location_id
        )
    })?;
    if let Some(variant) = shot.location_variant.as_mut() {
        *variant = bounded_line(variant, "Location variant", 120)?;
        if !variant.is_empty()
            && !location.variants.is_empty()
            && !location
                .variants
                .iter()
                .any(|candidate| candidate == variant)
        {
            return Err(format!(
                "Shot '{}' uses location variant '{}' that is not registered on location '{}'.",
                shot.id, variant, shot.location_id
            ));
        }
    }
    shot.camera.shot_type = bounded_line(&shot.camera.shot_type, "Camera shot type", 80)?
        .to_ascii_lowercase()
        .replace([' ', '_'], "-");
    shot.camera.movement = bounded_line(&shot.camera.movement, "Camera movement", 80)?
        .to_ascii_lowercase()
        .replace([' ', '_'], "-");
    shot.camera.angle = bounded_line(&shot.camera.angle, "Camera angle", 80)?.to_ascii_lowercase();
    shot.camera.framing = bounded_line(&shot.camera.framing, "Camera framing", 160)?;

    if shot.actors.len() > MAX_ACTORS_PER_SHOT {
        return Err(format!(
            "Shot '{}' exceeds the {MAX_ACTORS_PER_SHOT} actor limit.",
            shot.id
        ));
    }
    let mut actors = HashSet::new();
    for beat in &mut shot.actors {
        validate_actor_beat(beat, character_ids, prop_ids)?;
        if !actors.insert(beat.character_id.clone()) {
            return Err(format!(
                "Shot '{}' contains more than one actor beat for character '{}'.",
                shot.id, beat.character_id
            ));
        }
    }

    if shot.props.len() > 64 {
        return Err(format!("Shot '{}' is limited to 64 props.", shot.id));
    }
    let mut seen_props = HashSet::new();
    for prop in &mut shot.props {
        *prop = safe_id(prop, "Shot prop ID")?;
        if !prop_ids.contains(prop) {
            return Err(format!(
                "Shot '{}' references unknown prop '{}'.",
                shot.id, prop
            ));
        }
        if !seen_props.insert(prop.clone()) {
            return Err(format!("Shot '{}' repeats prop '{}'.", shot.id, prop));
        }
    }
    validate_unique_lines(&mut shot.ambience, "Shot ambience", 64)?;
    validate_unique_lines(&mut shot.foley, "Shot foley", 128)?;
    if let Some(engine) = shot.requested_engine.as_mut() {
        let normalized = normalized_engine(engine).ok_or_else(|| {
            format!(
                "Shot '{}' requested unsupported engine '{}'.",
                shot.id, engine
            )
        })?;
        *engine = normalized.to_string();
    }
    if let Some(transition) = shot.transition.as_mut() {
        *transition = bounded_line(transition, "Shot transition", 80)?;
    }
    shot.notes = bounded_text(&shot.notes, "Shot notes", 4_000)?;
    Ok(())
}

fn validate_and_normalize_input(
    workspace: &Workspace,
    project: &VideoProductionProject,
    mut input: StoryDirectorInput,
) -> Result<StoryDirectorInput, String> {
    if project.production_mode != "story" {
        return Err(
            "Narrative Scene Director is available only for Video Projects created in story mode. Existing tutorial projects remain on the tutorial pipeline."
                .to_string(),
        );
    }
    if input.characters.len() > MAX_CHARACTERS {
        return Err(format!(
            "Story projects support at most {MAX_CHARACTERS} characters."
        ));
    }
    if input.locations.is_empty() || input.locations.len() > MAX_LOCATIONS {
        return Err(format!(
            "Story projects require at least one location and support at most {MAX_LOCATIONS}."
        ));
    }
    if input.props.len() > MAX_PROPS {
        return Err(format!("Story projects support at most {MAX_PROPS} props."));
    }
    if input.shots.is_empty() || input.shots.len() > MAX_SHOTS {
        return Err(format!(
            "Story projects require at least one shot and support at most {MAX_SHOTS}."
        ));
    }

    input.language = bounded_line(&input.language, "Story language", 64)?;
    if input.language.is_empty() {
        return Err("Story language cannot be empty.".to_string());
    }
    input.visual_style = normalized_style(&input.visual_style)?;

    let mut character_ids = HashSet::new();
    for character in &mut input.characters {
        validate_character(workspace, project, character)?;
        if !character_ids.insert(character.id.clone()) {
            return Err(format!("Character ID '{}' is duplicated.", character.id));
        }
    }

    let mut location_ids = HashSet::new();
    for location in &mut input.locations {
        validate_location(workspace, project, location)?;
        if !location_ids.insert(location.id.clone()) {
            return Err(format!("Location ID '{}' is duplicated.", location.id));
        }
    }
    let location_map = input
        .locations
        .iter()
        .cloned()
        .map(|location| (location.id.clone(), location))
        .collect::<HashMap<_, _>>();

    let mut prop_ids = HashSet::new();
    for prop in &mut input.props {
        validate_prop(workspace, project, prop, &character_ids)?;
        if !prop_ids.insert(prop.id.clone()) {
            return Err(format!("Prop ID '{}' is duplicated.", prop.id));
        }
    }

    let mut voiced = HashSet::new();
    for entry in &mut input.voice_cast {
        validate_voice_cast(entry, &character_ids)?;
        if !voiced.insert(entry.character_id.clone()) {
            return Err(format!(
                "Voice cast has more than one assignment for character '{}'.",
                entry.character_id
            ));
        }
    }

    let mut shot_ids = HashSet::new();
    let mut orders = BTreeSet::new();
    let mut total = 0.0;
    for shot in &mut input.shots {
        validate_shot(shot, &character_ids, &location_map, &prop_ids)?;
        if !shot_ids.insert(shot.id.clone()) {
            return Err(format!("Shot ID '{}' is duplicated.", shot.id));
        }
        if !orders.insert(shot.order) {
            return Err(format!("Shot order {} is duplicated.", shot.order));
        }
        total += shot.duration_seconds;
    }
    if total > MAX_TOTAL_SECONDS {
        return Err("Story shot plan exceeds the 8-hour safety limit.".to_string());
    }
    input.shots.sort_by_key(|shot| shot.order);
    Ok(input)
}

fn choose_engine(style: &str, shot: &StoryShotInput) -> (String, String) {
    if let Some(requested) = shot.requested_engine.as_deref() {
        return (
            requested.to_string(),
            "Explicitly requested by the Scene Director input.".to_string(),
        );
    }

    let has_complex_action = shot
        .actors
        .iter()
        .any(|beat| action_is_complex(&beat.action));
    let has_movement = shot
        .actors
        .iter()
        .any(|beat| action_requires_movement(&beat.action));
    let complex_camera = COMPLEX_CAMERA_MOVES.contains(&shot.camera.movement.as_str());

    if style == "3d" {
        return (
            "blender-3d".to_string(),
            "The project explicitly requests a 3D visual style.".to_string(),
        );
    }
    if has_complex_action || complex_camera {
        return (
            "blender-2.5d".to_string(),
            "The shot needs complex character/object interaction or camera movement.".to_string(),
        );
    }
    if style == "2.5d" {
        return (
            "blender-grease-pencil".to_string(),
            "The project uses a 2.5D visual style and the shot does not require full 3D interaction."
                .to_string(),
        );
    }
    if shot.actors.is_empty() {
        return (
            "native-motion".to_string(),
            "The shot has no character acting and can use the lightweight native motion renderer."
                .to_string(),
        );
    }
    if has_movement || style == "2d" || style == "mixed" {
        return (
            "godot-2d".to_string(),
            "The shot is suitable for fast 2D skeletal/cutout character animation.".to_string(),
        );
    }
    (
        "blender-grease-pencil".to_string(),
        "Grease Pencil is the safe story-animation fallback for this shot.".to_string(),
    )
}

fn choose_available_engine(
    style: &str,
    shot: &StoryShotInput,
    preferred_engine: String,
    preferred_reason: String,
    capabilities: &StoryProductionCapabilities,
) -> (String, String) {
    if shot.requested_engine.is_some() {
        return (preferred_engine, preferred_reason);
    }
    let available = |engine: &str| {
        capabilities
            .engines
            .iter()
            .any(|item| item.id == engine && item.available)
    };
    if available(&preferred_engine) {
        return (preferred_engine, preferred_reason);
    }

    let complex = shot
        .actors
        .iter()
        .any(|beat| action_is_complex(&beat.action))
        || COMPLEX_CAMERA_MOVES.contains(&shot.camera.movement.as_str());

    if preferred_engine == "godot-2d" && available("blender-grease-pencil") {
        return (
            "blender-grease-pencil".to_string(),
            "Godot 4 is unavailable, so RepoTunnel selected the installed Blender story adapter for this otherwise-simple character shot.".to_string(),
        );
    }

    if preferred_engine.starts_with("blender-")
        && !complex
        && matches!(style, "2d" | "mixed")
        && available("godot-2d")
    {
        return (
            "godot-2d".to_string(),
            "The preferred Blender adapter is unavailable, so RepoTunnel selected the installed Godot 4 adapter for this compatible 2D shot.".to_string(),
        );
    }

    (preferred_engine, preferred_reason)
}

fn shot_render_key(
    input: &StoryDirectorInput,
    shot: &StoryShotInput,
    selected_engine: &str,
) -> Result<String, String> {
    let character_ids = shot
        .actors
        .iter()
        .map(|actor| actor.character_id.as_str())
        .collect::<HashSet<_>>();
    let prop_ids = shot
        .props
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();

    let relevant_characters = input
        .characters
        .iter()
        .filter(|character| character_ids.contains(character.id.as_str()))
        .collect::<Vec<_>>();
    let relevant_props = input
        .props
        .iter()
        .filter(|prop| prop_ids.contains(prop.id.as_str()))
        .collect::<Vec<_>>();
    let location = input
        .locations
        .iter()
        .find(|location| location.id == shot.location_id);

    let payload = serde_json::to_vec(&serde_json::json!({
        "shot": shot,
        "engine": selected_engine,
        "visualStyle": input.visual_style,
        "characters": relevant_characters,
        "props": relevant_props,
        "location": location,
    }))
    .map_err(|error| format!("Could not hash story shot: {error}"))?;
    let digest = Sha256::digest(payload);
    Ok(digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

fn director_hash(plan: &StoryDirectorPlan) -> Result<String, String> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "version": plan.version,
        "language": plan.language,
        "visualStyle": plan.visual_style,
        "characters": plan.characters,
        "locations": plan.locations,
        "props": plan.props,
        "voiceCast": plan.voice_cast,
        "shots": plan.shots,
    }))
    .map_err(|error| format!("Could not hash story director plan: {error}"))?;
    let digest = Sha256::digest(payload);
    Ok(digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve story plan parent folder.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create story plan folder: {error}"))?;
    let temporary = parent.join(format!(
        ".repotunnel-story-write-{}-{}.tmp",
        std::process::id(),
        now_millis()
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| format!("Could not create temporary story plan: {error}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Could not persist story plan: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("Could not finalize story plan: {error}"))
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Could not serialize story data: {error}"))?;
    write_atomic(path, &bytes)
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T, String> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("Could not read {label}: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("{label} is invalid: {error}"))
}

fn director_path(
    workspace: &Workspace,
    project: &VideoProductionProject,
    relative: &str,
    operation: AccessOperation,
    must_exist: bool,
) -> Result<PathBuf, String> {
    video_production::resolve_project_path(
        workspace,
        project,
        &format!("{}/{}", project.relative_path, relative),
        operation,
        must_exist,
    )
}

fn output_is_reusable(
    workspace: &Workspace,
    project: &VideoProductionProject,
    output_path: Option<&str>,
) -> bool {
    let Some(output_path) = output_path.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    video_production::resolve_project_path(
        workspace,
        project,
        output_path,
        AccessOperation::Read,
        true,
    )
    .ok()
    .and_then(|path| fs::symlink_metadata(path).ok())
    .is_some_and(|metadata| {
        !metadata.file_type().is_symlink() && metadata.is_file() && metadata.len() > 0
    })
}

fn build_render_queue(
    workspace: &Workspace,
    project: &VideoProductionProject,
    plan: &StoryDirectorPlan,
) -> Result<StoryRenderQueue, String> {
    let cache_path = director_path(
        workspace,
        project,
        CACHE_FILE,
        AccessOperation::Write,
        false,
    )?;
    let previous = if cache_path.is_file() {
        load_json::<StoryRenderQueue>(&cache_path, "story render cache").ok()
    } else {
        None
    };
    let previous_by_shot = previous
        .as_ref()
        .map(|queue| {
            queue
                .entries
                .iter()
                .map(|entry| (entry.shot_id.as_str(), entry))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    let now = now_millis();
    let mut changed_shot_ids = Vec::new();
    let mut reusable_shot_ids = Vec::new();
    let mut entries = Vec::with_capacity(plan.shots.len());

    for shot in &plan.shots {
        let reusable = previous_by_shot
            .get(shot.shot.id.as_str())
            .copied()
            .filter(|entry| {
                entry.render_key == shot.render_key
                    && entry.selected_engine == shot.selected_engine
                    && entry.status == "ready"
                    && output_is_reusable(workspace, project, entry.output_path.as_deref())
            });

        if let Some(previous) = reusable {
            reusable_shot_ids.push(shot.shot.id.clone());
            entries.push(StoryShotCacheEntry {
                shot_id: shot.shot.id.clone(),
                render_key: shot.render_key.clone(),
                selected_engine: shot.selected_engine.clone(),
                status: "ready".to_string(),
                output_path: previous.output_path.clone(),
                updated_at: previous.updated_at,
            });
        } else {
            changed_shot_ids.push(shot.shot.id.clone());
            entries.push(StoryShotCacheEntry {
                shot_id: shot.shot.id.clone(),
                render_key: shot.render_key.clone(),
                selected_engine: shot.selected_engine.clone(),
                status: "pending".to_string(),
                output_path: None,
                updated_at: now,
            });
        }
    }

    let queue = StoryRenderQueue {
        version: 1,
        project_id: project.id.clone(),
        content_hash: plan.content_hash.clone(),
        changed_shot_ids,
        reusable_shot_ids,
        entries,
        updated_at: now,
    };
    save_json(&cache_path, &queue)?;
    Ok(queue)
}

fn qa_issue(
    severity: &str,
    code: &str,
    shot_id: Option<&str>,
    character_id: Option<&str>,
    message: impl Into<String>,
) -> NarrativeQaIssue {
    NarrativeQaIssue {
        severity: severity.to_string(),
        code: code.to_string(),
        shot_id: shot_id.map(str::to_string),
        character_id: character_id.map(str::to_string),
        message: message.into(),
    }
}

pub(crate) fn action_capabilities() -> Vec<StoryActionCapability> {
    let mut actions = BTreeSet::new();
    for action in SIMPLE_2D_ACTIONS
        .iter()
        .chain(MOVEMENT_ACTIONS)
        .chain(COMPLEX_INTERACTION_ACTIONS)
    {
        actions.insert(*action);
    }
    actions
        .into_iter()
        .map(|action| StoryActionCapability {
            action: action.to_string(),
            complexity: if SIMPLE_2D_ACTIONS.contains(&action) {
                "simple-2d"
            } else if MOVEMENT_ACTIONS.contains(&action) {
                "movement"
            } else {
                "interaction"
            }
            .to_string(),
            requires_target: action_requires_target(action),
            requires_movement_anchors: action_requires_movement(action),
            requires_hand_target: action_requires_hand_target(action),
        })
        .collect()
}

fn find_on_path(names: &[&str]) -> Option<String> {
    let paths = env::var_os("PATH")?;
    for directory in env::split_paths(&paths) {
        for name in names {
            let candidate = directory.join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                (*name).to_string()
            });
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn find_godot4() -> Option<String> {
    for candidate in [find_on_path(&["godot4"]), find_on_path(&["godot"])]
        .into_iter()
        .flatten()
    {
        let output = Command::new(&candidate).arg("--version").output().ok()?;
        let text = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr).into_owned()
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        if output.status.success() && text.trim_start().starts_with('4') {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn production_capabilities() -> StoryProductionCapabilities {
    let godot4 = find_godot4();
    let engines = vec![
        StoryEngineCapability {
            id: "native-motion".to_string(),
            available: true,
            executable: None,
            best_for: "Technical graphics, transitions, inserts, and actor-free shots.".to_string(),
            notes: "Existing deterministic RepoTunnel renderer; not used for character acting."
                .to_string(),
        },
        StoryEngineCapability {
            id: "godot-2d".to_string(),
            available: godot4.is_some(),
            executable: godot4.clone(),
            best_for: "Fast 2D cutout/skeletal dialogue, walking, and reusable sets.".to_string(),
            notes: "Automatic story rendering requires Godot 4; older Godot versions are not treated as a compatible adapter.".to_string(),
        },
        StoryEngineCapability {
            id: "blender-grease-pencil".to_string(),
            available: find_on_path(&["blender"]).is_some(),
            executable: find_on_path(&["blender"]),
            best_for: "2D/2.5D Grease Pencil shots and richer camera staging.".to_string(),
            notes: "Shares the Blender executable with 2.5D/3D routing.".to_string(),
        },
        StoryEngineCapability {
            id: "blender-2.5d".to_string(),
            available: find_on_path(&["blender"]).is_some(),
            executable: find_on_path(&["blender"]),
            best_for: "Object interaction, doors, digging, carrying, IK, and camera moves."
                .to_string(),
            notes: "Preferred route for complex story interaction.".to_string(),
        },
        StoryEngineCapability {
            id: "blender-3d".to_string(),
            available: find_on_path(&["blender"]).is_some(),
            executable: find_on_path(&["blender"]),
            best_for: "Shots that explicitly benefit from full 3D.".to_string(),
            notes: "Not the default story route.".to_string(),
        },
        StoryEngineCapability {
            id: "opentoonz".to_string(),
            available: find_on_path(&["opentoonz"]).is_some(),
            executable: find_on_path(&["opentoonz"]),
            best_for: "Optional traditional/cutout 2D production.".to_string(),
            notes: "Optional external adapter. RepoTunnel does not synthesize OpenToonz scene files automatically; render through OpenToonz and register the exact current shot output with record_video_story_shot_render.".to_string(),
        },
        StoryEngineCapability {
            id: "synfig".to_string(),
            available: find_on_path(&["synfigstudio", "synfig"]).is_some(),
            executable: find_on_path(&["synfigstudio", "synfig"]),
            best_for: "Optional vector puppet/tween production.".to_string(),
            notes: "Optional external adapter. RepoTunnel does not synthesize Synfig scene files automatically; render through Synfig and register the exact current shot output with record_video_story_shot_render.".to_string(),
        },
        StoryEngineCapability {
            id: "rhubarb-lip-sync".to_string(),
            available: find_on_path(&["rhubarb"]).is_some(),
            executable: find_on_path(&["rhubarb"]),
            best_for: "Automatic mouth-shape timing for dialogue.".to_string(),
            notes:
                "Optional lip-sync helper; dialogue remains valid without automatic installation."
                    .to_string(),
        },
    ];

    StoryProductionCapabilities {
        actions: action_capabilities(),
        engines,
        animatic_width: 854,
        animatic_height: 480,
        animatic_fps: 12,
    }
}

pub(crate) fn compile_plan(
    workspace: &Workspace,
    project_id: &str,
    input: StoryDirectorInput,
) -> Result<StoryDirectorPlan, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let input = validate_and_normalize_input(workspace, &project, input)?;
    let now = now_millis();

    let capabilities = production_capabilities();
    let mut compiled = Vec::with_capacity(input.shots.len());
    for shot in &input.shots {
        let (preferred_engine, preferred_reason) = choose_engine(&input.visual_style, shot);
        let (selected_engine, engine_reason) = choose_available_engine(
            &input.visual_style,
            shot,
            preferred_engine,
            preferred_reason,
            &capabilities,
        );
        let render_key = shot_render_key(&input, shot, &selected_engine)?;
        compiled.push(StoryCompiledShot {
            shot: shot.clone(),
            selected_engine,
            engine_reason,
            render_key,
        });
    }

    let mut plan = StoryDirectorPlan {
        version: DIRECTOR_VERSION,
        action_library_version: ACTION_LIBRARY_VERSION,
        project_id: project.id.clone(),
        language: input.language,
        visual_style: input.visual_style,
        characters: input.characters,
        locations: input.locations,
        props: input.props,
        voice_cast: input.voice_cast,
        shots: compiled,
        content_hash: String::new(),
        updated_at: now,
    };
    plan.content_hash = director_hash(&plan)?;

    let path = director_path(
        workspace,
        &project,
        DIRECTOR_FILE,
        AccessOperation::Write,
        false,
    )?;
    save_json(&path, &plan)?;

    let animatic = animatic_plan_from(&plan);
    let animatic_path = director_path(
        workspace,
        &project,
        ANIMATIC_FILE,
        AccessOperation::Write,
        false,
    )?;
    save_json(&animatic_path, &animatic)?;

    let _render_queue = build_render_queue(workspace, &project, &plan)?;

    let report = qa_plan(&plan);
    let qa_path = director_path(
        workspace,
        &project,
        NARRATIVE_QA_FILE,
        AccessOperation::Write,
        false,
    )?;
    save_json(&qa_path, &report)?;

    let _ = video_production::update_project_status(
        workspace,
        project_id,
        "planning",
        Some(
            "Story Scene Director compiled shots, engine routes, animatic plan, and narrative QA.",
        ),
    );

    Ok(plan)
}

pub(crate) fn get_plan(
    workspace: &Workspace,
    project_id: &str,
) -> Result<StoryDirectorPlan, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        return Err("This Video Project uses the tutorial pipeline, not story mode.".to_string());
    }
    let path = director_path(
        workspace,
        &project,
        DIRECTOR_FILE,
        AccessOperation::Read,
        true,
    )?;
    load_json(&path, "story director plan")
}

pub(crate) fn get_animatic_plan(
    workspace: &Workspace,
    project_id: &str,
) -> Result<StoryAnimaticPlan, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let path = director_path(
        workspace,
        &project,
        ANIMATIC_FILE,
        AccessOperation::Read,
        true,
    )?;
    load_json(&path, "story animatic plan")
}

pub(crate) fn get_render_queue(
    workspace: &Workspace,
    project_id: &str,
) -> Result<StoryRenderQueue, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        return Err("This Video Project uses the tutorial pipeline, not story mode.".to_string());
    }
    let path = director_path(workspace, &project, CACHE_FILE, AccessOperation::Read, true)?;
    load_json(&path, "story render cache")
}

pub(crate) fn record_shot_render(
    workspace: &Workspace,
    project_id: &str,
    input: StoryShotRenderInput,
) -> Result<StoryRenderQueue, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        return Err(
            "Story shot outputs can be recorded only for story-mode Video Projects.".to_string(),
        );
    }

    let shot_id = safe_id(&input.shot_id, "Shot ID")?;
    let plan = get_plan(workspace, project_id)?;
    let compiled = plan
        .shots
        .iter()
        .find(|shot| shot.shot.id == shot_id)
        .ok_or_else(|| {
            format!("Story shot '{shot_id}' is not present in the current Scene Director plan.")
        })?;
    if input.render_key.trim() != compiled.render_key {
        return Err(format!(
            "Story shot '{shot_id}' render key is stale. Re-render this shot from the current Scene Director plan instead of registering an older output."
        ));
    }

    let output_path = input.output_path.trim();
    if output_path.is_empty() {
        return Err("Story shot outputPath cannot be empty.".to_string());
    }
    let resolved = video_production::resolve_project_path(
        workspace,
        &project,
        output_path,
        AccessOperation::Read,
        true,
    )?;
    let metadata = fs::symlink_metadata(&resolved)
        .map_err(|error| format!("Could not inspect story shot output: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(
            "Story shot output must be a non-empty regular file inside the Video Project."
                .to_string(),
        );
    }

    let mut queue = get_render_queue(workspace, project_id)?;
    let entry = queue
        .entries
        .iter_mut()
        .find(|entry| entry.shot_id == shot_id)
        .ok_or_else(|| format!("Story render queue does not contain shot '{shot_id}'."))?;
    if entry.render_key != compiled.render_key || entry.selected_engine != compiled.selected_engine
    {
        return Err(format!(
            "Story render queue for '{shot_id}' is stale. Recompile the Scene Director plan first."
        ));
    }

    let now = now_millis();
    entry.status = "ready".to_string();
    entry.output_path = Some(output_path.to_string());
    entry.updated_at = now;
    queue.changed_shot_ids.retain(|id| id != &shot_id);
    if !queue.reusable_shot_ids.iter().any(|id| id == &shot_id) {
        queue.reusable_shot_ids.push(shot_id.clone());
        queue.reusable_shot_ids.sort();
    }
    queue.updated_at = now;

    let cache_path = director_path(
        workspace,
        &project,
        CACHE_FILE,
        AccessOperation::Write,
        false,
    )?;
    save_json(&cache_path, &queue)?;

    let prefix = format!("{}/", project.relative_path);
    let inside = output_path
        .strip_prefix(&prefix)
        .ok_or_else(|| "Story shot output path escaped the Video Project.".to_string())?;
    video_production::register_asset(
        workspace,
        project_id,
        "story-shot",
        inside,
        Some(&format!("Story shot {shot_id}")),
    )?;

    Ok(queue)
}

pub(crate) fn verify_render_queue_complete(
    workspace: &Workspace,
    project_id: &str,
) -> Result<(usize, usize), String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        return Err(
            "Render-queue verification is available only for story-mode Video Projects."
                .to_string(),
        );
    }
    let plan = get_plan(workspace, project_id)?;
    let queue = get_render_queue(workspace, project_id)?;
    if queue.content_hash != plan.content_hash {
        return Err(
            "Story render queue does not match the current Scene Director plan. Recompile before final QA."
                .to_string(),
        );
    }

    let mut ready = 0usize;
    for shot in &plan.shots {
        let entry = queue
            .entries
            .iter()
            .find(|entry| entry.shot_id == shot.shot.id)
            .ok_or_else(|| {
                format!(
                    "Story render queue is missing current shot '{}'.",
                    shot.shot.id
                )
            })?;
        if entry.render_key != shot.render_key
            || entry.selected_engine != shot.selected_engine
            || entry.status != "ready"
        {
            return Err(format!(
                "Story shot '{}' is dirty or stale and must be rendered from the current Scene Director plan.",
                shot.shot.id
            ));
        }
        if !output_is_reusable(workspace, &project, entry.output_path.as_deref()) {
            return Err(format!(
                "Story shot '{}' has no current non-empty project-owned render output.",
                shot.shot.id
            ));
        }
        ready += 1;
    }

    if !queue.changed_shot_ids.is_empty() {
        return Err(format!(
            "{} story shot(s) are still marked changed and require rendering.",
            queue.changed_shot_ids.len()
        ));
    }
    Ok((ready, plan.shots.len()))
}

pub(crate) fn get_narrative_qa(
    workspace: &Workspace,
    project_id: &str,
) -> Result<NarrativeQaReport, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let path = director_path(
        workspace,
        &project,
        NARRATIVE_QA_FILE,
        AccessOperation::Read,
        true,
    )?;
    load_json(&path, "story narrative QA report")
}

pub(crate) fn resolve_voice_cast(
    workspace: &Workspace,
    project_id: &str,
    character_id: &str,
) -> Result<Option<StoryVoiceCastEntry>, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        return Ok(None);
    }
    let plan = get_plan(workspace, project_id)?;
    let character_id = safe_id(character_id, "Character ID")?;
    Ok(plan
        .voice_cast
        .into_iter()
        .find(|entry| entry.character_id == character_id))
}

fn animatic_plan_from(plan: &StoryDirectorPlan) -> StoryAnimaticPlan {
    let mut cursor = 0.0;
    let mut shots = Vec::with_capacity(plan.shots.len());
    for shot in &plan.shots {
        let start = cursor;
        cursor += shot.shot.duration_seconds;
        shots.push(StoryAnimaticShot {
            shot_id: shot.shot.id.clone(),
            scene_id: shot.shot.scene_id.clone(),
            order: shot.shot.order,
            start_seconds: start,
            end_seconds: cursor,
            duration_seconds: shot.shot.duration_seconds,
            selected_engine: shot.selected_engine.clone(),
            render_key: shot.render_key.clone(),
        });
    }
    StoryAnimaticPlan {
        version: 1,
        project_id: plan.project_id.clone(),
        width: 854,
        height: 480,
        fps: 12,
        total_duration_seconds: cursor,
        content_hash: plan.content_hash.clone(),
        shots,
        updated_at: plan.updated_at,
    }
}

pub(crate) fn qa_plan(plan: &StoryDirectorPlan) -> NarrativeQaReport {
    let mut issues = Vec::new();
    let voice_by_character = plan
        .voice_cast
        .iter()
        .map(|entry| (entry.character_id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let prop_ids = plan
        .props
        .iter()
        .map(|prop| prop.id.as_str())
        .collect::<HashSet<_>>();
    let character_ids = plan
        .characters
        .iter()
        .map(|character| character.id.as_str())
        .collect::<HashSet<_>>();

    let mut camera_types = HashSet::new();
    let mut idle_beats = 0usize;
    let mut actor_beats = 0usize;
    let mut complex_count = 0usize;
    let mut total_duration = 0.0;
    let mut voice_users = BTreeMap::<String, Vec<String>>::new();
    let mut previous_by_scene = HashMap::<String, (String, String)>::new();

    for entry in &plan.voice_cast {
        voice_users
            .entry(format!("{}::{}", entry.provider, entry.voice))
            .or_default()
            .push(entry.character_id.clone());
    }

    for character in &plan.characters {
        if character.asset_path.is_none() {
            issues.push(qa_issue(
                "warning",
                "character-uses-procedural-fallback",
                None,
                Some(&character.id),
                format!(
                    "Character '{}' has no registered render asset; Blender will use the articulated procedural fallback instead of a reusable rigged character.",
                    character.name
                ),
            ));
        }
    }
    for location in &plan.locations {
        if location.asset_path.is_none() {
            issues.push(qa_issue(
                "warning",
                "location-uses-procedural-fallback",
                None,
                None,
                format!(
                    "Location '{}' has no registered set asset; Blender will use a procedural floor instead of a reusable set.",
                    location.name
                ),
            ));
        }
    }
    for prop in &plan.props {
        if prop.asset_path.is_none() {
            issues.push(qa_issue(
                "warning",
                "prop-uses-procedural-fallback",
                None,
                None,
                format!(
                    "Prop '{}' has no registered render asset; Blender will use a procedural proxy if the prop appears in a shot.",
                    prop.name
                ),
            ));
        }
    }

    for shot in &plan.shots {
        let shot_id = shot.shot.id.as_str();
        total_duration += shot.shot.duration_seconds;
        camera_types.insert(shot.shot.camera.shot_type.clone());

        if shot.selected_engine == "native-motion" && !shot.shot.actors.is_empty() {
            issues.push(qa_issue(
                "error",
                "native-character-acting",
                Some(shot_id),
                None,
                "Native motion is reserved for actor-free technical/transition shots; route character acting to a story engine.",
            ));
        }

        if let Some((previous_location, previous_shot)) = previous_by_scene.get(&shot.shot.scene_id)
        {
            if previous_location != &shot.shot.location_id
                && shot
                    .shot
                    .transition
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
            {
                issues.push(qa_issue(
                    "warning",
                    "location-cut-without-transition",
                    Some(shot_id),
                    None,
                    format!(
                        "Scene '{}' changes location from '{}' in shot '{}' to '{}' without an explicit transition.",
                        shot.shot.scene_id, previous_location, previous_shot, shot.shot.location_id
                    ),
                ));
            }
        }
        previous_by_scene.insert(
            shot.shot.scene_id.clone(),
            (shot.shot.location_id.clone(), shot.shot.id.clone()),
        );

        for actor in &shot.shot.actors {
            actor_beats += 1;
            let dialogue = actor
                .dialogue
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if actor.action == "idle" && dialogue.is_none() {
                idle_beats += 1;
            }
            if action_is_complex(&actor.action) {
                complex_count += 1;
            }
            if dialogue.is_some() {
                if !voice_by_character.contains_key(actor.character_id.as_str()) {
                    issues.push(qa_issue(
                        "error",
                        "dialogue-missing-voice-cast",
                        Some(shot_id),
                        Some(&actor.character_id),
                        "Speaking character has no persistent voice-cast assignment.",
                    ));
                }
                if !actor.lip_sync {
                    issues.push(qa_issue(
                        "error",
                        "dialogue-lip-sync-disabled",
                        Some(shot_id),
                        Some(&actor.character_id),
                        "Speaking character has lip sync disabled.",
                    ));
                }
            }
            if action_requires_movement(&actor.action)
                && (actor
                    .start_anchor
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                    || actor.end_anchor.as_deref().unwrap_or("").trim().is_empty())
            {
                issues.push(qa_issue(
                    "error",
                    "movement-missing-anchors",
                    Some(shot_id),
                    Some(&actor.character_id),
                    format!(
                        "Action '{}' requires explicit startAnchor and endAnchor to prevent sliding/teleporting.",
                        actor.action
                    ),
                ));
            }
            if action_requires_target(&actor.action)
                && actor.target_id.as_deref().unwrap_or("").trim().is_empty()
            {
                issues.push(qa_issue(
                    "error",
                    "interaction-missing-target",
                    Some(shot_id),
                    Some(&actor.character_id),
                    format!(
                        "Action '{}' requires a target object or character.",
                        actor.action
                    ),
                ));
            }
            if action_requires_hand_target(&actor.action)
                && actor.hand_target.as_deref().unwrap_or("").trim().is_empty()
            {
                issues.push(qa_issue(
                    "error",
                    "interaction-missing-hand-target",
                    Some(shot_id),
                    Some(&actor.character_id),
                    format!(
                        "Action '{}' requires handTarget so hands stay attached to the interacted object.",
                        actor.action
                    ),
                ));
            }
            if let Some(target) = actor.target_id.as_deref() {
                if !prop_ids.contains(target) && !character_ids.contains(target) {
                    issues.push(qa_issue(
                        "error",
                        "unknown-interaction-target",
                        Some(shot_id),
                        Some(&actor.character_id),
                        format!(
                            "Interaction target '{target}' is not a registered prop or character."
                        ),
                    ));
                }
            }
        }

        if shot
            .shot
            .actors
            .iter()
            .any(|actor| action_is_complex(&actor.action))
            && shot.shot.foley.is_empty()
        {
            issues.push(qa_issue(
                "warning",
                "complex-action-no-foley",
                Some(shot_id),
                None,
                "Complex physical action has no planned Foley cues.",
            ));
        }
        if shot.shot.ambience.is_empty() {
            issues.push(qa_issue(
                "warning",
                "shot-no-ambience",
                Some(shot_id),
                None,
                "Story shot has no ambience cue; location sound will feel empty.",
            ));
        }
    }

    if plan.shots.len() >= 5 && camera_types.len() < 2 {
        issues.push(qa_issue(
            "warning",
            "low-shot-variety",
            None,
            None,
            "Five or more shots use fewer than two shot types; add close/medium/wide/insert variation.",
        ));
    }

    let idle_ratio = if actor_beats == 0 {
        0.0
    } else {
        idle_beats as f64 / actor_beats as f64
    };
    if actor_beats >= 4 && idle_ratio > 0.40 {
        issues.push(qa_issue(
            "warning",
            "high-idle-character-ratio",
            None,
            None,
            format!(
                "{:.0}% of actor beats are idle with no dialogue; add purposeful staging or reaction beats.",
                idle_ratio * 100.0
            ),
        ));
    }

    for (voice_key, characters) in voice_users {
        if characters.len() > 1 {
            issues.push(qa_issue(
                "warning",
                "shared-character-voice",
                None,
                None,
                format!(
                    "Voice '{}' is shared by characters {}. Keep this deliberate; principal characters should use distinct voices where available.",
                    voice_key,
                    characters.join(", ")
                ),
            ));
        }
    }

    let passed = !issues.iter().any(|issue| issue.severity == "error");
    NarrativeQaReport {
        version: 1,
        project_id: plan.project_id.clone(),
        passed,
        issues,
        metrics: NarrativeQaMetrics {
            shot_count: plan.shots.len(),
            character_count: plan.characters.len(),
            location_count: plan.locations.len(),
            prop_count: plan.props.len(),
            voiced_character_count: plan.voice_cast.len(),
            camera_shot_type_count: camera_types.len(),
            idle_actor_ratio: idle_ratio,
            complex_interaction_count: complex_count,
            total_duration_seconds: total_duration,
        },
        updated_at: now_millis(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        access::AccessOperation,
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
        video_production,
    };

    use super::{
        action_capabilities, choose_available_engine, choose_engine, compile_plan,
        get_render_queue, qa_plan, record_shot_render, verify_render_queue_complete,
        StoryActorBeat, StoryCamera, StoryCompiledShot, StoryDirectorInput, StoryDirectorPlan,
        StoryEngineCapability, StoryLocation, StoryProductionCapabilities, StoryShotInput,
        StoryShotRenderInput,
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> (std::path::PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repotunnel-video-director-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: format!("story-test-{counter}"),
            name: "Story director test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    fn shot(action: &str, dialogue: Option<&str>) -> StoryShotInput {
        StoryShotInput {
            id: "shot-1".to_string(),
            scene_id: "scene-1".to_string(),
            order: 1,
            duration_seconds: 4.0,
            location_id: "room".to_string(),
            location_variant: None,
            camera: StoryCamera {
                shot_type: "medium".to_string(),
                movement: "static".to_string(),
                angle: "eye-level".to_string(),
                framing: String::new(),
            },
            actors: vec![StoryActorBeat {
                character_id: "hero".to_string(),
                action: action.to_string(),
                target_id: None,
                start_anchor: None,
                end_anchor: None,
                emotion: String::new(),
                look_target: None,
                dialogue: dialogue.map(str::to_string),
                lip_sync: dialogue.is_some(),
                hand_target: None,
            }],
            props: vec![],
            ambience: vec!["room-tone".to_string()],
            foley: vec![],
            requested_engine: None,
            transition: None,
            notes: String::new(),
        }
    }

    #[test]
    fn complex_actions_route_to_blender_2_5d() {
        let shot = shot("dig", None);
        let (engine, _) = choose_engine("2d", &shot);
        assert_eq!(engine, "blender-2.5d");
    }

    #[test]
    fn simple_dialogue_routes_to_godot_2d() {
        let shot = shot("talk-neutral", Some("Hello"));
        let (engine, _) = choose_engine("2d", &shot);
        assert_eq!(engine, "godot-2d");
    }

    fn capabilities(godot: bool, blender: bool) -> StoryProductionCapabilities {
        let engine = |id: &str, available: bool| StoryEngineCapability {
            id: id.to_string(),
            available,
            executable: None,
            best_for: String::new(),
            notes: String::new(),
        };
        StoryProductionCapabilities {
            actions: vec![],
            engines: vec![
                engine("native-motion", true),
                engine("godot-2d", godot),
                engine("blender-grease-pencil", blender),
                engine("blender-2.5d", blender),
                engine("blender-3d", blender),
            ],
            animatic_width: 854,
            animatic_height: 480,
            animatic_fps: 12,
        }
    }

    #[test]
    fn automatic_simple_dialogue_falls_back_to_blender_when_godot4_is_unavailable() {
        let shot = shot("talk-neutral", Some("Hello"));
        let (preferred, reason) = choose_engine("2d", &shot);
        let (engine, fallback_reason) =
            choose_available_engine("2d", &shot, preferred, reason, &capabilities(false, true));
        assert_eq!(engine, "blender-grease-pencil");
        assert!(fallback_reason.contains("Godot 4 is unavailable"));
    }

    #[test]
    fn explicit_engine_request_is_never_silently_substituted() {
        let mut shot = shot("talk-neutral", Some("Hello"));
        shot.requested_engine = Some("godot-2d".to_string());
        let (preferred, reason) = choose_engine("2d", &shot);
        let (engine, _) =
            choose_available_engine("2d", &shot, preferred, reason, &capabilities(false, true));
        assert_eq!(engine, "godot-2d");
    }

    #[test]
    fn current_render_key_registration_becomes_reusable_and_stale_key_is_rejected() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project_with_mode(
            &workspace,
            "Story cache",
            Some("story"),
            None,
            Some(640),
            Some(360),
            Some(24),
        )
        .unwrap();

        let mut insert = shot("idle", None);
        insert.actors.clear();
        insert.requested_engine = Some("native-motion".to_string());
        insert.ambience = vec!["room-tone".to_string()];
        let plan = compile_plan(
            &workspace,
            &project.id,
            StoryDirectorInput {
                language: "en-US".to_string(),
                visual_style: "2d".to_string(),
                characters: vec![],
                locations: vec![StoryLocation {
                    id: "room".to_string(),
                    name: "Room".to_string(),
                    description: String::new(),
                    variants: vec![],
                    entrance_anchors: vec![],
                    interaction_anchors: vec![],
                    camera_anchors: vec![],
                    walkable_areas: vec![],
                    asset_path: None,
                }],
                props: vec![],
                voice_cast: vec![],
                shots: vec![insert],
            },
        )
        .unwrap();

        let compiled = &plan.shots[0];
        let output_relative = format!(
            "{}/story/shots/{}-cache-test.mp4",
            project.relative_path, compiled.shot.id
        );
        let output = video_production::resolve_project_path(
            &workspace,
            &project,
            &output_relative,
            AccessOperation::Write,
            false,
        )
        .unwrap();
        fs::write(&output, b"fake-video-for-cache-registration").unwrap();

        let queue = record_shot_render(
            &workspace,
            &project.id,
            StoryShotRenderInput {
                shot_id: compiled.shot.id.clone(),
                render_key: compiled.render_key.clone(),
                output_path: output_relative.clone(),
            },
        )
        .unwrap();
        assert_eq!(queue.changed_shot_ids, Vec::<String>::new());
        assert_eq!(queue.reusable_shot_ids, vec![compiled.shot.id.clone()]);
        assert_eq!(
            get_render_queue(&workspace, &project.id).unwrap().entries[0].status,
            "ready"
        );
        assert_eq!(
            verify_render_queue_complete(&workspace, &project.id).unwrap(),
            (1, 1)
        );

        let stale = record_shot_render(
            &workspace,
            &project.id,
            StoryShotRenderInput {
                shot_id: compiled.shot.id.clone(),
                render_key: "stale-render-key".to_string(),
                output_path: output_relative,
            },
        );
        assert!(stale.is_err());

        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Read).unwrap();
        assert!(project_root.join("story/cache/index.json").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn action_catalog_requires_hand_targets_for_interactions() {
        let actions = action_capabilities();
        let dig = actions.iter().find(|item| item.action == "dig").unwrap();
        assert!(dig.requires_target);
        assert!(dig.requires_hand_target);
        let walk = actions.iter().find(|item| item.action == "walk").unwrap();
        assert!(walk.requires_movement_anchors);
    }

    #[test]
    fn narrative_qa_catches_missing_voice_and_interaction_targets() {
        let mut dig = shot("dig", Some("Look here"));
        dig.actors[0].lip_sync = false;
        let plan = StoryDirectorPlan {
            version: 1,
            action_library_version: 1,
            project_id: "project".to_string(),
            language: "te-IN".to_string(),
            visual_style: "2d".to_string(),
            characters: vec![],
            locations: vec![],
            props: vec![],
            voice_cast: vec![],
            shots: vec![StoryCompiledShot {
                shot: dig,
                selected_engine: "blender-2.5d".to_string(),
                engine_reason: String::new(),
                render_key: "key".to_string(),
            }],
            content_hash: "hash".to_string(),
            updated_at: 1,
        };
        let report = qa_plan(&plan);
        assert!(!report.passed);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "dialogue-missing-voice-cast"));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "interaction-missing-target"));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "interaction-missing-hand-target"));
    }
}
