use super::*;

#[derive(Serialize)]
pub(super) struct RunReport {
    pub(super) job_id: uuid::Uuid,
    pub(super) job_directory: PathBuf,
    pub(super) skipped: usize,
    pub(super) run_timing: RunReportTiming,
    pub(super) summary: RunSummary,
    pub(super) attempts: Vec<AttemptOutcome>,
}

impl RunReport {
    pub(super) fn new(
        job: &HarborJob,
        mut attempts: Vec<AttemptOutcome>,
        skipped: usize,
        cold_image_and_cache: Duration,
    ) -> Self {
        attempts.sort_by(|left, right| left.trial_name().cmp(right.trial_name()));
        Self {
            job_id: job.id(),
            job_directory: job.directory().to_path_buf(),
            skipped,
            run_timing: RunReportTiming {
                cold_image_and_cache_ns: duration_ns(cold_image_and_cache),
            },
            summary: RunSummary::from_attempts(&attempts),
            attempts,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct RunReportTiming {
    pub(super) cold_image_and_cache_ns: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub(super) struct RunSummary {
    pub(super) total: usize,
    pub(super) scored: usize,
    pub(super) unscored: usize,
    pub(super) passed: usize,
    pub(super) failed: usize,
    pub(super) refused: usize,
    pub(super) errored: usize,
    pub(super) cleanup_failed: usize,
    pub(super) billing_unknown: usize,
    pub(super) billing_missing: usize,
    pub(super) known_estimated_cost_usd: Option<f64>,
    pub(super) priced_attempts: usize,
    pub(super) observed_estimated_cost_lower_bound_usd: Option<f64>,
    pub(super) observed_priced_attempts: usize,
}

impl RunSummary {
    fn from_attempts(attempts: &[AttemptOutcome]) -> Self {
        let mut summary = Self {
            total: attempts.len(),
            ..Self::default()
        };
        for attempt in attempts {
            match attempt {
                AttemptOutcome::Passed(result) => {
                    summary.scored += 1;
                    summary.passed += 1;
                    summary.record_exception(result.exception.as_ref().map(|error| error.kind));
                    summary.record_agent_snapshot(result.agent.as_ref(), true);
                    summary.record_cleanup(result.cleanup.is_failed());
                }
                AttemptOutcome::Failed(result) => {
                    summary.scored += 1;
                    summary.failed += 1;
                    summary.record_exception(result.exception.as_ref().map(|error| error.kind));
                    summary.record_agent_snapshot(result.agent.as_ref(), true);
                    summary.record_cleanup(result.cleanup.is_failed());
                }
                AttemptOutcome::Refused(failure) => {
                    summary.unscored += 1;
                    summary.refused += 1;
                    summary.errored += 1;
                    summary.record_agent_snapshot(
                        failure.agent.as_ref(),
                        failure.timing.agent_execution.is_some(),
                    );
                    summary.record_cleanup(failure.cleanup.is_failed());
                }
                AttemptOutcome::Errored(failure) => {
                    summary.unscored += 1;
                    summary.errored += 1;
                    summary.record_agent_snapshot(
                        failure.agent.as_ref(),
                        failure.timing.agent_execution.is_some(),
                    );
                    summary.record_cleanup(failure.cleanup.is_failed());
                }
            }
        }
        summary
    }

    fn record_agent(&mut self, agent: &nanocodex_eval::AgentResult) {
        self.record_estimated_cost(agent.cost_usd, agent.billing_completeness);
    }

    pub(super) fn record_exception(&mut self, kind: Option<EvalExceptionKind>) {
        self.errored += usize::from(kind.is_some());
        self.refused += usize::from(kind == Some(EvalExceptionKind::AgentSafetyRefusal));
    }

    fn record_agent_snapshot(
        &mut self,
        agent: Option<&nanocodex_eval::AgentResult>,
        expected: bool,
    ) {
        match agent {
            Some(agent) => {
                self.record_agent(agent);
                self.billing_missing += usize::from(expected && !agent.has_observed_usage());
            }
            None if expected => self.billing_missing += 1,
            None => {}
        }
    }

    pub(super) fn record_estimated_cost(
        &mut self,
        cost_usd: Option<f64>,
        billing_completeness: BillingCompleteness,
    ) {
        if let Some(cost_usd) = cost_usd {
            self.observed_estimated_cost_lower_bound_usd = Some(
                self.observed_estimated_cost_lower_bound_usd
                    .unwrap_or_default()
                    + cost_usd,
            );
            self.observed_priced_attempts += 1;
        }
        if billing_completeness == BillingCompleteness::Unknown {
            self.billing_unknown += 1;
            return;
        }
        let Some(cost_usd) = cost_usd else {
            return;
        };
        self.known_estimated_cost_usd =
            Some(self.known_estimated_cost_usd.unwrap_or_default() + cost_usd);
        self.priced_attempts += 1;
    }

    fn record_cleanup(&mut self, failed: bool) {
        self.cleanup_failed += usize::from(failed);
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", content = "details", rename_all = "snake_case")]
pub(super) enum AttemptOutcome {
    Passed(EvalResult),
    Failed(EvalResult),
    Refused(EvalFailure),
    Errored(EvalFailure),
}

impl AttemptOutcome {
    pub(super) fn from_terminal(outcome: EvalAttemptOutcome) -> Self {
        match outcome {
            EvalAttemptOutcome::Scored(result) => Self::from_result(result),
            EvalAttemptOutcome::Unscored(failure) => Self::from_failure(failure),
        }
    }

    const fn from_result(result: EvalResult) -> Self {
        match result.status {
            EvalStatus::Passed => Self::Passed(result),
            EvalStatus::Failed => Self::Failed(result),
        }
    }

    fn trial_name(&self) -> &str {
        match self {
            Self::Passed(result) | Self::Failed(result) => &result.trial_name,
            Self::Refused(failure) | Self::Errored(failure) => &failure.trial_name,
        }
    }

    fn from_failure(failure: EvalFailure) -> Self {
        if failure.exception.kind == EvalExceptionKind::AgentSafetyRefusal {
            Self::Refused(failure)
        } else {
            Self::Errored(failure)
        }
    }

    pub(super) const fn has_lifecycle_error(&self) -> bool {
        match self {
            Self::Passed(result) | Self::Failed(result) => result.exception.is_some(),
            Self::Refused(_) | Self::Errored(_) => true,
        }
    }
}

pub(super) struct Progress {
    pub(super) outcomes: Vec<AttemptOutcome>,
    pub(super) failed: usize,
}

impl Progress {
    pub(super) fn scored_results(&self) -> Vec<EvalResult> {
        scored_results(&self.outcomes)
    }
}

pub(super) fn scored_results(outcomes: &[AttemptOutcome]) -> Vec<EvalResult> {
    outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            AttemptOutcome::Passed(result) | AttemptOutcome::Failed(result) => Some(result.clone()),
            AttemptOutcome::Refused(_) | AttemptOutcome::Errored(_) => None,
        })
        .collect()
}

pub(super) async fn report_progress(
    mut events: EvalEventStream,
    mut expected_attempts: watch::Receiver<usize>,
    concurrency: usize,
    max_memory_mb: Option<u64>,
) -> Result<Progress> {
    let mut expected = *expected_attempts.borrow_and_update();
    let count = if expected == 1 { "" } else { "s" };
    if let Some(max_memory_mb) = max_memory_mb {
        eprintln!(
            "Running {expected} evaluation{count} (up to {concurrency} concurrent, \
             {max_memory_mb} MiB task-declared memory)"
        );
    } else {
        eprintln!("Running {expected} evaluation{count} (up to {concurrency} concurrent)");
    }
    let mut completed = 0;
    let mut outcomes = Vec::with_capacity(expected);
    let mut failed = 0;
    let mut expected_updates_open = true;
    while completed < expected {
        let event = if expected_updates_open {
            tokio::select! {
                update = expected_attempts.changed() => {
                    if update.is_ok() {
                        expected = *expected_attempts.borrow_and_update();
                    } else {
                        expected_updates_open = false;
                    }
                    continue;
                }
                event = events.recv() => event?,
            }
        } else {
            events.recv().await?
        }
        .ok_or_else(|| eyre!("event stream closed after {completed} of {expected} attempts"))?;
        match &event.kind {
            EvalEventKind::Completed(result) => {
                completed += 1;
                failed += usize::from(result.exception.is_some());
                let outcome = AttemptOutcome::from_result(result.as_ref().clone());
                write_progress_line(&outcome, completed, expected);
                outcomes.push(outcome);
            }
            EvalEventKind::Failed(failure) => {
                completed += 1;
                failed += 1;
                let outcome = AttemptOutcome::from_failure(failure.as_ref().clone());
                write_progress_line(&outcome, completed, expected);
                outcomes.push(outcome);
            }
            EvalEventKind::AttemptStarted { .. }
            | EvalEventKind::Agent(_)
            | EvalEventKind::VerifierStarted
            | EvalEventKind::VerifierOutput { .. }
            | EvalEventKind::VerifierCompleted(_)
            | EvalEventKind::RunCompleted { .. }
            | EvalEventKind::RunFailed { .. } => {}
        }
    }
    Ok(Progress { outcomes, failed })
}

fn write_progress_line(outcome: &AttemptOutcome, completed: usize, expected: usize) {
    match outcome {
        AttemptOutcome::Passed(result) => {
            let status = if result.exception.is_some() {
                Painted::new(format!("[PASS+ERROR {completed}/{expected}]")).yellow()
            } else {
                Painted::new(format!("[PASS {completed}/{expected}]")).green()
            };
            eprintln!(
                "{status} {} ({}){}{}",
                result.trial_name,
                result_duration(result),
                result_exception_suffix(result),
                cleanup_suffix(result.cleanup.is_failed()),
            );
        }
        AttemptOutcome::Failed(result) => {
            let status = if result.exception.is_some() {
                Painted::new(format!("[FAIL+ERROR {completed}/{expected}]")).red()
            } else {
                Painted::new(format!("[FAIL {completed}/{expected}]")).red()
            };
            eprintln!(
                "{status} {} ({}, reward={:.3}){}{}",
                result.trial_name,
                result_duration(result),
                result.verifier.rewards.values().sum::<f64>(),
                result_exception_suffix(result),
                cleanup_suffix(result.cleanup.is_failed()),
            );
        }
        AttemptOutcome::Refused(failure) => {
            let message = failure.exception.message.lines().next().unwrap_or_default();
            let status = Painted::new(format!("[REFUSED {completed}/{expected}]")).yellow();
            eprintln!(
                "{status} {} ({}): {message}{}",
                failure.trial_name,
                failure_duration(failure),
                cleanup_suffix(failure.cleanup.is_failed()),
            );
        }
        AttemptOutcome::Errored(failure) => {
            let message = failure.exception.message.lines().next().unwrap_or_default();
            let status = Painted::new(format!("[ERROR {completed}/{expected}]")).red();
            eprintln!(
                "{status} {} ({:?}, {}): {message}{}",
                failure.trial_name,
                failure.exception.kind,
                failure_duration(failure),
                cleanup_suffix(failure.cleanup.is_failed()),
            );
        }
    }
}

fn result_exception_suffix(result: &EvalResult) -> String {
    result
        .exception
        .as_ref()
        .map_or_else(String::new, |exception| {
            format!(", agent error={:?}", exception.kind)
        })
}

fn result_duration(result: &EvalResult) -> String {
    let phases = [
        Some(&result.timing.queue_wait),
        Some(&result.timing.environment_setup),
        Some(&result.timing.environment_readiness),
        Some(&result.timing.agent_setup),
        Some(&result.timing.agent_execution),
        Some(&result.timing.verifier),
        result.cleanup.agent.timing.as_ref(),
        result.cleanup.verifier.timing.as_ref(),
    ];
    format_milliseconds(sum_phase_milliseconds(phases))
}

fn failure_duration(failure: &EvalFailure) -> String {
    let phases = [
        Some(&failure.timing.queue_wait),
        failure.timing.environment_setup.as_ref(),
        failure.timing.environment_readiness.as_ref(),
        failure.timing.agent_setup.as_ref(),
        failure.timing.agent_execution.as_ref(),
        failure.timing.verifier.as_ref(),
        failure.cleanup.agent.timing.as_ref(),
        failure.cleanup.verifier.timing.as_ref(),
    ];
    format_milliseconds(sum_phase_milliseconds(phases))
}

fn sum_phase_milliseconds<'a>(phases: impl IntoIterator<Item = Option<&'a PhaseTiming>>) -> i64 {
    phases
        .into_iter()
        .flatten()
        .map(|phase| {
            phase
                .finished_at
                .signed_duration_since(phase.started_at)
                .num_milliseconds()
                .max(0)
        })
        .fold(0_i64, i64::saturating_add)
}

const fn cleanup_suffix(failed: bool) -> &'static str {
    if failed { " [cleanup failed]" } else { "" }
}

fn format_milliseconds(milliseconds: i64) -> String {
    let seconds = milliseconds / 1_000;
    let millis = milliseconds.unsigned_abs() % 1_000;
    format!("{seconds}.{millis:03}s")
}
