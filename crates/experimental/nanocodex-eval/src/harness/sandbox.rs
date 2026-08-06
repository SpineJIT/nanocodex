use super::*;

pub(super) struct HarnessVmResources {
    environment: VmEnvironment,
    backend: VmBackend,
    ca_bundle: Option<HarnessCaBundle>,
}

pub(super) struct HarnessRelease {
    pub(super) root: PathBuf,
    ca_bundle: Option<HarnessCaBundle>,
}

pub(super) fn prepare_harness_release(
    output_parent: &Path,
    harness_binary: &Path,
) -> InternalResult<HarnessRelease> {
    let releases = output_parent.join(".harness-releases");
    fs::create_dir_all(&releases)?;
    let temporary = tempfile::tempdir_in(&releases)?;
    let staged_command = temporary.path().join("command");
    reflink_or_sparse_copy(harness_binary, &staged_command)?;
    fs::set_permissions(&staged_command, fs::Permissions::from_mode(0o755))?;
    let mut header = [0_u8; 20];
    fs::File::open(&staged_command)?.read_exact(&mut header)?;
    validate_vm_guest_elf(&header, &staged_command)?;
    let ca_bundle = resolve_harness_ca_source()?
        .as_ref()
        .map(|source| stage_harness_ca_bundle(source, temporary.path()))
        .transpose()?;
    let root = temporary.keep();
    Ok(HarnessRelease { root, ca_bundle })
}

pub(super) async fn prepare_harness_vm_resources(
    task: &Task,
    vm: &VmResources,
    guest_memory_mb: u64,
    web_search: bool,
    release: &HarnessRelease,
) -> InternalResult<HarnessVmResources> {
    let environment = vm.environment(task).await?;
    let backend = vm
        .backend_for_task_with_guest_memory(
            VmBackend::builder()
                .retain_passed_rootfs(false)
                .retain_failed_rootfs(false)
                .web_search(web_search)
                .shared_directory(SharedDirectory::read_only(
                    HARNESS_SHARE_TAG,
                    release.root.clone(),
                )),
            task,
            guest_memory_mb,
        )
        .await?;
    Ok(HarnessVmResources {
        environment,
        backend,
        ca_bundle: release.ca_bundle,
    })
}

impl HarnessVmResources {
    pub(super) fn backend(&self) -> VmBackend {
        self.backend.clone()
    }

    pub(super) fn harness_attempt(
        &self,
        runtime: VmAttempt,
        attempt: EvalAttempt<'_>,
        command: HarnessExec,
        auth: HarnessAuth,
        guest: HarnessGuestConfig,
    ) -> InternalResult<AttemptAgent, VmAttemptError> {
        let session = runtime.session_handle()?;
        let runner = VmHarnessRunner::new(
            session,
            attempt,
            &self.environment,
            auth,
            self.ca_bundle,
            guest,
        )?;
        let api_base_url = runner.api_base_url().to_owned();
        let runner = Arc::new(runner);
        let readiness = Arc::clone(&runner);
        Ok(runtime
            .harness(command.api_base_url(api_base_url).command_runner(runner))
            .ready(async move { readiness.prepare().await }))
    }
}

#[derive(Clone, Copy)]
pub(super) struct HarnessCaBundle {
    pub(super) guest_environment: &'static str,
}

pub(super) struct HarnessCaSource {
    pub(super) path: PathBuf,
    pub(super) source_environment: &'static str,
    pub(super) guest_environment: &'static str,
}

pub(super) fn resolve_harness_ca_source() -> InternalResult<Option<HarnessCaSource>, io::Error> {
    for (source_environment, guest_environment) in [
        (
            HARNESS_SSL_CERT_FILE_ENVIRONMENT,
            HARNESS_SSL_CERT_FILE_ENVIRONMENT,
        ),
        (
            HARNESS_NIX_SSL_CERT_FILE_ENVIRONMENT,
            HARNESS_SSL_CERT_FILE_ENVIRONMENT,
        ),
    ] {
        let Some(path) = std::env::var_os(source_environment).filter(|value| !value.is_empty())
        else {
            continue;
        };
        return Ok(Some(HarnessCaSource {
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
            return Ok(Some(HarnessCaSource {
                path: fs::canonicalize(path)?,
                source_environment: "host_system",
                guest_environment: HARNESS_SSL_CERT_FILE_ENVIRONMENT,
            }));
        }
    }
    Ok(None)
}

pub(super) fn stage_harness_ca_bundle(
    source: &HarnessCaSource,
    share_root: &Path,
) -> InternalResult<HarnessCaBundle, io::Error> {
    let staged = share_root.join(HARNESS_CA_BUNDLE_FILENAME);
    reflink_or_sparse_copy(&source.path, &staged)?;
    if staged.metadata()?.len() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "harness CA bundle selected by {} is empty: {}",
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
        "staged the host CA bundle for the pinned guest harness"
    );
    Ok(HarnessCaBundle {
        guest_environment: source.guest_environment,
    })
}

pub(super) enum GuestAuth {
    ApiKey(Arc<str>),
    AuthFile(Vec<u8>),
}

pub(super) struct VmHarnessRunner {
    session: VmToolSessionHandle,
    workspace: String,
    environment: Vec<(String, String)>,
    auth_file: Option<Vec<u8>>,
    harness_home: String,
    harness_auth_file: String,
    capture_upstream: String,
    capture_listener: Mutex<Option<TcpListener>>,
    capture_base_url: String,
    api_exchanges: PathBuf,
}

impl VmHarnessRunner {
    fn new(
        session: VmToolSessionHandle,
        attempt: EvalAttempt<'_>,
        environment: &VmEnvironment,
        auth: HarnessAuth,
        ca_bundle: Option<HarnessCaBundle>,
        guest: HarnessGuestConfig,
    ) -> InternalResult<Self, VmAttemptError> {
        if !Path::new(&guest.home).is_absolute() || !Path::new(&guest.auth_file).is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "harness home and auth_file must be absolute guest paths",
            )
            .into());
        }
        if guest.api_key_environment.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "harness api_key_environment must not be empty",
            )
            .into());
        }
        let artifact_directory = attempt.directory().join("agent");
        fs::create_dir_all(&artifact_directory)?;
        let auth = match auth.kind {
            HarnessAuthKind::ApiKey(api_key) => GuestAuth::ApiKey(api_key),
            HarnessAuthKind::AuthFile(path) => {
                let contents = fs::read(&path)?;
                GuestAuth::AuthFile(contents)
            }
        };
        let capture_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let capture_port = capture_listener.local_addr()?.port();
        let capture_base_url = capture_proxy_vm_base_url(capture_port);
        let mut command_environment = environment.guest_environment(attempt.task());
        command_environment.extend(guest.environment.into_iter().map(|(key, value)| {
            let value = value
                .replace("{api_base_url}", &capture_base_url)
                .replace("{harness_home}", &guest.home)
                .replace("{auth_file}", &guest.auth_file);
            (key, value)
        }));
        command_environment.insert("NANOCODEX_HARNESS_HOME".to_owned(), guest.home.clone());
        command_environment.insert(
            "NANOCODEX_HARNESS_AUTH_FILE".to_owned(),
            guest.auth_file.clone(),
        );
        command_environment.insert(
            "NANOCODEX_HARNESS_API_BASE_URL".to_owned(),
            capture_base_url.clone(),
        );
        if let Some(ca_bundle) = ca_bundle {
            command_environment.insert(
                ca_bundle.guest_environment.to_owned(),
                HARNESS_CA_BUNDLE_FILE.to_owned(),
            );
        }
        let auth_file = match auth {
            GuestAuth::ApiKey(api_key) => {
                command_environment.insert(guest.api_key_environment.clone(), api_key.to_string());
                None
            }
            GuestAuth::AuthFile(contents) => {
                command_environment.remove(&guest.api_key_environment);
                Some(contents)
            }
        };
        let capture_upstream = guest
            .api_upstream
            .unwrap_or_else(|| HARNESS_CAPTURE_PROXY_API_UPSTREAM.to_owned());
        Ok(Self {
            session,
            workspace: environment.workspace().to_owned(),
            environment: command_environment.into_iter().collect(),
            auth_file,
            harness_home: guest.home,
            harness_auth_file: guest.auth_file,
            capture_upstream,
            capture_listener: Mutex::new(Some(capture_listener)),
            capture_base_url,
            api_exchanges: artifact_directory.join(HARNESS_API_EXCHANGES_FILENAME),
        })
    }

    fn api_base_url(&self) -> &str {
        &self.capture_base_url
    }

    async fn prepare(&self) -> InternalResult<(), VmAttemptError> {
        self.session.ready().await?;
        self.session
            .create_directory(HARNESS_SHARE_MOUNT, 0o755, None)
            .await?;
        let mount = self
            .session
            .command(
                VmCommand::new("/bin/mount")
                    .arg("-t")
                    .arg("virtiofs")
                    .arg("-o")
                    .arg("ro")
                    .arg(HARNESS_SHARE_TAG)
                    .arg(HARNESS_SHARE_MOUNT)
                    .environment(self.environment.clone())
                    .timeout(HARNESS_SETUP_TIMEOUT),
            )
            .await?;
        if mount.exit_code != 0 {
            return Err(io::Error::other(format!(
                "failed to mount the pinned harness in the guest (exit {}): {}",
                mount.exit_code,
                String::from_utf8_lossy(&mount.stderr).trim()
            ))
            .into());
        }
        self.session
            .create_directory(&self.harness_home, 0o700, None)
            .await?;
        if let Some(auth_file) = &self.auth_file {
            let parent = Path::new(&self.harness_auth_file).parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "harness auth_file has no parent",
                )
            })?;
            self.session
                .create_directory(parent.to_string_lossy(), 0o700, None)
                .await?;
            self.session
                .write_file(&self.harness_auth_file, auth_file.clone(), 0o600)
                .await?;
        }
        Ok(())
    }

    async fn start_capture_proxy(
        &self,
    ) -> InternalResult<ResponsesCaptureProxy, HarnessCommandRunnerError> {
        let listener = {
            let mut listener = self.capture_listener.lock().map_err(|_| {
                HarnessCommandRunnerError::new("Responses capture listener lock was poisoned")
            })?;
            listener.take().ok_or_else(|| {
                HarnessCommandRunnerError::new(
                    "Responses capture proxy was already started for this attempt",
                )
            })?
        };
        let proxy = ResponsesCaptureProxy::start(
            listener,
            ResponsesCaptureProxyConfig {
                upstream: self.capture_upstream.to_owned(),
                output: self.api_exchanges.clone(),
            },
        )
        .await
        .map_err(|error| HarnessCommandRunnerError::new(error.to_string()))?;
        Ok(proxy)
    }

    async fn stop_capture_proxy(
        &self,
        proxy: ResponsesCaptureProxy,
    ) -> InternalResult<(), HarnessCommandRunnerError> {
        match tokio::time::timeout(HARNESS_CAPTURE_PROXY_STOP_TIMEOUT, proxy.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(HarnessCommandRunnerError::new(error.to_string())),
            Err(_) => {
                return Err(HarnessCommandRunnerError::new(format!(
                    "Responses capture proxy did not stop within {:?}",
                    HARNESS_CAPTURE_PROXY_STOP_TIMEOUT
                )));
            }
        }
        Ok(())
    }
}

pub(super) fn capture_proxy_vm_base_url(port: u16) -> String {
    format!("http://{}:{port}", Gvproxy::HOST_IPV4)
}

impl HarnessCommandRunner for VmHarnessRunner {
    fn run<'a>(
        &'a self,
        arguments: Vec<String>,
        timeout: Duration,
    ) -> Pin<
        Box<
            dyn Future<Output = InternalResult<HarnessCommandOutput, HarnessCommandRunnerError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let capture_proxy = self.start_capture_proxy().await?;
            let mut command = VmCommand::new(HARNESS_GUEST_BINARY)
                .current_directory(&self.workspace)
                .environment(self.environment.clone())
                .timeout(timeout)
                .max_output_bytes(HARNESS_OUTPUT_BYTES);
            for argument in arguments {
                command = command.arg(argument);
            }
            let result = self.session.command(command).await;
            self.stop_capture_proxy(capture_proxy).await?;
            match result {
                Ok(output) => Ok(HarnessCommandOutput {
                    status: HarnessCommandStatus::Exited(output.exit_code),
                    stdout: output.stdout,
                    stderr: output.stderr,
                }),
                Err(VmToolSessionError::GuestTimeout { output, .. }) => Ok(HarnessCommandOutput {
                    status: HarnessCommandStatus::TimedOut,
                    stdout: output.stdout,
                    stderr: output.stderr,
                }),
                Err(error) => Err(HarnessCommandRunnerError::new(error.to_string())),
            }
        })
    }
}

pub(super) fn validate_vm_guest_elf(bytes: &[u8], path: &Path) -> InternalResult<()> {
    let header = bytes.get(..20).ok_or_else(|| {
        harness_error!(
            "VM guest executable is too short to contain an ELF header: {}",
            path.display()
        )
    })?;
    if &header[..4] != b"\x7fELF" {
        return Err(harness_error!(
            "VM guest executable is not an ELF executable: {}",
            path.display()
        ));
    }
    let class = header[4];
    let byte_order = header[5];
    let machine = u16::from_le_bytes([header[18], header[19]]);
    if class != 2 || byte_order != 1 || machine != VM_GUEST_ELF_MACHINE {
        return Err(harness_error!(
            "VM guest executable {} has ELF class {class}, byte order {byte_order}, and e_machine \
             {machine}; target {VM_GUEST_TARGET} requires 64-bit little-endian e_machine \
             {VM_GUEST_ELF_MACHINE}",
            path.display()
        ));
    }
    Ok(())
}
