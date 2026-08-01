use super::*;

pub(super) fn resolve_rerun_source(eval: &Run) -> Result<RerunSelection> {
    let job = match &eval.retry.rerun_from {
        Some(job) => resolve_job_path(job, eval.output.as_deref())?,
        None => latest_completed_job(eval.output.as_deref())?,
    };
    if !job.join("result.json").is_file() {
        return Err(eyre!(
            "rerun source is not a completed Evaluator job: {}",
            job.display()
        ));
    }
    let matcher = retry_matcher(&eval.retry)?;
    let queue = retained_retry_task_names(
        &job,
        eval.retry.statuses.include_refused,
        eval.retry.statuses.include_errored,
        matcher.as_ref(),
    )?;
    let tasks = retained_retry_task_roots(&queue.lineage, &queue.task_names)?;
    if tasks.is_empty() {
        let filter = if eval.retry.match_task.is_empty() && eval.retry.names.is_empty() {
            String::new()
        } else {
            format!(
                " matching names {:?} or regular expressions {:?}",
                eval.retry.names, eval.retry.match_task
            )
        };
        return Err(eyre!(
            "no unresolved tasks{filter}; inspect the queue with \
             `nanocodex eval --rerun --list`"
        ));
    }
    eprintln!(
        "{}",
        retry_selection_summary(eval, &queue, &job, tasks.len())
    );
    if !eval.retry.list && !eval.json {
        for task in &tasks {
            eprintln!("  {}", short_task_name(Task::load(task)?.name()));
        }
    }
    Ok(RerunSelection { job, tasks })
}

pub(super) fn retry_matcher(retry: &RetryArgs) -> Result<Option<RegexSet>> {
    let mut patterns = retry.match_task.clone();
    patterns.extend(retry.names.iter().map(|name| regex::escape(name)));
    (!patterns.is_empty())
        .then(|| RegexSet::new(patterns))
        .transpose()
        .map_err(Into::into)
}

pub(super) fn retry_selection_summary(
    eval: &Run,
    queue: &RetainedRetryQueue,
    job: &Path,
    selected: usize,
) -> String {
    let run = if queue.lineage.len() == 1 {
        "run"
    } else {
        "runs"
    };
    let job = job
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<job>");
    if eval.retry.list {
        if selected == queue.unresolved_tasks {
            format!(
                "{} unresolved task{} across {} {run} (latest {job})",
                queue.unresolved_tasks,
                if queue.unresolved_tasks == 1 { "" } else { "s" },
                queue.lineage.len()
            )
        } else {
            format!(
                "{selected} selected of {} unresolved tasks across {} {run} (latest {job})",
                queue.unresolved_tasks,
                queue.lineage.len()
            )
        }
    } else {
        format!(
            "Retrying {selected} of {} unresolved task{} across {} {run} (latest {job})",
            queue.unresolved_tasks,
            if queue.unresolved_tasks == 1 { "" } else { "s" },
            queue.lineage.len()
        )
    }
}

pub(super) fn write_task_names(tasks: &[PathBuf], json: bool) -> Result<()> {
    let names = tasks
        .iter()
        .map(|task| Task::load(task).map(|task| task.name().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        serde_json::to_writer_pretty(io::stdout().lock(), &names)?;
        println!();
    } else {
        for name in names {
            println!("{}", short_task_name(&name));
        }
    }
    Ok(())
}

pub(super) fn short_task_name(name: &str) -> &str {
    name.rsplit_once('/').map_or(name, |(_, name)| name)
}

pub(super) fn latest_completed_job(output: Option<&Path>) -> Result<PathBuf> {
    if let Some(job) = completed_job_from_last_run(output, Path::new(LAST_RUN_FILE)) {
        return Ok(job);
    }
    let current = std::env::current_dir()?;
    let mut roots = vec![output.map_or_else(|| current.clone(), Path::to_path_buf)];
    if output.is_none() {
        roots.extend(
            fs::read_dir(&current)?
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_type().ok()?.is_dir().then_some(entry.path())),
        );
    }
    let mut candidates = Vec::new();
    for root in roots {
        collect_completed_job(&root, &mut candidates);
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                collect_completed_job(&entry.path(), &mut candidates);
            }
        }
    }
    candidates.sort_unstable_by_key(|(started_at, _)| *started_at);
    candidates.pop().map(|(_, job)| job).ok_or_else(|| {
        eyre!("no completed Evaluator job was found; run an eval or pass --rerun-from <JOB>")
    })
}

pub(super) fn completed_job_from_last_run(
    output: Option<&Path>,
    last_run: &Path,
) -> Option<PathBuf> {
    if let Ok(retained) = read_json::<LastRun>(last_run)
        && let Ok(job) = resolve_job_path(&retained.job, output)
        && job.join("result.json").is_file()
    {
        return Some(job);
    }
    None
}

pub(super) fn collect_completed_job(
    directory: &Path,
    candidates: &mut Vec<(DateTime<Utc>, PathBuf)>,
) {
    if !directory.join("result.json").is_file() || !directory.join("run.json").is_file() {
        return;
    }
    let Ok(identity) = read_json::<RetainedJobIdentity>(&directory.join("job.json")) else {
        return;
    };
    let Ok(directory) = fs::canonicalize(directory) else {
        return;
    };
    candidates.push((identity.started_at, directory));
}

pub(super) fn resolve_job_path(job: &Path, output: Option<&Path>) -> Result<PathBuf> {
    let candidate = if job.is_dir() {
        job.to_path_buf()
    } else if job.components().count() == 1 {
        output
            .unwrap_or_else(|| Path::new(DEFAULT_OUTPUT_DIRECTORY))
            .join(job)
    } else {
        job.to_path_buf()
    };
    fs::canonicalize(&candidate).map_err(|error| {
        eyre!(
            "Evaluator job does not exist: {}: {error}",
            candidate.display()
        )
    })
}

pub(super) fn retained_retry_task_names(
    job: &Path,
    include_refused: bool,
    include_errored: bool,
    matcher: Option<&RegexSet>,
) -> Result<RetainedRetryQueue> {
    let lineage = retained_retry_lineage(job)?;
    let mut statuses = BTreeMap::new();
    for job in &lineage {
        for (task_name, status) in retained_task_statuses(job)? {
            statuses.insert(task_name, status);
        }
    }
    let retryable_names = statuses
        .into_iter()
        .filter_map(|(task_name, status)| {
            // A pass at any repetition resolves the task. Otherwise, verifier
            // failures are retried by default and the opt-in lifecycle axes
            // independently select refusals and errors.
            let retryable = !status.passed
                && (status.failed
                    || (include_refused && status.refused)
                    || (include_errored && status.errored));
            retryable.then_some(task_name)
        })
        .collect::<BTreeSet<_>>();
    let unresolved_tasks = retryable_names.len();
    let task_names = retryable_names
        .into_iter()
        .filter(|task_name| matcher.is_none_or(|matcher| matcher.is_match(task_name)))
        .collect();
    Ok(RetainedRetryQueue {
        task_names,
        unresolved_tasks,
        lineage,
    })
}

pub(super) fn retained_retry_task_roots(
    lineage: &[PathBuf],
    selected_names: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeMap::new();
    for job in lineage {
        let retained: RetainedRun = read_json(&job.join("run.json"))?;
        for retained_task in retained.tasks {
            let task = Task::load(&retained_task.root)?;
            if selected_names.contains(task.name()) {
                roots.insert(task.name().to_owned(), retained_task.root);
            }
        }
    }
    let missing = selected_names
        .iter()
        .filter(|name| !roots.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(eyre!(
            "retry lineage does not retain task definitions for {}",
            missing.join(", ")
        ));
    }
    Ok(roots.into_values().collect())
}

pub(super) fn retained_retry_lineage(job: &Path) -> Result<Vec<PathBuf>> {
    let mut current = fs::canonicalize(job)?;
    let mut seen = BTreeSet::new();
    let mut lineage = Vec::new();
    loop {
        if !seen.insert(current.clone()) {
            return Err(eyre!(
                "retry lineage contains a cycle at {}",
                current.display()
            ));
        }
        lineage.push(current.clone());
        let Some(parent) = load_required_invocation(&current)?.rerun_from else {
            break;
        };
        current = fs::canonicalize(&parent).map_err(|error| {
            eyre!(
                "retry parent {} recorded by {} is unavailable: {error}",
                parent.display(),
                current.join(INVOCATION_FILE).display()
            )
        })?;
    }
    lineage.reverse();
    Ok(lineage)
}

pub(super) fn retained_task_statuses(job: &Path) -> Result<BTreeMap<String, RetainedTrialStatus>> {
    let mut statuses: BTreeMap<String, RetainedTrialStatus> = BTreeMap::new();
    for entry in fs::read_dir(job)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let result_path = entry.path().join("result.json");
        if !result_path.is_file() {
            continue;
        }
        let result: RetainedTrialResult = read_json(&result_path)?;
        let status = result.status();
        statuses
            .entry(result.task_name)
            .and_modify(|retained| *retained = retained.merge(status))
            .or_insert(status);
    }
    Ok(statuses)
}

impl RetainedTrialResult {
    fn status(&self) -> RetainedTrialStatus {
        let scored = self.scored;
        let passed = scored
            && self
                .verifier_result
                .as_ref()
                .is_some_and(|verifier| verifier.rewards.values().all(|reward| *reward > 0.0));
        let exception_type = self
            .exception_info
            .as_ref()
            .map(|exception| exception.exception_type.as_str());
        let (refused, errored) = match exception_type {
            Some(exception) => (
                exception == "AgentSafetyRefusalError",
                exception != "CleanupError",
            ),
            None => (
                self.outcome == EvalOutcome::SafetyRefusal,
                matches!(
                    self.outcome,
                    EvalOutcome::SafetyRefusal
                        | EvalOutcome::AgentTimeout
                        | EvalOutcome::InfrastructureError
                ),
            ),
        };
        RetainedTrialStatus {
            passed,
            failed: scored && !passed,
            refused,
            errored,
        }
    }
}

impl RetainedTrialStatus {
    const fn merge(self, other: Self) -> Self {
        Self {
            passed: self.passed || other.passed,
            failed: self.failed || other.failed,
            refused: self.refused || other.refused,
            errored: self.errored || other.errored,
        }
    }
}

pub(super) fn load_invocation(job: &Path) -> Result<Option<RunInvocation>> {
    let path = job.join(INVOCATION_FILE);
    match fs::read(&path) {
        Ok(contents) => {
            let invocation: RunInvocation = serde_json::from_slice(&contents)?;
            if invocation.version != INVOCATION_VERSION {
                return Err(eyre!(
                    "unsupported Evaluator invocation version {} in {}",
                    invocation.version,
                    path.display()
                ));
            }
            Ok(Some(invocation))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn load_required_invocation(job: &Path) -> Result<RunInvocation> {
    load_invocation(job)?.ok_or_else(|| {
        eyre!(
            "unsupported retained evaluation in {}; start a new job",
            job.display()
        )
    })
}

pub(super) fn persist_invocation(job: &Path, invocation: &RunInvocation) -> Result<()> {
    let path = job.join(INVOCATION_FILE);
    if path.is_file() {
        let retained: RunInvocation = read_json(&path)?;
        if retained == *invocation {
            return Ok(());
        }
        if !retained.same_workload(invocation) {
            return Err(eyre!(
                "retry invocation conflicts with durable {}",
                path.display()
            ));
        }
        info!(
            target: "nanocodex_eval",
            previous_concurrency = retained.concurrency,
            concurrency = invocation.concurrency,
            previous_max_memory_mb = retained.max_memory_mb,
            max_memory_mb = invocation.max_memory_mb,
            "updated scheduling for resumed evaluation"
        );
    }
    write_json_atomic(&path, invocation)
}

pub(super) fn record_last_run(job: &Path) -> Result<()> {
    let job = fs::canonicalize(job)?;
    write_json_atomic(Path::new(LAST_RUN_FILE), &LastRun { job })
}

pub(super) fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub(super) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("JSON path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .map(|_| ())
        .map_err(Into::into)
}
