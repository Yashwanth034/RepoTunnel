//! Bounded, project-scoped static frontend design hygiene checks.
//! Never calls this a screenshot review, accessibility certification,
//! aesthetic score, or proof that interactive UI actually works.
use std::{collections::BTreeSet, fs, path::Path};

use serde::Serialize;

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::Workspace,
    project_index,
};

const MAX_FILES: usize = 24;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ISSUES: usize = 60;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesignIssue {
    pub(crate) severity: &'static str,
    pub(crate) code: &'static str,
    pub(crate) path: Option<String>,
    pub(crate) line: Option<usize>,
    pub(crate) explanation: &'static str,
    pub(crate) fix: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesignQualityAudit {
    pub(crate) scan_kind: &'static str,
    pub(crate) files_inspected: Vec<String>,
    pub(crate) issues: Vec<DesignIssue>,
    pub(crate) issue_count: usize,
    pub(crate) truncated_issues: bool,
    pub(crate) screenshot_review_complete: bool,
    pub(crate) required_browser_review: Vec<&'static str>,
    pub(crate) caveats: Vec<&'static str>,
}

fn issue(
    severity: &'static str,
    code: &'static str,
    path: Option<&str>,
    line: Option<usize>,
    explanation: &'static str,
    fix: &'static str,
) -> DesignIssue {
    DesignIssue {
        severity,
        code,
        path: path.map(str::to_owned),
        line,
        explanation,
        fix,
    }
}

fn first_line(haystack_lower: &str, needle: &str) -> Option<usize> {
    haystack_lower.find(needle).map(|offset| {
        1 + haystack_lower[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
    })
}

fn jsx_markup(path: &str) -> bool {
    [".html", ".htm", ".jsx", ".tsx", ".vue", ".svelte", ".astro"]
        .iter()
        .any(|suffix| path.to_lowercase().ends_with(suffix))
}

fn styling(path: &str) -> bool {
    [".css", ".scss", ".sass", ".less"]
        .iter()
        .any(|suffix| path.to_lowercase().ends_with(suffix))
}

fn allowed_source(path: &str) -> bool {
    jsx_markup(path)
        || styling(path)
        || [".ts", ".js", ".mjs", ".mts"]
            .iter()
            .any(|s| path.to_lowercase().ends_with(s))
}

fn sources_issues(sources: &[(&str, &str)]) -> Vec<DesignIssue> {
    let mut problems = Vec::new();
    let mut found_motion = false;
    let mut found_reduced_motion = false;
    let mut found_focus_visible = false;
    let mut found_outline_none = false;
    let mut outline_path: Option<&str> = None;
    let mut found_responsive = false;
    let mut saw_style = false;
    let mut motion_path: Option<&str> = None;
    let mut saw_markup = false;
    let mut found_heading = false;
    for (path, text) in sources {
        let lower = text.to_lowercase();
        let is_markup = jsx_markup(path);
        let is_style = styling(path);
        saw_markup |= is_markup;
        saw_style |= is_style;
        if is_markup
            && (lower.contains("<h1")
                || lower.contains("role=\"heading\"")
                || lower.contains("role='heading'"))
        {
            found_heading = true;
        }
        found_focus_visible |= lower.contains(":focus-visible") || lower.contains("focus-visible:");
        let no_outline = [
            "outline: none",
            "outline:none",
            "outline: 0",
            "outline:0",
            "outline=\"none\"",
            "outline: unset",
        ]
        .iter()
        .any(|x| lower.contains(x));
        if no_outline {
            found_outline_none = true;
            outline_path = Some(path);
        }
        found_reduced_motion |=
            lower.contains("prefers-reduced-motion") || lower.contains("motion-reduce:");
        if is_style
            && (lower.contains("animation:")
                || lower.contains("@keyframes")
                || lower.contains("transition:"))
            || is_markup
                && (lower.contains("animate-")
                    || lower.contains("framer-motion")
                    || lower.contains("motion."))
        {
            found_motion = true;
            motion_path = Some(path);
        }
        found_responsive |= lower.contains("@media")
            || lower.contains("@container")
            || lower.contains("sm:")
            || lower.contains("md:")
            || lower.contains("lg:")
            || lower.contains("min-width:")
            || lower.contains("max-width:");

        for (needle, code, explanation, fix, severity) in [
            (
                "lorem ipsum",
                "placeholder-copy",
                "Lorem ipsum placeholder content is still present.",
                "Replace it with meaningful, product-specific real content.",
                "high",
            ),
            (
                "href=\"#\"",
                "dead-link",
                "A navigation/CTA anchor points only to '#'.",
                "Link to a real route, section or working action; test it in the browser.",
                "high",
            ),
            (
                "href='#'",
                "dead-link",
                "A navigation/CTA anchor points only to '#'.",
                "Link to a real route, section or working action; test it in the browser.",
                "high",
            ),
            (
                "coming soon",
                "incomplete-section",
                "A 'coming soon' placeholder may indicate unfinished product content.",
                "Either implement the promised section or accurately scope it as unavailable.",
                "review",
            ),
            (
                "your company",
                "generic-copy",
                "Generic company placeholder may still be present.",
                "Replace with genuine product identity and meaningful copy.",
                "review",
            ),
            (
                "revolutionize your",
                "generic-headline",
                "Generic AI-style marketing copy weakens distinctive product positioning.",
                "Describe a concrete customer outcome backed by a real product demonstration.",
                "review",
            ),
            (
                "unlock the power",
                "generic-headline",
                "Cliché marketing headline may obscure product purpose.",
                "Lead with what the product actually does and for whom.",
                "review",
            ),
            (
                "elevate your",
                "generic-headline",
                "Generic marketing headline may obscure differentiators.",
                "Write a specific, credible product promise.",
                "review",
            ),
            (
                "seamless experience",
                "generic-headline",
                "Vague promise is not proof of a usable experience.",
                "Demonstrate one real workflow, with authentic visual proof.",
                "review",
            ),
            (
                "trusted by 10",
                "unverified-proof",
                "Customer-count claim may be fabricated or a placeholder.",
                "Keep only verified customer or usage evidence; otherwise remove.",
                "review",
            ),
            (
                "99.9% uptime",
                "unverified-proof",
                "Specific reliability claim requires a verifiable measurement.",
                "Link to real measured uptime or avoid the claim.",
                "review",
            ),
        ] {
            if let Some(line) = first_line(&lower, needle) {
                problems.push(issue(
                    severity,
                    code,
                    Some(path),
                    Some(line),
                    explanation,
                    fix,
                ));
            }
        }
        if is_markup {
            // Scan explicit <img ...> tags; Next Image and dynamic markup need
            // browser accessibility checks and are not certified by this scan.
            for (start, _) in lower.match_indices("<img") {
                let remainder = &lower[start + 4..];
                let close = remainder.find('>').unwrap_or(remainder.len()).min(8192);
                let tag = &remainder[..close];
                if !tag.contains("alt=") && !tag.contains("...") && !tag.contains("{...") {
                    let line = 1 + lower[..start].bytes().filter(|b| *b == b'\n').count();
                    problems.push(issue(
                        "high",
                        "missing-image-alt",
                        Some(path),
                        Some(line),
                        "HTML image has no alt attribute.",
                        "Supply informative alt text, or alt=\"\" for decorative imagery.",
                    ));
                    if problems.len() >= MAX_ISSUES {
                        break;
                    }
                }
            }
        }
        if lower.contains("from-purple-")
            && lower.contains("to-blue-")
            && lower.contains("gradient-")
        {
            problems.push(issue("review","generic-gradient",Some(path),first_line(&lower,"gradient-"),
                "Common purple-to-blue preset may reflect unconsidered template styling.",
                "Verify this is appropriate for the actual brand; choose product-specific tokens and composition."));
        }
        if problems.len() >= MAX_ISSUES {
            break;
        }
    }
    if found_outline_none && !found_focus_visible {
        problems.push(issue(
            "high",
            "focus-indicator",
            outline_path,
            None,
            "Focus outlines may be disabled without a visible keyboard alternative.",
            "Add clearly visible :focus-visible styles; verify keyboard navigation in browser.",
        ));
    }
    if found_motion && !found_reduced_motion {
        problems.push(issue(
            "high",
            "reduced-motion",
            motion_path,
            None,
            "Animation/transition patterns were found without a detected reduced-motion override.",
            "Honor prefers-reduced-motion and test animation fallback at runtime.",
        ));
    }
    if saw_style && !found_responsive {
        problems.push(issue("review","responsive-layout",None,None,
            "No obvious breakpoint/container or responsive utility pattern was detected in the inspected sources.",
            "Check real mobile/tablet screens and add necessary layout adjustments."));
    }
    if saw_markup && !found_heading {
        problems.push(issue(
            "review",
            "page-heading",
            None,
            None,
            "No semantic primary heading detected in the inspected markup.",
            "Confirm one useful h1/heading hierarchy for each page in the browser DOM.",
        ));
    }
    problems.truncate(MAX_ISSUES);
    problems
}

pub(crate) fn audit(workspace: &Workspace, paths: &[String]) -> Result<DesignQualityAudit, String> {
    if paths.is_empty() || paths.len() > MAX_FILES {
        return Err(format!(
            "Supply 1–{MAX_FILES} workspace-relative frontend source files to inspect."
        ));
    }
    let mut unique = BTreeSet::new();
    let mut total = 0u64;
    let mut files = Vec::with_capacity(paths.len());
    for relative in paths {
        if relative.is_empty()
            || relative.len() > 300
            || !allowed_source(relative)
            || !unique.insert(relative.as_str())
        {
            return Err("Sources must be unique workspace-relative frontend files (HTML, JS/TS/JSX/TSX, Vue/Svelte/Astro, or CSS).".into());
        }
        let path = resolve_workspace_path(workspace, relative, AccessOperation::Read, true)?;
        let meta = fs::symlink_metadata(&path)
            .map_err(|_| format!("Cannot inspect requested frontend source: {relative}"))?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_FILE_BYTES {
            return Err(format!("Frontend file is non-regular, aliased or exceeds the 256 KiB scan limit: {relative}"));
        }
        let root = Path::new(&workspace.path)
            .canonicalize()
            .map_err(|_| "Cannot verify approved project root for design audit.")?;
        if !project_index::should_include_entry(
            workspace,
            path.parent().unwrap_or(&root),
            &path,
            false,
        )? {
            return Err(format!(
                "Frontend source excluded by project file rules: {relative}"
            ));
        }
        total = total
            .checked_add(meta.len())
            .ok_or("Frontend audit byte count overflow.")?;
        if total > MAX_TOTAL_BYTES {
            return Err("Frontend audit exceeds the 2 MiB total scan limit.".into());
        }
        // Use RepoTunnel's existing bounded file reader, including its
        // opened-handle authorization checks on Linux.
        let opened = crate::filesystem::read_file(workspace, relative)?;
        if opened.size > MAX_FILE_BYTES || opened.content.len() as u64 > MAX_FILE_BYTES {
            return Err(format!(
                "Frontend source changed or exceeded the scan limit: {relative}"
            ));
        }
        files.push((relative.clone(), opened.content));
    }
    let borrowed = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect::<Vec<_>>();
    let issues = sources_issues(&borrowed);
    Ok(DesignQualityAudit {
        scan_kind:"boundedStaticSourceReviewOnly",
        files_inspected:paths.to_vec(),
        issue_count:issues.len(),
        truncated_issues:issues.len()>=MAX_ISSUES,
        issues,
        screenshot_review_complete:false,
        required_browser_review:vec![
            "Start or inspect actual dev server and use browser_take_screenshot to VIEW the rendered page, not just source code.",
            "Review mobile (~390px), tablet (~768px), desktop (~1440px), scrolled content and one interactive state.",
            "Look for generic slide-deck layout, oversized typography, repetitive cards, irrelevant artwork, empty sections, clipping and overflow.",
            "Check semantic/keyboard navigation and working buttons/forms through browser semantic actions, not just screenshots.",
            "Compare implementation to product-specific get_website_design_brief and iterate before claiming visual quality.",
        ],
        caveats:vec![
            "This scans only explicitly named approved project source files. Other CSS, templates and dependencies may alter the result.",
            "Detections are heuristics requiring human/model review; absence of issues is NOT a pass or evidence of visual quality.",
            "Screenshots, image rights, font rendering, contrast, semantics and behavior are NOT inspected by this tool.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_generic_dead_copy_accessibility_and_motion() {
        let problems = sources_issues(&[
            (
                "site/index.html",
                r##"<h1>Revolutionize Your Workflow</h1><a href="#">Try now</a><img src="logo.png">Lorem ipsum"##,
            ),
            (
                "site/app.css",
                "div {animation: bounce 2s infinite; outline: none;}",
            ),
        ]);
        let codes = problems.iter().map(|i| i.code).collect::<BTreeSet<_>>();
        for expected in [
            "dead-link",
            "placeholder-copy",
            "generic-headline",
            "missing-image-alt",
            "reduced-motion",
            "focus-indicator",
        ] {
            assert!(codes.contains(expected), "missing {expected}: {codes:?}");
        }
    }
    #[test]
    fn avoids_false_alarms_when_good_patterns_present() {
        let findings=sources_issues(&[
            ("src/App.tsx",r#"<main><h1>Invoice reconciliation for teams</h1><img src="logo" alt="Company mark"/><a href="/signup">Start</a></main>"#),
            ("src/app.css","@media(max-width: 650px){main{padding:1rem}} @media(prefers-reduced-motion: reduce){*{animation:none!important}} button:focus-visible{outline:2px solid red}"),
        ]);
        assert!(findings.is_empty(), "{findings:?}");
    }
    #[test]
    fn scans_real_approved_files_and_rejects_non_frontend_content() {
        use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(
            temp.path().join("src/page.tsx"),
            r##"<main><h1>Join</h1><a href="#">Join</a></main>"##,
        )
        .unwrap();
        let workspace = Workspace {
            id: "workspace-website-quality-test".into(),
            name: "website-quality-test".into(),
            path: temp.path().to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let report = audit(&workspace, &["src/page.tsx".into()]).unwrap();
        assert_eq!(report.files_inspected, vec!["src/page.tsx"]);
        assert!(report.issues.iter().any(|i| i.code == "dead-link"));
        assert!(!report.screenshot_review_complete);
        assert!(audit(&workspace, &["src/../../outside.tsx".into()]).is_err());
        assert!(audit(&workspace, &[".env".into()]).is_err());
        assert!(audit(&workspace, &["src/missing.tsx".into()]).is_err());
        assert!(audit(&workspace, &vec!["src/page.tsx".into(); 25]).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("page.tsx", temp.path().join("src/link.tsx")).unwrap();
            assert!(audit(&workspace, &["src/link.tsx".into()]).is_err());
        }
    }

    #[test]
    fn ignores_non_visual_code_for_heading_and_breakpoints() {
        assert!(sources_issues(&[(
            "src/utils.ts",
            "export const sum = (a:number,b:number) => a+b;"
        )])
        .is_empty());
        assert!(allowed_source("src/page.astro"));
        assert!(!allowed_source(".env"));
        assert!(!allowed_source("src/data.json"));
    }
}
