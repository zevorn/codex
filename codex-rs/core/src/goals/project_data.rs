use crate::config::Config;
use chrono::DateTime;
use chrono::Utc;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::ConfigLayerStackOrdering;
use codex_exec_server::LOCAL_FS;
use codex_git_utils::resolve_root_git_project_for_trust;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use tokio::fs;

const GOAL_DIR: &str = "goal";
const RUNS_DIR: &str = "runs";
const REVIEWS_DIR: &str = "reviews";
const MEMORY_FILE: &str = "memory.md";
const GOAL_FILE: &str = "goal.md";
const STATUS_FILE: &str = "status.md";
const RUN_PREFIX: &str = "goal-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GoalProjectDataRunPolicy {
    CreateNew,
    ReuseLatest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GoalProjectData {
    goal_root: AbsolutePathBuf,
    memory_file: AbsolutePathBuf,
    run_dir: AbsolutePathBuf,
    reviews_dir: AbsolutePathBuf,
}

impl GoalProjectData {
    #[cfg(test)]
    pub(super) fn from_dot_codex_run(dot_codex: AbsolutePathBuf, run_name: &str) -> Self {
        let goal_root = dot_codex.join(GOAL_DIR);
        let run_dir = goal_root.join(RUNS_DIR).join(run_name);
        Self::from_goal_root_run(goal_root, run_dir)
    }

    fn from_goal_root_run(goal_root: AbsolutePathBuf, run_dir: AbsolutePathBuf) -> Self {
        let memory_file = goal_root.join(MEMORY_FILE);
        let reviews_dir = run_dir.join(REVIEWS_DIR);
        Self {
            goal_root,
            memory_file,
            run_dir,
            reviews_dir,
        }
    }

    pub(super) fn guidance(&self) -> String {
        let goal_root = self.goal_root.as_path().display();
        let memory_file = self.memory_file.as_path().display();
        let run_dir = self.run_dir.as_path().display();
        let goal_file = self.run_dir.join(GOAL_FILE);
        let status_file = self.run_dir.join(STATUS_FILE);
        let reviews_dir = self.reviews_dir.as_path().display();
        format!(
            "Codex goal data:\n- Use Codex-owned project goal data under `{goal_root}`. This path is derived from the top-level project `.codex` directory.\n- Shared project memory lives at `{memory_file}` and can contain reusable lessons across goals.\n- Current goal run data lives under `{run_dir}`. Inspect `{}`, `{}`, and `{reviews_dir}/round-*-review-result.md` when present.\n- For this goal review gate, do not inspect legacy or third-party goal/memory directories. Treat unresolved findings from Codex goal-data files as blockers only when they still apply to the scoped modifications.",
            goal_file.as_path().display(),
            status_file.as_path().display()
        )
    }

    pub(super) async fn record_goal_snapshot(
        &self,
        goal: &codex_state::ThreadGoal,
    ) -> std::io::Result<()> {
        fs::create_dir_all(self.reviews_dir.as_path()).await?;
        ensure_project_memory_file(self.memory_file.as_path()).await?;
        fs::write(
            self.run_dir.join(GOAL_FILE).as_path(),
            render_goal_file(goal),
        )
        .await?;
        fs::write(
            self.run_dir.join(STATUS_FILE).as_path(),
            render_status_file(goal),
        )
        .await?;
        Ok(())
    }

    pub(super) async fn record_review_output(
        &self,
        output: &ReviewOutputEvent,
    ) -> std::io::Result<AbsolutePathBuf> {
        fs::create_dir_all(self.reviews_dir.as_path()).await?;
        let round = next_review_round(self.reviews_dir.as_path()).await?;
        let path = self
            .reviews_dir
            .join(format!("round-{round:04}-review-result.md"));
        fs::write(path.as_path(), render_review_output_file(round, output)).await?;
        Ok(path)
    }
}

pub(super) async fn resolve_goal_project_data(config: &Config, goal_id: &str) -> GoalProjectData {
    let goal_root = resolve_goal_root(config).await;
    let run_dir = latest_goal_run_dir(&goal_root, goal_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| goal_root.join(RUNS_DIR).join(goal_run_dir_name(Utc::now())));
    GoalProjectData::from_goal_root_run(goal_root, run_dir)
}

pub(super) async fn record_goal_project_data(
    config: &Config,
    goal: &codex_state::ThreadGoal,
    run_policy: GoalProjectDataRunPolicy,
) -> std::io::Result<()> {
    let goal_root = resolve_goal_root(config).await;
    let run_dir = match run_policy {
        GoalProjectDataRunPolicy::CreateNew => {
            unique_goal_run_dir(&goal_root, goal.updated_at).await?
        }
        GoalProjectDataRunPolicy::ReuseLatest => {
            match latest_goal_run_dir(&goal_root, &goal.goal_id).await? {
                Some(run_dir) => run_dir,
                None => unique_goal_run_dir(&goal_root, goal.updated_at).await?,
            }
        }
    };
    GoalProjectData::from_goal_root_run(goal_root, run_dir)
        .record_goal_snapshot(goal)
        .await
}

async fn resolve_goal_root(config: &Config) -> AbsolutePathBuf {
    if let Some(dot_codex) = top_level_project_dot_codex(config) {
        return dot_codex.join(GOAL_DIR);
    }

    let project_root = resolve_root_git_project_for_trust(LOCAL_FS.as_ref(), &config.cwd)
        .await
        .unwrap_or_else(|| config.cwd.clone());
    project_root.join(".codex").join(GOAL_DIR)
}

fn top_level_project_dot_codex(config: &Config) -> Option<AbsolutePathBuf> {
    top_level_project_dot_codex_from_layers(&config.config_layer_stack)
}

fn top_level_project_dot_codex_from_layers(
    config_layer_stack: &ConfigLayerStack,
) -> Option<AbsolutePathBuf> {
    config_layer_stack
        .get_layers(
            ConfigLayerStackOrdering::LowestPrecedenceFirst,
            /*include_disabled*/ false,
        )
        .into_iter()
        .find_map(|layer| match &layer.name {
            ConfigLayerSource::Project { dot_codex_folder } => Some(dot_codex_folder.clone()),
            _ => None,
        })
}

async fn latest_goal_run_dir(
    goal_root: &AbsolutePathBuf,
    goal_id: &str,
) -> io::Result<Option<AbsolutePathBuf>> {
    let runs_dir = goal_root.join(RUNS_DIR);
    let mut entries = match fs::read_dir(runs_dir.as_path()).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let goal_id_line = render_goal_id_line(goal_id);
    let mut latest_run_name: Option<String> = None;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let Some(run_name) = entry.file_name().to_str().map(ToString::to_string) else {
            continue;
        };
        if !run_name.starts_with(RUN_PREFIX) {
            continue;
        }
        let goal_file = runs_dir.join(&run_name).join(GOAL_FILE);
        let goal_file = match fs::read_to_string(goal_file.as_path()).await {
            Ok(goal_file) => goal_file,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if !goal_file.lines().any(|line| line == goal_id_line) {
            continue;
        }
        if latest_run_name
            .as_ref()
            .is_none_or(|latest| run_name.as_str() > latest.as_str())
        {
            latest_run_name = Some(run_name);
        }
    }
    Ok(latest_run_name.map(|run_name| runs_dir.join(run_name)))
}

async fn unique_goal_run_dir(
    goal_root: &AbsolutePathBuf,
    timestamp: DateTime<Utc>,
) -> io::Result<AbsolutePathBuf> {
    let runs_dir = goal_root.join(RUNS_DIR);
    fs::create_dir_all(runs_dir.as_path()).await?;
    let base_name = goal_run_dir_name(timestamp);
    let mut candidate = runs_dir.join(&base_name);
    if !fs::try_exists(candidate.as_path()).await? {
        return Ok(candidate);
    }
    for suffix in 2.. {
        candidate = runs_dir.join(format!("{base_name}-{suffix:04}"));
        if !fs::try_exists(candidate.as_path()).await? {
            return Ok(candidate);
        }
    }
    unreachable!("unbounded suffix search should return")
}

fn goal_run_dir_name(timestamp: DateTime<Utc>) -> String {
    timestamp.format("goal-%Y-%m-%d_%H-%M-%SZ").to_string()
}

fn render_goal_id_line(goal_id: &str) -> String {
    format!("- Goal ID: `{goal_id}`")
}

async fn ensure_project_memory_file(path: &Path) -> std::io::Result<()> {
    if fs::try_exists(path).await? {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(
        path,
        "# Goal Memory\n\nProject-level reusable lessons for Codex goals.\n",
    )
    .await
}

fn render_goal_file(goal: &codex_state::ThreadGoal) -> String {
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string());
    format!(
        "# Goal\n\n{}\n- Thread ID: `{}`\n- Created: `{}`\n- Token budget: `{}`\n\n## Objective\n\n{}\n",
        render_goal_id_line(&goal.goal_id),
        goal.thread_id,
        goal.created_at.to_rfc3339(),
        token_budget,
        goal.objective
    )
}

fn render_status_file(goal: &codex_state::ThreadGoal) -> String {
    format!(
        "# Goal Status\n\n- Status: `{}`\n- Updated: `{}`\n- Tokens used: `{}`\n- Time used seconds: `{}`\n",
        goal.status.as_str(),
        goal.updated_at.to_rfc3339(),
        goal.tokens_used,
        goal.time_used_seconds
    )
}

async fn next_review_round(reviews_dir: &Path) -> std::io::Result<u32> {
    let mut max_round = 0;
    let mut entries = fs::read_dir(reviews_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(ToString::to_string) else {
            continue;
        };
        if let Some(round) = parse_review_round(&name) {
            max_round = max_round.max(round);
        }
    }
    Ok(max_round.saturating_add(1))
}

fn parse_review_round(name: &str) -> Option<u32> {
    name.strip_prefix("round-")?
        .strip_suffix("-review-result.md")?
        .parse()
        .ok()
}

fn render_review_output_file(round: u32, output: &ReviewOutputEvent) -> String {
    let mut rendered = format!(
        "# Review Round {round:04}\n\n- Overall correctness: `{}`\n- Overall confidence: `{}`\n\n## Explanation\n\n{}\n",
        output.overall_correctness, output.overall_confidence_score, output.overall_explanation
    );
    if output.findings.is_empty() {
        rendered.push_str("\n## Findings\n\nNone.\n");
    } else {
        rendered.push_str("\n## Findings\n");
        for finding in &output.findings {
            rendered.push_str(&format!(
                "\n### [{}] {}\n\n{}\n\n- File: `{}`\n- Lines: {}-{}\n- Confidence: `{}`\n",
                finding.priority,
                finding.title,
                finding.body,
                finding.code_location.absolute_file_path.display(),
                finding.code_location.line_range.start,
                finding.code_location.line_range.end,
                finding.confidence_score
            ));
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerStack;
    use codex_config::ConfigRequirements;
    use codex_config::ConfigRequirementsToml;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::ReviewCodeLocation;
    use codex_protocol::protocol::ReviewFinding;
    use codex_protocol::protocol::ReviewLineRange;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use toml::Value as TomlValue;

    #[test]
    fn guidance_uses_codex_goal_run_layout() {
        let dot_codex = AbsolutePathBuf::from_absolute_path("/repo/.codex")
            .expect("test path should be absolute");
        let project_data =
            GoalProjectData::from_dot_codex_run(dot_codex, "goal-2026-05-24_15-30-12Z");
        let guidance = project_data.guidance();

        assert!(guidance.contains("`/repo/.codex/goal`"));
        assert!(guidance.contains("`/repo/.codex/goal/memory.md`"));
        assert!(guidance.contains("`/repo/.codex/goal/runs/goal-2026-05-24_15-30-12Z/goal.md`"));
        assert!(guidance.contains("`/repo/.codex/goal/runs/goal-2026-05-24_15-30-12Z/status.md`"));
        assert!(guidance.contains(
            "`/repo/.codex/goal/runs/goal-2026-05-24_15-30-12Z/reviews/round-*-review-result.md`"
        ));
        assert!(guidance.contains("do not inspect legacy or third-party"));
        assert!(!guidance.contains(&["humani", "ze"].concat()));
    }

    #[tokio::test]
    async fn record_goal_snapshot_writes_shared_memory_and_run_files() {
        let temp = tempdir().expect("create tempdir");
        let dot_codex = AbsolutePathBuf::from_absolute_path(temp.path().join(".codex"))
            .expect("test path should be absolute");
        let project_data =
            GoalProjectData::from_dot_codex_run(dot_codex, "goal-1970-01-01_00-00-20Z");
        let goal = test_goal("goal:1");

        project_data
            .record_goal_snapshot(&goal)
            .await
            .expect("record goal snapshot");

        let root = temp.path().join(".codex/goal");
        assert!(root.join("memory.md").exists());
        let run_dir = root.join("runs/goal-1970-01-01_00-00-20Z");
        let goal_file = std::fs::read_to_string(run_dir.join("goal.md")).expect("read goal file");
        let status_file =
            std::fs::read_to_string(run_dir.join("status.md")).expect("read status file");

        assert!(goal_file.contains("- Goal ID: `goal:1`"));
        assert!(goal_file.contains("## Objective\n\nfinish the stack"));
        assert!(status_file.contains("- Status: `active`"));
    }

    #[tokio::test]
    async fn record_review_output_uses_next_round_number() {
        let temp = tempdir().expect("create tempdir");
        let dot_codex = AbsolutePathBuf::from_absolute_path(temp.path().join(".codex"))
            .expect("test path should be absolute");
        let project_data =
            GoalProjectData::from_dot_codex_run(dot_codex, "goal-2026-05-24_15-30-12Z");
        project_data
            .record_review_output(&ReviewOutputEvent {
                findings: vec![ReviewFinding {
                    title: "Fix the gate".to_string(),
                    body: "The goal gate did not inspect status.".to_string(),
                    confidence_score: 0.9,
                    priority: 1,
                    code_location: ReviewCodeLocation {
                        absolute_file_path: PathBuf::from("/repo/src/lib.rs"),
                        line_range: ReviewLineRange { start: 3, end: 4 },
                    },
                }],
                overall_correctness: "patch is incorrect".to_string(),
                overall_explanation: "Needs another round.".to_string(),
                overall_confidence_score: 0.91,
            })
            .await
            .expect("record first review");
        let second = project_data
            .record_review_output(&ReviewOutputEvent {
                overall_correctness: "patch is correct".to_string(),
                overall_explanation: "No findings.".to_string(),
                overall_confidence_score: 0.99,
                findings: Vec::new(),
            })
            .await
            .expect("record second review");

        assert_eq!(
            second.as_path(),
            temp.path().join(
                ".codex/goal/runs/goal-2026-05-24_15-30-12Z/reviews/round-0002-review-result.md"
            )
        );
        let second = std::fs::read_to_string(second.as_path()).expect("read second review");
        assert!(second.contains("# Review Round 0002"));
        assert!(second.contains("None."));
    }

    #[tokio::test]
    async fn latest_goal_run_dir_finds_latest_matching_date_run() {
        let temp = tempdir().expect("create tempdir");
        let goal_root = AbsolutePathBuf::from_absolute_path(temp.path().join(".codex/goal"))
            .expect("test path should be absolute");
        let old_run = goal_root.join("runs/goal-2026-05-24_15-30-12Z");
        let latest_run = goal_root.join("runs/goal-2026-05-24_15-31-12Z");
        let other_goal_run = goal_root.join("runs/goal-2026-05-24_15-32-12Z");
        let legacy_run = goal_root.join("runs/goal_1");
        std::fs::create_dir_all(old_run.as_path()).expect("create old run");
        std::fs::create_dir_all(latest_run.as_path()).expect("create latest run");
        std::fs::create_dir_all(other_goal_run.as_path()).expect("create other goal run");
        std::fs::create_dir_all(legacy_run.as_path()).expect("create legacy run");
        std::fs::write(old_run.join(GOAL_FILE).as_path(), "- Goal ID: `goal:1`\n")
            .expect("write old goal");
        std::fs::write(
            latest_run.join(GOAL_FILE).as_path(),
            "- Goal ID: `goal:1`\n",
        )
        .expect("write latest goal");
        std::fs::write(
            other_goal_run.join(GOAL_FILE).as_path(),
            "- Goal ID: `goal:2`\n",
        )
        .expect("write other goal");
        std::fs::write(
            legacy_run.join(GOAL_FILE).as_path(),
            "- Goal ID: `goal:1`\n",
        )
        .expect("write legacy goal");

        let run_dir = latest_goal_run_dir(&goal_root, "goal:1")
            .await
            .expect("find latest goal run")
            .expect("matching run should exist");

        assert_eq!(latest_run, run_dir);
    }

    #[tokio::test]
    async fn unique_goal_run_dir_adds_suffix_for_same_timestamp() {
        let temp = tempdir().expect("create tempdir");
        let goal_root = AbsolutePathBuf::from_absolute_path(temp.path().join(".codex/goal"))
            .expect("test path should be absolute");
        let timestamp = Utc.timestamp_opt(1779611412, 0).single().unwrap();
        let first_run = unique_goal_run_dir(&goal_root, timestamp)
            .await
            .expect("create first run path");
        std::fs::create_dir_all(first_run.as_path()).expect("create first run");

        let second_run = unique_goal_run_dir(&goal_root, timestamp)
            .await
            .expect("create second run path");

        assert_eq!(
            goal_root.join("runs/goal-2026-05-24_08-30-12Z-0002"),
            second_run
        );
    }

    #[test]
    fn top_level_project_dot_codex_prefers_lowest_precedence_project_layer() {
        let root_dot_codex = AbsolutePathBuf::from_absolute_path("/repo/.codex")
            .expect("test path should be absolute");
        let child_dot_codex = AbsolutePathBuf::from_absolute_path("/repo/child/.codex")
            .expect("test path should be absolute");
        let stack = ConfigLayerStack::new(
            vec![
                ConfigLayerEntry::new(
                    ConfigLayerSource::Project {
                        dot_codex_folder: root_dot_codex.clone(),
                    },
                    TomlValue::Table(toml::map::Map::new()),
                ),
                ConfigLayerEntry::new(
                    ConfigLayerSource::Project {
                        dot_codex_folder: child_dot_codex,
                    },
                    TomlValue::Table(toml::map::Map::new()),
                ),
            ],
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .expect("project layers should be ordered root to child");

        let layer = top_level_project_dot_codex_from_layers(&stack);

        assert_eq!(Some(root_dot_codex), layer);
    }

    fn test_goal(goal_id: &str) -> codex_state::ThreadGoal {
        codex_state::ThreadGoal {
            thread_id: ThreadId::new(),
            goal_id: goal_id.to_string(),
            objective: "finish the stack".to_string(),
            status: codex_state::ThreadGoalStatus::Active,
            token_budget: Some(100),
            tokens_used: 10,
            time_used_seconds: 5,
            created_at: chrono::Utc.timestamp_opt(10, 0).single().unwrap(),
            updated_at: chrono::Utc.timestamp_opt(20, 0).single().unwrap(),
        }
    }
}
