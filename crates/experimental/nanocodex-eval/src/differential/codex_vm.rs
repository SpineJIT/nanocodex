use super::*;

pub(super) struct DiffVmResources {
    environment: VmEnvironment,
    nanocodex: VmBackend,
    codex: VmBackend,
    codex_ca_bundle: Option<DiffCodexCaBundle>,
}

pub(super) struct DiffCodexRelease {
    pub(super) root: PathBuf,
    ca_bundle: Option<DiffCodexCaBundle>,
}

pub(super) fn prepare_diff_codex_release(
    output_parent: &Path,
    codex_binary: &Path,
) -> InternalResult<DiffCodexRelease> {
    let releases = output_parent.join(".codex-releases");
    fs::create_dir_all(&releases)?;
    let temporary = tempfile::tempdir_in(&releases)?;
    let staged_codex = temporary.path().join("codex");
    reflink_or_sparse_copy(codex_binary, &staged_codex)?;
    fs::set_permissions(&staged_codex, fs::Permissions::from_mode(0o755))?;
    let mut header = [0_u8; 20];
    fs::File::open(&staged_codex)?.read_exact(&mut header)?;
    validate_vm_guest_elf(&header, &staged_codex)?;
    let ca_bundle = resolve_diff_codex_ca_source()?
        .as_ref()
        .map(|source| stage_diff_codex_ca_bundle(source, temporary.path()))
        .transpose()?;
    let root = temporary.keep();
    Ok(DiffCodexRelease { root, ca_bundle })
}

pub(super) async fn prepare_diff_vm_resources(
    task: &Task,
    vm: &VmResources,
    guest_memory_mb: u64,
    web_search: bool,
    codex_release: &DiffCodexRelease,
) -> InternalResult<DiffVmResources> {
    let environment = vm.environment(task).await?;
    let nanocodex = vm
        .backend_for_task_with_guest_memory(
            VmBackend::builder()
                .retain_passed_rootfs(false)
                .retain_failed_rootfs(false)
                .web_search(web_search),
            task,
            guest_memory_mb,
        )
        .await?;
    let codex = vm
        .backend_for_task_with_guest_memory(
            VmBackend::builder()
                .retain_passed_rootfs(false)
                .retain_failed_rootfs(false)
                .web_search(web_search)
                .shared_directory(SharedDirectory::read_only(
                    DIFF_CODEX_SHARE_TAG,
                    codex_release.root.clone(),
                )),
            task,
            guest_memory_mb,
        )
        .await?;
    Ok(DiffVmResources {
        environment,
        nanocodex,
        codex,
        codex_ca_bundle: codex_release.ca_bundle,
    })
}

impl DiffVmResources {
    pub(super) fn nanocodex_backend(&self) -> VmBackend {
        self.nanocodex.clone()
    }

    pub(super) fn codex_backend(&self) -> VmBackend {
        self.codex.clone()
    }

    pub(super) fn codex_attempt(
        &self,
        runtime: VmAttempt,
        attempt: EvalAttempt<'_>,
        codex: CodexExec,
        auth: CodexAuth,
        version: Arc<OnceLock<String>>,
        progress: DiffProgress,
    ) -> InternalResult<AttemptAgent, VmAttemptError> {
        let model_catalog_override = codex.model_tool_mode().map(|(model, tool_mode)| {
            ResponsesModelCatalogOverride::tool_mode(model, tool_mode.as_str())
        });
        let session = runtime.session_handle()?;
        let runner = DiffVmCodexRunner::new(
            session,
            attempt,
            &self.environment,
            auth,
            self.codex_ca_bundle,
            version,
            progress,
        )?
        .model_catalog_override(model_catalog_override);
        let api_base_url = runner.api_base_url().to_owned();
        let runner = Arc::new(runner);
        let readiness = Arc::clone(&runner);
        Ok(runtime
            .codex(codex.api_base_url(api_base_url).command_runner(runner))
            .ready(async move { readiness.prepare().await }))
    }
}

#[derive(Clone, Copy)]
pub(super) struct DiffCodexCaBundle {
    pub(super) guest_environment: &'static str,
}

pub(super) struct DiffCodexCaSource {
    pub(super) path: PathBuf,
    pub(super) source_environment: &'static str,
    pub(super) guest_environment: &'static str,
}

pub(super) fn resolve_diff_codex_ca_source() -> InternalResult<Option<DiffCodexCaSource>, io::Error>
{
    for (source_environment, guest_environment) in [
        (
            DIFF_CODEX_CA_CERTIFICATE_ENVIRONMENT,
            DIFF_CODEX_CA_CERTIFICATE_ENVIRONMENT,
        ),
        (
            DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT,
            DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT,
        ),
        (
            DIFF_CODEX_NIX_SSL_CERT_FILE_ENVIRONMENT,
            DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT,
        ),
    ] {
        let Some(path) = std::env::var_os(source_environment).filter(|value| !value.is_empty())
        else {
            continue;
        };
        return Ok(Some(DiffCodexCaSource {
            path: fs::canonicalize(PathBuf::from(path))?,
            source_environment,
            guest_environment,
        }));
    }
    for path in [
        Path::new("/etc/ssl/certs/ca-certificates.crt"),
        Path::new("/etc/ssl/cert.pem"),
    ] {
        if path.is_file() {
            return Ok(Some(DiffCodexCaSource {
                path: fs::canonicalize(path)?,
                source_environment: "host_system",
                guest_environment: DIFF_CODEX_SSL_CERT_FILE_ENVIRONMENT,
            }));
        }
    }
    Ok(None)
}

pub(super) fn stage_diff_codex_ca_bundle(
    source: &DiffCodexCaSource,
    codex_share_root: &Path,
) -> InternalResult<DiffCodexCaBundle, io::Error> {
    let staged = codex_share_root.join(DIFF_CODEX_CA_BUNDLE_FILENAME);
    reflink_or_sparse_copy(&source.path, &staged)?;
    if staged.metadata()?.len() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Codex CA bundle selected by {} is empty: {}",
                source.source_environment,
                source.path.display()
            ),
        ));
    }
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o444))?;
    info!(
        target: "nanocodex_eval",
        source_environment = source.source_environment,
        source_path = %source.path.display(),
        staged_path = %staged.display(),
        "staged the host CA bundle for the pinned guest Codex release"
    );
    Ok(DiffCodexCaBundle {
        guest_environment: source.guest_environment,
    })
}

pub(super) enum DiffVmCodexAuth {
    ApiKey(Arc<str>),
    AuthFile {
        contents: Vec<u8>,
        cloud_config_cache: Option<Vec<u8>>,
    },
}

pub(super) struct DiffVmCodexRunner {
    session: VmToolSessionHandle,
    workspace: String,
    environment: Vec<(String, String)>,
    auth_file: Option<Vec<u8>>,
    cloud_config_cache: Option<Vec<u8>>,
    capture_upstream: &'static str,
    model_catalog_override: Option<ResponsesModelCatalogOverride>,
    capture_listener: Mutex<Option<TcpListener>>,
    capture_base_url: String,
    api_exchanges: PathBuf,
    version: Arc<OnceLock<String>>,
    progress: DiffProgress,
}

impl DiffVmCodexRunner {
    fn new(
        session: VmToolSessionHandle,
        attempt: EvalAttempt<'_>,
        environment: &VmEnvironment,
        auth: CodexAuth,
        ca_bundle: Option<DiffCodexCaBundle>,
        version: Arc<OnceLock<String>>,
        progress: DiffProgress,
    ) -> InternalResult<Self, VmAttemptError> {
        let artifact_directory = attempt.directory().join("agent");
        fs::create_dir_all(&artifact_directory)?;
        let auth = match auth.kind {
            CodexAuthKind::ApiKey(api_key) => DiffVmCodexAuth::ApiKey(api_key),
            CodexAuthKind::AuthFile(path) => {
                let contents = fs::read(&path)?;
                let cloud_config_cache = read_optional_codex_cloud_config_cache(&path)?;
                DiffVmCodexAuth::AuthFile {
                    contents,
                    cloud_config_cache,
                }
            }
        };
        let mut command_environment = environment.guest_environment(attempt.task());
        command_environment.insert("CODEX_HOME".to_owned(), DIFF_CODEX_HOME.to_owned());
        if let Some(ca_bundle) = ca_bundle {
            command_environment.insert(
                ca_bundle.guest_environment.to_owned(),
                DIFF_CODEX_CA_BUNDLE_FILE.to_owned(),
            );
        }
        let (auth_file, cloud_config_cache, capture_upstream) = match auth {
            DiffVmCodexAuth::ApiKey(api_key) => {
                command_environment.insert("OPENAI_API_KEY".to_owned(), api_key.to_string());
                (None, None, DIFF_CAPTURE_PROXY_API_UPSTREAM)
            }
            DiffVmCodexAuth::AuthFile {
                contents,
                cloud_config_cache,
            } => {
                command_environment.remove("OPENAI_API_KEY");
                (
                    Some(contents),
                    cloud_config_cache,
                    DIFF_CAPTURE_PROXY_CHATGPT_UPSTREAM,
                )
            }
        };
        let capture_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let capture_port = capture_listener.local_addr()?.port();
        let capture_base_url = capture_proxy_vm_base_url(capture_port);
        Ok(Self {
            session,
            workspace: environment.workspace().to_owned(),
            environment: command_environment.into_iter().collect(),
            auth_file,
            cloud_config_cache,
            capture_upstream,
            model_catalog_override: None,
            capture_listener: Mutex::new(Some(capture_listener)),
            capture_base_url,
            api_exchanges: artifact_directory.join(DIFF_API_EXCHANGES_FILENAME),
            version,
            progress,
        })
    }

    fn model_catalog_override(
        mut self,
        model_catalog_override: Option<ResponsesModelCatalogOverride>,
    ) -> Self {
        self.model_catalog_override = model_catalog_override;
        self
    }

    fn api_base_url(&self) -> &str {
        &self.capture_base_url
    }

    async fn prepare(&self) -> InternalResult<(), VmAttemptError> {
        self.session.ready().await?;
        self.session
            .create_directory(DIFF_CODEX_SHARE_MOUNT, 0o755, None)
            .await?;
        let mount = self
            .session
            .command(
                VmCommand::new("/bin/mount")
                    .arg("-t")
                    .arg("virtiofs")
                    .arg("-o")
                    .arg("ro")
                    .arg(DIFF_CODEX_SHARE_TAG)
                    .arg(DIFF_CODEX_SHARE_MOUNT)
                    .environment(self.environment.clone())
                    .timeout(DIFF_CODEX_VERSION_TIMEOUT),
            )
            .await?;
        if mount.exit_code != 0 {
            return Err(io::Error::other(format!(
                "failed to mount the pinned Codex release in the guest (exit {}): {}",
                mount.exit_code,
                String::from_utf8_lossy(&mount.stderr).trim()
            ))
            .into());
        }
        self.session
            .create_directory(DIFF_CODEX_HOME, 0o700, None)
            .await?;
        if let Some(auth_file) = &self.auth_file {
            self.session
                .write_file(DIFF_CODEX_AUTH_FILE, auth_file.clone(), 0o600)
                .await?;
        }
        if let Some(cloud_config_cache) = &self.cloud_config_cache {
            self.session
                .write_file(
                    DIFF_CODEX_CLOUD_CONFIG_CACHE_FILE,
                    cloud_config_cache.clone(),
                    0o600,
                )
                .await?;
        }
        let version = self
            .session
            .command(
                VmCommand::new(DIFF_CODEX_GUEST_BINARY)
                    .arg("--version")
                    .current_directory(&self.workspace)
                    .environment(self.environment.clone())
                    .timeout(DIFF_CODEX_VERSION_TIMEOUT),
            )
            .await?;
        if version.exit_code != 0 {
            return Err(io::Error::other(format!(
                "pinned guest Codex --version exited {}: {}",
                version.exit_code,
                String::from_utf8_lossy(&version.stderr).trim()
            ))
            .into());
        }
        let version = String::from_utf8(version.stdout)
            .map_err(io::Error::other)?
            .trim()
            .to_owned();
        if version.is_empty() {
            return Err(
                io::Error::other("pinned guest Codex --version returned no version").into(),
            );
        }
        if let Some(existing) = self.version.get() {
            if existing != &version {
                return Err(io::Error::other(format!(
                    "pinned guest Codex version changed from {existing} to {version}"
                ))
                .into());
            }
        } else {
            self.version
                .set(version)
                .map_err(|_| io::Error::other("failed to retain pinned guest Codex version"))?;
        }
        Ok(())
    }

    async fn start_capture_proxy(
        &self,
    ) -> InternalResult<ResponsesCaptureProxy, CodexCommandRunnerError> {
        let listener = {
            let mut listener = self.capture_listener.lock().map_err(|_| {
                CodexCommandRunnerError::new("Responses capture listener lock was poisoned")
            })?;
            listener.take().ok_or_else(|| {
                CodexCommandRunnerError::new(
                    "Responses capture proxy was already started for this attempt",
                )
            })?
        };
        let proxy = ResponsesCaptureProxy::start(
            listener,
            ResponsesCaptureProxyConfig {
                upstream: self.capture_upstream.to_owned(),
                output: self.api_exchanges.clone(),
                model_catalog_override: self.model_catalog_override.clone(),
            },
        )
        .await
        .map_err(|error| CodexCommandRunnerError::new(error.to_string()))?;
        self.progress.emit(
            "codex",
            "api.capture.started",
            format!("{} → {}", self.capture_base_url, self.capture_upstream),
        );
        Ok(proxy)
    }

    async fn stop_capture_proxy(
        &self,
        proxy: ResponsesCaptureProxy,
    ) -> InternalResult<(), CodexCommandRunnerError> {
        match tokio::time::timeout(DIFF_CAPTURE_PROXY_STOP_TIMEOUT, proxy.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(CodexCommandRunnerError::new(error.to_string())),
            Err(_) => {
                return Err(CodexCommandRunnerError::new(format!(
                    "Responses capture proxy did not stop within {:?}",
                    DIFF_CAPTURE_PROXY_STOP_TIMEOUT
                )));
            }
        }
        self.progress.emit(
            "codex",
            "api.capture.completed",
            self.api_exchanges.display().to_string(),
        );
        Ok(())
    }
}

pub(super) fn capture_proxy_vm_base_url(port: u16) -> String {
    format!("http://{}:{port}", Gvproxy::HOST_IPV4)
}

pub(super) fn read_optional_codex_cloud_config_cache(
    auth_file: &Path,
) -> InternalResult<Option<Vec<u8>>, io::Error> {
    let Some(codex_home) = auth_file.parent() else {
        return Ok(None);
    };
    let cache = codex_home.join(DIFF_CODEX_CLOUD_CONFIG_CACHE_FILENAME);
    match fs::read(cache) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl CodexCommandRunner for DiffVmCodexRunner {
    fn run<'a>(
        &'a self,
        arguments: Vec<String>,
        timeout: Duration,
    ) -> Pin<
        Box<
            dyn Future<Output = InternalResult<CodexCommandOutput, CodexCommandRunnerError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let capture_proxy = self.start_capture_proxy().await?;
            let mut command = VmCommand::new(DIFF_CODEX_GUEST_BINARY)
                .current_directory(&self.workspace)
                .environment(self.environment.clone())
                .timeout(timeout)
                .max_output_bytes(DIFF_CODEX_OUTPUT_BYTES)
                .mirror_output(DIFF_CODEX_LIVE_STDOUT_FILE, DIFF_CODEX_LIVE_STDERR_FILE);
            for argument in arguments {
                command = command.arg(argument);
            }
            let session = self.session.clone();
            let command = async move { session.command(command).await };
            tokio::pin!(command);
            let mut progress =
                DiffCodexProgress::new(self.progress.clone(), self.api_exchanges.clone());
            let mut progress_interval = tokio::time::interval_at(
                tokio::time::Instant::now() + DIFF_CODEX_PROGRESS_POLL,
                DIFF_CODEX_PROGRESS_POLL,
            );
            progress_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let result = loop {
                tokio::select! {
                    result = &mut command => break result,
                    _ = progress_interval.tick() => {
                        progress.poll(&self.session).await;
                    }
                }
            };
            progress.poll_api(true).await;
            self.stop_capture_proxy(capture_proxy).await?;
            match result {
                Ok(output) => Ok(CodexCommandOutput {
                    status: CodexCommandStatus::Exited(output.exit_code),
                    stdout: {
                        progress.observe_stdout(&output.stdout, true);
                        output.stdout
                    },
                    stderr: {
                        progress.observe_stderr(&output.stderr, true);
                        output.stderr
                    },
                }),
                Err(VmToolSessionError::GuestTimeout { output, .. }) => Ok(CodexCommandOutput {
                    status: CodexCommandStatus::TimedOut,
                    stdout: {
                        progress.observe_stdout(&output.stdout, true);
                        output.stdout
                    },
                    stderr: {
                        progress.observe_stderr(&output.stderr, true);
                        output.stderr
                    },
                }),
                Err(error) => Err(CodexCommandRunnerError::new(error.to_string())),
            }
        })
    }
}

pub(super) struct DiffCodexProgress {
    reporter: DiffProgress,
    api_exchanges: PathBuf,
    api_offset: u64,
    stdout_offset: usize,
    stderr_offset: usize,
    api_read_failed: bool,
    stdout_read_failed: bool,
    stderr_read_failed: bool,
}

impl DiffCodexProgress {
    const fn new(reporter: DiffProgress, api_exchanges: PathBuf) -> Self {
        Self {
            reporter,
            api_exchanges,
            api_offset: 0,
            stdout_offset: 0,
            stderr_offset: 0,
            api_read_failed: false,
            stdout_read_failed: false,
            stderr_read_failed: false,
        }
    }

    async fn poll(&mut self, session: &VmToolSessionHandle) {
        self.poll_api(false).await;
        if !self.stdout_read_failed {
            match session.read_file(DIFF_CODEX_LIVE_STDOUT_FILE).await {
                Ok(contents) => self.observe_stdout(&contents, false),
                Err(error) if progress_file_is_not_ready(&error) => {}
                Err(error) => {
                    self.stdout_read_failed = true;
                    warn!(
                        target: "nanocodex_eval",
                        error = %error,
                        "stopped polling the live stock-Codex stdout mirror"
                    );
                }
            }
        }
        if !self.stderr_read_failed {
            match session.read_file(DIFF_CODEX_LIVE_STDERR_FILE).await {
                Ok(contents) => self.observe_stderr(&contents, false),
                Err(error) if progress_file_is_not_ready(&error) => {}
                Err(error) => {
                    self.stderr_read_failed = true;
                    warn!(
                        target: "nanocodex_eval",
                        error = %error,
                        "stopped polling the live stock-Codex stderr mirror"
                    );
                }
            }
        }
    }

    async fn poll_api(&mut self, terminal: bool) {
        if self.api_read_failed {
            return;
        }
        let mut input = match tokio::fs::File::open(&self.api_exchanges).await {
            Ok(input) => input,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.api_read_failed = true;
                warn!(
                    target: "nanocodex_eval",
                    path = %self.api_exchanges.display(),
                    %error,
                    "stopped polling the live stock-Codex API exchange log"
                );
                return;
            }
        };
        let length = match input.metadata().await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.api_read_failed = true;
                warn!(
                    target: "nanocodex_eval",
                    path = %self.api_exchanges.display(),
                    %error,
                    "stopped polling stock-Codex API exchange metadata"
                );
                return;
            }
        };
        if length < self.api_offset {
            self.api_offset = 0;
        }
        if let Err(error) = input.seek(io::SeekFrom::Start(self.api_offset)).await {
            self.api_read_failed = true;
            warn!(
                target: "nanocodex_eval",
                path = %self.api_exchanges.display(),
                %error,
                "stopped seeking in the stock-Codex API exchange log"
            );
            return;
        }
        let mut pending = Vec::new();
        if let Err(error) = input.read_to_end(&mut pending).await {
            self.api_read_failed = true;
            warn!(
                target: "nanocodex_eval",
                path = %self.api_exchanges.display(),
                %error,
                "stopped reading the stock-Codex API exchange log"
            );
            return;
        }
        let (lines, consumed) = newly_completed_lines(&pending, 0, terminal);
        for line in lines {
            match serde_json::from_slice::<serde_json::Value>(line) {
                Ok(exchange) => self.reporter.observe_api_exchange("codex", &exchange),
                Err(error) => warn!(
                    target: "nanocodex_eval",
                    comparison_arm = "codex",
                    event_bytes = line.len(),
                    %error,
                    "live stock-Codex API exchange was not JSON"
                ),
            }
        }
        self.api_offset = self
            .api_offset
            .saturating_add(u64::try_from(consumed).unwrap_or(u64::MAX));
    }

    fn observe_stdout(&mut self, contents: &[u8], terminal: bool) {
        let (lines, next_offset) = newly_completed_lines(contents, self.stdout_offset, terminal);
        for line in lines {
            match serde_json::from_slice::<serde_json::Value>(line) {
                Ok(event) => self.reporter.observe_codex(&event),
                Err(error) => warn!(
                    target: "nanocodex_eval",
                    comparison_arm = "codex",
                    event_bytes = line.len(),
                    error = %error,
                    "live stock-Codex output was not a JSON event"
                ),
            }
        }
        self.stdout_offset = next_offset;
    }

    fn observe_stderr(&mut self, contents: &[u8], terminal: bool) {
        let (lines, next_offset) = newly_completed_lines(contents, self.stderr_offset, terminal);
        for line in lines {
            self.reporter.observe_codex_diagnostic(line);
        }
        self.stderr_offset = next_offset;
    }
}

pub(super) fn progress_file_is_not_ready(error: &VmToolSessionError) -> bool {
    matches!(error, VmToolSessionError::Guest(message) if message.contains("No such file"))
}

pub(super) fn newly_completed_lines(
    contents: &[u8],
    offset: usize,
    terminal: bool,
) -> (Vec<&[u8]>, usize) {
    if offset > contents.len() {
        return (Vec::new(), 0);
    }
    let pending = &contents[offset..];
    let end = if terminal {
        contents.len()
    } else {
        pending
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(offset, |line_end| offset + line_end + 1)
    };
    let lines = contents[offset..end]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    (lines, end)
}

pub(super) fn validate_vm_guest_elf(bytes: &[u8], path: &Path) -> InternalResult<()> {
    let header = bytes.get(..20).ok_or_else(|| {
        diff_error!(
            "VM guest executable is too short to contain an ELF header: {}",
            path.display()
        )
    })?;
    if &header[..4] != b"\x7fELF" {
        return Err(diff_error!(
            "VM guest executable is not an ELF executable: {}",
            path.display()
        ));
    }
    let class = header[4];
    let byte_order = header[5];
    let machine = u16::from_le_bytes([header[18], header[19]]);
    if class != 2 || byte_order != 1 || machine != VM_GUEST_ELF_MACHINE {
        return Err(diff_error!(
            "VM guest executable {} has ELF class {class}, byte order {byte_order}, and e_machine \
             {machine}; target {VM_GUEST_TARGET} requires 64-bit little-endian e_machine \
             {VM_GUEST_ELF_MACHINE}",
            path.display()
        ));
    }
    Ok(())
}
