use crate::recorder::wav_writer::WavWriter;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use tracing::{debug, error, info};

use std::time::{Duration, Instant};
use tauri::Emitter;

/// Simple result type using String for errors
pub type Result<T> = std::result::Result<T, String>;

/// Recording mode - Manual or VAD-based
#[derive(Debug, Clone)]
pub enum RecordingMode {
    Manual,
    Vad {
        threshold: f32,
        silence_timeout_ms: u32,
        detector: Arc<Mutex<voice_activity_detector::VoiceActivityDetector>>,
    },
}

/// VAD segment state
struct VadSegmentState {
    is_speaking: bool,
    last_speech_time: Option<Instant>,
    segment_count: u32,
    audio_buffer: Vec<f32>,
    output_folder: PathBuf,
    sample_rate: u32,
    channels: u16,
    current_writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
    current_file_path: Option<PathBuf>,
}

/// Audio recording metadata - returned to frontend
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioRecording {
    pub audio_data: Vec<f32>, // Empty for file-based recording
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_seconds: f32,
    pub file_path: Option<String>, // Path to the WAV file
}

/// Simple recorder commands for worker thread communication
#[derive(Debug)]
enum RecorderCmd {
    Start(mpsc::Sender<()>), // Response channel to confirm command processed
    Stop(mpsc::Sender<()>),  // Response channel to confirm command processed
    Shutdown,
}

/// Simplified recorder state
pub struct RecorderState {
    cmd_tx: Option<mpsc::Sender<RecorderCmd>>,
    worker_handle: Option<JoinHandle<()>>,
    writer: Option<Arc<Mutex<WavWriter>>>,
    is_recording: Arc<AtomicBool>,
    sample_rate: u32,
    channels: u16,
    file_path: Option<PathBuf>,
    mode: RecordingMode,
    vad_state: Option<Arc<Mutex<VadSegmentState>>>,
}

impl RecorderState {
    pub fn new() -> Self {
        Self {
            cmd_tx: None,
            worker_handle: None,
            writer: None,
            is_recording: Arc::new(AtomicBool::new(false)),
            sample_rate: 0,
            channels: 0,
            file_path: None,
            mode: RecordingMode::Manual,
            vad_state: None,
        }
    }

    /// List available recording devices by name
    pub fn enumerate_devices(&self) -> Result<Vec<String>> {
        let host = cpal::default_host();
        let devices = host
            .input_devices()
            .map_err(|e| format!("Failed to get input devices: {}", e))?
            .filter_map(|device| device.name().ok())
            .collect();

        Ok(devices)
    }

    /// Initialize recording session - creates stream and WAV writer
    pub fn init_session(
        &mut self,
        device_name: String,
        output_folder: PathBuf,
        recording_id: String,
        preferred_sample_rate: Option<u32>,
    ) -> Result<()> {
        // Clean up any existing session
        self.close_session()?;

        // Create file path
        let file_path = output_folder.join(format!("{}.wav", recording_id));

        // Find the device
        let host = cpal::default_host();
        let device = find_device(&host, &device_name)?;

        // Get optimal config for voice with optional preferred sample rate
        let config = get_optimal_config(&device, preferred_sample_rate)?;
        let sample_format = config.sample_format();
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        // Create WAV writer
        let writer = WavWriter::new(file_path.clone(), sample_rate, channels)
            .map_err(|e| format!("Failed to create WAV file: {}", e))?;
        let writer = Arc::new(Mutex::new(writer));

        // Create stream config
        let stream_config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        // Create fresh recording flag
        self.is_recording = Arc::new(AtomicBool::new(false));
        let is_recording = self.is_recording.clone();

        // Create command channel for worker thread
        let (cmd_tx, cmd_rx) = mpsc::channel();

        // Clone for the worker thread
        let writer_clone = writer.clone();
        let is_recording_clone = is_recording.clone();

        // Create the worker thread that owns the stream
        let worker = thread::spawn(move || {
            // Build the stream IN this thread (required for macOS)
            let stream = match build_input_stream(
                &device,
                &stream_config,
                sample_format,
                is_recording_clone,
                writer_clone,
            ) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to build stream: {}", e);
                    return;
                }
            };

            // Start the stream
            if let Err(e) = stream.play() {
                error!("Failed to start stream: {}", e);
                return;
            }

            info!("Audio stream started successfully");

            // Keep thread alive by waiting for commands
            // This blocks but is responsive - no sleeping!
            loop {
                match cmd_rx.recv() {
                    Ok(RecorderCmd::Start(reply_tx)) => {
                        is_recording.store(true, Ordering::Relaxed);
                        info!("Recording started");
                        let _ = reply_tx.send(()); // Confirm command processed
                    }
                    Ok(RecorderCmd::Stop(reply_tx)) => {
                        is_recording.store(false, Ordering::Relaxed);
                        info!("Recording stopped");
                        let _ = reply_tx.send(()); // Confirm command processed
                    }
                    Ok(RecorderCmd::Shutdown) | Err(_) => {
                        info!("Shutting down audio worker");
                        break;
                    }
                }
            }
            // Stream automatically drops here
        });

        // Store everything
        self.cmd_tx = Some(cmd_tx);
        self.worker_handle = Some(worker);
        self.writer = Some(writer);
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.file_path = Some(file_path);

        info!(
            "Recording session initialized: {} Hz, {} channels, file: {:?}",
            sample_rate, channels, self.file_path
        );

        Ok(())
    }

    /// Initialize VAD recording session - creates stream with VAD detection
    pub fn init_vad_session(
        &mut self,
        device_name: String,
        output_folder: PathBuf,
        threshold: f32,
        silence_timeout_ms: u32,
        preferred_sample_rate: Option<u32>,
        app_handle: tauri::AppHandle,
    ) -> Result<()> {
        // Clean up any existing session
        self.close_session()?;

        // Store the recording mode
        let vad_detector = voice_activity_detector::VoiceActivityDetector::builder()
            .sample_rate(preferred_sample_rate.unwrap_or(16000) as i64)
            .chunk_size(512usize)
            .build()
            .map_err(|e| format!("Failed to create VAD detector: {:?}", e))?;

        self.mode = RecordingMode::Vad {
            threshold,
            silence_timeout_ms,
            detector: Arc::new(Mutex::new(vad_detector)),
        };

        // Find the device
        let host = cpal::default_host();
        let device = find_device(&host, &device_name)?;

        // Get optimal config for voice with preferred sample rate (16kHz optimal for VAD)
        let config = get_optimal_config(&device, preferred_sample_rate)?;
        let sample_format = config.sample_format();
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        // Create stream config
        let stream_config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        // Initialize VAD state
        let vad_state = Arc::new(Mutex::new(VadSegmentState {
            is_speaking: false,
            last_speech_time: None,
            segment_count: 0,
            audio_buffer: Vec::new(),
            output_folder: output_folder.clone(),
            sample_rate,
            channels,
            current_writer: None,
            current_file_path: None,
        }));
        self.vad_state = Some(vad_state.clone());

        // Create fresh recording flag
        self.is_recording = Arc::new(AtomicBool::new(false));
        let is_recording = self.is_recording.clone();

        // Create command channel for worker thread
        let (cmd_tx, cmd_rx) = mpsc::channel();

        // Clone for the worker thread
        let mode_clone = self.mode.clone();
        let is_recording_clone = is_recording.clone();
        let vad_state_clone = vad_state.clone();

        // Create the worker thread that owns the stream
        let worker = thread::spawn(move || {
            // Build the stream IN this thread (required for macOS)
            let stream = match build_input_stream_vad(
                &device,
                &stream_config,
                sample_format,
                is_recording_clone,
                mode_clone,
                vad_state_clone,
                app_handle,
            ) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to build VAD stream: {}", e);
                    return;
                }
            };

            // Start the stream
            if let Err(e) = stream.play() {
                error!("Failed to start VAD stream: {}", e);
                return;
            }

            info!("VAD audio stream started successfully");

            // Keep thread alive by waiting for commands
            loop {
                match cmd_rx.recv() {
                    Ok(RecorderCmd::Start(reply_tx)) => {
                        is_recording.store(true, Ordering::Relaxed);
                        info!("VAD recording started");
                        let _ = reply_tx.send(());
                    }
                    Ok(RecorderCmd::Stop(reply_tx)) => {
                        is_recording.store(false, Ordering::Relaxed);
                        info!("VAD recording stopped");
                        let _ = reply_tx.send(());
                    }
                    Ok(RecorderCmd::Shutdown) | Err(_) => {
                        info!("Shutting down VAD audio worker");
                        break;
                    }
                }
            }
        });

        // Store everything
        self.cmd_tx = Some(cmd_tx);
        self.worker_handle = Some(worker);
        self.writer = None; // VAD creates writers dynamically
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.file_path = None; // VAD creates files dynamically

        info!(
            "VAD recording session initialized: {} Hz, {} channels",
            sample_rate, channels
        );

        // Auto-start VAD (it runs continuously)
        self.start_recording()?;

        Ok(())
    }

    /// Start recording - send command to worker thread and wait for confirmation
    pub fn start_recording(&mut self) -> Result<()> {
        if let Some(tx) = &self.cmd_tx {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(RecorderCmd::Start(reply_tx))
                .map_err(|e| format!("Failed to send start command: {}", e))?;
            // Wait for worker thread to confirm the command was processed
            reply_rx.recv()
                .map_err(|e| format!("Failed to receive start confirmation: {}", e))?;
        } else {
            return Err("No recording session initialized".to_string());
        }
        Ok(())
    }

    /// Stop recording - return file info
    pub fn stop_recording(&mut self) -> Result<AudioRecording> {
        // Send stop command to worker thread and wait for confirmation
        if let Some(tx) = &self.cmd_tx {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(RecorderCmd::Stop(reply_tx))
                .map_err(|e| format!("Failed to send stop command: {}", e))?;
            // Wait for worker thread to confirm the command was processed
            reply_rx.recv()
                .map_err(|e| format!("Failed to receive stop confirmation: {}", e))?;
        }

        // Finalize the WAV file and get metadata
        let (sample_rate, channels, duration) = if let Some(writer) = &self.writer {
            let mut w = writer
                .lock()
                .map_err(|e| format!("Failed to lock writer: {}", e))?;
            w.finalize()
                .map_err(|e| format!("Failed to finalize WAV: {}", e))?;
            w.get_metadata()
        } else {
            (self.sample_rate, self.channels, 0.0)
        };

        let file_path = self
            .file_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        info!("Recording stopped: {:.2}s, file: {:?}", duration, file_path);

        Ok(AudioRecording {
            audio_data: Vec::new(), // Empty for file-based recording
            sample_rate,
            channels,
            duration_seconds: duration,
            file_path,
        })
    }

    /// Cancel recording - stop and delete the file
    pub fn cancel_recording(&mut self) -> Result<()> {
        // Send stop command
        if let Some(tx) = &self.cmd_tx {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = tx.send(RecorderCmd::Stop(reply_tx));
            let _ = reply_rx.recv(); // Wait for confirmation but ignore errors during cancel
        }

        // Delete the file if it exists
        if let Some(file_path) = &self.file_path {
            std::fs::remove_file(file_path).ok(); // Ignore errors
            debug!("Deleted recording file: {:?}", file_path);
        }

        // Clear the session
        self.close_session()?;

        Ok(())
    }

    /// Close the recording session
    pub fn close_session(&mut self) -> Result<()> {
        // Send shutdown command to worker thread
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(RecorderCmd::Shutdown);
        }

        // Wait for worker thread to finish
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        // Finalize and drop the writer
        if let Some(writer) = self.writer.take() {
            if let Ok(mut w) = writer.lock() {
                let _ = w.finalize(); // Ignore errors during cleanup
            }
        }

        // Clear state
        self.file_path = None;
        self.sample_rate = 0;
        self.channels = 0;

        debug!("Recording session closed");
        Ok(())
    }

    /// Get current recording ID if actively recording
    pub fn get_current_recording_id(&self) -> Option<String> {
        if self.is_recording.load(Ordering::Acquire) {
            self.file_path
                .as_ref()
                .and_then(|path| path.file_stem())
                .and_then(|stem| stem.to_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    }
}

/// Find a recording device by name
fn find_device(host: &cpal::Host, device_name: &str) -> Result<Device> {
    // Handle "default" device
    if device_name.to_lowercase() == "default" {
        return host
            .default_input_device()
            .ok_or_else(|| "No default input device available".to_string());
    }

    // Find specific device
    let devices: Vec<_> = host.input_devices().map_err(|e| e.to_string())?.collect();

    for device in devices {
        if let Ok(name) = device.name() {
            if name == device_name {
                return Ok(device);
            }
        }
    }

    Err(format!("Device '{}' not found", device_name))
}

/// Check if a sample format is supported by the recorder
fn is_supported_format(format: SampleFormat) -> bool {
    matches!(format, SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16)
}

/// Get optimal configuration for voice recording
fn get_optimal_config(
    device: &Device,
    preferred_sample_rate: Option<u32>,
) -> Result<cpal::SupportedStreamConfig> {
    // Use preferred sample rate or default to 16kHz for voice
    let target_sample_rate = preferred_sample_rate.unwrap_or(16000);

    let configs: Vec<_> = device
        .supported_input_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| is_supported_format(c.sample_format()))
        .collect();

    if configs.is_empty() {
        return Err("No supported input configurations (need F32, I16, or U16 format)".to_string());
    }

    // Try to find mono config with target sample rate
    for config in &configs {
        if config.channels() == 1 {
            let min_rate = config.min_sample_rate().0;
            let max_rate = config.max_sample_rate().0;
            if min_rate <= target_sample_rate && max_rate >= target_sample_rate {
                return Ok(config.with_sample_rate(cpal::SampleRate(target_sample_rate)));
            }
        }
    }

    // Try stereo with target sample rate if mono not available
    for config in &configs {
        let min_rate = config.min_sample_rate().0;
        let max_rate = config.max_sample_rate().0;
        if min_rate <= target_sample_rate && max_rate >= target_sample_rate {
            return Ok(config.with_sample_rate(cpal::SampleRate(target_sample_rate)));
        }
    }

    // If target rate not supported, try to find closest rate
    let mut best_config = None;
    let mut best_diff = u32::MAX;

    for config in &configs {
        // Prefer mono
        if config.channels() == 1 {
            let min_rate = config.min_sample_rate().0;
            let max_rate = config.max_sample_rate().0;

            // Find closest supported rate
            let closest_rate = if target_sample_rate < min_rate {
                min_rate
            } else if target_sample_rate > max_rate {
                max_rate
            } else {
                target_sample_rate
            };

            let diff = (closest_rate as i32 - target_sample_rate as i32).abs() as u32;
            if diff < best_diff {
                best_diff = diff;
                best_config = Some(config.with_sample_rate(cpal::SampleRate(closest_rate)));
            }
        }
    }

    // Return best config or fall back to default
    best_config
        .or_else(|| device.default_input_config().ok())
        .ok_or_else(|| "Failed to find suitable audio configuration".to_string())
}

/// Build input stream for any supported sample format
fn build_input_stream(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    is_recording: Arc<AtomicBool>,
    writer: Arc<Mutex<WavWriter>>,
) -> Result<Stream> {
    let err_fn = |err: cpal::StreamError| error!("Audio stream error: {}", err);

    let stream = match sample_format {
        SampleFormat::F32 => device
            .build_input_stream(
                config,
                move |data: &[f32], _: &_| {
                    if is_recording.load(Ordering::Relaxed) {
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_samples_f32(data);
                        }
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("Failed to build F32 stream: {}", e))?,
        SampleFormat::I16 => device
            .build_input_stream(
                config,
                move |data: &[i16], _: &_| {
                    if is_recording.load(Ordering::Relaxed) {
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_samples_i16(data);
                        }
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("Failed to build I16 stream: {}", e))?,
        SampleFormat::U16 => device
            .build_input_stream(
                config,
                move |data: &[u16], _: &_| {
                    if is_recording.load(Ordering::Relaxed) {
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_samples_u16(data);
                        }
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("Failed to build U16 stream: {}", e))?,
        _ => return Err(format!("Unsupported sample format: {:?}", sample_format)),
    };

    Ok(stream)
}

/// Build input stream with VAD detection capabilities
fn build_input_stream_vad(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    is_recording: Arc<AtomicBool>,
    mode: RecordingMode,
    vad_state: Arc<Mutex<VadSegmentState>>,
    app_handle: tauri::AppHandle,
) -> Result<Stream> {
    let err_fn = |err: cpal::StreamError| error!("Audio stream error: {}", err);

    // Extract VAD parameters
    let (threshold, silence_timeout, detector) = match mode {
        RecordingMode::Vad { threshold, silence_timeout_ms, detector } => {
            (threshold, Duration::from_millis(silence_timeout_ms as u64), detector)
        }
        RecordingMode::Manual => {
            return Err("build_input_stream_vad called with Manual mode".to_string());
        }
    };

    let err_fn = |err: cpal::StreamError| error!("Audio stream error: {}", err);

    // Create a macro to handle VAD processing for different sample formats
    macro_rules! process_vad_samples {
        ($data:expr, $sample_type:ty) => {{
            if !is_recording.load(Ordering::Relaxed) {
                return;
            }

            // Get VAD state lock
            let mut state = match vad_state.lock() {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to lock VAD state: {}", e);
                    return;
                }
            };

            // Convert samples to f32 and buffer them
            let f32_samples: Vec<f32> = $data.iter().map(|&s| cpal::Sample::to_float_sample(s)).collect();
            state.audio_buffer.extend_from_slice(&f32_samples);

            // Process in 512-sample chunks
            while state.audio_buffer.len() >= 512 {
                let chunk: Vec<f32> = state.audio_buffer.drain(..512).collect();

                // Run VAD detection
                let is_speech = {
                    let mut vad = match detector.lock() {
                        Ok(v) => v,
                        Err(e) => {
                            error!("Failed to lock VAD detector: {}", e);
                            continue;
                        }
                    };
                    let probability = vad.predict(chunk.iter().copied());
                    probability > threshold
                };

                let now = Instant::now();

                // Update last speech time
                if is_speech {
                    state.last_speech_time = Some(now);
                }

                // Check if we should be recording
                let should_record = if let Some(last) = state.last_speech_time {
                    now.duration_since(last) < silence_timeout
                } else {
                    false
                };

                // Handle segment start
                if should_record && !state.is_speaking {
                    state.is_speaking = true;
                    state.segment_count += 1;

                    // Create new WAV file for this segment
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    let file_name = format!("vad_{}_{}.wav", timestamp, state.segment_count);
                    let file_path = state.output_folder.join(&file_name);

                    match create_wav_writer(&file_path, state.sample_rate, state.channels) {
                        Ok(writer) => {
                            state.current_writer = Some(writer);
                            state.current_file_path = Some(file_path.clone());
                            info!("VAD: Speech started - segment {} - {:?}", state.segment_count, file_path);
                            let _ = app_handle.emit("vad-speech-start", ());
                        }
                        Err(e) => {
                            error!("Failed to create WAV writer: {}", e);
                        }
                    }
                }

                // Handle segment end
                if !should_record && state.is_speaking {
                    state.is_speaking = false;

                    // Finalize writer and get file path
                    if let Some(writer) = state.current_writer.take() {
                        drop(writer); // Finalize by dropping

                        if let Some(file_path) = state.current_file_path.take() {
                            info!("VAD: Speech ended - segment {} - {:?}", state.segment_count, file_path);

                            // Read file contents and emit event
                            match std::fs::read(&file_path) {
                                Ok(bytes) => {
                                    #[derive(serde::Serialize, Clone)]
                                    struct VadSpeechDetectedEvent {
                                        #[serde(rename = "filePath")]
                                        file_path: String,
                                        #[serde(rename = "fileContents")]
                                        file_contents: Vec<u8>,
                                    }

                                    let _ = app_handle.emit("vad-speech-detected", VadSpeechDetectedEvent {
                                        file_path: file_path.to_string_lossy().to_string(),
                                        file_contents: bytes,
                                    });
                                }
                                Err(e) => {
                                    error!("Failed to read VAD file {:?}: {}", file_path, e);
                                }
                            }
                        }
                    }
                }

                // Write audio to file if currently in speech segment
                if state.is_speaking {
                    if let Some(ref mut writer) = state.current_writer {
                        for sample in &chunk {
                            let _ = writer.write_sample(*sample);
                        }
                    }
                }
            }
        }};
    }

    let stream = match sample_format {
        SampleFormat::F32 => device
            .build_input_stream(
                config,
                move |data: &[f32], _: &_| { process_vad_samples!(data, f32); },
                err_fn,
                None,
            )
            .map_err(|e| format!("Failed to build VAD F32 stream: {}", e))?,
        SampleFormat::I16 => device
            .build_input_stream(
                config,
                move |data: &[i16], _: &_| { process_vad_samples!(data, i16); },
                err_fn,
                None,
            )
            .map_err(|e| format!("Failed to build VAD I16 stream: {}", e))?,
        SampleFormat::U16 => device
            .build_input_stream(
                config,
                move |data: &[u16], _: &_| { process_vad_samples!(data, u16); },
                err_fn,
                None,
            )
            .map_err(|e| format!("Failed to build VAD U16 stream: {}", e))?,
        _ => return Err(format!("Unsupported sample format for VAD: {:?}", sample_format)),
    };

    Ok(stream)
}

/// Helper to create WAV writer
fn create_wav_writer(
    path: &PathBuf,
    sample_rate: u32,
    channels: u16,
) -> Result<hound::WavWriter<std::io::BufWriter<std::fs::File>>> {
    use hound::{WavSpec, WavWriter};
    use std::fs;
    use std::io::BufWriter;

    let spec = WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    let file = fs::File::create(path)
        .map_err(|e| format!("Failed to create WAV file: {}", e))?;
    let writer = WavWriter::new(BufWriter::new(file), spec)
        .map_err(|e| format!("Failed to create WAV writer: {}", e))?;
    Ok(writer)
}

impl Drop for RecorderState {
    fn drop(&mut self) {
        let _ = self.close_session();
    }
}
