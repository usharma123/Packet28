use super::*;

pub struct ExecutionContext {
    pub request_id: u64,
    pub target: String,
    pub budget: ExecutionBudget,
    pub policy_context: Value,
    pub reducer_input: Value,
    pub(crate) memory: Arc<Mutex<PacketCache>>,
    shared: Map<String, Value>,
}

impl ExecutionContext {
    pub fn set_shared(&mut self, key: impl Into<String>, value: Value) {
        self.shared.insert(key.into(), value);
    }

    pub fn shared_value(&self, key: &str) -> Option<&Value> {
        self.shared.get(key)
    }

    pub fn shared_json(&self) -> Value {
        Value::Object(self.shared.clone())
    }

    pub fn cache_entries(&self) -> Result<Vec<context_memory_core::PacketCacheEntry>, KernelError> {
        let cache = self
            .memory
            .lock()
            .map_err(|source| KernelError::CacheLock {
                detail: source.to_string(),
            })?;
        Ok(cache.entries())
    }

    pub fn cache_recall(
        &self,
        query: &str,
        options: &RecallOptions,
    ) -> Result<Vec<RecallHit>, KernelError> {
        let cache = self
            .memory
            .lock()
            .map_err(|source| KernelError::CacheLock {
                detail: source.to_string(),
            })?;
        Ok(cache.recall(query, options))
    }
}

type ReducerFn = dyn Fn(&mut ExecutionContext, &[KernelPacket]) -> Result<ReducerResult, KernelError>
    + Send
    + Sync;

pub struct Kernel {
    reducers: HashMap<String, Arc<ReducerFn>>,
    next_request_id: AtomicU64,
    pub(crate) memory: Arc<Mutex<PacketCache>>,
    persist_config: Option<PersistConfig>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    pub fn new() -> Self {
        Self {
            reducers: HashMap::new(),
            next_request_id: AtomicU64::new(1),
            memory: Arc::new(Mutex::new(PacketCache::new())),
            persist_config: None,
        }
    }

    pub fn with_v1_reducers() -> Self {
        let mut kernel = Self::new();
        register_v1_reducers(&mut kernel);
        kernel
    }

    pub fn with_v1_reducers_and_persistence(config: PersistConfig) -> Self {
        let mut kernel = Self {
            reducers: HashMap::new(),
            next_request_id: AtomicU64::new(1),
            memory: Arc::new(Mutex::new(PacketCache::load_from_disk(&config))),
            persist_config: Some(config),
        };
        register_v1_reducers(&mut kernel);
        kernel
    }

    pub fn register_reducer<F>(&mut self, target: impl Into<String>, reducer: F)
    where
        F: Fn(&mut ExecutionContext, &[KernelPacket]) -> Result<ReducerResult, KernelError>
            + Send
            + Sync
            + 'static,
    {
        self.reducers.insert(target.into(), Arc::new(reducer));
    }

    pub fn reducer_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.reducers.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn execute(&self, req: KernelRequest) -> Result<KernelResponse, KernelError> {
        let mut hooks = NoopDeltaReuseHooks;
        self.execute_with_hooks(req, &mut hooks)
    }

    pub fn execute_with_hooks(
        &self,
        req: KernelRequest,
        hooks: &mut dyn DeltaReuseHooks,
    ) -> Result<KernelResponse, KernelError> {
        let target = req.target.trim().to_string();
        if target.is_empty() {
            return Err(KernelError::EmptyTarget);
        }

        let reducer = self
            .reducers
            .get(&target)
            .ok_or_else(|| KernelError::UnknownTarget {
                target: target.clone(),
                registered: self.reducer_names(),
            })?;

        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let input_usage = usage_for_packets(&req.input_packets);
        let policy_guard = load_policy_guard(&req.policy_context)?;
        let mut governance = GovernanceAudit {
            enabled: policy_guard.is_some(),
            config_path: policy_guard.as_ref().map(|p| p.config_path.clone()),
            ..GovernanceAudit::default()
        };

        if let Some(policy_guard) = &policy_guard {
            if should_enforce_policy_for_target(&target) {
                governance.reducer_execution = Some(enforce_reducer_execution_policy(
                    &target,
                    &policy_guard.config,
                )?);
                let input_audits =
                    audit_packets_against_policy(&policy_guard.config, &req.input_packets)?;
                ensure_policy_audits_pass(&target, "input", &input_audits)?;
                governance.input_audits = input_audits;
            }
        }

        enforce_budget(&target, BudgetStage::Input, req.budget, input_usage)?;

        let cache_lookup = if cache_enabled_for_request(&target, &req.policy_context) {
            let cache_input = cache_input_for_request(&req, &target, policy_guard.as_ref());
            Some({
                let cache = self
                    .memory
                    .lock()
                    .map_err(|source| KernelError::CacheLock {
                        detail: source.to_string(),
                    })?;
                cache.lookup_with_hooks(&target, &cache_input, hooks)
            })
        } else {
            None
        };

        if let Some(entry) = cache_lookup.as_ref().and_then(|lookup| lookup.entry.clone()) {
            let output_packets = entry
                .packets
                .into_iter()
                .map(|packet| KernelPacket {
                    packet_id: packet.packet_id,
                    format: default_packet_format(),
                    body: packet.body,
                    token_usage: packet.token_usage,
                    runtime_ms: packet.runtime_ms,
                    metadata: packet.metadata,
                })
                .collect::<Vec<_>>();
            let output_packet_count = output_packets.len();

            if let Some(policy_guard) = &policy_guard {
                if should_enforce_policy_for_target(&target) {
                    let output_audits =
                        audit_packets_against_policy(&policy_guard.config, &output_packets)?;
                    ensure_policy_audits_pass(&target, "output", &output_audits)?;
                    governance.output_audits = output_audits;
                }
            }

            let output_usage = usage_for_packets(&output_packets);
            let total_usage = BudgetUsage {
                tokens: input_usage.tokens.saturating_add(output_usage.tokens),
                bytes: input_usage.bytes.saturating_add(output_usage.bytes),
                runtime_ms: input_usage
                    .runtime_ms
                    .saturating_add(output_usage.runtime_ms),
            };
            enforce_budget(&target, BudgetStage::Total, req.budget, total_usage)?;
            let entry_age_secs = now_unix().saturating_sub(entry.created_at_unix);

            return Ok(KernelResponse {
                request_id,
                target: target.clone(),
                output_packets,
                audit: KernelAudit {
                    reducer: target,
                    input_packets: req.input_packets.len(),
                    output_packets: output_packet_count,
                    budget: req.budget,
                    input_usage,
                    output_usage,
                    total_usage,
                    governance,
                },
                metadata: merge_json(
                    entry.metadata,
                    json!({
                        "cache": {
                            "hit": true,
                            "key": cache_lookup
                                .as_ref()
                                .map(|lookup| lookup.cache_key.clone())
                                .unwrap_or_default(),
                            "entry_age_secs": entry_age_secs,
                            "miss_reason": Value::Null,
                        }
                    }),
                ),
            });
        }

        let mut ctx = ExecutionContext {
            request_id,
            target: target.clone(),
            budget: req.budget,
            policy_context: req.policy_context.clone(),
            reducer_input: req.reducer_input,
            memory: self.memory.clone(),
            shared: Map::new(),
        };

        let started_at = Instant::now();
        let reducer_result = reducer(&mut ctx, &req.input_packets)?;
        let elapsed_ms = started_at.elapsed().as_millis() as u64;
        if let Some(policy_guard) = &policy_guard {
            if should_enforce_policy_for_target(&target) {
                let output_audits = audit_packets_against_policy(
                    &policy_guard.config,
                    &reducer_result.output_packets,
                )?;
                ensure_policy_audits_pass(&target, "output", &output_audits)?;
                governance.output_audits = output_audits;
            }
        }
        let output_packet_count = reducer_result.output_packets.len();

        let output_usage = usage_for_packets(&reducer_result.output_packets);
        let total_usage = BudgetUsage {
            tokens: input_usage.tokens.saturating_add(output_usage.tokens),
            bytes: input_usage.bytes.saturating_add(output_usage.bytes),
            runtime_ms: elapsed_ms,
        };

        enforce_budget(&target, BudgetStage::Total, req.budget, total_usage)?;

        let output_packets = reducer_result.output_packets;
        let mut response = KernelResponse {
            request_id,
            target: target.clone(),
            output_packets: output_packets.clone(),
            audit: KernelAudit {
                reducer: target.clone(),
                input_packets: req.input_packets.len(),
                output_packets: output_packet_count,
                budget: req.budget,
                input_usage,
                output_usage,
                total_usage,
                governance,
            },
            metadata: merge_json(
                merge_json(ctx.shared_json(), reducer_result.metadata),
                json!({
                    "cache": {
                        "hit": false,
                        "key": cache_lookup
                            .as_ref()
                            .map(|lookup| lookup.cache_key.clone())
                            .unwrap_or_default(),
                        "entry_age_secs": Value::Null,
                        "miss_reason": "not_found",
                    }
                }),
            ),
        };

        if let Some(cache_lookup) = cache_lookup {
            let mut cache = self
                .memory
                .lock()
                .map_err(|source| KernelError::CacheLock {
                    detail: source.to_string(),
                })?;
            let packets = output_packets
                .iter()
                .map(|packet| CachePacket {
                    packet_id: packet.packet_id.clone(),
                    body: packet.body.clone(),
                    token_usage: packet.token_usage,
                    runtime_ms: packet.runtime_ms,
                    metadata: packet.metadata.clone(),
                })
                .collect();

            let metadata = response.metadata.clone();
            cache.put_with_hooks(&target, &cache_lookup, packets, metadata, hooks);
            if let Some(persist_config) = &self.persist_config {
                cache.evict_expired(persist_config.ttl_secs);
                let _ = cache.save_to_disk(persist_config);
            }
            let stats = cache.stats();
            if let Some(cache_obj) = response
                .metadata
                .as_object_mut()
                .and_then(|metadata| metadata.get_mut("cache"))
                .and_then(Value::as_object_mut)
            {
                cache_obj.insert("evictions".to_string(), json!(stats.evictions));
            } else {
                response.metadata = merge_json(
                    response.metadata,
                    json!({
                        "cache": {
                            "evictions": stats.evictions,
                        }
                    }),
                );
            }
        }

        Ok(response)
    }

    pub fn execute_sequence(
        &self,
        req: KernelSequenceRequest,
    ) -> Result<KernelSequenceResponse, KernelError> {
        let mut observer = NoopSequenceObserver;
        self.execute_sequence_with_observer(req, &mut observer)
    }

    pub fn execute_sequence_with_observer(
        &self,
        req: KernelSequenceRequest,
        observer: &mut dyn SequenceObserver,
    ) -> Result<KernelSequenceResponse, KernelError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let req = normalize_sequence_request(req)?;
        let task_id = resolve_sequence_task_id(&req);
        let budget = req.budget;
        let reactive = req.reactive;
        let original_steps = req.steps;

        let mut remaining = original_steps.clone();
        let mut step_results = Vec::new();
        let mut scheduled = Vec::new();
        let mut skipped = Vec::new();
        let mut consumed_estimate = context_scheduler_core::StepEstimate::default();
        let mut budget_exhausted = false;
        let mut completed_success = BTreeSet::<String>::new();
        let mut last_event_count = 0usize;
        let mut replans = Vec::<Value>::new();

        if reactive.enabled {
            if let Some(task_id) = task_id.as_deref() {
                let snapshot = load_agent_snapshot_for_task(self, task_id)?;
                last_event_count = snapshot.event_count;
                let mutations = build_reactive_kernel_mutations(
                    &remaining,
                    &original_steps,
                    &snapshot,
                    &completed_success,
                    reactive.mode,
                    reactive.append_focused_map,
                    None,
                );
                if !mutations.is_empty() {
                    let schedule_mutations = to_schedule_mutations(&mutations);
                    let applied = context_scheduler_core::apply_mutations(
                        &remaining
                            .iter()
                            .map(schedule_step_from_kernel)
                            .collect::<Vec<_>>(),
                        &schedule_mutations,
                    )
                    .map_err(|source| KernelError::SchedulerFailed {
                        detail: source.to_string(),
                    })?;
                    record_replan_cancellations(
                        &remaining,
                        &applied.applied,
                        &mut skipped,
                        &mut step_results,
                    );
                    remaining = apply_kernel_mutations(&remaining, &mutations);
                    replans.push(json!({
                        "trigger": "initial_state",
                        "event_count": snapshot.event_count,
                        "applied_mutations": applied.applied,
                    }));
                    observer.on_replan_applied(
                        None,
                        snapshot.event_count,
                        replans.last().unwrap_or(&Value::Null),
                    );
                }
            }
        }

        while !remaining.is_empty() {
            let schedule =
                context_scheduler_core::schedule(context_scheduler_core::ScheduleRequest {
                    steps: remaining
                        .iter()
                        .map(schedule_step_from_kernel)
                        .collect::<Vec<_>>(),
                    budget: schedule_budget_remaining(budget, consumed_estimate),
                })
                .map_err(|source| KernelError::SchedulerFailed {
                    detail: source.to_string(),
                })?;

            let Some(next_step_id) = schedule.ordered_steps.first().map(|step| step.id.clone())
            else {
                budget_exhausted = schedule.budget_exhausted;
                for step in remaining.drain(..) {
                    skipped.push(step.id.clone());
                    step_results.push(KernelStepResponse {
                        id: step.id,
                        target: step.target,
                        status: "skipped".to_string(),
                        response: None,
                        failure: Some(KernelFailure {
                            code: if budget_exhausted {
                                "budget_exceeded".to_string()
                            } else {
                                "dependency_not_satisfied".to_string()
                            },
                            message: if budget_exhausted {
                                "step skipped: budget_exceeded".to_string()
                            } else {
                                "step skipped: dependency_not_satisfied".to_string()
                            },
                            target: None,
                        }),
                    });
                }
                break;
            };

            let next_idx = remaining
                .iter()
                .position(|step| step.id == next_step_id)
                .expect("scheduled step must exist in remaining plan");
            let original = remaining.remove(next_idx);
            let position = scheduled.len() + 1;
            observer.on_step_started(position, &original);
            let estimate = kernel_step_estimate(&original);
            consumed_estimate = context_scheduler_core::StepEstimate {
                tokens: consumed_estimate.tokens.saturating_add(estimate.tokens),
                bytes: consumed_estimate.bytes.saturating_add(estimate.bytes),
                runtime_ms: consumed_estimate
                    .runtime_ms
                    .saturating_add(estimate.runtime_ms),
            };

            let response = self.execute(KernelRequest {
                target: original.target.clone(),
                input_packets: original.input_packets.clone(),
                budget: if original.budget == ExecutionBudget::default() {
                    budget
                } else {
                    original.budget
                },
                policy_context: policy_context_with_task_id(
                    original.policy_context.clone(),
                    task_id.as_deref(),
                ),
                reducer_input: original.reducer_input.clone(),
            });

            match response {
                Ok(response) => {
                    scheduled.push(original.id.clone());
                    completed_success.insert(original.id.clone());
                    remove_satisfied_dependency(&mut remaining, &original.id);
                    observer.on_step_completed(position, &original, &response);
                    step_results.push(KernelStepResponse {
                        id: original.id.clone(),
                        target: original.target.clone(),
                        status: "ok".to_string(),
                        response: Some(response),
                        failure: None,
                    });

                    if reactive.enabled {
                        if let Some(task_id) = task_id.as_deref() {
                            let snapshot = load_agent_snapshot_for_task(self, task_id)?;
                            if snapshot.event_count > last_event_count {
                                let mutations = build_reactive_kernel_mutations(
                                    &remaining,
                                    &original_steps,
                                    &snapshot,
                                    &completed_success,
                                    reactive.mode,
                                    reactive.append_focused_map,
                                    Some(&original.id),
                                );
                                if !mutations.is_empty() {
                                    let schedule_mutations = to_schedule_mutations(&mutations);
                                    let applied = context_scheduler_core::apply_mutations(
                                        &remaining
                                            .iter()
                                            .map(schedule_step_from_kernel)
                                            .collect::<Vec<_>>(),
                                        &schedule_mutations,
                                    )
                                    .map_err(|source| KernelError::SchedulerFailed {
                                        detail: source.to_string(),
                                    })?;
                                    record_replan_cancellations(
                                        &remaining,
                                        &applied.applied,
                                        &mut skipped,
                                        &mut step_results,
                                    );
                                    remaining = apply_kernel_mutations(&remaining, &mutations);
                                    replans.push(json!({
                                        "trigger": "task_state_update",
                                        "after_step": original.id,
                                        "event_count": snapshot.event_count,
                                        "applied_mutations": applied.applied,
                                    }));
                                    observer.on_replan_applied(
                                        Some(&original.id),
                                        snapshot.event_count,
                                        replans.last().unwrap_or(&Value::Null),
                                    );
                                }
                                last_event_count = snapshot.event_count;
                            }
                        }
                    }
                }
                Err(err) => {
                    let failure = err.structured();
                    observer.on_step_failed(position, &original, &failure);
                    let failed_dependents = remove_failed_dependents(&mut remaining, &original.id);
                    step_results.push(KernelStepResponse {
                        id: original.id.clone(),
                        target: original.target.clone(),
                        status: "failed".to_string(),
                        response: None,
                        failure: Some(failure),
                    });
                    for skipped_step in failed_dependents {
                        skipped.push(skipped_step.id.clone());
                        step_results.push(KernelStepResponse {
                            id: skipped_step.id,
                            target: skipped_step.target,
                            status: "skipped".to_string(),
                            response: None,
                            failure: Some(KernelFailure {
                                code: "dependency_failed".to_string(),
                                message: "step skipped due to failed dependency".to_string(),
                                target: None,
                            }),
                        });
                    }
                }
            }
        }

        Ok(KernelSequenceResponse {
            request_id,
            scheduled,
            skipped,
            budget_exhausted,
            step_results,
            metadata: json!({
                "estimated_usage": {
                    "tokens": consumed_estimate.tokens,
                    "bytes": consumed_estimate.bytes,
                    "runtime_ms": consumed_estimate.runtime_ms,
                },
                "reactive": {
                    "enabled": reactive.enabled,
                    "task_id": task_id,
                    "replans": replans,
                }
            }),
        })
    }
}

pub fn execute(req: KernelRequest) -> Result<KernelResponse, KernelError> {
    Kernel::with_v1_reducers().execute(req)
}

pub fn execute_sequence(req: KernelSequenceRequest) -> Result<KernelSequenceResponse, KernelError> {
    Kernel::with_v1_reducers().execute_sequence(req)
}

pub fn load_packet_file(path: &Path) -> Result<KernelPacket, KernelError> {
    let raw = std::fs::read_to_string(path).map_err(|source| KernelError::PacketRead {
        path: path.to_string_lossy().to_string(),
        detail: source.to_string(),
    })?;

    let value: Value = serde_json::from_str(&raw).map_err(|source| KernelError::PacketParse {
        path: path.to_string_lossy().to_string(),
        detail: source.to_string(),
    })?;

    Ok(KernelPacket::from_value(
        value,
        Some(path.to_string_lossy().to_string()),
    ))
}

pub fn register_v1_reducers(kernel: &mut Kernel) {
    kernel.register_reducer("agenty.state.write", run_agenty_state_write);
    kernel.register_reducer("agenty.state.snapshot", run_agenty_state_snapshot);
    kernel.register_reducer("contextq.correlate", run_contextq_correlate);
    kernel.register_reducer("contextq.manage", run_contextq_manage);
    kernel.register_reducer("contextq.assemble", run_contextq_assemble);
    kernel.register_reducer("governed.assemble", run_governed_assemble);
    kernel.register_reducer("guardy.check", run_guardy_check);
    kernel.register_reducer("diffy.analyze", run_diffy_analyze_reducer);
    kernel.register_reducer("testy.impact", run_testy_impact_reducer);
    kernel.register_reducer("stacky.slice", run_stacky_slice);
    kernel.register_reducer("buildy.reduce", run_buildy_reduce);
    kernel.register_reducer("proxy.run", run_proxy_run);
    kernel.register_reducer("mapy.repo", run_mapy_repo);
}
