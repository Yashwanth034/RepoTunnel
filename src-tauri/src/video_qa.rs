use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::AppHandle;

use crate::{
    access::AccessOperation, models::Workspace, video, video_director, video_production,
    video_render,
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoQaCheck {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoQaReport {
    pub(crate) project_id: String,
    pub(crate) asset_path: String,
    pub(crate) final_asset: bool,
    pub(crate) passed: bool,
    pub(crate) checks: Vec<VideoQaCheck>,
    pub(crate) report_path: String,
    pub(crate) checked_at: u64,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn check(id: &str, status: &str, message: impl Into<String>) -> VideoQaCheck {
    VideoQaCheck {
        id: id.to_string(),
        status: status.to_string(),
        message: message.into(),
    }
}

fn parse_rate(value: Option<&str>) -> Option<f64> {
    let value = value?.trim();
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.parse::<f64>().ok()?;
        let denominator = denominator.parse::<f64>().ok()?;
        if denominator.abs() < f64::EPSILON {
            return None;
        }
        let rate = numerator / denominator;
        return rate.is_finite().then_some(rate);
    }
    value.parse::<f64>().ok().filter(|rate| rate.is_finite())
}

fn probe_media(ffprobe: &Path, path: &Path) -> Result<serde_json::Value, String> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not start FFprobe for Video Project QA: {error}"))?;
    if !output.status.success() {
        let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.len() > 3000 {
            detail.truncate(3000);
        }
        return Err(if detail.is_empty() {
            format!("FFprobe exited with status {}.", output.status)
        } else {
            format!("FFprobe failed: {detail}")
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Could not parse FFprobe QA output: {error}"))
}

fn ffmpeg_filter_analysis(
    ffmpeg: &Path,
    path: &Path,
    media_flag: &str,
    filter_flag: &str,
    filter: &str,
) -> Result<String, String> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostats", "-loglevel", "info", "-i"])
        .arg(path)
        .arg(media_flag)
        .arg(filter_flag)
        .arg(filter)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not start FFmpeg Video Project QA analysis: {error}"))?;
    if !output.status.success() {
        let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.len() > 3000 {
            detail.truncate(3000);
        }
        return Err(if detail.is_empty() {
            format!("FFmpeg QA analysis exited with status {}.", output.status)
        } else {
            format!("FFmpeg QA analysis failed: {detail}")
        });
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let start = text.len().saturating_sub(96 * 1024);
    Ok(text[start..].to_string())
}

fn json_number(value: Option<&serde_json::Value>) -> Option<f64> {
    value.and_then(|value| {
        value.as_f64().or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<f64>().ok())
        })
    })
}

fn parse_loudnorm_summary(output: &str) -> Option<(f64, f64, f64)> {
    let start = output.rfind('{')?;
    let end = output[start..].find('}')? + start + 1;
    let value: serde_json::Value = serde_json::from_str(&output[start..end]).ok()?;
    Some((
        json_number(value.get("input_i"))?,
        json_number(value.get("input_tp"))?,
        json_number(value.get("input_lra"))?,
    ))
}

fn add_advanced_media_checks(
    checks: &mut Vec<VideoQaCheck>,
    ffmpeg: Option<&Path>,
    path: &Path,
    has_audio: bool,
) {
    let Some(ffmpeg) = ffmpeg else {
        checks.push(check(
            "advancedMediaQa",
            "warn",
            "FFmpeg is unavailable, so black/static-frame and audio loudness/silence checks were skipped.",
        ));
        return;
    };

    match ffmpeg_filter_analysis(
        ffmpeg,
        path,
        "-an",
        "-vf",
        "blackdetect=d=0.5:pic_th=0.98,freezedetect=n=-60dB:d=2",
    ) {
        Ok(output) => {
            let black_segments = output.matches("black_start:").count();
            checks.push(if black_segments == 0 {
                check("blackFrames", "pass", "No sustained black-frame segment was detected.")
            } else {
                check(
                    "blackFrames",
                    "warn",
                    format!(
                        "Detected {black_segments} sustained black-frame segment(s); verify intentional transitions/outro."
                    ),
                )
            });

            let frozen_segments = output.matches("freeze_start:").count();
            checks.push(if frozen_segments == 0 {
                check(
                    "staticFrames",
                    "pass",
                    "No sustained frozen/static segment was detected.",
                )
            } else {
                check(
                    "staticFrames",
                    "warn",
                    format!(
                        "Detected {frozen_segments} sustained static segment(s); verify intentional still scenes."
                    ),
                )
            });
        }
        Err(error) => checks.push(check("visualSignalQa", "warn", error)),
    }

    if !has_audio {
        return;
    }

    match ffmpeg_filter_analysis(
        ffmpeg,
        path,
        "-vn",
        "-af",
        "silencedetect=n=-50dB:d=1.5,loudnorm=I=-16:TP=-1.5:LRA=11:print_format=json",
    ) {
        Ok(output) => {
            let silence_segments = output.matches("silence_start:").count();
            checks.push(if silence_segments == 0 {
                check(
                    "audioSilence",
                    "pass",
                    "No sustained audio silence longer than the QA threshold was detected.",
                )
            } else {
                check(
                    "audioSilence",
                    "warn",
                    format!(
                        "Detected {silence_segments} sustained silence segment(s); verify pauses, intro, and outro are intentional."
                    ),
                )
            });

            match parse_loudnorm_summary(&output) {
                Some((integrated, true_peak, range)) => {
                    checks.push(if (-20.0..=-12.0).contains(&integrated) {
                        check(
                            "audioLoudness",
                            "pass",
                            format!(
                                "Integrated loudness is {integrated:.1} LUFS (LRA {range:.1} LU)."
                            ),
                        )
                    } else {
                        check(
                            "audioLoudness",
                            "warn",
                            format!(
                                "Integrated loudness is {integrated:.1} LUFS (LRA {range:.1} LU); review mastering for dialogue-focused delivery."
                            ),
                        )
                    });
                    checks.push(if true_peak >= 0.0 {
                        check(
                            "audioPeak",
                            "fail",
                            format!(
                                "Measured true peak is {true_peak:.1} dBTP, indicating clipping/overs."
                            ),
                        )
                    } else if true_peak > -1.0 {
                        check(
                            "audioPeak",
                            "warn",
                            format!(
                                "Measured true peak is {true_peak:.1} dBTP; consider more headroom."
                            ),
                        )
                    } else {
                        check(
                            "audioPeak",
                            "pass",
                            format!("Measured true peak is {true_peak:.1} dBTP."),
                        )
                    });
                }
                None => checks.push(check(
                    "audioLoudness",
                    "warn",
                    "FFmpeg completed audio QA but did not return a parseable loudness summary.",
                )),
            }
        }
        Err(error) => checks.push(check("audioSignalQa", "warn", error)),
    }
}

fn render_metadata_for_asset(
    workspace: &Workspace,
    project_id: &str,
    asset_path: &str,
) -> Option<video_render::VideoRenderResult> {
    video_render::list_render_jobs(workspace, project_id)
        .ok()?
        .into_iter()
        .filter_map(|job| job.result)
        .find(|result| result.output_path == asset_path)
}

pub(crate) fn qa_project(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    asset_path: Option<&str>,
) -> Result<VideoQaReport, String> {
    let ffprobe = video::available_ffprobe_program(app);
    let ffmpeg = video::available_ffmpeg_program(app);
    qa_project_with_tools(
        workspace,
        project_id,
        asset_path,
        ffprobe.as_deref(),
        ffmpeg.as_deref(),
    )
}

fn qa_project_with_tools(
    workspace: &Workspace,
    project_id: &str,
    asset_path: Option<&str>,
    ffprobe: Option<&Path>,
    ffmpeg: Option<&Path>,
) -> Result<VideoQaReport, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let selected = match asset_path.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => project
            .final_export
            .clone()
            .ok_or_else(|| "Video Project has no registered final export to QA.".to_string())?,
    };
    let path = video_production::resolve_project_path(
        workspace,
        &project,
        &selected,
        AccessOperation::Read,
        true,
    )?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect Video Project QA asset: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(
            "Video Project QA target must be a non-empty regular project-owned file.".to_string(),
        );
    }

    let final_asset = project.final_export.as_deref() == Some(selected.as_str());
    let mut checks = vec![check(
        "projectOwnership",
        "pass",
        "QA target is a regular file inside the selected Video Project.",
    )];

    let probe = match ffprobe {
        Some(ffprobe) => match probe_media(ffprobe, &path) {
            Ok(value) => Some(value),
            Err(error) => {
                checks.push(check("ffprobe", "fail", error));
                None
            }
        },
        None => {
            checks.push(check(
                "ffprobe",
                "fail",
                "FFprobe is unavailable. RepoTunnel cannot mark a final export complete without deterministic stream validation.",
            ));
            None
        }
    };

    if let Some(probe) = probe.as_ref() {
        let streams = probe
            .get("streams")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let videos = streams
            .iter()
            .filter(|stream| {
                stream.get("codec_type").and_then(|value| value.as_str()) == Some("video")
            })
            .collect::<Vec<_>>();
        let audios = streams
            .iter()
            .filter(|stream| {
                stream.get("codec_type").and_then(|value| value.as_str()) == Some("audio")
            })
            .collect::<Vec<_>>();
        let subtitles = streams
            .iter()
            .filter(|stream| {
                stream.get("codec_type").and_then(|value| value.as_str()) == Some("subtitle")
            })
            .collect::<Vec<_>>();

        if videos.len() == 1 {
            checks.push(check(
                "videoStream",
                "pass",
                "Exactly one video stream is present.",
            ));
            let width = videos[0]
                .get("width")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let height = videos[0]
                .get("height")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            if width == u64::from(project.width) && height == u64::from(project.height) {
                checks.push(check(
                    "resolution",
                    "pass",
                    format!(
                        "Resolution is {}x{} as configured.",
                        project.width, project.height
                    ),
                ));
            } else {
                checks.push(check(
                    "resolution",
                    "fail",
                    format!(
                        "Expected {}x{}, found {}x{}.",
                        project.width, project.height, width, height
                    ),
                ));
            }

            let fps = parse_rate(
                videos[0]
                    .get("avg_frame_rate")
                    .and_then(|value| value.as_str())
                    .or_else(|| {
                        videos[0]
                            .get("r_frame_rate")
                            .and_then(|value| value.as_str())
                    }),
            );
            match fps {
                Some(fps) if (fps - f64::from(project.fps)).abs() <= 0.5 => checks.push(check(
                    "frameRate",
                    "pass",
                    format!("Frame rate is {fps:.2} FPS."),
                )),
                Some(fps) => checks.push(check(
                    "frameRate",
                    "fail",
                    format!("Expected about {} FPS, found {fps:.2} FPS.", project.fps),
                )),
                None => checks.push(check(
                    "frameRate",
                    "fail",
                    "Could not determine the output frame rate.",
                )),
            }
        } else {
            checks.push(check(
                "videoStream",
                "fail",
                format!("Expected exactly one video stream, found {}.", videos.len()),
            ));
        }

        if audios.is_empty() {
            checks.push(check(
                "audioStream",
                "warn",
                "No audio stream is present. Confirm this is intentional.",
            ));
        } else if audios.len() == 1 {
            checks.push(check(
                "audioStream",
                "pass",
                "Exactly one audio stream is present.",
            ));
        } else {
            checks.push(check(
                "audioStream",
                "warn",
                format!("Multiple audio streams are present ({}).", audios.len()),
            ));
        }

        add_advanced_media_checks(&mut checks, ffmpeg, &path, !audios.is_empty());

        let duration = probe
            .get("format")
            .and_then(|value| value.get("duration"))
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<f64>().ok());
        match duration {
            Some(duration) if duration.is_finite() && duration > 0.0 => checks.push(check(
                "duration",
                "pass",
                format!("Output duration is {duration:.2} seconds."),
            )),
            _ => checks.push(check(
                "duration",
                "fail",
                "Output duration is missing or invalid.",
            )),
        }

        let render_metadata = render_metadata_for_asset(workspace, project_id, &selected);
        let delivery = render_metadata
            .as_ref()
            .map(|result| result.caption_delivery.as_str());
        let had_subtitle_input = render_metadata
            .as_ref()
            .is_some_and(|result| result.subtitle_path.is_some());
        let has_sidecar = project.current_subtitle.is_some();

        if !subtitles.is_empty() && has_sidecar {
            checks.push(check(
                "subtitleDuplication",
                "fail",
                "The output contains an embedded subtitle stream while the current Video Project preview also attaches a sidecar subtitle. This can display duplicate captions.",
            ));
        } else {
            checks.push(check(
                "subtitleDuplication",
                "pass",
                "No simultaneous embedded-plus-sidecar subtitle presentation was detected.",
            ));
        }

        match delivery {
            Some("embedded") if subtitles.len() == 1 && !has_sidecar => checks.push(check(
                "captionDelivery",
                "pass",
                "Embedded subtitle delivery matches the render request.",
            )),
            Some("embedded") => checks.push(check(
                "captionDelivery",
                "fail",
                format!(
                    "Embedded subtitle delivery expected one embedded stream and no sidecar; found {} embedded stream(s), sidecar={has_sidecar}.",
                    subtitles.len()
                ),
            )),
            Some("burned") if subtitles.is_empty() && !has_sidecar => checks.push(check(
                "captionDelivery",
                "pass",
                "Burned caption delivery has no selectable/sidecar duplicate.",
            )),
            Some("burned+sidecar") if subtitles.is_empty() && has_sidecar => checks.push(check(
                "captionDelivery",
                "pass",
                "Burned+sidecar caption delivery matches the render request.",
            )),
            Some("sidecar") if !had_subtitle_input || (subtitles.is_empty() && has_sidecar) => {
                checks.push(check(
                    "captionDelivery",
                    "pass",
                    "Sidecar caption delivery matches the render request.",
                ))
            }
            Some("none") if subtitles.is_empty() && !has_sidecar => checks.push(check(
                "captionDelivery",
                "pass",
                "No-caption delivery matches the render request.",
            )),
            Some(mode) => checks.push(check(
                "captionDelivery",
                "fail",
                format!(
                    "Caption delivery '{mode}' does not match the final stream/sidecar state (embedded={}, sidecar={has_sidecar}).",
                    subtitles.len()
                ),
            )),
            None => checks.push(check(
                "captionDelivery",
                "warn",
                "No matching render-job metadata was found for this asset; duplicate subtitle presentation was still checked.",
            )),
        }
    }

    if final_asset {
        checks.push(check(
            "finalRegistration",
            "pass",
            "QA target is the currently registered final export.",
        ));
        if project.production_mode == "story" {
            match video_director::get_narrative_qa(workspace, project_id) {
                Ok(report) if report.passed => checks.push(check(
                    "narrativePlan",
                    "pass",
                    format!(
                        "Story narrative QA passed for {} shot(s), {} character(s), and {} location(s).",
                        report.metrics.shot_count,
                        report.metrics.character_count,
                        report.metrics.location_count
                    ),
                )),
                Ok(report) => {
                    let blocking = report
                        .issues
                        .iter()
                        .filter(|issue| issue.severity == "error")
                        .count();
                    checks.push(check(
                        "narrativePlan",
                        "fail",
                        format!(
                            "Story narrative QA still has {blocking} blocking issue(s). Fix the Scene Director plan before final completion."
                        ),
                    ));
                }
                Err(error) => checks.push(check(
                    "narrativePlan",
                    "fail",
                    format!(
                        "Story final completion requires a current Scene Director narrative-QA report: {error}"
                    ),
                )),
            }

            match video_director::verify_render_queue_complete(workspace, project_id) {
                Ok((ready, total)) => checks.push(check(
                    "storyShotCache",
                    "pass",
                    format!(
                        "All {ready}/{total} Scene Director shot render(s) match the current render keys and have project-owned outputs."
                    ),
                )),
                Err(error) => checks.push(check(
                    "storyShotCache",
                    "fail",
                    format!(
                        "Story final completion requires every planned shot to be rendered from the current plan: {error}"
                    ),
                )),
            }
        }
    } else {
        checks.push(check(
            "finalRegistration",
            "warn",
            "QA target is not the currently registered final export; this report will not mark the project completed.",
        ));
    }

    let passed = !checks.iter().any(|item| item.status == "fail");
    let checked_at = now_millis();
    let report_inside = format!("qa/report-{checked_at}.json");
    let report_path = format!("{}/{}", project.relative_path, report_inside);
    let mut report = VideoQaReport {
        project_id: project.id.clone(),
        asset_path: selected,
        final_asset,
        passed,
        checks,
        report_path,
        checked_at,
    };

    let report_file = video_production::resolve_project_path(
        workspace,
        &project,
        &report.report_path,
        AccessOperation::Write,
        false,
    )?;
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("Could not serialize Video Project QA report: {error}"))?;
    fs::write(&report_file, bytes)
        .map_err(|error| format!("Could not save Video Project QA report: {error}"))?;
    video_production::register_asset(
        workspace,
        project_id,
        "qa-report",
        &report_inside,
        Some(if passed {
            "Video Project QA passed"
        } else {
            "Video Project QA requires attention"
        }),
    )?;

    if final_asset {
        let failures = report
            .checks
            .iter()
            .filter(|item| item.status == "fail")
            .count();
        let warnings = report
            .checks
            .iter()
            .filter(|item| item.status == "warn")
            .count();
        let detail = if passed {
            format!("Final Video Project QA passed with {warnings} warning(s).")
        } else {
            format!("Final Video Project QA found {failures} failure(s) and {warnings} warning(s).")
        };
        video_production::record_qa_result(workspace, project_id, passed, &detail)?;
    }

    report.passed = passed;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        access::AccessOperation,
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
        video_director, video_production,
    };

    use super::{parse_loudnorm_summary, parse_rate, qa_project_with_tools};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn program_on_path(name: &str) -> Option<PathBuf> {
        let executable = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        env::var_os("PATH")
            .into_iter()
            .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
            .map(|directory| directory.join(&executable))
            .find(|path| path.is_file())
    }

    fn temp_workspace() -> (std::path::PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repotunnel-video-qa-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: format!("qa-{counter}"),
            name: "Video QA".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    fn generate_video(ffmpeg: &std::path::Path, output: &std::path::Path, size: &str) {
        let status = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size={size}:rate=12"),
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(output)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn parses_fractional_frame_rates() {
        assert!((parse_rate(Some("30000/1001")).unwrap() - 29.970).abs() < 0.01);
        assert_eq!(parse_rate(Some("30/1")).unwrap(), 30.0);
        assert!(parse_rate(Some("0/0")).is_none());
    }

    #[test]
    fn parses_ffmpeg_loudnorm_summary() {
        let output = r#"noise before
{
    "input_i" : "-15.40",
    "input_tp" : "-1.20",
    "input_lra" : "3.10"
}
noise after"#;
        let (integrated, peak, range) = parse_loudnorm_summary(output).unwrap();
        assert!((integrated + 15.4).abs() < 0.01);
        assert!((peak + 1.2).abs() < 0.01);
        assert!((range - 3.1).abs() < 0.01);
    }

    #[test]
    fn final_export_stays_in_review_until_qa_passes() {
        let Some(ffmpeg) = program_on_path("ffmpeg") else {
            return;
        };
        let Some(ffprobe) = program_on_path("ffprobe") else {
            return;
        };

        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "QA pass",
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();
        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Write).unwrap();
        let final_path = project_root.join("renders/final/test.mp4");
        generate_video(&ffmpeg, &final_path, "640x360");
        video_production::register_asset(
            &workspace,
            &project.id,
            "final-video",
            "renders/final/test.mp4",
            Some("QA test final"),
        )
        .unwrap();
        video_production::set_render_outputs(
            &workspace,
            &project.id,
            Some("renders/final/test.mp4"),
            None,
            Some("renders/final/test.mp4"),
            None,
        )
        .unwrap();

        let before = video_production::get_project(&workspace, &project.id).unwrap();
        assert_eq!(before.status, "review");

        let report =
            qa_project_with_tools(&workspace, &project.id, None, Some(&ffprobe), Some(&ffmpeg))
                .unwrap();
        assert!(report.passed);
        assert!(report.checks.iter().any(|item| item.id == "blackFrames"));
        assert!(report.checks.iter().any(|item| item.id == "audioLoudness"));
        assert!(report.checks.iter().any(|item| item.id == "audioPeak"));
        let after = video_production::get_project(&workspace, &project.id).unwrap();
        assert_eq!(after.status, "completed");
        assert!(!after.attention_required);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn story_final_qa_rejects_dirty_scene_director_shots() {
        let Some(ffmpeg) = program_on_path("ffmpeg") else {
            return;
        };
        let Some(ffprobe) = program_on_path("ffprobe") else {
            return;
        };

        let (root, workspace) = temp_workspace();
        let project = video_production::create_project_with_mode(
            &workspace,
            "Story QA dirty shot",
            Some("story"),
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();

        let input: video_director::StoryDirectorInput = serde_json::from_value(serde_json::json!({
            "language": "en-US",
            "visualStyle": "2d",
            "locations": [{
                "id": "room",
                "name": "Room",
                "description": "Simple room"
            }],
            "shots": [{
                "id": "shot-1",
                "sceneId": "scene-1",
                "order": 1,
                "durationSeconds": 1.0,
                "locationId": "room",
                "requestedEngine": "native-motion",
                "ambience": ["room-tone"],
                "camera": {
                    "shotType": "wide",
                    "movement": "static",
                    "angle": "eye-level"
                }
            }]
        }))
        .unwrap();
        let plan = video_director::compile_plan(&workspace, &project.id, input).unwrap();
        assert_eq!(plan.shots.len(), 1);
        assert!(
            video_director::get_narrative_qa(&workspace, &project.id)
                .unwrap()
                .passed
        );

        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Write).unwrap();
        let final_path = project_root.join("renders/final/story.mp4");
        generate_video(&ffmpeg, &final_path, "640x360");
        video_production::register_asset(
            &workspace,
            &project.id,
            "final-video",
            "renders/final/story.mp4",
            Some("Story QA final"),
        )
        .unwrap();
        video_production::set_render_outputs(
            &workspace,
            &project.id,
            Some("renders/final/story.mp4"),
            None,
            Some("renders/final/story.mp4"),
            None,
        )
        .unwrap();

        let report =
            qa_project_with_tools(&workspace, &project.id, None, Some(&ffprobe), Some(&ffmpeg))
                .unwrap();
        assert!(!report.passed);
        assert!(report
            .checks
            .iter()
            .any(|item| item.id == "narrativePlan" && item.status == "pass"));
        assert!(report
            .checks
            .iter()
            .any(|item| item.id == "storyShotCache" && item.status == "fail"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qa_failure_keeps_final_export_in_review() {
        let Some(ffmpeg) = program_on_path("ffmpeg") else {
            return;
        };
        let Some(ffprobe) = program_on_path("ffprobe") else {
            return;
        };

        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "QA fail",
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();
        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Write).unwrap();
        let final_path = project_root.join("renders/final/wrong.mp4");
        generate_video(&ffmpeg, &final_path, "320x180");
        video_production::register_asset(
            &workspace,
            &project.id,
            "final-video",
            "renders/final/wrong.mp4",
            Some("Wrong-size final"),
        )
        .unwrap();
        video_production::set_render_outputs(
            &workspace,
            &project.id,
            Some("renders/final/wrong.mp4"),
            None,
            Some("renders/final/wrong.mp4"),
            None,
        )
        .unwrap();

        let report =
            qa_project_with_tools(&workspace, &project.id, None, Some(&ffprobe), Some(&ffmpeg))
                .unwrap();
        assert!(!report.passed);
        assert!(report
            .checks
            .iter()
            .any(|item| item.id == "resolution" && item.status == "fail"));
        let after = video_production::get_project(&workspace, &project.id).unwrap();
        assert_eq!(after.status, "review");
        assert!(after.attention_required);
        assert!(after.last_error.is_none());

        fs::remove_dir_all(root).unwrap();
    }
}
