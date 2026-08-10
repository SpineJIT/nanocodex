use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_terminal_transition(
    runtime: &SpineRuntime,
    session_recipe: &SpineSessionRecipe,
    family: &mut SpineFamily,
    active_session_id: &mut String,
    fail_stop_transition: &mut bool,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    finished_sender: &mpsc::UnboundedSender<FinishedTurn>,
    next_turn_id: &mut u64,
    turns: &mut VecDeque<TrackedTurn>,
    delivery_faults: &mut DeliveryFaults,
    transition_gate: &mut TransitionGate,
    handoff: &mut SpineInputHandoff,
    commands: &mut mpsc::UnboundedReceiver<WorkerCommand>,
    intents: &mut mpsc::UnboundedReceiver<IntentCommand>,
    source_session_id: &str,
    receipt: &nanocodex::TerminalToolReceipt,
) -> Result<()> {
    let mut transition = Box::pin(finish_terminal_transition(
        runtime,
        session_recipe,
        family,
        active_session_id,
        fail_stop_transition,
        updates,
        transition_gate,
        source_session_id,
        receipt,
    ));
    loop {
        tokio::select! {
            result = &mut transition => {
                let delivery = result?;
                drop(transition);
                while let Ok(command) = commands.try_recv() {
                    route_transition_command(updates, handoff, command);
                }
                return deliver(
                    runtime,
                    family,
                    updates,
                    finished_sender,
                    next_turn_id,
                    turns,
                    delivery_faults,
                    delivery,
                    handoff.take_immediate(),
                ).await;
            }
            Some(intent) = intents.recv() => reject_transition_intent(intent),
            Some(command) = commands.recv() => route_transition_command(updates, handoff, command),
        }
    }
}

fn reject_transition_intent(intent: IntentCommand) {
    let _ = intent
        .response
        .send(Err(SpineRuntimeError::TransitionInProgress));
}

fn route_transition_command(
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    handoff: &mut SpineInputHandoff,
    command: WorkerCommand,
) {
    let error = SpineRuntimeError::TransitionInProgress.to_string();
    match command {
        WorkerCommand::Prompt {
            target,
            prompt_id,
            prompt,
        } => buffer_transition_input(
            updates,
            handoff,
            target,
            prompt_id,
            prompt,
            SpineInputLane::Deferred,
        ),
        WorkerCommand::SpineInput {
            target,
            id,
            prompt,
            lane,
        } => buffer_transition_input(updates, handoff, target, id, prompt, lane),
        WorkerCommand::Steer { target, id, prompt } => {
            buffer_transition_input(
                updates,
                handoff,
                target,
                id,
                prompt,
                SpineInputLane::Immediate,
            );
        }
        WorkerCommand::Cancel { target } => {
            let _ = updates.send(WorkerEvent::CancelFailed { target, error });
        }
        WorkerCommand::InterruptForSteers {
            target, prompt_id, ..
        } => {
            let _ = updates.send(WorkerEvent::InterruptedSteersKept { target, prompt_id });
        }
        WorkerCommand::SetFastMode { .. } => {
            let _ = updates.send(WorkerEvent::FastModeChangeFailed { error });
        }
        WorkerCommand::SetThinking { .. } => {
            let _ = updates.send(WorkerEvent::ThinkingChangeFailed { error });
        }
        WorkerCommand::OpenBtw { .. }
        | WorkerCommand::CloseBtw { .. }
        | WorkerCommand::EditHistorical { .. }
        | WorkerCommand::SwitchMainBranch { .. }
        | WorkerCommand::McpLogin { .. }
        | WorkerCommand::McpReload { .. }
        | WorkerCommand::VoiceAgentEvent(_)
        | WorkerCommand::Voice(_) => {}
    }
}

fn buffer_transition_input(
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    handoff: &mut SpineInputHandoff,
    target: PaneId,
    id: u64,
    prompt: SubmittedPrompt,
    lane: SpineInputLane,
) {
    if target != PaneId::Main {
        let _ = updates.send(WorkerEvent::PromptRejected {
            target,
            prompt_id: id,
            prompt,
            error: "Spine has no BTW branch".to_owned(),
        });
        return;
    }
    handoff.buffer(BufferedSpineInput { id, prompt, lane });
    let _ = updates.send(WorkerEvent::SpineInputBuffered { target, id, lane });
}

#[allow(clippy::too_many_arguments)]
async fn finish_terminal_transition(
    runtime: &SpineRuntime,
    session_recipe: &SpineSessionRecipe,
    family: &mut SpineFamily,
    active_session_id: &mut String,
    fail_stop_transition: &mut bool,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    transition_gate: &mut TransitionGate,
    source_session_id: &str,
    receipt: &nanocodex::TerminalToolReceipt,
) -> Result<SpineDelivery> {
    let transition = runtime.transition_for_receipt(source_session_id, receipt)?;
    transition_gate.wait_before_fork().await;
    let delivery_id = format!("delivery-{}", Uuid::now_v7());
    let delivery = match transition.kind() {
        SpineTransitionKind::Open => {
            let child_session_id = family.fork(source_session_id).await?;
            let delivery = commit_transition(
                runtime,
                &transition,
                child_session_id.clone(),
                None,
                delivery_id,
                fail_stop_transition,
            )?;
            *active_session_id = child_session_id;
            delivery
        }
        SpineTransitionKind::Close => {
            let parent_session_id = transition
                .parent_session_id()
                .ok_or_else(|| eyre!("Spine close has no parent session"))?
                .to_owned();
            if !family.contains(&parent_session_id) {
                let parent = load_validated_session(runtime, session_recipe, &parent_session_id)?;
                let delivery = commit_transition(
                    runtime,
                    &transition,
                    parent_session_id.clone(),
                    Some(source_session_id.to_owned()),
                    delivery_id,
                    fail_stop_transition,
                )?;
                *active_session_id = parent_session_id;
                shutdown_and_drop_current_family(family, updates).await;
                *family = restore_family(session_recipe, updates, parent).await?;
                return Ok(delivery);
            }
            let delivery = commit_transition(
                runtime,
                &transition,
                parent_session_id.clone(),
                Some(source_session_id.to_owned()),
                delivery_id,
                fail_stop_transition,
            )?;
            *active_session_id = parent_session_id;
            family.shutdown_closed(source_session_id).await;
            delivery
        }
        SpineTransitionKind::Next => {
            let parent_session_id = transition
                .parent_session_id()
                .ok_or_else(|| eyre!("Spine next has no parent session"))?
                .to_owned();
            if !family.contains(&parent_session_id) {
                let durable_parent =
                    load_validated_session(runtime, session_recipe, &parent_session_id)?;
                *fail_stop_transition = true;
                shutdown_and_drop_current_family(family, updates).await;
                let mut parent = restore_family(session_recipe, updates, durable_parent).await?;
                let sibling_session_id = match parent.fork(&parent_session_id).await {
                    Ok(session_id) => session_id,
                    Err(error) => {
                        let _ = parent.shutdown().await;
                        return Err(error);
                    }
                };
                let delivery = match commit_transition(
                    runtime,
                    &transition,
                    sibling_session_id.clone(),
                    Some(source_session_id.to_owned()),
                    delivery_id,
                    fail_stop_transition,
                ) {
                    Ok(delivery) => delivery,
                    Err(error) => {
                        let _ = parent.shutdown().await;
                        return Err(error);
                    }
                };
                *active_session_id = sibling_session_id;
                *family = parent;
                return Ok(delivery);
            }
            let sibling_session_id = family.fork(&parent_session_id).await?;
            let delivery = commit_transition(
                runtime,
                &transition,
                sibling_session_id.clone(),
                Some(source_session_id.to_owned()),
                delivery_id,
                fail_stop_transition,
            )?;
            *active_session_id = sibling_session_id;
            family.shutdown_closed(source_session_id).await;
            delivery
        }
    };
    Ok(delivery)
}

fn commit_transition(
    runtime: &SpineRuntime,
    transition: &nanocodex_spine_runtime::SpineTransition,
    active_session_id: String,
    closed_session_id: Option<String>,
    delivery_id: String,
    fail_stop_transition: &mut bool,
) -> Result<SpineDelivery> {
    let delivery = runtime.commit(
        transition,
        active_session_id,
        closed_session_id,
        delivery_id,
    )?;
    *fail_stop_transition = true;
    Ok(delivery)
}

fn load_validated_session(
    runtime: &SpineRuntime,
    session_recipe: &SpineSessionRecipe,
    session_id: &str,
) -> Result<nanocodex::agent::rollout::DurableSession> {
    let durable = session_recipe.load(session_id)?;
    if durable.thread_id() != session_id {
        return Err(eyre!(
            "restored Spine session ID does not match the requested journal session"
        ));
    }
    let expected_cache_key = runtime.prompt_cache_key()?;
    let cache_key = durable_prompt_cache_key(&durable)?;
    if cache_key != expected_cache_key {
        return Err(eyre!(
            "restored Spine session prompt cache key does not match the root journal"
        ));
    }
    Ok(durable)
}

async fn restore_family(
    session_recipe: &SpineSessionRecipe,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    durable: nanocodex::agent::rollout::DurableSession,
) -> Result<SpineFamily> {
    let session_id = durable.thread_id().to_owned();
    let configured = session_recipe.build_resumed(durable).await?;
    if configured.handle.session_id().to_string() != session_id {
        return Err(eyre!(
            "restored Spine session ID does not match the requested journal session"
        ));
    }
    Ok(SpineFamily::new(configured, updates.clone()))
}

async fn shutdown_and_drop_current_family(
    family: &mut SpineFamily,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
) {
    let previous = std::mem::replace(family, SpineFamily::empty(updates.clone()));
    if let Err(error) = previous.shutdown().await {
        let _ = updates.send(WorkerEvent::SpineTreeFailed {
            error: format!("previous Spine session cleanup failed: {error}"),
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn deliver(
    runtime: &SpineRuntime,
    family: &SpineFamily,
    updates: &mpsc::UnboundedSender<WorkerEvent>,
    finished_sender: &mpsc::UnboundedSender<FinishedTurn>,
    next_turn_id: &mut u64,
    turns: &mut VecDeque<TrackedTurn>,
    delivery_faults: &mut DeliveryFaults,
    delivery: SpineDelivery,
    immediate_inputs: Vec<BufferedSpineInput>,
) -> Result<()> {
    delivery_faults.check_claim()?;
    runtime.claim_delivery(&delivery)?;
    let displayed_prompt = runtime.delivery_prompt(&delivery)?;
    let prompt = delivery_prompt_with_immediate_inputs(
        Prompt::new(displayed_prompt.clone()),
        &immediate_inputs,
    );
    let agent = family.agent(delivery.target_session_id())?;
    let session_id = delivery.target_session_id().to_owned();
    let id = *next_turn_id;
    *next_turn_id = next_turn_id.saturating_add(1);
    delivery_faults.check_prompt_acceptance()?;
    let turn = agent.prompt(prompt).await?;
    let control = turn.control();
    delivery_faults.check_accepted_sync()?;
    if let Err(error) = runtime.accept_delivery(&delivery) {
        let _ = control.cancel().await;
        return Err(error.into());
    }
    let finished = finished_sender.clone();
    tokio::spawn(async move {
        let result = match turn.result().await {
            Ok(result) => match agent.flush_rollout().await {
                Ok(()) => Ok(result),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let _ = finished.send(FinishedTurn {
            id,
            prompt_id: None,
            source_session_id: session_id,
            result,
        });
    });
    turns.push_back(TrackedTurn { id, control });
    let _ = updates.send(WorkerEvent::SpineContinuation {
        delivery_id: delivery.id().to_owned(),
        prompt: displayed_prompt,
    });
    for input in immediate_inputs {
        let _ = updates.send(WorkerEvent::SpineInputDelivered {
            target: PaneId::Main,
            id: input.id,
            prompt: input.prompt,
        });
    }
    Ok(())
}

fn delivery_prompt_with_immediate_inputs(prompt: Prompt, inputs: &[BufferedSpineInput]) -> Prompt {
    if inputs.is_empty() {
        return prompt;
    }
    let mut content = match prompt.instruction {
        PromptInput::Text(text) => vec![UserInput::Text { text }],
        PromptInput::Content(content) => content,
    };
    for input in inputs {
        let prompt = input.prompt.clone().into_prompt();
        match prompt.instruction {
            PromptInput::Text(text) => content.push(UserInput::Text { text }),
            PromptInput::Content(mut items) => content.append(&mut items),
        }
    }
    Prompt::content(content)
}
