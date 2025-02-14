use crate::{
    metrics::{self, record_metric, Endpoint, HasLabelsCore, Outcome},
    server_axum::api_orchestrator_integration_impls::*,
    Error, Result, StreamingCoordinatorIdleSnafu, StreamingCoordinatorSpawnSnafu,
    StreamingExecuteSnafu, WebSocketTaskPanicSnafu,
};

use axum::extract::ws::{Message, WebSocket};
use futures::{Future, FutureExt};
use orchestrator::coordinator::{self, Coordinator, DockerBackend};
use snafu::prelude::*;
use std::{
    convert::TryFrom,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, Semaphore},
    task::{AbortHandle, JoinSet},
    time,
};
use tracing::{error, instrument, Instrument};

type Meta = Arc<serde_json::Value>;

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum HandshakeMessage {
    #[serde(rename = "websocket/connected")]
    Connected {
        payload: Connected,
        #[allow(unused)]
        meta: Meta,
    },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connected {
    i_accept_this_is_an_unsupported_api: bool,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum WSMessageRequest {
    #[serde(rename = "output/execute/wsExecuteRequest")]
    ExecuteRequest { payload: ExecuteRequest, meta: Meta },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecuteRequest {
    channel: String,
    mode: String,
    edition: String,
    crate_type: String,
    tests: bool,
    code: String,
    backtrace: bool,
}

impl TryFrom<ExecuteRequest> for coordinator::ExecuteRequest {
    type Error = ExecuteRequestParseError;

    fn try_from(value: ExecuteRequest) -> Result<Self, Self::Error> {
        let ExecuteRequest {
            channel,
            mode,
            edition,
            crate_type,
            tests,
            code,
            backtrace,
        } = value;

        Ok(coordinator::ExecuteRequest {
            channel: parse_channel(&channel)?,
            mode: parse_mode(&mode)?,
            edition: parse_edition(&edition)?,
            crate_type: parse_crate_type(&crate_type)?,
            tests,
            backtrace,
            code,
        })
    }
}

#[derive(Debug, Snafu)]
pub(crate) enum ExecuteRequestParseError {
    #[snafu(context(false))]
    Channel { source: ParseChannelError },

    #[snafu(context(false))]
    CrateType { source: ParseCrateTypeError },

    #[snafu(context(false))]
    Mode { source: ParseModeError },

    #[snafu(context(false))]
    Edition { source: ParseEditionError },
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type")]
enum MessageResponse {
    #[serde(rename = "websocket/error")]
    Error { payload: WSError, meta: Meta },

    #[serde(rename = "featureFlags")]
    FeatureFlags { payload: FeatureFlags, meta: Meta },

    #[serde(rename = "output/execute/wsExecuteBegin")]
    ExecuteBegin { meta: Meta },

    #[serde(rename = "output/execute/wsExecuteStdout")]
    ExecuteStdout { payload: String, meta: Meta },

    #[serde(rename = "output/execute/wsExecuteStderr")]
    ExecuteStderr { payload: String, meta: Meta },

    #[serde(rename = "output/execute/wsExecuteEnd")]
    ExecuteEnd {
        payload: ExecuteResponse,
        meta: Meta,
    },
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WSError {
    error: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FeatureFlags {}

impl From<crate::FeatureFlags> for FeatureFlags {
    fn from(_value: crate::FeatureFlags) -> Self {
        Self {}
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecuteResponse {
    success: bool,
    exit_detail: String,
}

#[instrument(skip_all, fields(ws_id))]
pub(crate) async fn handle(socket: WebSocket, feature_flags: FeatureFlags) {
    static WEBSOCKET_ID: AtomicU64 = AtomicU64::new(0);

    metrics::LIVE_WS.inc();
    let start = Instant::now();

    let id = WEBSOCKET_ID.fetch_add(1, Ordering::SeqCst);
    tracing::Span::current().record("ws_id", &id);

    handle_core(socket, feature_flags).await;

    metrics::LIVE_WS.dec();
    let elapsed = start.elapsed();
    metrics::DURATION_WS.observe(elapsed.as_secs_f64());
}

type ResponseTx = mpsc::Sender<Result<MessageResponse>>;
type SharedCoordinator = Arc<Coordinator<DockerBackend>>;

/// Manages a limited amount of access to the `Coordinator`.
///
/// Has a number of responsibilities:
///
/// - Constructs the `Coordinator` on demand.
///
/// - Only allows one job of a certain kind at a time (e.g. executing
///   vs formatting). Older jobs will be cancelled.
///
/// - Allows limited parallelism between jobs of different types.
struct CoordinatorManager {
    coordinator: SharedCoordinator,
    tasks: JoinSet<Result<()>>,
    semaphore: Arc<Semaphore>,
    abort_handles: [Option<AbortHandle>; Self::N_KINDS],
}

impl CoordinatorManager {
    const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    const SESSION_TIMEOUT: Duration = Duration::from_secs(45 * 60);

    const N_PARALLEL: usize = 2;

    const N_KINDS: usize = 1;
    const KIND_EXECUTE: usize = 0;

    async fn new() -> Self {
        Self {
            coordinator: Arc::new(Coordinator::new_docker().await),
            tasks: Default::default(),
            semaphore: Arc::new(Semaphore::new(Self::N_PARALLEL)),
            abort_handles: Default::default(),
        }
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    async fn join_next(&mut self) -> Option<Result<Result<()>, tokio::task::JoinError>> {
        self.tasks.join_next().await
    }

    async fn spawn<F, Fut>(&mut self, handler: F) -> CoordinatorManagerResult<()>
    where
        F: FnOnce(SharedCoordinator) -> Fut,
        F: 'static + Send,
        Fut: Future<Output = Result<()>>,
        Fut: 'static + Send,
    {
        let coordinator = self.coordinator.clone();
        let semaphore = self.semaphore.clone();

        let new_abort_handle = self.tasks.spawn(
            async move {
                let _permit = semaphore.acquire().await;
                handler(coordinator).await
            }
            .in_current_span(),
        );

        let kind = Self::KIND_EXECUTE; // TODO: parameterize when we get a second kind
        let old_abort_handle = self.abort_handles[kind].replace(new_abort_handle);

        if let Some(abort_handle) = old_abort_handle {
            abort_handle.abort();
        }

        Ok(())
    }

    async fn idle(&mut self) -> CoordinatorManagerResult<()> {
        use coordinator_manager_error::*;

        Arc::get_mut(&mut self.coordinator)
            .context(OutstandingCoordinatorIdleSnafu)?
            .idle()
            .await
            .context(IdleSnafu)?;

        Ok(())
    }

    async fn shutdown(mut self) -> CoordinatorManagerResult<()> {
        use coordinator_manager_error::*;

        self.tasks.shutdown().await;
        Arc::into_inner(self.coordinator)
            .context(OutstandingCoordinatorShutdownSnafu)?
            .shutdown()
            .await
            .context(ShutdownSnafu)?;

        Ok(())
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum CoordinatorManagerError {
    #[snafu(display("The coordinator is still referenced and cannot be idled"))]
    OutstandingCoordinatorIdle,

    #[snafu(display("Could not idle the coordinator"))]
    Idle { source: coordinator::Error },

    #[snafu(display("The coordinator is still referenced and cannot be shut down"))]
    OutstandingCoordinatorShutdown,

    #[snafu(display("Could not shut down the coordinator"))]
    Shutdown { source: coordinator::Error },
}

type CoordinatorManagerResult<T, E = CoordinatorManagerError> = std::result::Result<T, E>;

async fn handle_core(mut socket: WebSocket, feature_flags: FeatureFlags) {
    if !connect_handshake(&mut socket).await {
        return;
    }

    let (tx, mut rx) = mpsc::channel(3);

    let ff = MessageResponse::FeatureFlags {
        payload: feature_flags,
        meta: create_server_meta(),
    };

    if tx.send(Ok(ff)).await.is_err() {
        return;
    }

    let mut manager = CoordinatorManager::new().await;
    tokio::pin! {
        let session_timeout = time::sleep(CoordinatorManager::SESSION_TIMEOUT);
    }

    loop {
        tokio::select! {
            request = socket.recv() => {
                metrics::WS_INCOMING.inc();

                match request {
                    None => {
                        // browser disconnected
                        break;
                    }
                    Some(Ok(Message::Text(txt))) => handle_msg(txt, &tx, &mut manager).await,
                    Some(Ok(_)) => {
                        // unknown message type
                        continue;
                    }
                    Some(Err(e)) => super::record_websocket_error(e.to_string()),
                }
            },

            resp = rx.recv() => {
                let resp = resp.expect("The rx should never close as we have a tx");
                let success = resp.is_ok();
                let resp = resp.unwrap_or_else(error_to_response);
                let resp = response_to_message(resp);

                if socket.send(resp).await.is_err() {
                    // We can't send a response
                    break;
                }

                let success = if success { "true" } else { "false" };
                metrics::WS_OUTGOING.with_label_values(&[success]).inc();
            },

            // We don't care if there are no running tasks
            Some(task) = manager.join_next() => {
                let Err(error) = task else { continue };
                // The task was cancelled; no need to report
                let Ok(panic) = error.try_into_panic() else { continue };

                let text = match panic.downcast::<String>() {
                    Ok(text) => *text,
                    Err(panic) => match panic.downcast::<&str>() {
                        Ok(text) => text.to_string(),
                        _ => "An unknown panic occurred".into(),
                    }
                };
                let error = WebSocketTaskPanicSnafu { text }.fail();

                if tx.send(error).await.is_err() {
                    // We can't send a response
                    break;
                }
            },

            _ = time::sleep(CoordinatorManager::IDLE_TIMEOUT), if manager.is_empty() => {
                let idled = manager.idle().await.context(StreamingCoordinatorIdleSnafu);

                let Err(error) = idled else { continue };

                if tx.send(Err(error)).await.is_err() {
                    // We can't send a response
                    break;
                }
            },

            _ = &mut session_timeout => {
                break;
            }
        }
    }

    drop((tx, rx, socket));
    if let Err(e) = manager.shutdown().await {
        error!("Could not shut down the Coordinator: {e:?}");
    }
}

async fn connect_handshake(socket: &mut WebSocket) -> bool {
    let Some(Ok(Message::Text(txt))) = socket.recv().await else {
        return false;
    };
    let Ok(HandshakeMessage::Connected { payload, .. }) =
        serde_json::from_str::<HandshakeMessage>(&txt)
    else {
        return false;
    };
    if !payload.i_accept_this_is_an_unsupported_api {
        return false;
    }
    socket.send(Message::Text(txt)).await.is_ok()
}

fn create_server_meta() -> Meta {
    Arc::new(serde_json::json!({ "sequenceNumber": -1 }))
}

fn error_to_response(error: Error) -> MessageResponse {
    let error = error.to_string();
    let payload = WSError { error };
    // TODO: thread through the Meta from the originating request
    let meta = create_server_meta();

    MessageResponse::Error { payload, meta }
}

fn response_to_message(response: MessageResponse) -> Message {
    const LAST_CHANCE_ERROR: &str =
        r#"{ "type": "WEBSOCKET_ERROR", "error": "Unable to serialize JSON" }"#;
    let resp = serde_json::to_string(&response).unwrap_or_else(|_| LAST_CHANCE_ERROR.into());
    Message::Text(resp)
}

async fn handle_msg(txt: String, tx: &ResponseTx, manager: &mut CoordinatorManager) {
    use WSMessageRequest::*;

    let msg = serde_json::from_str(&txt).context(crate::DeserializationSnafu);

    match msg {
        Ok(ExecuteRequest { payload, meta }) => {
            // TODO: Should a single execute / build / etc. session have a timeout of some kind?
            let spawned = manager
                .spawn({
                    let tx = tx.clone();
                    |coordinator| {
                        handle_execute(tx, coordinator, payload, meta)
                            .context(StreamingExecuteSnafu)
                    }
                })
                .await
                .context(StreamingCoordinatorSpawnSnafu);

            if let Err(e) = spawned {
                tx.send(Err(e)).await.ok(/* We don't care if the channel is closed */);
            }
        }
        Err(e) => {
            tx.send(Err(e)).await.ok(/* We don't care if the channel is closed */);
        }
    }
}

#[derive(Debug)]
enum CompletedOrAbandoned<T> {
    Abandoned,
    Completed(T),
}

macro_rules! abandon_if_closed {
    ($sent:expr) => {
        if $sent.is_err() {
            return Ok(CompletedOrAbandoned::Abandoned);
        }
    };
}

async fn handle_execute(
    tx: ResponseTx,
    coordinator: SharedCoordinator,
    req: ExecuteRequest,
    meta: Meta,
) -> ExecuteResult<()> {
    use execute_error::*;
    use CompletedOrAbandoned::*;

    let req = coordinator::ExecuteRequest::try_from(req).context(BadRequestSnafu)?;

    let labels_core = req.labels_core();

    let start = Instant::now();
    let v = handle_execute_inner(tx, coordinator, req, meta).await;
    let elapsed = start.elapsed();

    let outcome = match &v {
        Ok(Abandoned) => Outcome::Abandoned,
        Ok(Completed(v)) => *v,
        Err(_) => Outcome::ErrorServer,
    };

    record_metric(Endpoint::Execute, labels_core, outcome, elapsed);

    v?;
    Ok(())
}

async fn handle_execute_inner(
    tx: ResponseTx,
    coordinator: SharedCoordinator,
    req: coordinator::ExecuteRequest,
    meta: Meta,
) -> ExecuteResult<CompletedOrAbandoned<Outcome>> {
    use execute_error::*;
    use CompletedOrAbandoned::*;

    let coordinator::ActiveExecution {
        mut task,
        mut stdout_rx,
        mut stderr_rx,
    } = coordinator.begin_execute(req).await.context(BeginSnafu)?;

    let sent = tx
        .send(Ok(MessageResponse::ExecuteBegin { meta: meta.clone() }))
        .await;
    abandon_if_closed!(sent);

    let send_stdout = |payload| async {
        let meta = meta.clone();
        tx.send(Ok(MessageResponse::ExecuteStdout { payload, meta }))
            .await
    };

    let send_stderr = |payload| async {
        let meta = meta.clone();
        tx.send(Ok(MessageResponse::ExecuteStderr { payload, meta }))
            .await
    };

    let status = loop {
        tokio::select! {
            status = &mut task => break status,

            Some(stdout) = stdout_rx.recv() => {
                let sent = send_stdout(stdout).await;
                abandon_if_closed!(sent);
            },

            Some(stderr) = stderr_rx.recv() => {
                let sent = send_stderr(stderr).await;
                abandon_if_closed!(sent);
            },
        }
    };

    // Drain any remaining output
    while let Some(Some(stdout)) = stdout_rx.recv().now_or_never() {
        let sent = send_stdout(stdout).await;
        abandon_if_closed!(sent);
    }

    while let Some(Some(stderr)) = stderr_rx.recv().now_or_never() {
        let sent = send_stderr(stderr).await;
        abandon_if_closed!(sent);
    }

    let status = status.context(EndSnafu)?;
    let outcome = Outcome::from_success(&status);

    let coordinator::ExecuteResponse {
        success,
        exit_detail,
    } = status;

    let sent = tx
        .send(Ok(MessageResponse::ExecuteEnd {
            payload: ExecuteResponse {
                success,
                exit_detail,
            },
            meta,
        }))
        .await;
    abandon_if_closed!(sent);

    Ok(Completed(outcome))
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub(crate) enum ExecuteError {
    #[snafu(display("The request could not be parsed"))]
    BadRequest { source: ExecuteRequestParseError },

    #[snafu(display("Could not begin the execution session"))]
    Begin { source: coordinator::ExecuteError },

    #[snafu(display("Could not end the execution session"))]
    End { source: coordinator::ExecuteError },
}

type ExecuteResult<T, E = ExecuteError> = std::result::Result<T, E>;
