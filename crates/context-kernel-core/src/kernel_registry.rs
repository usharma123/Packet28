use super::*;

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
    kernel.register_reducer("packet28.broker_memory.write", run_broker_memory_write);
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
    kernel.register_reducer("mapy.query", run_mapy_query);
}
