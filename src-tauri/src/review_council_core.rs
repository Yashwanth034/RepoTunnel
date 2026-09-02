use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ReviewerKind {
    Code,
    Security,
    Test,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pass,
    RevisionRequired,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReviewRef<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) revision: u32,
    pub(crate) artifact_hash: &'a str,
    pub(crate) reviewer: ReviewerKind,
    pub(crate) verdict: Verdict,
    pub(crate) blocking_issues: &'a [String],
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultRef<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) revision: u32,
    pub(crate) artifact_hash: &'a str,
    pub(crate) verdict: Verdict,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AggregatedCouncil {
    pub(crate) verdict: Verdict,
    pub(crate) blocking_issues: Vec<String>,
}
const REVIEWERS: [ReviewerKind; 3] = [
    ReviewerKind::Code,
    ReviewerKind::Security,
    ReviewerKind::Test,
];
fn matching_review<'a>(
    reviews: &'a [ReviewRef<'a>],
    run_id: &str,
    revision: u32,
    artifact_hash: &str,
    reviewer: ReviewerKind,
) -> Option<ReviewRef<'a>> {
    reviews.iter().rev().copied().find(|review| {
        review.run_id == run_id
            && review.revision == revision
            && review.artifact_hash == artifact_hash
            && review.reviewer == reviewer
    })
}
pub(crate) fn next_missing_reviewer(
    reviews: &[ReviewRef<'_>],
    run_id: &str,
    revision: u32,
    artifact_hash: &str,
) -> Option<ReviewerKind> {
    REVIEWERS.into_iter().find(|reviewer| {
        !reviews.iter().rev().any(|review| {
            review.run_id == run_id
                && review.revision == revision
                && review.artifact_hash == artifact_hash
                && review.reviewer == *reviewer
        })
    })
}
pub(crate) fn aggregate(
    reviews: &[ReviewRef<'_>],
    run_id: &str,
    revision: u32,
    artifact_hash: &str,
) -> Result<AggregatedCouncil, String> {
    let mut selected = Vec::with_capacity(REVIEWERS.len());
    for reviewer in REVIEWERS {
        let review = reviews
            .iter()
            .rev()
            .find(|review| {
                review.run_id == run_id
                    && review.revision == revision
                    && review.artifact_hash == artifact_hash
                    && review.reviewer == reviewer
            })
            .ok_or_else(|| format!("{:?} review is missing for council aggregation.", reviewer))?;
        selected.push(review);
    }
    let verdict = if selected
        .iter()
        .all(|review| review.verdict == Verdict::Pass)
    {
        Verdict::Pass
    } else {
        Verdict::RevisionRequired
    };
    let mut seen = HashSet::new();
    let mut blocking_issues = Vec::new();
    for issue in selected
        .iter()
        .flat_map(|review| review.blocking_issues.iter())
    {
        let trimmed = issue.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            blocking_issues.push(trimmed.to_string());
        }
    }
    if verdict == Verdict::RevisionRequired && blocking_issues.is_empty() {
        return Err(
            "Council REVISION_REQUIRED must contain at least one blocking issue.".to_string(),
        );
    }
    Ok(AggregatedCouncil {
        verdict,
        blocking_issues,
    })
}
pub(crate) fn exact_approval_matches(
    reviews: &[ReviewRef<'_>],
    result: Option<ResultRef<'_>>,
    run_id: &str,
    revision: u32,
    artifact_hash: &str,
    computed_artifact_hash: &str,
) -> bool {
    if artifact_hash.is_empty() || computed_artifact_hash != artifact_hash {
        return false;
    }
    let all_pass = REVIEWERS.into_iter().all(|reviewer| {
        matching_review(reviews, run_id, revision, artifact_hash, reviewer)
            .is_some_and(|review| review.verdict == Verdict::Pass)
    });
    if !all_pass {
        return false;
    }
    result.is_some_and(|result| {
        result.run_id == run_id
            && result.revision == revision
            && result.artifact_hash == artifact_hash
            && result.verdict == Verdict::Pass
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn issues(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }
    fn review<'a>(
        reviewer: ReviewerKind,
        verdict: Verdict,
        blocking_issues: &'a [String],
    ) -> ReviewRef<'a> {
        ReviewRef {
            run_id: "run-1",
            revision: 2,
            artifact_hash: "hash-2",
            reviewer,
            verdict,
            blocking_issues,
        }
    }
    #[test]
    fn all_three_pass_aggregate_to_pass() {
        let e = Vec::new();
        let r = [
            review(ReviewerKind::Code, Verdict::Pass, &e),
            review(ReviewerKind::Security, Verdict::Pass, &e),
            review(ReviewerKind::Test, Verdict::Pass, &e),
        ];
        let x = aggregate(&r, "run-1", 2, "hash-2").unwrap();
        assert_eq!(x.verdict, Verdict::Pass);
        assert!(x.blocking_issues.is_empty());
    }
    #[test]
    fn any_single_reviewer_failure_requires_revision() {
        for failing in REVIEWERS {
            let b = issues(&["blocking"]);
            let e = Vec::new();
            let r = REVIEWERS.map(|k| {
                if k == failing {
                    review(k, Verdict::RevisionRequired, &b)
                } else {
                    review(k, Verdict::Pass, &e)
                }
            });
            assert_eq!(
                aggregate(&r, "run-1", 2, "hash-2").unwrap().verdict,
                Verdict::RevisionRequired
            );
        }
    }
    #[test]
    fn multiple_failures_deduplicate_blocking_issues() {
        let a = issues(&["Unsafe path", "Missing test"]);
        let b = issues(&[" unsafe path ", "Secret exposure"]);
        let c = issues(&["MISSING TEST", "Regression risk"]);
        let r = [
            review(ReviewerKind::Code, Verdict::RevisionRequired, &a),
            review(ReviewerKind::Security, Verdict::RevisionRequired, &b),
            review(ReviewerKind::Test, Verdict::RevisionRequired, &c),
        ];
        assert_eq!(
            aggregate(&r, "run-1", 2, "hash-2").unwrap().blocking_issues,
            vec![
                "Unsafe path",
                "Missing test",
                "Secret exposure",
                "Regression risk"
            ]
        );
    }
    #[test]
    fn sequential_resume_selects_first_missing_reviewer() {
        let e = Vec::new();
        let c = review(ReviewerKind::Code, Verdict::Pass, &e);
        let s = review(ReviewerKind::Security, Verdict::Pass, &e);
        assert_eq!(
            next_missing_reviewer(&[], "run-1", 2, "hash-2"),
            Some(ReviewerKind::Code)
        );
        assert_eq!(
            next_missing_reviewer(&[c], "run-1", 2, "hash-2"),
            Some(ReviewerKind::Security)
        );
        assert_eq!(
            next_missing_reviewer(&[c, s], "run-1", 2, "hash-2"),
            Some(ReviewerKind::Test)
        );
    }
    #[test]
    fn exact_approval_requires_run_revision_hash_three_passes_and_result() {
        let e = Vec::new();
        let r = [
            review(ReviewerKind::Code, Verdict::Pass, &e),
            review(ReviewerKind::Security, Verdict::Pass, &e),
            review(ReviewerKind::Test, Verdict::Pass, &e),
        ];
        let x = ResultRef {
            run_id: "run-1",
            revision: 2,
            artifact_hash: "hash-2",
            verdict: Verdict::Pass,
        };
        assert!(exact_approval_matches(
            &r,
            Some(x),
            "run-1",
            2,
            "hash-2",
            "hash-2"
        ));
        assert!(!exact_approval_matches(
            &r,
            Some(x),
            "wrong-run",
            2,
            "hash-2",
            "hash-2"
        ));
        assert!(!exact_approval_matches(
            &r,
            Some(x),
            "run-1",
            3,
            "hash-2",
            "hash-2"
        ));
        assert!(!exact_approval_matches(
            &r,
            Some(x),
            "run-1",
            2,
            "hash-2",
            "mutated-hash"
        ));
        assert!(!exact_approval_matches(
            &r, None, "run-1", 2, "hash-2", "hash-2"
        ));
    }
    #[test]
    fn revision_required_without_blocking_issue_is_invalid() {
        let e = Vec::new();
        let r = [
            review(ReviewerKind::Code, Verdict::RevisionRequired, &e),
            review(ReviewerKind::Security, Verdict::Pass, &e),
            review(ReviewerKind::Test, Verdict::Pass, &e),
        ];
        assert!(aggregate(&r, "run-1", 2, "hash-2").is_err());
    }
}
