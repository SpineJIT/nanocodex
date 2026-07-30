use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chromiumoxide::{
    Page,
    cdp::browser_protocol::page::{
        EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat,
        StartScreencastParams, StopScreencastParams,
    },
};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::{io::AsyncWriteExt, process::Command, sync::watch, task::JoinHandle, time::timeout};

use crate::{BrowserVideoArtifact, trace_serialized};

use super::{BrowserError, evaluate_typed};

const VIDEO_STOP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_VIDEO_FRAMES: usize = 18_000;

pub(super) struct VideoState {
    page: Page,
    path: PathBuf,
    started_at: Instant,
    frames: Arc<AtomicUsize>,
    width: u32,
    height: u32,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), BrowserError>>>,
    stopped: bool,
}

impl Drop for VideoState {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = &self.task {
            task.abort();
        }
        if self.stopped {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let page = self.page.clone();
        runtime.spawn(async move {
            let _ = page.execute(StopScreencastParams::default()).await;
        });
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "encoder, screencast, acknowledgement, and cleanup ordering form one lifecycle"
)]
pub(super) async fn start(
    page: &Page,
    output_dir: &Path,
    sequence: u64,
    frames_per_second: u8,
    quality: u8,
    executable: Option<&Path>,
) -> Result<VideoState, BrowserError> {
    let viewport: Viewport = evaluate_typed(
        page,
        "({ width: window.innerWidth, height: window.innerHeight })",
    )
    .await?;
    let path = output_dir.join(format!("browser-video-{sequence}.webm"));
    let mut command = Command::new(executable.unwrap_or_else(|| Path::new("ffmpeg")));
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "image2pipe",
            "-framerate",
            &frames_per_second.to_string(),
            "-vcodec",
            "mjpeg",
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-pix_fmt",
            "yuv420p",
            "-threads",
            "1",
            "-y",
        ])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(BrowserError::VideoEncoderStart)?;
    let mut stdin = child.stdin.take().ok_or(BrowserError::VideoEncoderStdin)?;
    let mut events = page.event_listener::<EventScreencastFrame>().await?;
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let frames = Arc::new(AtomicUsize::new(0));
    let task_frames = Arc::clone(&frames);
    let task_page = page.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                event = events.next() => {
                    let Some(event) = event else {
                        break;
                    };
                    trace_serialized("devtools.Page.screencastFrame", event.as_ref());
                    let retained = task_frames.load(Ordering::Relaxed) < MAX_VIDEO_FRAMES;
                    if retained {
                        let frame = STANDARD
                            .decode(AsRef::<str>::as_ref(&event.data))
                            .map_err(BrowserError::VideoFrameDecode)?;
                        stdin.write_all(&frame).await?;
                        task_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    task_page
                        .execute(ScreencastFrameAckParams::new(event.session_id))
                        .await?;
                }
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
        stdin.shutdown().await?;
        drop(stdin);
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            return Err(BrowserError::VideoEncoderFailed {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(4_000)
                    .collect(),
            });
        }
        Ok(())
    });
    let params = StartScreencastParams {
        format: Some(StartScreencastFormat::Jpeg),
        quality: Some(i64::from(quality)),
        max_width: Some(i64::from(viewport.width)),
        max_height: Some(i64::from(viewport.height)),
        every_nth_frame: Some(1),
    };
    if let Err(error) = page.execute(params).await {
        let _ = stop_tx.send(true);
        task.abort();
        return Err(error.into());
    }
    Ok(VideoState {
        page: page.clone(),
        path,
        started_at: Instant::now(),
        frames,
        width: viewport.width,
        height: viewport.height,
        stop: stop_tx,
        task: Some(task),
        stopped: false,
    })
}

pub(super) async fn stop(
    page: &Page,
    mut state: VideoState,
) -> Result<BrowserVideoArtifact, BrowserError> {
    page.execute(StopScreencastParams::default()).await?;
    state.stopped = true;
    let _ = state.stop.send(true);
    let task = state.task.take().ok_or(BrowserError::VideoUnavailable)?;
    timeout(VIDEO_STOP_TIMEOUT, task)
        .await
        .map_err(|_| BrowserError::VideoStopTimeout)?
        .map_err(BrowserError::VideoTask)??;
    Ok(BrowserVideoArtifact {
        path: state.path.clone(),
        frame_count: state.frames.load(Ordering::Relaxed),
        duration_ms: u64::try_from(state.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        width: state.width,
        height: state.height,
    })
}

#[derive(Deserialize)]
struct Viewport {
    width: u32,
    height: u32,
}
