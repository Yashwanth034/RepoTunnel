use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    agents::{
        assign_work_snapshot, heartbeat_snapshot, join_or_reattach_snapshot,
        TASK_ASSIGNMENT_LEASE_MS,
    },
    context::{acknowledge_authority_revision_snapshot, build_context_packet_from_events},
    gates::{request_gate_snapshot, MissionHumanGateKind, MissionHumanGateStatus},
    human::{submit_directive_snapshot, submit_question_snapshot},
    model::{
        AgentMission, MissionAssignmentKind, MissionEvent, MissionEventActor, MissionEventKind,
        MissionId, MissionSnapshot,
    },
    resources::MissionResourceSlots,
    scheduler::{record_failure_snapshot, schedule_once_snapshot, ReplanReason},
    store::MissionStore,
    tasks::{
        claim_task_snapshot, expire_task_assignments_snapshot, propose_tasks_snapshot,
        set_task_dependencies_snapshot, MissionTaskProposal, MissionTaskStatus,
    },
};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "repotunnel-mission-scale-{name}-{}-{nonce:x}-{sequence:x}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create scale-test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn snapshot(label: &str) -> MissionSnapshot {
    MissionSnapshot::new(
        AgentMission::new(
            MissionId::parse(format!("mission-scale-{label}")).expect("mission id"),
            format!("workspace-scale-{label}"),
            "Exercise Mission Control at bounded synthetic scale".into(),
            1_000,
        )
        .expect("mission"),
    )
}

fn principal(index: usize) -> Vec<String> {
    vec![format!("scale-principal-{index}")]
}

fn join_agents(snapshot: &mut MissionSnapshot, count: usize, now: u64) -> Vec<String> {
    (0..count)
        .map(|index| {
            join_or_reattach_snapshot(snapshot, &principal(index), Some("synthetic-scale"), now)
                .expect("join scale agent")
                .agent
                .id
        })
        .collect()
}

fn task_proposals(count: usize) -> Vec<MissionTaskProposal> {
    (0..count)
        .map(|index| MissionTaskProposal {
            kind: "implement".into(),
            title: format!("scale task {index}"),
            acceptance: vec![format!("scale task {index} is factually verified")],
            depends_on: Vec::new(),
            requested_paths: vec![format!("src/scale/{index}.rs")],
            priority: (index % 5 + 1) as u8,
        })
        .collect()
}

fn percentile(samples: &mut [Duration], percentile: usize) -> Duration {
    samples.sort_unstable();
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let index = ((samples.len() - 1) * percentile).div_ceil(100);
    samples[index.min(samples.len() - 1)]
}

#[test]
fn task15_scale_matrix_keeps_status_heartbeat_scheduler_and_resource_admission_bounded() {
    let cases = [
        (1usize, 20usize),
        (5, 100),
        (10, 500),
        (50, 2_000),
        (100, 5_000),
    ];

    for (agent_count, task_count) in cases {
        let mut state = snapshot(&format!("{agent_count}-{task_count}"));
        let joined = join_agents(&mut state, agent_count, 10_000);

        let task_started = Instant::now();
        propose_tasks_snapshot(&mut state, task_proposals(task_count), 11_000)
            .expect("build synthetic task graph");
        let task_build_elapsed = task_started.elapsed();

        assert_eq!(state.agents().len(), agent_count);
        assert_eq!(state.tasks().len(), task_count);
        assert_eq!(state.ready_task_ids().len(), task_count);
        assert_eq!(state.progress().registered_agent_count, agent_count);
        assert_eq!(state.progress().known_task_count, task_count);

        let mut status_samples = Vec::with_capacity(64);
        for _ in 0..64 {
            let started = Instant::now();
            let summary = state.summary();
            std::hint::black_box(summary.progress.known_task_count);
            status_samples.push(started.elapsed());
        }

        let mut heartbeat_samples = Vec::with_capacity(joined.len());
        for (index, agent_id) in joined.iter().enumerate() {
            let started = Instant::now();
            heartbeat_snapshot(
                &mut state,
                agent_id,
                &principal(index),
                12_000 + index as u64,
            )
            .expect("heartbeat scale agent");
            heartbeat_samples.push(started.elapsed());
        }

        let slots = MissionResourceSlots::for_scheduler_tests(32, 8);
        let scheduler_started = Instant::now();
        let tick = schedule_once_snapshot(&mut state, 20_000, slots).expect("schedule scale work");
        let scheduler_elapsed = scheduler_started.elapsed();

        assert!(
            tick.assignments.len() <= 8,
            "writer slots must cap assignments"
        );
        assert!(tick.assignments.len() <= agent_count);
        if agent_count >= 8 {
            assert_eq!(tick.assignments.len(), 8);
        } else {
            assert_eq!(tick.assignments.len(), agent_count);
        }

        let status_p50 = percentile(&mut status_samples, 50);
        let status_p95 = percentile(&mut status_samples, 95);
        let heartbeat_p50 = percentile(&mut heartbeat_samples, 50);
        let heartbeat_p95 = percentile(&mut heartbeat_samples, 95);
        eprintln!(
            "task15-scale agents={agent_count} tasks={task_count} task_build={task_build_elapsed:?} status_p50={status_p50:?} status_p95={status_p95:?} heartbeat_p50={heartbeat_p50:?} heartbeat_p95={heartbeat_p95:?} scheduler={scheduler_elapsed:?} assignments={}",
            tick.assignments.len()
        );
    }
}

#[test]
fn task15_hundred_agent_human_control_context_and_loop_protection_remain_non_blocking() {
    let mut state = snapshot("human-control");
    let agents = join_agents(&mut state, 100, 10_000);
    propose_tasks_snapshot(&mut state, task_proposals(5_000), 11_000)
        .expect("create 5k task graph");

    let first_task = state.ready_task_ids()[0].clone();
    let first_agent = agents[0].clone();
    assign_work_snapshot(
        &mut state,
        &first_agent,
        MissionAssignmentKind::Implement,
        Some(first_task.clone()),
        1,
        12_000,
    )
    .expect("assign primary work");
    state.task_mut(&first_task).expect("task").owner_agent_id = Some(first_agent.clone());
    state.task_mut(&first_task).expect("task").status = MissionTaskStatus::InProgress;
    acknowledge_authority_revision_snapshot(&mut state, &first_agent, 1).expect("ack revision one");

    let question_started = Instant::now();
    let question = submit_question_snapshot(&mut state, "What remains?", 13_000)
        .expect("Ask Swarm remains available");
    let question_elapsed = question_started.elapsed();
    assert_eq!(question.authority_revision, 1);
    assert_eq!(state.mission().authority_revision(), 1);
    assert_eq!(
        state.mission().status(),
        super::model::MissionStatus::Running
    );
    assert_eq!(
        state
            .agents()
            .iter()
            .find(|agent| agent.id == first_agent)
            .and_then(|agent| agent.binding.assignment.as_ref())
            .map(|assignment| assignment.authority_revision),
        Some(1)
    );

    let directive_started = Instant::now();
    let directive = submit_directive_snapshot(
        &mut state,
        "Keep Team Mode behavior unchanged while continuing Mission work.",
        14_000,
    )
    .expect("directive");
    let directive_elapsed = directive_started.elapsed();
    assert_eq!(directive.authority_revision, 2);
    assert_eq!(state.mission().authority_revision(), 2);
    assert!(
        state
            .task(&first_task)
            .expect("task")
            .needs_directive_recheck
    );
    assert!(acknowledge_authority_revision_snapshot(&mut state, &first_agent, 1).is_err());
    acknowledge_authority_revision_snapshot(&mut state, &first_agent, 2)
        .expect("refresh worker authority");
    assert!(
        !state
            .task(&first_task)
            .expect("task")
            .needs_directive_recheck
    );

    let duplicate_before = state.tasks().len();
    let duplicate_source = state.tasks()[1].clone();
    let duplicate = MissionTaskProposal {
        kind: duplicate_source.kind.clone(),
        title: "duplicate wording is allowed to differ".into(),
        acceptance: duplicate_source.acceptance.clone(),
        depends_on: duplicate_source.depends_on.clone(),
        requested_paths: duplicate_source.requested_paths.clone(),
        priority: 5,
    };
    let merged =
        propose_tasks_snapshot(&mut state, vec![duplicate], 15_000).expect("dedupe proposal");
    assert_eq!(state.tasks().len(), duplicate_before);
    assert_eq!(merged[0].id, duplicate_source.id);

    let failure_task = state
        .tasks()
        .iter()
        .find(|task| task.id != first_task && task.status == MissionTaskStatus::Ready)
        .expect("failure test task")
        .id
        .clone();
    assert_eq!(
        record_failure_snapshot(&mut state, &failure_task, "same patch", "same test failure")
            .expect("failure one"),
        None
    );
    assert_eq!(
        record_failure_snapshot(&mut state, &failure_task, "same patch", "same test failure")
            .expect("failure two"),
        None
    );
    assert_eq!(
        record_failure_snapshot(&mut state, &failure_task, "same patch", "same test failure")
            .expect("failure three"),
        Some(ReplanReason::RepeatedFailure)
    );
    assert!(
        state
            .task(&failure_task)
            .expect("task")
            .needs_diversification
    );

    let cycle_a = propose_tasks_snapshot(
        &mut state,
        vec![MissionTaskProposal::simple("cycle-a", "src/cycle/a.rs", 3)],
        16_000,
    )
    .expect("cycle a")[0]
        .id
        .clone();
    let cycle_b = propose_tasks_snapshot(
        &mut state,
        vec![MissionTaskProposal {
            kind: "implement".into(),
            title: "cycle-b".into(),
            acceptance: vec!["cycle-b accepted".into()],
            depends_on: vec![cycle_a.clone()],
            requested_paths: vec!["src/cycle/b.rs".into()],
            priority: 3,
        }],
        16_001,
    )
    .expect("cycle b")[0]
        .id
        .clone();
    assert!(set_task_dependencies_snapshot(&mut state, &cycle_a, vec![cycle_b]).is_err());

    state.event_projection_mut().last_event_seq = 80;
    state.event_projection_mut().retained_raw_start_seq = 1;
    let events = (1..=80)
        .map(|seq| MissionEvent {
            seq,
            mission_id: state.mission().id().clone(),
            authority_revision: state.mission().authority_revision(),
            kind: if seq % 17 == 0 {
                MissionEventKind::HumanDirective
            } else {
                MissionEventKind::Finding
            },
            actor: MissionEventActor::System,
            task_id: (seq % 2 == 0).then(|| first_task.clone()),
            created_at: 20_000 + seq,
            payload_summary: format!("event-{seq}"),
        })
        .collect::<Vec<_>>();
    let assignment_id = state
        .agents()
        .iter()
        .find(|agent| agent.id == first_agent)
        .and_then(|agent| agent.binding.assignment.as_ref())
        .map(|assignment| assignment.id.clone())
        .expect("active assignment");
    let context_started = Instant::now();
    let packet =
        build_context_packet_from_events(&state, &first_agent, Some(&assignment_id), &events, 80)
            .expect("bounded context packet");
    let context_elapsed = context_started.elapsed();
    assert!(packet.relevant_events.len() <= 64);
    assert!(packet.latest_human_directives.len() <= 8);

    eprintln!(
        "task15-human-control agents=100 tasks={} question={question_elapsed:?} directive={directive_elapsed:?} context={context_elapsed:?} relevant_events={} directives={}",
        state.tasks().len(),
        packet.relevant_events.len(),
        packet.latest_human_directives.len()
    );
}

#[test]
fn task15_restart_recovery_preserves_gate_and_reassigns_expired_work_without_replay() {
    let directory = TestDirectory::new("recovery");
    let store = MissionStore::new(directory.0.clone());
    let mut state = snapshot("recovery");
    let agents = join_agents(&mut state, 2, 10_000);
    let task_id = propose_tasks_snapshot(
        &mut state,
        vec![MissionTaskProposal::simple(
            "recover task",
            "src/recover.rs",
            5,
        )],
        11_000,
    )
    .expect("task")[0]
        .id
        .clone();

    claim_task_snapshot(&mut state, &task_id, &agents[0], 12_000).expect("claim recovery task");
    let gate = request_gate_snapshot(
        &mut state,
        Some(task_id.clone()),
        MissionHumanGateKind::GitPush,
        "git-push|remote=origin|branch=main",
        "Publishing remains human gated",
        Some(&agents[0]),
        13_000,
    )
    .expect("pending human gate");
    assert_eq!(gate.status, MissionHumanGateStatus::Pending);

    store.save(&state).expect("first durable save");
    store
        .save(&state)
        .expect("second durable save creates previous copy");
    fs::write(
        store.snapshot_path(state.mission().id()),
        b"{corrupt-primary",
    )
    .expect("corrupt primary snapshot");

    let mut recovered = store
        .load(state.mission().id())
        .expect("recover snapshot")
        .expect("snapshot exists");
    assert_eq!(recovered.human_gates().len(), 1);
    assert_eq!(
        recovered.human_gates()[0].status,
        MissionHumanGateStatus::Pending
    );
    let recovered_agent = recovered
        .agents()
        .iter()
        .find(|agent| agent.id == agents[0])
        .expect("recovered agent");
    assert_eq!(recovered_agent.binding.principal_fingerprint.len(), 64);

    let expired_at = 12_000 + TASK_ASSIGNMENT_LEASE_MS + 1;
    let expired_tasks = expire_task_assignments_snapshot(&mut recovered, expired_at);
    assert_eq!(expired_tasks, vec![task_id.clone()]);
    assert_eq!(
        recovered.task(&task_id).expect("expired task").status,
        MissionTaskStatus::Ready
    );

    heartbeat_snapshot(&mut recovered, &agents[1], &principal(1), expired_at)
        .expect("second worker heartbeat");
    let reassigned = claim_task_snapshot(&mut recovered, &task_id, &agents[1], expired_at + 1)
        .expect("reassign expired task");
    assert_eq!(reassigned.task_id.as_deref(), Some(task_id.as_str()));
    assert!(claim_task_snapshot(&mut recovered, &task_id, &agents[0], expired_at + 2).is_err());
}
