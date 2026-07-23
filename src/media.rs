use std::{
    env, fmt,
    fs::{self, File, OpenOptions},
    io::{Cursor, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use image::ImageReader;

use crate::bridge::BridgeStream;

const STREAM_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;
const VIDEO_FPS: u16 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
    pub sequence: u64,
}

#[derive(Debug)]
pub enum MediaUpdate {
    Frame(MediaFrame),
    Finished,
    Failed(String),
}

pub struct MediaPlayback {
    updates: Receiver<MediaUpdate>,
    cancelled: Arc<AtomicBool>,
}

pub struct MediaSink {
    updates: SyncSender<MediaUpdate>,
    cancelled: Arc<AtomicBool>,
}

impl fmt::Debug for MediaPlayback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MediaPlayback { .. }")
    }
}

impl MediaPlayback {
    pub fn channel() -> (MediaSink, Self) {
        let (updates, receiver) = mpsc::sync_channel(2);
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            MediaSink {
                updates,
                cancelled: Arc::clone(&cancelled),
            },
            Self {
                updates: receiver,
                cancelled,
            },
        )
    }

    pub fn try_update(&self) -> Result<MediaUpdate, TryRecvError> {
        self.updates.try_recv()
    }
}

impl Drop for MediaPlayback {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl MediaSink {
    pub fn send(&self, update: MediaUpdate) -> Result<(), mpsc::SendError<MediaUpdate>> {
        self.updates.send(update)
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

pub fn decode_image_stream(
    mut stream: BridgeStream,
    max_width: u16,
    max_height: u16,
    updates: &MediaSink,
) -> Result<()> {
    let mut encoded = Vec::new();
    while !stream.is_closed() {
        if updates.cancelled() {
            return Ok(());
        }
        if let Some(chunk) = stream.read_timeout(STREAM_TIMEOUT)? {
            if encoded.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
                bail!("image exceeds the 128 MiB preview limit");
            }
            encoded.extend_from_slice(&chunk);
        }
    }
    let frame = decode_image(&encoded, max_width, max_height)?;
    updates
        .send(MediaUpdate::Frame(frame))
        .map_err(|_| anyhow!("image preview was closed"))?;
    let _ = updates.send(MediaUpdate::Finished);
    Ok(())
}

pub fn decode_image(encoded: &[u8], max_width: u16, max_height: u16) -> Result<MediaFrame> {
    if encoded.is_empty() {
        bail!("image file is empty");
    }
    let reader = ImageReader::new(Cursor::new(encoded))
        .with_guessed_format()
        .context("image format is not recognized")?;
    let decoded = reader.decode().context("image decoder rejected the file")?;
    let resized = decoded.thumbnail(max_width.max(1) as u32, max_height.max(1) as u32);
    let rgba = resized.to_rgba8();
    let width = u16::try_from(rgba.width()).context("decoded image is too wide")?;
    let height = u16::try_from(rgba.height()).context("decoded image is too tall")?;
    Ok(MediaFrame {
        width,
        height,
        rgba: rgba.into_raw(),
        sequence: 0,
    })
}

pub fn decode_video_stream(
    mut stream: BridgeStream,
    width: u16,
    height: u16,
    updates: &MediaSink,
) -> Result<()> {
    let (temporary, mut file) = TemporaryMedia::create()?;
    while !stream.is_closed() {
        if updates.cancelled() {
            return Ok(());
        }
        if let Some(chunk) = stream.read_timeout(STREAM_TIMEOUT)? {
            file.write_all(&chunk)
                .context("failed to cache encoded video data")?;
        }
    }
    file.flush()
        .context("failed to flush encoded video cache")?;
    drop(file);
    if updates.cancelled() {
        return Ok(());
    }
    decode_video_file(temporary.path(), width, height, updates)
}

fn decode_video_file(path: &Path, width: u16, height: u16, updates: &MediaSink) -> Result<()> {
    let width = width.max(1);
    let height = height.max(2);
    let mut child = spawn_ffmpeg(path, width, height)?;
    let mut stdout = child
        .stdout
        .take()
        .context("FFmpeg stdout was not opened")?;
    let stderr = child
        .stderr
        .take()
        .context("FFmpeg stderr was not opened")?;

    let error_reader = thread::spawn(move || read_decoder_error(stderr));
    let frame_size = width as usize * height as usize * 4;
    let mut sequence = 0u64;
    let mut delivered = false;

    loop {
        let mut rgba = vec![0; frame_size];
        match stdout.read_exact(&mut rgba) {
            Ok(()) => {
                delivered = true;
                let frame = MediaFrame {
                    width,
                    height,
                    rgba,
                    sequence,
                };
                sequence = sequence.wrapping_add(1);
                if updates.send(MediaUpdate::Frame(frame)).is_err() {
                    terminate(&mut child);
                    let _ = error_reader.join();
                    return Ok(());
                }
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => {
                terminate(&mut child);
                let _ = error_reader.join();
                return Err(error).context("failed to read a decoded video frame");
            }
        }
    }

    let status = child.wait().context("failed to wait for FFmpeg")?;
    let decoder_error = error_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = decoder_error.trim();
        if detail.is_empty() {
            bail!("FFmpeg exited with {status}");
        }
        bail!("FFmpeg: {detail}");
    }
    if !delivered {
        bail!("video decoder produced no frames");
    }
    let _ = updates.send(MediaUpdate::Finished);
    Ok(())
}

fn spawn_ffmpeg(path: &Path, width: u16, height: u16) -> Result<Child> {
    let filter = format!(
        "fps={VIDEO_FPS},scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black"
    );
    Command::new(ffmpeg_executable())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-i",
        ])
        .arg(path)
        .args([
            "-map", "0:v:0", "-an", "-vf", &filter, "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            "could not start the controller-side FFmpeg decoder; install FFmpeg, set \
             MUXLOOM_FFMPEG, or use a Muxloom release bundle"
        })
}

fn ffmpeg_executable() -> PathBuf {
    if let Some(path) = env::var_os("MUXLOOM_FFMPEG").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(executable) = env::current_exe() {
        let name = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let bundled = executable.with_file_name(name);
        if bundled.is_file() {
            return bundled;
        }
    }
    PathBuf::from(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    })
}

fn read_decoder_error(mut stderr: impl Read) -> String {
    let mut bytes = Vec::new();
    let _ = stderr.by_ref().take(64 * 1024).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

struct TemporaryMedia {
    path: PathBuf,
}

impl TemporaryMedia {
    fn create() -> Result<(Self, File)> {
        for attempt in 0..100u8 {
            let path = env::temp_dir().join(format!(
                "muxloom-media-{}-{}-{attempt}.video",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("failed to create controller media cache"),
            }
        }
        bail!("could not allocate a unique controller media cache file")
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryMedia {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

    use super::*;

    #[test]
    fn image_decoder_preserves_aspect_ratio_and_rgba_pixels() {
        let source = RgbaImage::from_fn(4, 2, |x, _| {
            if x < 2 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        });
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(source)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();

        let frame = decode_image(encoded.get_ref(), 2, 2).unwrap();
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.rgba.len(), 8);
        assert!(frame.rgba[0] > frame.rgba[2]);
        assert!(frame.rgba[6] > frame.rgba[4]);
    }

    #[test]
    fn ffmpeg_decodes_an_encoded_video_into_rgba_frames_when_available() {
        let ffmpeg = ffmpeg_executable();
        if !Command::new(&ffmpeg)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "muxloom-video-preview-{}-{nonce}.mp4",
            std::process::id()
        ));
        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=16x8",
                "-frames:v",
                "1",
                "-c:v",
                "mpeg4",
                "-y",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let (updates, playback) = MediaPlayback::channel();

        decode_video_file(&path, 8, 4, &updates).unwrap();
        let _ = std::fs::remove_file(&path);

        let MediaUpdate::Frame(frame) = playback.try_update().unwrap() else {
            panic!("decoder did not produce a frame");
        };
        assert_eq!((frame.width, frame.height), (8, 4));
        assert_eq!(frame.rgba.len(), 8 * 4 * 4);
        assert!(matches!(playback.try_update(), Ok(MediaUpdate::Finished)));
    }
}
