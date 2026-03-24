use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonRequest {
    Execute {
        request: KernelRequest,
    },
    ExecuteSequence {
        spec: TaskSubmitSpec,
    },
    Status,
    Stop,
    TaskStatus {
        task_id: String,
    },
    TaskAwaitHandoff {
        request: TaskAwaitHandoffRequest,
    },
    TaskMarkHandoffConsumed {
        request: TaskMarkHandoffConsumedRequest,
    },
    TaskLaunchAgent {
        request: TaskLaunchAgentRequest,
    },
    TaskCancel {
        task_id: String,
    },
    TaskSubscribe {
        task_id: String,
        replay_last: usize,
    },
    WatchList {
        task_id: Option<String>,
    },
    WatchRemove {
        watch_id: String,
    },
    CoverCheck {
        request: CoverCheckRequest,
    },
    PacketFetch {
        request: PacketFetchRequest,
    },
    TestShard {
        request: TestShardRequest,
    },
    TestMap {
        request: TestMapRequest,
    },
    ContextStoreList {
        request: ContextStoreListRequest,
    },
    ContextStoreGet {
        request: ContextStoreGetRequest,
    },
    ContextStorePrune {
        request: ContextStorePruneDaemonRequest,
    },
    ContextStoreStats {
        request: ContextStoreStatsRequest,
    },
    ContextRecall {
        request: ContextRecallRequest,
    },
    BrokerGetContext {
        request: BrokerGetContextRequest,
    },
    BrokerEstimateContext {
        request: BrokerEstimateContextRequest,
    },
    BrokerPrepareHandoff {
        request: BrokerPrepareHandoffRequest,
    },
    BrokerValidatePlan {
        request: BrokerValidatePlanRequest,
    },
    BrokerDecompose {
        request: BrokerDecomposeRequest,
    },
    BrokerWriteState {
        request: BrokerWriteStateRequest,
    },
    BrokerWriteStateBatch {
        request: BrokerWriteStateBatchRequest,
    },
    BrokerTaskStatus {
        request: BrokerTaskStatusRequest,
    },
    HookIngest {
        request: HookIngestRequest,
    },
    Packet28Search {
        request: Packet28SearchRequest,
    },
    DaemonIndexStatus {
        request: DaemonIndexStatusRequest,
    },
    DaemonIndexRebuild {
        request: DaemonIndexRebuildRequest,
    },
    DaemonIndexClear {
        request: DaemonIndexClearRequest,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Packet28SearchRequest {
    pub request: packet28_reducer_core::SearchRequest,
    pub force_indexed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonResponse {
    Execute {
        response: KernelResponse,
    },
    ExecuteSequence {
        response: KernelSequenceResponse,
        task: TaskRecord,
        watches: Vec<WatchRegistration>,
    },
    Status {
        status: DaemonStatus,
    },
    Ack {
        message: String,
    },
    TaskStatus {
        task: Option<TaskRecord>,
    },
    TaskAwaitHandoff {
        response: TaskAwaitHandoffResponse,
    },
    TaskMarkHandoffConsumed {
        response: TaskMarkHandoffConsumedResponse,
    },
    TaskLaunchAgent {
        response: TaskLaunchAgentResponse,
    },
    TaskCancel {
        task: Option<TaskRecord>,
        removed_watch_ids: Vec<String>,
    },
    TaskSubscribeAck {
        task_id: String,
        replayed: usize,
    },
    WatchList {
        watches: Vec<WatchRegistration>,
    },
    WatchRemove {
        removed: Option<WatchRegistration>,
    },
    CoverCheck {
        response: CoverCheckResponse,
    },
    PacketFetch {
        response: PacketFetchResponse,
    },
    TestShard {
        response: TestShardResponse,
    },
    TestMap {
        response: TestMapResponse,
    },
    ContextStoreList {
        response: ContextStoreListResponse,
    },
    ContextStoreGet {
        response: ContextStoreGetResponse,
    },
    ContextStorePrune {
        response: ContextStorePruneResponse,
    },
    ContextStoreStats {
        response: ContextStoreStatsResponse,
    },
    ContextRecall {
        response: ContextRecallResponse,
    },
    BrokerGetContext {
        response: BrokerGetContextResponse,
    },
    BrokerEstimateContext {
        response: BrokerEstimateContextResponse,
    },
    BrokerPrepareHandoff {
        response: BrokerPrepareHandoffResponse,
    },
    BrokerValidatePlan {
        response: BrokerValidatePlanResponse,
    },
    BrokerDecompose {
        response: BrokerDecomposeResponse,
    },
    BrokerWriteState {
        response: BrokerWriteStateResponse,
    },
    BrokerWriteStateBatch {
        response: BrokerWriteStateBatchResponse,
    },
    BrokerTaskStatus {
        response: BrokerTaskStatusResponse,
    },
    HookIngest {
        response: HookIngestResponse,
    },
    Packet28Search {
        response: packet28_reducer_core::SearchResult,
    },
    DaemonIndexStatus {
        response: DaemonIndexStatusResponse,
    },
    DaemonIndexRebuild {
        response: DaemonIndexRebuildResponse,
    },
    DaemonIndexClear {
        response: DaemonIndexClearResponse,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonRuntimeInfo {
    pub pid: u32,
    pub started_at_unix: u64,
    pub ready_at_unix: Option<u64>,
    pub socket_path: String,
    pub workspace_root: String,
    pub log_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonStatus {
    pub pid: u32,
    pub socket_path: String,
    pub workspace_root: String,
    pub started_at_unix: u64,
    pub ready_at_unix: Option<u64>,
    pub log_path: String,
    pub uptime_secs: u64,
    pub tasks: Vec<TaskRecord>,
    pub watches: Vec<WatchRegistration>,
    pub index: Option<DaemonIndexStatusResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonEvent {
    pub kind: String,
    pub occurred_at_unix: u64,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonEventFrame {
    pub seq: u64,
    pub task_id: String,
    pub event: DaemonEvent,
}
