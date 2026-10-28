use serde::{Deserialize, Serialize};

use crate::models::SystemResourceSnapshot;

use super::{
    model::{MissionAssignment, MissionAssignmentKind, MissionSnapshot},
    task_workspace::TaskWorkspaceBackend,
    tasks::MissionTask,
};

const GIB: u64 = 1024 * 1024 * 1024;
const MAX_COGNITIVE_SLOTS: usize = 32;
const MAX_WRITER_SLOTS: usize = 8;
const MAX_HEAVY_PROCESS_SLOTS: usize = 2;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MissionResourceClass {
    Reasoning,
    ReadOnly,
    Writer,
    HeavyProcess,
    BrowserMutation,
    AiWorkspaceMutation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionResourceSlots {
    pub(crate) reasoning: usize,
    pub(crate) read_only: usize,
    pub(crate) writers: usize,
    pub(crate) heavy_processes: usize,
    pub(crate) browser_mutations: usize,
    pub(crate) ai_workspace_mutations: usize,
}

impl MissionResourceSlots {
    #[cfg(test)]
    pub(crate) fn for_scheduler_tests(reasoning: usize, writers: usize) -> Self {
        Self {
            reasoning,
            read_only: reasoning,
            writers,
            heavy_processes: writers,
            browser_mutations: writers.min(1),
            ai_workspace_mutations: writers.min(1),
        }
    }

    pub(crate) fn capacity(self, class: MissionResourceClass) -> usize {
        match class {
            MissionResourceClass::Reasoning => self.reasoning,
            MissionResourceClass::ReadOnly => self.read_only,
            MissionResourceClass::Writer => self.writers,
            MissionResourceClass::HeavyProcess => self.heavy_processes,
            MissionResourceClass::BrowserMutation => self.browser_mutations,
            MissionResourceClass::AiWorkspaceMutation => self.ai_workspace_mutations,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MissionResourceUsage {
    pub(crate) reasoning: usize,
    pub(crate) read_only: usize,
    pub(crate) writers: usize,
    pub(crate) heavy_processes: usize,
    pub(crate) browser_mutations: usize,
    pub(crate) ai_workspace_mutations: usize,
}

impl MissionResourceUsage {
    pub(crate) fn count(self, class: MissionResourceClass) -> usize {
        match class {
            MissionResourceClass::Reasoning => self.reasoning,
            MissionResourceClass::ReadOnly => self.read_only,
            MissionResourceClass::Writer => self.writers,
            MissionResourceClass::HeavyProcess => self.heavy_processes,
            MissionResourceClass::BrowserMutation => self.browser_mutations,
            MissionResourceClass::AiWorkspaceMutation => self.ai_workspace_mutations,
        }
    }

    pub(crate) fn increment(&mut self, class: MissionResourceClass) {
        let slot = match class {
            MissionResourceClass::Reasoning => &mut self.reasoning,
            MissionResourceClass::ReadOnly => &mut self.read_only,
            MissionResourceClass::Writer => &mut self.writers,
            MissionResourceClass::HeavyProcess => &mut self.heavy_processes,
            MissionResourceClass::BrowserMutation => &mut self.browser_mutations,
            MissionResourceClass::AiWorkspaceMutation => &mut self.ai_workspace_mutations,
        };
        *slot = slot.saturating_add(1);
    }

    pub(crate) fn remaining(
        self,
        slots: MissionResourceSlots,
        class: MissionResourceClass,
    ) -> usize {
        slots.capacity(class).saturating_sub(self.count(class))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionDecision {
    Admitted,
    DeniedCapacity,
}

pub(crate) fn admit_assignment(
    class: MissionResourceClass,
    slots: MissionResourceSlots,
    usage: MissionResourceUsage,
) -> AdmissionDecision {
    if usage.count(class) < slots.capacity(class) {
        AdmissionDecision::Admitted
    } else {
        AdmissionDecision::DeniedCapacity
    }
}

fn available_gib(resources: &SystemResourceSnapshot) -> Option<u64> {
    resources.memory_available_bytes.map(|bytes| bytes / GIB)
}

fn disk_available_gib(resources: &SystemResourceSnapshot) -> Option<u64> {
    resources
        .workspace_disk_available_bytes
        .map(|bytes| bytes / GIB)
}

pub(crate) fn derive_resource_slots(
    resources: &SystemResourceSnapshot,
    task_workspace_backend: TaskWorkspaceBackend,
) -> MissionResourceSlots {
    let cpu_known = resources.logical_cpu_count > 0;
    let cpu = resources.logical_cpu_count.clamp(1, 64);
    let memory_gib = available_gib(resources);
    let disk_gib = disk_available_gib(resources);
    let low_memory = memory_gib.is_some_and(|gib| gib < 2);
    let low_disk = disk_gib.is_some_and(|gib| gib < 5);
    let low_cpu = cpu_known && cpu <= 2;
    let overloaded = resources
        .load_average_1m
        .is_some_and(|load| load > (cpu as f64 * 0.85));

    let mut cognitive_total = if !cpu_known {
        8
    } else if low_cpu || low_memory {
        4
    } else {
        cpu.saturating_mul(2).clamp(8, MAX_COGNITIVE_SLOTS)
    };
    if overloaded {
        cognitive_total = (cognitive_total / 2).max(2);
    }
    if memory_gib.is_some_and(|gib| gib < 4) {
        cognitive_total = cognitive_total.min(4);
    }
    cognitive_total = cognitive_total.clamp(2, MAX_COGNITIVE_SLOTS);
    let reasoning = (cognitive_total / 2).max(1);
    let read_only = cognitive_total.saturating_sub(reasoning).max(1);

    let heavy_processes = if low_cpu || low_memory || low_disk || overloaded {
        1
    } else if cpu >= 6 && memory_gib.unwrap_or(8) >= 6 {
        MAX_HEAVY_PROCESS_SLOTS
    } else {
        1
    };

    let writers = match task_workspace_backend {
        TaskWorkspaceBackend::SerializedDirect => 1,
        TaskWorkspaceBackend::SparseOverlay => {
            if low_cpu || low_memory || low_disk || overloaded {
                1
            } else {
                let memory_cap = match memory_gib {
                    Some(gib) if gib < 4 => 1,
                    Some(gib) if gib < 8 => 2,
                    Some(gib) if gib < 16 => 4,
                    Some(_) => MAX_WRITER_SLOTS,
                    None => 2,
                };
                (cpu / 2).max(2).min(memory_cap).min(MAX_WRITER_SLOTS)
            }
        }
    };

    MissionResourceSlots {
        reasoning,
        read_only,
        writers,
        heavy_processes,
        browser_mutations: 1,
        ai_workspace_mutations: 1,
    }
}

pub(crate) fn classify_task(task: &MissionTask) -> MissionResourceClass {
    let kind = task.kind.trim().to_ascii_lowercase();
    if kind.contains("browser") {
        MissionResourceClass::BrowserMutation
    } else if kind.contains("ai_workspace") || kind.contains("desktop") || kind.contains("gui") {
        MissionResourceClass::AiWorkspaceMutation
    } else if ["build", "compile", "test", "benchmark", "lint", "verify"]
        .iter()
        .any(|needle| kind.contains(needle))
    {
        MissionResourceClass::HeavyProcess
    } else if ["research", "read", "inspect", "analysis"]
        .iter()
        .any(|needle| kind.contains(needle))
    {
        MissionResourceClass::ReadOnly
    } else {
        MissionResourceClass::Writer
    }
}

pub(crate) fn classify_assignment(
    snapshot: &MissionSnapshot,
    assignment: &MissionAssignment,
) -> MissionResourceClass {
    match assignment.kind {
        MissionAssignmentKind::Implement => assignment
            .task_id
            .as_deref()
            .and_then(|task_id| snapshot.task(task_id))
            .map(classify_task)
            .unwrap_or(MissionResourceClass::Writer),
        MissionAssignmentKind::Plan
        | MissionAssignmentKind::Review
        | MissionAssignmentKind::Deliberate
        | MissionAssignmentKind::AnswerHuman
        | MissionAssignmentKind::Audit => MissionResourceClass::Reasoning,
    }
}

pub(crate) fn active_usage(snapshot: &MissionSnapshot, now: u64) -> MissionResourceUsage {
    let mut usage = MissionResourceUsage::default();
    for agent in snapshot.agents() {
        if agent.binding.lease_expires_at <= now {
            continue;
        }
        let Some(assignment) = agent
            .binding
            .assignment
            .as_ref()
            .filter(|assignment| assignment.lease_expires_at > now)
        else {
            continue;
        };
        usage.increment(classify_assignment(snapshot, assignment));
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mission::{
            agents::join_or_reattach_snapshot,
            model::{AgentMission, MissionId, MissionSnapshot},
            scheduler::schedule_once_snapshot,
            tasks::{propose_tasks_snapshot, MissionTaskProposal},
        },
        models::SystemResourceSnapshot,
    };

    fn system(cpu: usize, available_gib: Option<u64>, load: Option<f64>) -> SystemResourceSnapshot {
        const GIB: u64 = 1024 * 1024 * 1024;
        SystemResourceSnapshot {
            platform: "linux".into(),
            architecture: "x86_64".into(),
            logical_cpu_count: cpu,
            load_average_1m: load,
            memory_total_bytes: available_gib.map(|value| value.saturating_add(4) * GIB),
            memory_available_bytes: available_gib.map(|value| value * GIB),
            workspace_disk_total_bytes: Some(512 * GIB),
            workspace_disk_available_bytes: Some(200 * GIB),
            gpu_available: false,
            gpu_devices: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn low_resource_machine_uses_conservative_bounded_slots() {
        let slots = derive_resource_slots(
            &system(2, Some(1), Some(1.8)),
            TaskWorkspaceBackend::SerializedDirect,
        );
        assert_eq!(slots.writers, 1);
        assert_eq!(slots.browser_mutations, 1);
        assert_eq!(slots.ai_workspace_mutations, 1);
        assert!(slots.heavy_processes <= 1);
        assert!((1..=4).contains(&slots.reasoning));
        assert!((1..=4).contains(&slots.read_only));
    }

    #[test]
    fn eight_core_sixteen_gib_machine_scales_reasoning_but_keeps_costly_classes_bounded() {
        let slots = derive_resource_slots(
            &system(8, Some(12), Some(2.0)),
            TaskWorkspaceBackend::SparseOverlay,
        );
        assert!((8..=32).contains(&slots.reasoning));
        assert!((8..=32).contains(&slots.read_only));
        assert!((2..=8).contains(&slots.writers));
        assert_eq!(slots.heavy_processes, 2);
        assert_eq!(slots.browser_mutations, 1);
        assert_eq!(slots.ai_workspace_mutations, 1);
    }

    #[test]
    fn unknown_metrics_fail_to_bounded_defaults_instead_of_unlimited_admission() {
        let slots = derive_resource_slots(
            &system(0, None, None),
            TaskWorkspaceBackend::SerializedDirect,
        );
        assert!((1..=8).contains(&slots.reasoning));
        assert!((1..=8).contains(&slots.read_only));
        assert_eq!(slots.writers, 1);
        assert!((1..=2).contains(&slots.heavy_processes));
        assert_eq!(slots.browser_mutations, 1);
        assert_eq!(slots.ai_workspace_mutations, 1);
    }

    #[test]
    fn no_isolation_fallback_never_admits_parallel_writers_even_on_large_machine() {
        let slots = derive_resource_slots(
            &system(32, Some(96), Some(1.0)),
            TaskWorkspaceBackend::SerializedDirect,
        );
        assert_eq!(slots.writers, 1);
        let isolated = derive_resource_slots(
            &system(32, Some(96), Some(1.0)),
            TaskWorkspaceBackend::SparseOverlay,
        );
        assert!(isolated.writers > 1);
        assert!(isolated.writers <= 8);
    }

    #[test]
    fn admission_subtracts_existing_active_usage() {
        let slots = MissionResourceSlots {
            reasoning: 4,
            read_only: 4,
            writers: 1,
            heavy_processes: 1,
            browser_mutations: 1,
            ai_workspace_mutations: 1,
        };
        let usage = MissionResourceUsage {
            reasoning: 0,
            read_only: 0,
            writers: 1,
            heavy_processes: 0,
            browser_mutations: 0,
            ai_workspace_mutations: 0,
        };
        assert_eq!(
            admit_assignment(MissionResourceClass::Writer, slots, usage),
            AdmissionDecision::DeniedCapacity
        );
        assert_eq!(
            admit_assignment(MissionResourceClass::Reasoning, slots, usage),
            AdmissionDecision::Admitted
        );
    }

    #[test]
    fn one_hundred_registered_agents_do_not_become_one_hundred_active_writers() {
        let mut snapshot = MissionSnapshot::new(
            AgentMission::new(
                MissionId::parse("mission-resource-100").unwrap(),
                "workspace-resource".into(),
                "Use many agents without saturating the machine".into(),
                1_000,
            )
            .unwrap(),
        );
        for index in 0..100 {
            join_or_reattach_snapshot(
                &mut snapshot,
                &[format!("openai-session:resource-{index}")],
                None,
                10_000,
            )
            .unwrap();
        }
        let proposals = (0..100)
            .map(|index| {
                MissionTaskProposal::simple(
                    &format!("writer-{index}"),
                    &format!("src/resource-{index}.rs"),
                    3,
                )
            })
            .collect::<Vec<_>>();
        propose_tasks_snapshot(&mut snapshot, proposals, 10_100).unwrap();
        let slots = derive_resource_slots(
            &system(32, Some(96), Some(1.0)),
            TaskWorkspaceBackend::SerializedDirect,
        );

        let first = schedule_once_snapshot(&mut snapshot, 11_000, slots).unwrap();
        assert_eq!(first.assignments.len(), 1);
        let second = schedule_once_snapshot(&mut snapshot, 11_100, slots).unwrap();
        assert!(second.assignments.is_empty());
        let active = snapshot
            .agents()
            .iter()
            .filter(|agent| agent.binding.assignment.is_some())
            .count();
        assert_eq!(active, 1);
    }
}
