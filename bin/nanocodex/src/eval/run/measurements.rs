use super::*;

pub(super) struct RunMeasurements {
    pub(super) observability: Duration,
    pub(super) task_loading: Duration,
    pub(super) vm_runtime: Duration,
    pub(super) vm_environments: Duration,
    pub(super) evaluation_setup: Duration,
    pub(super) attempts: Duration,
    pub(super) harbor_finish: Duration,
    pub(super) output: Duration,
    pub(super) total: Duration,
}

#[derive(Default)]
struct AttemptMeasurementTotals {
    runtime_complete_attempts: usize,
    runtime_lower_bound_attempts: usize,
    runtime_missing_attempts: usize,
    usage_complete_attempts: usize,
    usage_lower_bound_attempts: usize,
    usage_missing_attempts: usize,
    exact_model_ns: u64,
    exact_warmup_ns: u64,
    exact_tool_work_ns: u64,
    exact_tool_wall_ns: u64,
    exact_response_retries: u64,
    exact_input_tokens: u64,
    exact_cached_input_tokens: u64,
    observed_model_ns: u64,
    observed_warmup_ns: u64,
    observed_tool_work_ns: u64,
    observed_tool_wall_ns: u64,
    observed_response_retries: u64,
    observed_input_tokens: u64,
    observed_cached_input_tokens: u64,
}

impl AttemptMeasurementTotals {
    fn from_results(results: &[EvalResult]) -> Self {
        let mut totals = Self::default();
        for result in results {
            let Some(agent) = result.agent.as_ref() else {
                totals.runtime_missing_attempts = totals.runtime_missing_attempts.saturating_add(1);
                totals.usage_missing_attempts = totals.usage_missing_attempts.saturating_add(1);
                continue;
            };
            let metadata = &agent.metadata;
            totals.observed_model_ns = totals
                .observed_model_ns
                .saturating_add(metadata.model_duration_ns);
            totals.observed_warmup_ns = totals
                .observed_warmup_ns
                .saturating_add(metadata.warmup_duration_ns);
            totals.observed_tool_work_ns = totals
                .observed_tool_work_ns
                .saturating_add(metadata.tool_work_duration_ns);
            totals.observed_tool_wall_ns = totals
                .observed_tool_wall_ns
                .saturating_add(metadata.tool_wall_duration_ns);
            totals.observed_response_retries = totals
                .observed_response_retries
                .saturating_add(u64::from(metadata.response_retries));
            if metadata.runtime_completeness == MeasurementCompleteness::Complete {
                totals.runtime_complete_attempts =
                    totals.runtime_complete_attempts.saturating_add(1);
                totals.exact_model_ns = totals
                    .exact_model_ns
                    .saturating_add(metadata.model_duration_ns);
                totals.exact_warmup_ns = totals
                    .exact_warmup_ns
                    .saturating_add(metadata.warmup_duration_ns);
                totals.exact_tool_work_ns = totals
                    .exact_tool_work_ns
                    .saturating_add(metadata.tool_work_duration_ns);
                totals.exact_tool_wall_ns = totals
                    .exact_tool_wall_ns
                    .saturating_add(metadata.tool_wall_duration_ns);
                totals.exact_response_retries = totals
                    .exact_response_retries
                    .saturating_add(u64::from(metadata.response_retries));
            } else {
                totals.runtime_lower_bound_attempts =
                    totals.runtime_lower_bound_attempts.saturating_add(1);
            }

            if !agent.has_observed_usage() {
                totals.usage_missing_attempts = totals.usage_missing_attempts.saturating_add(1);
                continue;
            }
            totals.observed_input_tokens = totals
                .observed_input_tokens
                .saturating_add(agent.usage.input_tokens);
            totals.observed_cached_input_tokens = totals
                .observed_cached_input_tokens
                .saturating_add(agent.usage.cached_input_tokens);
            if agent.billing_completeness == BillingCompleteness::Complete {
                totals.usage_complete_attempts = totals.usage_complete_attempts.saturating_add(1);
                totals.exact_input_tokens = totals
                    .exact_input_tokens
                    .saturating_add(agent.usage.input_tokens);
                totals.exact_cached_input_tokens = totals
                    .exact_cached_input_tokens
                    .saturating_add(agent.usage.cached_input_tokens);
            } else {
                totals.usage_lower_bound_attempts =
                    totals.usage_lower_bound_attempts.saturating_add(1);
            }
        }
        totals
    }
}

#[derive(Serialize)]
struct RetainedRunMeasurements {
    schema_version: u32,
    observability_ns: u64,
    task_loading_ns: u64,
    vm_runtime_build_ns: u64,
    cold_image_and_cache_ns: u64,
    evaluation_setup_ns: u64,
    attempts_wall_ns: u64,
    harbor_finish_ns: u64,
    output_ns: u64,
    total_wall_ns: u64,
}

impl RunMeasurements {
    pub(super) fn persist(&self, job: &Path) -> Result<()> {
        write_json_atomic(
            &job.join("timing.json"),
            &RetainedRunMeasurements {
                schema_version: 1,
                observability_ns: duration_ns(self.observability),
                task_loading_ns: duration_ns(self.task_loading),
                vm_runtime_build_ns: duration_ns(self.vm_runtime),
                cold_image_and_cache_ns: duration_ns(self.vm_environments),
                evaluation_setup_ns: duration_ns(self.evaluation_setup),
                attempts_wall_ns: duration_ns(self.attempts),
                harbor_finish_ns: duration_ns(self.harbor_finish),
                output_ns: duration_ns(self.output),
                total_wall_ns: duration_ns(self.total),
            },
        )
    }

    pub(super) fn record(
        &self,
        results: &[EvalResult],
        attempt_count: usize,
        errored_attempt_count: usize,
    ) {
        let measurements = AttemptMeasurementTotals::from_results(results);
        let verifier_ns = results
            .iter()
            .map(|result| {
                result
                    .timing
                    .verifier
                    .finished_at
                    .signed_duration_since(result.timing.verifier.started_at)
                    .num_nanoseconds()
                    .and_then(|duration| u64::try_from(duration).ok())
                    .unwrap_or_default()
            })
            .sum::<u64>();
        info!(
            target: "nanocodex_eval",
            duration_ns = duration_ns(self.total),
            observability_duration_ns = duration_ns(self.observability),
            task_loading_duration_ns = duration_ns(self.task_loading),
            vm_runtime_duration_ns = duration_ns(self.vm_runtime),
            vm_environments_duration_ns = duration_ns(self.vm_environments),
            evaluation_setup_duration_ns = duration_ns(self.evaluation_setup),
            attempts_wall_duration_ns = duration_ns(self.attempts),
            harbor_finish_duration_ns = duration_ns(self.harbor_finish),
            output_duration_ns = duration_ns(self.output),
            attempt_count,
            scored_attempt_count = results.len(),
            errored_attempt_count,
            runtime_complete_attempt_count = measurements.runtime_complete_attempts,
            runtime_lower_bound_attempt_count = measurements.runtime_lower_bound_attempts,
            runtime_missing_attempt_count = measurements.runtime_missing_attempts,
            usage_complete_attempt_count = measurements.usage_complete_attempts,
            usage_lower_bound_attempt_count = measurements.usage_lower_bound_attempts,
            usage_missing_attempt_count = measurements.usage_missing_attempts,
            attempts_model_duration_ns = measurements.exact_model_ns,
            attempts_warmup_duration_ns = measurements.exact_warmup_ns,
            attempts_tool_work_duration_ns = measurements.exact_tool_work_ns,
            attempts_tool_wall_duration_ns = measurements.exact_tool_wall_ns,
            attempts_observed_model_duration_lower_bound_ns = measurements.observed_model_ns,
            attempts_observed_warmup_duration_lower_bound_ns = measurements.observed_warmup_ns,
            attempts_observed_tool_work_duration_lower_bound_ns =
                measurements.observed_tool_work_ns,
            attempts_observed_tool_wall_duration_lower_bound_ns =
                measurements.observed_tool_wall_ns,
            attempts_verifier_duration_ns = verifier_ns,
            response_retries = measurements.exact_response_retries,
            observed_response_retries_lower_bound = measurements.observed_response_retries,
            input_tokens = measurements.exact_input_tokens,
            cached_input_tokens = measurements.exact_cached_input_tokens,
            observed_input_tokens_lower_bound = measurements.observed_input_tokens,
            observed_cached_input_tokens_lower_bound =
                measurements.observed_cached_input_tokens,
            "evaluation run completed"
        );
    }
}
