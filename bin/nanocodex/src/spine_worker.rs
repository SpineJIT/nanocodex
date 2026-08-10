use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

#[cfg(test)]
use std::{future::Future, pin::Pin};

#[cfg(test)]
use tokio::sync::Notify;

use eyre::{Result, WrapErr, eyre};
use nanocodex::tools::mcp::McpHandle;
use nanocodex::{
    AgentEvents, Nanocodex, NanocodexError, Thinking, TurnCompletion, TurnControl, TurnResult,
    agent::input::{Prompt, PromptInput, UserInput},
};
use nanocodex_spine_runtime::{
    SpineAbortReason, SpineDelivery, SpineIntentRequest, SpineIntentSink, SpinePrepareFuture,
    SpineRuntime, SpineRuntimeError, SpineTransitionKind,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    app_core::{WorkerCapabilities, WorkerCapability, WorkerHandle},
    browser::ConfiguredBrowser,
    config::{AgentArgs, ConfiguredAgent, ToolCustomizer},
    mpp::MppAdapter,
    subagents::ChildAgents,
    tui::{PaneId, SpineInputLane, SubmittedPrompt, WorkerCommand, WorkerEvent},
    vm::ConfiguredVm,
    vm::VmArgs,
};

#[path = "spine_worker/family.rs"]
mod family;
#[path = "spine_worker/handoff.rs"]
mod handoff;
#[path = "spine_worker/transition.rs"]
mod transition;

use family::SpineFamily;
use handoff::{BufferedSpineInput, SpineInputHandoff};
use transition::{deliver, drive_terminal_transition};

pub(crate) struct SpineIntentChannel {
    sender: mpsc::UnboundedSender<IntentCommand>,
}

/// Rebuilds one stopped Spine node from its standard Nanocodex rollout.
///
/// The recipe is owned by the Spine binary rather than the public SDK. It
/// always reinstalls the same terminal Spine tools while letting Nanocodex
/// restore session, lineage, and prompt-cache identity from its own rollout.
#[derive(Clone)]
pub(crate) struct SpineSessionRecipe {
    config: AgentArgs,
    vm: VmArgs,
    tools: ToolCustomizer,
    codex_home: PathBuf,
    #[cfg(test)]
    resumed_builder: Option<TestResumedAgentBuilder>,
}

#[cfg(test)]
pub(crate) type TestResumedAgentFuture =
    Pin<Box<dyn Future<Output = Result<ConfiguredAgent>> + Send>>;
#[cfg(test)]
pub(crate) type TestResumedAgentBuilder =
    Arc<dyn Fn(nanocodex::agent::rollout::DurableSession) -> TestResumedAgentFuture + Send + Sync>;

impl SpineSessionRecipe {
    pub(crate) fn new(
        config: AgentArgs,
        vm: VmArgs,
        intents: Arc<SpineIntentChannel>,
        codex_home: PathBuf,
    ) -> Self {
        let intent_sink: Arc<dyn SpineIntentSink> = intents;
        let tools: ToolCustomizer = Arc::new(move |tools, agent| {
            let _ = agent;
            nanocodex_spine_runtime::with_spine_tools(tools, Arc::clone(&intent_sink))
        });
        Self {
            config,
            vm,
            tools,
            codex_home,
            #[cfg(test)]
            resumed_builder: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_resumed_builder(
        config: AgentArgs,
        vm: VmArgs,
        intents: Arc<SpineIntentChannel>,
        codex_home: PathBuf,
        resumed_builder: TestResumedAgentBuilder,
    ) -> Self {
        let mut recipe = Self::new(config, vm, intents, codex_home);
        recipe.resumed_builder = Some(resumed_builder);
        recipe
    }

    pub(crate) async fn build_root(&self) -> Result<ConfiguredAgent> {
        self.config
            .clone()
            .build_with_tool_customizer_in_codex_home(
                self.vm.clone(),
                Arc::clone(&self.tools),
                self.codex_home.clone(),
            )
            .await
    }

    pub(crate) fn load(
        &self,
        session_id: &str,
    ) -> Result<nanocodex::agent::rollout::DurableSession> {
        nanocodex::agent::rollout::RolloutConfig::new(&self.codex_home)
            .load_session(session_id)
            .wrap_err_with(|| format!("could not load Spine rollout for session {session_id}"))
    }

    pub(crate) async fn build_resumed(
        &self,
        durable: nanocodex::agent::rollout::DurableSession,
    ) -> Result<ConfiguredAgent> {
        #[cfg(test)]
        if let Some(builder) = &self.resumed_builder {
            return builder(durable).await;
        }
        self.config
            .clone()
            .build_resumed_with_tool_customizer_in_codex_home(
                durable,
                self.vm.clone(),
                Arc::clone(&self.tools),
                self.codex_home.clone(),
            )
            .await
    }
}

impl SpineIntentChannel {
    pub(crate) fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<IntentCommand>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Arc::new(Self { sender }), receiver)
    }
}

impl SpineIntentSink for SpineIntentChannel {
    fn prepare(&self, request: SpineIntentRequest) -> SpinePrepareFuture {
        let sender = self.sender.clone();
        Box::pin(async move {
            let (response, receiver) = oneshot::channel();
            sender
                .send(IntentCommand { request, response })
                .map_err(|_| SpineRuntimeError::CoordinatorStopped)?;
            receiver
                .await
                .map_err(|_| SpineRuntimeError::CoordinatorStopped)?
        })
    }
}

pub(crate) struct SpineWorker {
    runtime: Arc<SpineRuntime>,
    session_recipe: SpineSessionRecipe,
    family: SpineFamily,
    active_session_id: String,
    initial_delivery: Option<SpineDelivery>,
    manual_delivery: Option<SpineDelivery>,
    initial_status: Option<String>,
    commands: mpsc::UnboundedReceiver<WorkerCommand>,
    intents: mpsc::UnboundedReceiver<IntentCommand>,
    updates: mpsc::UnboundedSender<WorkerEvent>,
    finished: mpsc::UnboundedReceiver<FinishedTurn>,
    finished_sender: mpsc::UnboundedSender<FinishedTurn>,
    turns: VecDeque<TrackedTurn>,
    next_turn_id: u64,
    transition_in_progress: bool,
    handoff: SpineInputHandoff,
    fail_stop_transition: bool,
    delivery_faults: DeliveryFaults,
    transition_gate: TransitionGate,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryFault {
    Claim,
    PromptAcceptance,
    AcceptedSync,
}

#[cfg(test)]
#[derive(Default)]
struct DeliveryFaults {
    next: Option<DeliveryFault>,
}

#[cfg(not(test))]
#[derive(Default)]
struct DeliveryFaults;

impl DeliveryFaults {
    #[cfg(test)]
    fn normal() -> Self {
        Self::default()
    }

    #[cfg(not(test))]
    const fn normal() -> Self {
        Self
    }

    #[cfg(test)]
    const fn failing_at(next: DeliveryFault) -> Self {
        Self { next: Some(next) }
    }

    #[cfg(test)]
    fn check_claim(&mut self) -> Result<()> {
        self.check(DeliveryFault::Claim)
    }

    #[cfg(not(test))]
    const fn check_claim(&mut self) -> Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn check_prompt_acceptance(&mut self) -> Result<()> {
        self.check(DeliveryFault::PromptAcceptance)
    }

    #[cfg(not(test))]
    const fn check_prompt_acceptance(&mut self) -> Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn check_accepted_sync(&mut self) -> Result<()> {
        self.check(DeliveryFault::AcceptedSync)
    }

    #[cfg(not(test))]
    const fn check_accepted_sync(&mut self) -> Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn check(&mut self, stage: DeliveryFault) -> Result<()> {
        if self.next == Some(stage) {
            self.next = None;
            return Err(eyre!("injected Spine delivery {stage:?} failure"));
        }
        Ok(())
    }
}

#[derive(Default)]
struct TransitionGate {
    #[cfg(test)]
    block_before_fork: VecDeque<BlockingTransitionGate>,
}

#[cfg(test)]
struct BlockingTransitionGate {
    started: oneshot::Sender<()>,
    release: Arc<Notify>,
}

impl TransitionGate {
    #[cfg(test)]
    fn block_before_fork(started: oneshot::Sender<()>, release: Arc<Notify>) -> Self {
        Self {
            block_before_fork: VecDeque::from([BlockingTransitionGate { started, release }]),
        }
    }

    #[cfg(test)]
    fn with_transition_gates(gates: Vec<(oneshot::Sender<()>, Arc<Notify>)>) -> Self {
        Self {
            block_before_fork: gates
                .into_iter()
                .map(|(started, release)| BlockingTransitionGate { started, release })
                .collect(),
        }
    }

    async fn wait_before_fork(&mut self) {
        #[cfg(test)]
        if let Some(gate) = self.block_before_fork.pop_front() {
            let _ = gate.started.send(());
            gate.release.notified().await;
        }
    }
}

pub(crate) struct IntentCommand {
    request: SpineIntentRequest,
    response: oneshot::Sender<Result<(), SpineRuntimeError>>,
}

pub(crate) struct SpineWorkerInitial {
    pub(crate) initial_delivery: Option<SpineDelivery>,
    pub(crate) manual_delivery: Option<SpineDelivery>,
    pub(crate) initial_status: Option<String>,
    pub(crate) root_session_id: String,
    pub(crate) active_session_id: String,
    pub(crate) capabilities: WorkerCapabilities,
}

impl SpineWorker {
    pub(crate) fn start(
        configured: ConfiguredAgent,
        runtime: Arc<SpineRuntime>,
        intents: mpsc::UnboundedReceiver<IntentCommand>,
        session_recipe: SpineSessionRecipe,
        initial: SpineWorkerInitial,
    ) -> WorkerHandle<WorkerCommand, WorkerEvent> {
        Self::start_inner(
            configured,
            runtime,
            intents,
            session_recipe,
            initial,
            DeliveryFaults::normal(),
            TransitionGate::default(),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_delivery_fault(
        configured: ConfiguredAgent,
        runtime: Arc<SpineRuntime>,
        intents: mpsc::UnboundedReceiver<IntentCommand>,
        session_recipe: SpineSessionRecipe,
        initial: SpineWorkerInitial,
        delivery_fault: DeliveryFault,
    ) -> WorkerHandle<WorkerCommand, WorkerEvent> {
        Self::start_inner(
            configured,
            runtime,
            intents,
            session_recipe,
            initial,
            DeliveryFaults::failing_at(delivery_fault),
            TransitionGate::default(),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_transition_gate(
        configured: ConfiguredAgent,
        runtime: Arc<SpineRuntime>,
        intents: mpsc::UnboundedReceiver<IntentCommand>,
        session_recipe: SpineSessionRecipe,
        initial: SpineWorkerInitial,
        started: oneshot::Sender<()>,
        release: Arc<Notify>,
    ) -> WorkerHandle<WorkerCommand, WorkerEvent> {
        Self::start_inner(
            configured,
            runtime,
            intents,
            session_recipe,
            initial,
            DeliveryFaults::normal(),
            TransitionGate::block_before_fork(started, release),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_transition_gates(
        configured: ConfiguredAgent,
        runtime: Arc<SpineRuntime>,
        intents: mpsc::UnboundedReceiver<IntentCommand>,
        session_recipe: SpineSessionRecipe,
        initial: SpineWorkerInitial,
        gates: Vec<(oneshot::Sender<()>, Arc<Notify>)>,
    ) -> WorkerHandle<WorkerCommand, WorkerEvent> {
        Self::start_inner(
            configured,
            runtime,
            intents,
            session_recipe,
            initial,
            DeliveryFaults::normal(),
            TransitionGate::with_transition_gates(gates),
        )
    }

    fn start_inner(
        configured: ConfiguredAgent,
        runtime: Arc<SpineRuntime>,
        intents: mpsc::UnboundedReceiver<IntentCommand>,
        session_recipe: SpineSessionRecipe,
        initial: SpineWorkerInitial,
        delivery_faults: DeliveryFaults,
        transition_gate: TransitionGate,
    ) -> WorkerHandle<WorkerCommand, WorkerEvent> {
        let SpineWorkerInitial {
            initial_delivery,
            manual_delivery,
            initial_status,
            root_session_id,
            active_session_id,
            capabilities,
        } = initial;
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (updates, update_receiver) = mpsc::unbounded_channel();
        let tree_updates = updates.clone();
        if let Err(error) = runtime.set_tree_observer(Arc::new(move |snapshot| {
            let _ = tree_updates.send(WorkerEvent::SpineTreeUpdated { snapshot });
        })) {
            let _ = updates.send(WorkerEvent::SpineTreeFailed {
                error: error.to_string(),
            });
        }
        let family = SpineFamily::new(configured, updates.clone());
        let (finished_sender, finished) = mpsc::unbounded_channel();
        let worker = Self {
            runtime,
            session_recipe,
            family,
            active_session_id,
            initial_delivery,
            manual_delivery,
            initial_status,
            commands: command_receiver,
            intents,
            updates,
            finished,
            finished_sender,
            turns: VecDeque::new(),
            next_turn_id: 1,
            transition_in_progress: false,
            handoff: SpineInputHandoff::default(),
            fail_stop_transition: false,
            delivery_faults,
            transition_gate,
        };
        let task = tokio::spawn(worker.run());

        WorkerHandle::new(
            commands,
            update_receiver,
            Arc::from(root_session_id),
            capabilities,
            async move {
                task.await
                    .map_err(|error| eyre!("Spine application worker failed: {error}"))?
            },
        )
    }

    async fn run(mut self) -> Result<()> {
        if let Some(message) = self.initial_status.take() {
            let _ = self.updates.send(WorkerEvent::SpineStatus { message });
        }
        if let Some(delivery) = self.initial_delivery.take() {
            deliver(
                &self.runtime,
                &self.family,
                &self.updates,
                &self.finished_sender,
                &mut self.next_turn_id,
                &mut self.turns,
                &mut self.delivery_faults,
                delivery,
                Vec::new(),
            )
            .await?;
        }
        let outcome: Result<()> = loop {
            tokio::select! {
                Some(finished) = self.finished.recv() => {
                    self.finish_turn(finished).await?;
                }
                Some(intent) = self.intents.recv() => {
                    self.prepare_intent(intent);
                }
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        break Ok(());
                    };
                    self.handle_command(command).await?;
                }
                else => break Ok(()),
            }
        };
        let shutdown = self.family.shutdown().await;
        outcome?;
        shutdown
    }

    fn prepare_intent(&self, intent: IntentCommand) {
        let result = if self.transition_in_progress {
            Err(SpineRuntimeError::TransitionInProgress)
        } else {
            self.runtime.prepare(intent.request).map(|_| ())
        };
        let _ = intent.response.send(result);
    }

    async fn handle_command(&mut self, command: WorkerCommand) -> Result<()> {
        match command {
            WorkerCommand::Prompt {
                target,
                prompt_id,
                prompt,
            } => {
                self.submit_spine_input(target, prompt_id, prompt, SpineInputLane::Deferred)
                    .await?
            }
            WorkerCommand::SpineInput {
                target,
                id,
                prompt,
                lane,
            } => self.submit_spine_input(target, id, prompt, lane).await?,
            WorkerCommand::Steer { target, id, prompt } => {
                self.submit_spine_input(target, id, prompt, SpineInputLane::Immediate)
                    .await?
            }
            WorkerCommand::Cancel { target } => self.cancel(target).await,
            WorkerCommand::InterruptForSteers {
                target,
                prompt_id,
                steer_ids,
                prompt,
            } => {
                self.interrupt_for_steers(target, prompt_id, steer_ids, prompt)
                    .await?
            }
            WorkerCommand::SetFastMode { enabled } => self.set_fast_mode(enabled).await,
            WorkerCommand::SetThinking { thinking } => self.set_thinking(thinking).await,
            WorkerCommand::OpenBtw { .. }
            | WorkerCommand::CloseBtw { .. }
            | WorkerCommand::EditHistorical { .. }
            | WorkerCommand::SwitchMainBranch { .. }
            | WorkerCommand::McpLogin { .. }
            | WorkerCommand::McpReload { .. }
            | WorkerCommand::VoiceAgentEvent(_)
            | WorkerCommand::Voice(_) => {}
        }
        Ok(())
    }

    async fn submit_spine_input(
        &mut self,
        target: PaneId,
        id: u64,
        prompt: SubmittedPrompt,
        lane: SpineInputLane,
    ) -> Result<()> {
        if target != PaneId::Main {
            self.reject_prompt(target, id, prompt, "Spine has no BTW branch".to_owned());
            return Ok(());
        }
        if self.transition_in_progress {
            self.buffer_spine_input(id, prompt, lane);
            return Ok(());
        }
        if self.turns.is_empty() {
            return self.start_spine_input(id, prompt).await;
        }
        if lane == SpineInputLane::Deferred {
            self.buffer_spine_input(id, prompt, lane);
            return Ok(());
        }
        self.steer_or_buffer(id, prompt).await
    }

    async fn start_spine_input(&mut self, id: u64, prompt: SubmittedPrompt) -> Result<()> {
        let rejected_prompt = prompt.clone();
        let _ = self.updates.send(WorkerEvent::SpineInputStarted {
            target: PaneId::Main,
            id,
            prompt: prompt.clone(),
        });
        match self.start_turn(Some(id), prompt.into_prompt()).await {
            Ok(()) => {
                if let Some(delivery) = self.manual_delivery.clone() {
                    if let Err(error) = self.accept_manual_delivery(&delivery).await {
                        self.cancel_latest_turn().await;
                        return Err(error);
                    }
                    self.manual_delivery = None;
                }
            }
            Err(error) => self.reject_prompt(PaneId::Main, id, rejected_prompt, error.to_string()),
        }
        Ok(())
    }

    async fn steer_or_buffer(&mut self, id: u64, prompt: SubmittedPrompt) -> Result<()> {
        let Some(turn) = self.turns.back() else {
            return self.start_spine_input(id, prompt).await;
        };
        match turn.control.steer(prompt.clone().into_prompt()).await {
            Ok(()) => {
                let _ = self.updates.send(WorkerEvent::SpineInputSteering {
                    target: PaneId::Main,
                    id,
                    prompt,
                });
                let _ = self.updates.send(WorkerEvent::SteerAdmitted {
                    target: PaneId::Main,
                    id,
                });
            }
            Err(_) => self.buffer_spine_input(id, prompt, SpineInputLane::Deferred),
        }
        Ok(())
    }

    fn buffer_spine_input(&mut self, id: u64, prompt: SubmittedPrompt, lane: SpineInputLane) {
        self.handoff.buffer(BufferedSpineInput { id, prompt, lane });
        let _ = self.updates.send(WorkerEvent::SpineInputBuffered {
            target: PaneId::Main,
            id,
            lane,
        });
    }

    async fn cancel(&mut self, target: PaneId) {
        if target != PaneId::Main {
            let _ = self.updates.send(WorkerEvent::CancelSettled { target });
            return;
        }
        let Some(turn) = self.turns.back() else {
            let _ = self.updates.send(WorkerEvent::CancelSettled { target });
            return;
        };
        match turn.control.cancel().await {
            Ok(()) => {
                let _ = self.updates.send(WorkerEvent::CancelAccepted { target });
            }
            Err(error) => {
                let _ = self.updates.send(WorkerEvent::CancelFailed {
                    target,
                    error: error.to_string(),
                });
            }
        }
    }

    async fn interrupt_for_steers(
        &mut self,
        target: PaneId,
        prompt_id: u64,
        steer_ids: Vec<u64>,
        prompt: SubmittedPrompt,
    ) -> Result<()> {
        if self.turns.is_empty() {
            let _ = self
                .updates
                .send(WorkerEvent::InterruptedSteersResubmitted {
                    target,
                    prompt_id,
                    steer_ids,
                });
            return self
                .submit_spine_input(target, prompt_id, prompt, SpineInputLane::Immediate)
                .await;
        }
        self.cancel(target).await;
        let _ = self
            .updates
            .send(WorkerEvent::InterruptedSteersKept { target, prompt_id });
        Ok(())
    }

    async fn set_fast_mode(&mut self, enabled: bool) {
        let result = self.family.set_fast_mode(enabled).await;
        let event = match result {
            Ok(()) => WorkerEvent::FastModeChanged { enabled },
            Err(error) => WorkerEvent::FastModeChangeFailed {
                error: error.to_string(),
            },
        };
        let _ = self.updates.send(event);
    }

    async fn set_thinking(&mut self, thinking: Thinking) {
        let result = self.family.set_thinking(thinking).await;
        let event = match result {
            Ok(()) => WorkerEvent::ThinkingChanged { thinking },
            Err(error) => WorkerEvent::ThinkingChangeFailed {
                error: error.to_string(),
            },
        };
        let _ = self.updates.send(event);
    }

    async fn start_turn(&mut self, prompt_id: Option<u64>, prompt: Prompt) -> Result<()> {
        let agent = self.family.agent(&self.active_session_id)?;
        let session_id = self.active_session_id.clone();
        let id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        let span = tracing::Span::none();
        let _ = self.updates.send(WorkerEvent::TurnTraceStarted {
            target: PaneId::Main,
            id,
            span,
        });
        let turn = agent.prompt(prompt).await?;
        let control = turn.control();
        let finished = self.finished_sender.clone();
        tokio::spawn(async move {
            let result = turn.result().await;
            let result = match result {
                Ok(result) => match agent.flush_rollout().await {
                    Ok(()) => Ok(result),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            let _ = finished.send(FinishedTurn {
                id,
                prompt_id,
                source_session_id: session_id,
                result,
            });
        });
        self.turns.push_back(TrackedTurn { id, control });
        Ok(())
    }

    async fn accept_manual_delivery(&mut self, delivery: &SpineDelivery) -> Result<()> {
        self.delivery_faults.check_accepted_sync()?;
        self.runtime
            .accept_delivery(delivery)
            .map_err(|error| eyre!(error))
    }

    async fn cancel_latest_turn(&self) {
        if let Some(turn) = self.turns.back()
            && let Err(error) = turn.control.cancel().await
        {
            tracing::warn!(%error, "could not cancel a manually confirmed Spine delivery turn");
        }
    }

    async fn finish_turn(&mut self, finished: FinishedTurn) -> Result<()> {
        self.remove_turn(finished.id);
        let result = finished.result;
        let source_session_id = finished.source_session_id;
        let prompt_id = finished.prompt_id;
        match result {
            Ok(result) => {
                if let TurnCompletion::TerminalTool { receipt } = result.completion() {
                    self.transition_in_progress = true;
                    self.fail_stop_transition = false;
                    let outcome = drive_terminal_transition(
                        &self.runtime,
                        &self.session_recipe,
                        &mut self.family,
                        &mut self.active_session_id,
                        &mut self.fail_stop_transition,
                        &self.updates,
                        &self.finished_sender,
                        &mut self.next_turn_id,
                        &mut self.turns,
                        &mut self.delivery_faults,
                        &mut self.transition_gate,
                        &mut self.handoff,
                        &mut self.commands,
                        &mut self.intents,
                        &source_session_id,
                        receipt,
                    )
                    .await;
                    self.transition_in_progress = false;
                    if let Err(error) = outcome {
                        if self.fail_stop_transition {
                            return Err(error);
                        }
                        self.abort_prepared_for_source(
                            &source_session_id,
                            SpineAbortReason::TerminalTransitionFailed,
                        )?;
                        self.finish_prompt(PaneId::Main, prompt_id, error.to_string());
                        self.start_next_deferred_input().await?;
                        return Ok(());
                    }
                } else {
                    self.abort_prepared_for_source(
                        &source_session_id,
                        SpineAbortReason::MissingTerminalReceipt,
                    )?;
                }
                self.finish_prompt(PaneId::Main, prompt_id, String::new());
            }
            Err(NanocodexError::TurnCancelled) => {
                self.abort_prepared_for_source(
                    &source_session_id,
                    SpineAbortReason::TurnCancelled,
                )?;
                self.finish_prompt(PaneId::Main, prompt_id, String::new());
            }
            Err(error) => {
                self.abort_prepared_for_source(&source_session_id, SpineAbortReason::TurnFailed)?;
                self.finish_prompt(PaneId::Main, prompt_id, error.to_string());
            }
        }
        self.start_next_deferred_input().await?;
        Ok(())
    }

    async fn start_next_deferred_input(&mut self) -> Result<()> {
        if self.transition_in_progress || !self.turns.is_empty() {
            return Ok(());
        }
        let Some(input) = self.handoff.take_deferred() else {
            return Ok(());
        };
        self.start_spine_input(input.id, input.prompt).await
    }

    fn abort_prepared_for_source(
        &self,
        source_session_id: &str,
        reason: SpineAbortReason,
    ) -> Result<()> {
        let Some(transition) = self.runtime.pending_transition()? else {
            return Ok(());
        };
        if transition.source_session_id() == source_session_id {
            self.runtime.abort_prepared(&transition, reason, None)?;
        }
        Ok(())
    }

    fn remove_turn(&mut self, id: u64) {
        if let Some(index) = self.turns.iter().position(|turn| turn.id == id) {
            let _ = self.turns.remove(index);
        }
    }

    fn finish_prompt(&self, target: PaneId, prompt_id: Option<u64>, error: String) {
        if prompt_id.is_some() {
            let _ = self.updates.send(WorkerEvent::TurnFinished {
                target,
                main_branch_id: Some(0),
                error: (!error.is_empty()).then_some(error),
            });
        }
    }

    fn reject_prompt(
        &self,
        target: PaneId,
        prompt_id: u64,
        prompt: SubmittedPrompt,
        error: String,
    ) {
        let _ = self.updates.send(WorkerEvent::PromptRejected {
            target,
            prompt_id,
            prompt,
            error,
        });
    }
}

struct TrackedTurn {
    id: u64,
    control: TurnControl,
}

struct FinishedTurn {
    id: u64,
    prompt_id: Option<u64>,
    source_session_id: String,
    result: std::result::Result<TurnResult, NanocodexError>,
}

pub(crate) fn durable_prompt_cache_key(
    durable: &nanocodex::agent::rollout::DurableSession,
) -> Result<String> {
    serde_json::to_value(durable.snapshot())?
        .get("prompt_cache_key")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| eyre!("Spine rollout has no prompt cache key"))
}

pub(crate) const fn capabilities() -> WorkerCapabilities {
    WorkerCapabilities::empty()
        .with(WorkerCapability::Prompt)
        .with(WorkerCapability::Steer)
        .with(WorkerCapability::Cancel)
        .with(WorkerCapability::FastMode)
        .with(WorkerCapability::Thinking)
        .with(WorkerCapability::SpineInputHandoff)
}
