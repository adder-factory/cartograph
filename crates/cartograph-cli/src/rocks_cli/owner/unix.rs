use std::fmt::Write as _;
use std::{
    fs,
    future::Future,
    io,
    os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use cartograph_mcp::{
    ToolCall, ToolCallContext, ToolError, ToolErrorCode, ToolHandler, ToolResult,
};
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
    sync::watch,
};

const REQUEST_BYTES: usize = 64 * 1024;
const RESPONSE_BYTES: usize = 2 * 1024 * 1024 + 4096;
const CONNECTIONS: usize = 8;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    project: String,
    expected_owner: Option<String>,
    tool: String,
    arguments: Map<String, Value>,
    timeout_millis: u32,
    tool_ceiling: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Ok {
        owner_instance: String,
        result: ToolResult,
    },
    Error {
        code: String,
        message: String,
    },
}

pub(crate) struct OwnerClient {
    stream: UnixStream,
    project: String,
}

impl OwnerClient {
    pub(crate) async fn connect(root: &Path) -> Result<Option<Self>, String> {
        let (path, project, uid) = endpoint(root)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(unavailable()),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != uid {
            return Err("native project owner endpoint is unsafe".to_owned());
        }
        let stream = match tokio::time::timeout(IO_TIMEOUT, UnixStream::connect(path)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            _ => return Err(unavailable()),
        };
        if stream.peer_cred().map_err(|_| unavailable())?.uid() != uid {
            return Err("native project owner identity could not be verified".to_owned());
        }
        Ok(Some(Self { stream, project }))
    }

    pub(crate) async fn call_owned(
        mut self,
        call: ToolCall,
        context: ToolCallContext,
        expected_owner: Option<String>,
    ) -> Result<(String, ToolResult), ToolError> {
        let remaining = context
            .deadline()
            .saturating_duration_since(tokio::time::Instant::now());
        let timeout_millis = u32::try_from(remaining.min(Duration::from_secs(300)).as_millis())
            .map_err(|_| super::transport_error(unavailable()))?;
        if timeout_millis == 0 {
            return Err(super::transport_error(
                "native owner request deadline expired",
            ));
        }
        let request = Request {
            version: 4,
            project: self.project,
            expected_owner,
            tool: call.name,
            arguments: call.arguments,
            timeout_millis,
            tool_ceiling: context.tool_ceiling().map(<[String]>::to_vec),
        };
        // Framing is bounded before admission. Once a complete request is sent,
        // a stop must notify the owner and retain its actual terminal reply.
        tokio::select! {
            biased;
            sent = tokio::time::timeout(IO_TIMEOUT, write_frame(&mut self.stream, &request, REQUEST_BYTES)) => {
                sent.map_err(|_| super::transport_error(unavailable()))?
                    .map_err(super::transport_error)?;
            },
            () = context.cancellation().cancelled() => return Err(super::transport_error("native owner request was cancelled before admission; any transport outcome is unverified")),
            () = tokio::time::sleep_until(context.deadline()) => return Err(super::transport_error("native owner request deadline expired before admission; any transport outcome is unverified")),
        }
        let (mut reader, mut writer) = self.stream.into_split();
        let reply = read_frame::<Reply>(&mut reader, RESPONSE_BYTES);
        tokio::pin!(reply);
        let reply = tokio::select! {
            biased;
            result = &mut reply => result,
            () = context.cancellation().cancelled() => {
                let _notified = tokio::time::timeout(IO_TIMEOUT, writer.shutdown()).await;
                reply.await
            },
            () = tokio::time::sleep_until(context.deadline()) => {
                let _notified = tokio::time::timeout(IO_TIMEOUT, writer.shutdown()).await;
                reply.await
            },
        }
        .map_err(super::transport_error)?;
        match reply {
            Reply::Ok {
                owner_instance,
                result,
            } => Ok((owner_instance, result)),
            Reply::Error { code, message } => {
                let code = match code.as_str() {
                    "invalid_arguments" => ToolErrorCode::InvalidArguments,
                    "not_found" => ToolErrorCode::NotFound,
                    "conflict" => ToolErrorCode::Conflict,
                    "not_ready" => ToolErrorCode::NotReady,
                    "timeout" => ToolErrorCode::Timeout,
                    "unavailable" => ToolErrorCode::Unavailable,
                    _ => return Err(ToolError::internal()),
                };
                Err(ToolError::safe(code, message).unwrap_or_else(|_| ToolError::internal()))
            }
        }
    }
}

type Connection = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(crate) struct OwnerListener {
    listener: UnixListener,
    _guard: SocketGuard,
    project: String,
    uid: u32,
    instance: String,
    connections: FuturesUnordered<Connection>,
}

impl OwnerListener {
    /// Called only after this process has acquired the project's native primary.
    pub(crate) fn bind(root: &Path) -> Result<Self, String> {
        let (path, project, uid) = endpoint(root)?;
        let mut instance = [0_u8; 16];
        getrandom::fill(&mut instance).map_err(|_| unavailable())?;
        let mut instance_hex = String::with_capacity(32);
        for byte in instance {
            write!(&mut instance_hex, "{byte:02x}").map_err(|_| unavailable())?;
        }
        let instance = instance_hex;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
                match std::os::unix::net::UnixStream::connect(&path) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        fs::remove_file(&path).map_err(|_| unavailable())?;
                    }
                    _ => return Err("another project owner endpoint is active".to_owned()),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err("native project owner endpoint is unsafe".to_owned()),
        }
        let listener = UnixListener::bind(&path).map_err(|_| unavailable())?;
        let metadata = fs::symlink_metadata(&path).map_err(|_| unavailable())?;
        let guard = SocketGuard {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        fs::set_permissions(&guard.path, fs::Permissions::from_mode(0o600))
            .map_err(|_| unavailable())?;
        Ok(Self {
            listener,
            _guard: guard,
            project,
            uid,
            instance,
            connections: FuturesUnordered::new(),
        })
    }

    pub(crate) async fn run(
        &mut self,
        handler: Arc<dyn ToolHandler>,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let (closing, _) = watch::channel(false);
        let result = loop {
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break Ok(()); }
                }
                _ = self.connections.next(), if !self.connections.is_empty() => {},
                accepted = self.listener.accept(), if self.connections.len() < CONNECTIONS => {
                    let Ok((stream, _)) = accepted else { break Err(unavailable()) };
                    if stream.peer_cred().is_ok_and(|credentials| credentials.uid() == self.uid) {
                        self.connections.push(Box::pin(connection(stream, handler.clone(), self.project.clone(), self.instance.clone(), closing.subscribe())));
                    }
                }
            }
        };
        let _ = closing.send(true);
        // All request futures stay here until cancelled native work has been
        // acknowledged by the project owner. Nothing is spawned or detached.
        while self.connections.next().await.is_some() {}
        result
    }
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn endpoint(root: &Path) -> Result<(PathBuf, String, u32), String> {
    endpoint_for_user(root, rustix::process::geteuid().as_raw())
}

fn endpoint_for_user(root: &Path, uid: u32) -> Result<(PathBuf, String, u32), String> {
    let root = fs::canonicalize(root).map_err(|_| unavailable())?;
    let state = root.join(".cartograph");
    match fs::symlink_metadata(&state) {
        Ok(metadata)
            if metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid => {
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        _ => return Err("native project state is unsafe".to_owned()),
    }
    let mut hash = blake3::Hasher::new_derive_key("cartograph-native-owner-root-v1");
    hash.update(root.as_os_str().as_encoded_bytes());
    let project = hash.finalize().to_hex().to_string();
    let path = state.join("owner.sock");
    // macOS sockaddr_un has a 104-byte pathname; leave room for its NUL.
    let path = if path.as_os_str().as_encoded_bytes().len() < 100 {
        path
    } else {
        let directory = PathBuf::from(format!("/tmp/cartograph-owner-{uid}"));
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(unavailable()),
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|_| unavailable())?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
        {
            return Err("native project owner directory is unsafe".to_owned());
        }
        directory.join(format!("{project}.sock"))
    };
    Ok((path, project, uid))
}

async fn connection(
    mut stream: UnixStream,
    handler: Arc<dyn ToolHandler>,
    project: String,
    owner_instance: String,
    mut closing: watch::Receiver<bool>,
) {
    let request: Request =
        match tokio::time::timeout(IO_TIMEOUT, read_frame(&mut stream, REQUEST_BYTES)).await {
            Ok(Ok(request)) => request,
            _ => return,
        };
    if request.version != 4
        || request.project != project
        || !(1..=300_000).contains(&request.timeout_millis)
    {
        return;
    }
    if request
        .expected_owner
        .as_ref()
        .is_some_and(|expected| expected != &owner_instance)
    {
        let reply = Reply::Error {
            code: "conflict".to_owned(),
            message: "native project owner changed; prior job outcome is unverified".to_owned(),
        };
        let _ = tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &reply, RESPONSE_BYTES))
            .await;
        return;
    }
    if !handler
        .tools()
        .iter()
        .any(|tool| tool.name() == request.tool)
    {
        return;
    }
    let context = ToolCallContext::local(Duration::from_millis(u64::from(request.timeout_millis)));
    let context = if let Some(ceiling) = request.tool_ceiling {
        match context.with_tool_ceiling(ceiling.iter().map(String::as_str)) {
            Ok(context) => context,
            Err(_) => return,
        }
    } else {
        context
    };
    if !context.allows_tool(&request.tool) {
        return;
    }
    let operation = handler.call(
        ToolCall {
            name: request.tool,
            arguments: request.arguments,
        },
        context.clone(),
    );
    tokio::pin!(operation);
    let mut unexpected = [0_u8; 1];
    let result = tokio::select! {
        result = &mut operation => result,
        _ = stream.read(&mut unexpected) => { context.cancellation().cancel(); operation.await },
        _ = closing.changed() => { context.cancellation().cancel(); operation.await },
        () = tokio::time::sleep_until(context.deadline()) => { context.cancellation().cancel(); operation.await },
    };
    let reply = match result {
        Ok(result) => Reply::Ok {
            owner_instance,
            result,
        },
        Err(error) => Reply::Error {
            code: error.code().as_str().to_owned(),
            message: error.wire_message().to_owned(),
        },
    };
    let _ =
        tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &reply, RESPONSE_BYTES)).await;
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    input: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> Result<T, String> {
    let length = usize::try_from(input.read_u32().await.map_err(|_| unavailable())?)
        .map_err(|_| unavailable())?;
    if length == 0 || length > limit {
        return Err("native owner frame exceeded its bound".to_owned());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .await
        .map_err(|_| unavailable())?;
    serde_json::from_slice(&bytes).map_err(|_| "native owner frame is invalid".to_owned())
}

async fn write_frame<T: Serialize>(
    output: &mut (impl AsyncWrite + Unpin),
    value: &T,
    limit: usize,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| unavailable())?;
    if bytes.len() > limit {
        return Err("native owner frame exceeded its bound".to_owned());
    }
    output
        .write_u32(u32::try_from(bytes.len()).map_err(|_| unavailable())?)
        .await
        .map_err(|_| unavailable())?;
    output.write_all(&bytes).await.map_err(|_| unavailable())
}

fn unavailable() -> String {
    "native project owner transport is unavailable".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_handler::RocksHandler;
    use cartograph_agent::RocksProjectRuntime;
    use cartograph_db::rocks::StorageRequest;
    use serde_json::json;

    struct RetainedJobHandler {
        definition: cartograph_mcp::ToolDefinition,
        actions: std::sync::Mutex<Vec<String>>,
    }
    impl ToolHandler for RetainedJobHandler {
        fn tools(&self) -> Vec<cartograph_mcp::ToolDefinition> {
            vec![self.definition.clone()]
        }
        fn call(&self, call: ToolCall, _: ToolCallContext) -> cartograph_mcp::BoxToolFuture<'_> {
            Box::pin(async move {
                let action = call
                    .arguments
                    .get("action")
                    .and_then(Value::as_str)
                    .ok_or_else(ToolError::internal)?;
                self.actions
                    .lock()
                    .map_err(|_| ToolError::internal())?
                    .push(action.to_owned());
                let status = if action == "cancel" {
                    "cancelled"
                } else {
                    "running"
                };
                let report = json!({"jobId":7, "status":status});
                Ok(
                    ToolResult::text(report.to_string()).with_structured_content(
                        report
                            .as_object()
                            .cloned()
                            .ok_or_else(ToolError::internal)?,
                    ),
                )
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expiring_status_observer_never_polls_or_cancels_retained_job()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join(".cartograph"))?;
        let handler = Arc::new(RetainedJobHandler {
            definition: RocksHandler::definitions(false)?
                .into_iter()
                .find(|definition| definition.name() == "cartograph_admin")
                .ok_or("admin missing")?,
            actions: std::sync::Mutex::new(Vec::new()),
        });
        let mut listener = OwnerListener::bind(root.path())?;
        let (stop, stopped) = watch::channel(false);
        let serving = handler.clone();
        let task = tokio::spawn(async move { listener.run(serving, stopped).await });
        let context = ToolCallContext::local(Duration::from_secs(1));
        let observer = context.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            crate::rocks_cli::remote_job::run(
                root.path(),
                ToolCall {
                    name: "cartograph_admin".to_owned(),
                    arguments: serde_json::from_value(json!({"action":"status", "jobId":7}))?,
                },
                context,
                |_| {},
            ),
        )
        .await??;
        let report = result.structured_content().ok_or("status missing")?;
        assert_eq!(report["status"], "running");
        tokio::time::sleep_until(observer.deadline()).await;
        observer.cancellation().cancel();
        let observed_actions = handler
            .actions
            .lock()
            .map_err(|_| "actions poisoned")?
            .clone();
        stop.send(true)?;
        tokio::time::timeout(Duration::from_secs(3), task).await???;
        assert_eq!(observed_actions, ["status"]);
        Ok(())
    }

    #[test]
    fn endpoint_refuses_a_state_directory_owned_by_a_different_user()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let state = root.path().join(".cartograph");
        fs::create_dir(&state)?;
        fs::write(state.join("preserve"), b"existing private state")?;
        let before = fs::metadata(&state)?;
        let other_uid = before.uid().checked_add(1).ok_or("uid overflow")?;
        assert_eq!(
            endpoint_for_user(root.path(), other_uid),
            Err("native project state is unsafe".to_owned())
        );
        assert_eq!(fs::metadata(&state)?.mode(), before.mode());
        assert_eq!(fs::read(state.join("preserve"))?, b"existing private state");
        assert_eq!(
            endpoint(root.path())?.2,
            rustix::process::geteuid().as_raw()
        );
        Ok(())
    }

    struct NativeReadHandler {
        store: cartograph_db::rocks::AsyncProjectStore,
        definition: cartograph_mcp::ToolDefinition,
        gate: std::sync::Mutex<
            Option<(
                tokio::sync::oneshot::Sender<()>,
                std::sync::mpsc::Receiver<()>,
            )>,
        >,
    }

    impl ToolHandler for NativeReadHandler {
        fn tools(&self) -> Vec<cartograph_mcp::ToolDefinition> {
            vec![self.definition.clone()]
        }
        fn drain_cancelled_calls(&self) -> bool {
            true
        }
        fn call(&self, _: ToolCall, context: ToolCallContext) -> cartograph_mcp::BoxToolFuture<'_> {
            Box::pin(async move {
                let (entered, release) = self
                    .gate
                    .lock()
                    .map_err(|_| ToolError::internal())?
                    .take()
                    .ok_or_else(ToolError::internal)?;
                let request = StorageRequest::new(Duration::from_secs(30))
                    .map_err(|_| ToolError::internal())?;
                let budget =
                    cartograph_db::rocks::ReadBudget::new(100, 1_048_576, Duration::from_secs(30))
                        .map_err(|_| ToolError::internal())?;
                let operation = self
                    .store
                    .state(None, budget, 0, 0, &request, move |_, budget| {
                        let _entered = entered.send(());
                        release
                            .recv_timeout(Duration::from_secs(10))
                            .map_err(|_| cartograph_db::rocks::RocksError::Cancelled)?;
                        budget.checkpoint()?;
                        Ok(())
                    });
                tokio::pin!(operation);
                let result = tokio::select! {
                    result = &mut operation => result,
                    () = context.cancellation().cancelled() => { request.cancel(); operation.await },
                };
                result.map_err(|_| {
                    super::super::transport_error(
                        "native read was cancelled after its worker drained",
                    )
                })?;
                Ok(ToolResult::text("native read finished"))
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_owner_client_half_closes_and_retains_the_real_native_completion()
    -> Result<(), Box<dyn std::error::Error>> {
        use cartograph_db::rocks::{AsyncProjectStore, StorageRuntime, StorePolicy, WorkerPolicy};
        let root = tempfile::tempdir()?;
        let directory = root.path().join(".cartograph/data");
        std::fs::create_dir_all(&directory)?;
        let runtime = StorageRuntime::new(WorkerPolicy::default())?;
        let store = AsyncProjectStore::open(
            &runtime,
            directory,
            cartograph_domain::ContentDigest::from_bytes([42; 32]),
            StorePolicy::default(),
            &StorageRequest::new(Duration::from_secs(30))?,
        )
        .await?;
        let definition = RocksHandler::definitions(false)?
            .into_iter()
            .find(|definition| definition.name() == "cartograph_playbook")
            .ok_or("playbook missing")?;
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let handler = Arc::new(NativeReadHandler {
            store,
            definition,
            gate: std::sync::Mutex::new(Some((entered, released))),
        });
        let mut listener = OwnerListener::bind(root.path())?;
        let (stop, stopped) = watch::channel(false);
        let serving = tokio::spawn(async move { listener.run(handler, stopped).await });
        let client = OwnerClient::connect(root.path())
            .await?
            .ok_or("owner missing")?;
        let context = ToolCallContext::local(Duration::from_secs(30));
        let control = context.clone();
        let calling = tokio::spawn(async move {
            client
                .call_owned(
                    ToolCall {
                        name: "cartograph_playbook".to_owned(),
                        arguments: Map::new(),
                    },
                    context,
                    None,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), started).await??;
        control.cancellation().cancel();
        tokio::time::sleep(Duration::from_millis(25)).await;
        let returned_early = calling.is_finished();
        release.send(())?;
        let actual = tokio::time::timeout(Duration::from_secs(3), calling).await??;
        assert!(
            !returned_early,
            "client returned while its native worker still ran"
        );
        assert!(
            actual.is_err_and(|error| error.wire_message().contains("after its worker drained"))
        );
        stop.send(true)?;
        tokio::time::timeout(Duration::from_secs(3), serving).await???;
        runtime
            .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
            .await?;
        Ok(())
    }

    async fn start_owner(
        root: &Path,
    ) -> Result<
        (
            Arc<RocksHandler>,
            watch::Sender<bool>,
            tokio::task::JoinHandle<Result<(), String>>,
        ),
        Box<dyn std::error::Error>,
    > {
        let runtime =
            RocksProjectRuntime::open(root, &StorageRequest::new(Duration::from_secs(30))?).await?;
        let handler = Arc::new(RocksHandler::new(runtime, false)?);
        let mut listener = OwnerListener::bind(root)?;
        let (stop, stopped) = watch::channel(false);
        let serving = handler.clone();
        let task = tokio::spawn(async move { listener.run(serving, stopped).await });
        Ok((handler, stop, task))
    }

    async fn admin(
        root: &Path,
        action: &str,
        job: Option<u64>,
        expected_owner: Option<String>,
    ) -> Result<(String, ToolResult), ToolError> {
        let mut arguments = Map::from_iter([("action".to_owned(), json!(action))]);
        if let Some(job) = job {
            arguments.insert("jobId".to_owned(), json!(job));
        }
        OwnerClient::connect(root)
            .await
            .map_err(super::super::transport_error)?
            .ok_or_else(|| super::super::transport_error("owner missing"))?
            .call_owned(
                ToolCall {
                    name: "cartograph_admin".to_owned(),
                    arguments,
                },
                ToolCallContext::local(Duration::from_secs(10)),
                expected_owner,
            )
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restarted_owner_rejects_old_job_polling_and_cancellation_before_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let (handler, stop, task) = start_owner(root.path()).await?;
        let (first, started) = admin(root.path(), "embedding-status", None, None).await?;
        assert_eq!(
            started.structured_content().ok_or("job missing")?["jobId"],
            1
        );
        stop.send(true)?;
        tokio::time::timeout(Duration::from_secs(5), task).await???;
        handler.close().await?;
        let (handler, stop, task) = start_owner(root.path()).await?;
        let (second, started) = admin(root.path(), "embedding-status", None, None).await?;
        assert_ne!(first, second);
        assert_eq!(
            started.structured_content().ok_or("job missing")?["jobId"],
            1
        );
        for action in ["status", "cancel"] {
            let stale = admin(root.path(), action, Some(1), Some(first.clone())).await;
            assert!(matches!(stale, Err(error) if error.code() == ToolErrorCode::Conflict));
        }
        let (_, current) = admin(root.path(), "status", Some(1), Some(second)).await?;
        assert_eq!(
            current.structured_content().ok_or("job missing")?["jobId"],
            1
        );
        stop.send(true)?;
        tokio::time::timeout(Duration::from_secs(5), task).await???;
        handler.close().await?;
        Ok(())
    }
}
