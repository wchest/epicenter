use crate::recorder::wav_writer::WavWriter;
use crate::recorder::vad::VadProcessorHandle;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use tracing::{debug, error, info};
use tauri::{Manager, Emitter};

/// Simple result type using String for errors
pub type Result<T> = std::result::Result<T, String>;

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

/// VAD state for voice activity detection
#[derive(Debug, Clone, Copy, Serialize)]
pub enum VadState {
    Idle,
    Listening,
    SpeechDetected,
    Processing,
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
    // VAD-specific fields
    vad_processor: Arc<VadProcessorHandle>,
    vad_enabled: Arc<AtomicBool>,
    vad_state: Arc<Mutex<VadState>>,
    vad_output_folder: Option<PathBuf>,
    app_handle: Option<tauri::AppHandle>,
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
            vad_processor: Arc::new(VadProcessorHandle::new()),
            vad_enabled: Arc::new(AtomicBool::new(false)),
            vad_state: Arc::new(Mutex::new(VadState::Idle)),
            vad_output_folder: None,
            app_handle: None,
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

    /// Start VAD recording session
    pub fn start_vad_recording(
        &mut self,
        device_name: String,
        output_folder: PathBuf,
        aggressiveness: u8,
        audio_threshold: Option<f32>,
        silence_timeout_ms: Option<u32>,
        app_handle: tauri::AppHandle,
    ) -> Result<()> {
        // Clean up any existing session
        self.stop_vad_recording()?;

        // Store app handle and output folder for VAD
        self.app_handle = Some(app_handle.clone());
        self.vad_output_folder = Some(output_folder.clone());

        // Find the device
        let host = cpal::default_host();
        let device = find_device(&host, &device_name)?;

        // Get optimal config for voice
        let config = get_optimal_config(&device, Some(16000))?; // 16kHz is optimal for VAD
        let sample_format = config.sample_format();
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        // Initialize VAD processor with custom settings
        self.vad_processor.init_with_settings(
            sample_rate,
            channels,
            aggressiveness,
            audio_threshold.unwrap_or(200.0),
            silence_timeout_ms.unwrap_or(800)
        )?;

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
        let vad_processor = self.vad_processor.clone();
        let vad_enabled = self.vad_enabled.clone();
        let vad_state_clone = self.vad_state.clone();
        let output_folder_clone = output_folder;
        let app_handle_clone = app_handle;

        // Store sample rate and channels
        self.sample_rate = sample_rate;
        self.channels = channels;

        // Create the worker thread that owns the stream
        let worker = thread::spawn(move || {
            // Build the stream IN this thread
            let stream = match build_vad_input_stream(
                &device,
                &stream_config,
                sample_format,
                vad_processor,
                vad_enabled.clone(),
                vad_state_clone,
                output_folder_clone,
                app_handle_clone,
                sample_rate,
                channels,
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
            vad_enabled.store(true, Ordering::Relaxed);

            // Keep thread alive by waiting for commands
            loop {
                match cmd_rx.recv() {
                    Ok(RecorderCmd::Stop(reply_tx)) => {
                        vad_enabled.store(false, Ordering::Relaxed);
                        info!("VAD recording stopped");
                        let _ = reply_tx.send(());
                        break;
                    }
                    Ok(RecorderCmd::Shutdown) | Err(_) => {
                        info!("Shutting down VAD worker");
                        vad_enabled.store(false, Ordering::Relaxed);
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Store everything
        self.cmd_tx = Some(cmd_tx);
        self.worker_handle = Some(worker);
        self.vad_enabled.store(true, Ordering::Relaxed);
        *self.vad_state.lock().unwrap() = VadState::Listening;

        info!("VAD recording started: {} Hz, {} channels", sample_rate, channels);
        Ok(())
    }

    /// Stop VAD recording
    pub fn stop_vad_recording(&mut self) -> Result<()> {
        // Send stop command to worker thread
        if let Some(tx) = &self.cmd_tx {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = tx.send(RecorderCmd::Stop(reply_tx));
            let _ = reply_rx.recv();
        }

        // Wait for worker thread to finish
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        // Reset VAD state
        self.vad_enabled.store(false, Ordering::Relaxed);
        *self.vad_state.lock().unwrap() = VadState::Idle;
        self.vad_processor.reset()?;
        self.vad_output_folder = None;
        self.app_handle = None;
        self.cmd_tx = None;

        debug!("VAD recording stopped");
        Ok(())
    }

    /// Get current VAD state
    pub fn get_vad_state(&self) -> VadState {
        *self.vad_state.lock().unwrap()
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
        .collect();

    if configs.is_empty() {
        return Err("No supported input configurations".to_string());
    }

    // Filter for supported sample formats only
    let supported_formats = [SampleFormat::F32, SampleFormat::I16, SampleFormat::U16];
    let compatible_configs: Vec<_> = configs
        .iter()
        .filter(|config| supported_formats.contains(&config.sample_format()))
        .collect();

    if compatible_configs.is_empty() {
        return Err("No configurations with supported sample formats (F32, I16, U16)".to_string());
    }

    // Try to find mono config with target sample rate and supported format
    for config in &compatible_configs {
        if config.channels() == 1 {
            let min_rate = config.min_sample_rate().0;
            let max_rate = config.max_sample_rate().0;
            if min_rate <= target_sample_rate && max_rate >= target_sample_rate {
                return Ok(config.with_sample_rate(cpal::SampleRate(target_sample_rate)));
            }
        }
    }

    // Try stereo with target sample rate if mono not available
    for config in &compatible_configs {
        let min_rate = config.min_sample_rate().0;
        let max_rate = config.max_sample_rate().0;
        if min_rate <= target_sample_rate && max_rate >= target_sample_rate {
            return Ok(config.with_sample_rate(cpal::SampleRate(target_sample_rate)));
        }
    }

    // If target rate not supported, try to find closest rate
    let mut best_config = None;
    let mut best_diff = u32::MAX;

    for config in &compatible_configs {
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

    // If still no best config, take any compatible config
    if best_config.is_none() && !compatible_configs.is_empty() {
        let config = compatible_configs[0];
        let min_rate = config.min_sample_rate().0;
        let max_rate = config.max_sample_rate().0;
        let rate = if min_rate <= target_sample_rate && max_rate >= target_sample_rate {
            target_sample_rate
        } else {
            min_rate // Use minimum rate as fallback
        };
        best_config = Some(config.with_sample_rate(cpal::SampleRate(rate)));
    }

    best_config.ok_or_else(|| "Failed to find suitable audio configuration".to_string())
}

/// Build input stream for any supported sample format
fn build_input_stream(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    is_recording: Arc<AtomicBool>,
    writer: Arc<Mutex<WavWriter>>,
) -> Result<Stream> {
    let err_fn = |err| error!("Audio stream error: {}", err);

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

/// Build input stream for VAD processing
fn build_vad_input_stream(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    vad_processor: Arc<VadProcessorHandle>,
    vad_enabled: Arc<AtomicBool>,
    vad_state: Arc<Mutex<VadState>>,
    output_folder: PathBuf,
    app_handle: tauri::AppHandle,
    sample_rate: u32,
    channels: u16,
) -> Result<Stream> {
    let err_fn = |err| error!("VAD audio stream error: {}", err);

    // Create a unique ID generator
    let recording_counter = Arc::new(Mutex::new(0u32));

    let stream = match sample_format {
        SampleFormat::F32 => {
            let vad_processor = vad_processor.clone();
            let vad_enabled = vad_enabled.clone();
            let vad_state = vad_state.clone();
            let output_folder = output_folder.clone();
            let app_handle = app_handle.clone();
            let recording_counter = recording_counter.clone();

            device
                .build_input_stream(
                    config,
                    move |data: &[f32], _: &_| {
                        let vad_is_enabled = vad_enabled.load(Ordering::Relaxed);
                        println!("🔧 RECORDER: Callback triggered, vad_enabled={}, samples={}", vad_is_enabled, data.len());
                        if vad_is_enabled {
                            // Process audio through VAD
                            println!("🎤 RECORDER: Processing {} samples through VAD", data.len());
                            match vad_processor.process_audio(data) {
                                Ok(Some(speech_audio)) => {
                                        println!("🎯 RECORDER: Received speech audio with {} samples!", speech_audio.len());
                                    // Speech segment complete - save to file
                                    *vad_state.lock().unwrap() = VadState::Processing;

                                    let duration_seconds = speech_audio.len() as f32 / (sample_rate * channels as u32) as f32;
                                    let audio_level = if !speech_audio.is_empty() {
                                        speech_audio.iter().map(|&s| s.abs()).sum::<f32>() / speech_audio.len() as f32
                                    } else {
                                        0.0
                                    };

                                    println!("💾 SAVING SPEECH SEGMENT: {} samples, {:.2}s duration, avg_level: {:.6}",
                                             speech_audio.len(), duration_seconds, audio_level);

                                    // Generate unique filename
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_millis();
                                    let mut counter = recording_counter.lock().unwrap();
                                    *counter += 1;
                                    let filename = format!("vad_{}_{}.wav", timestamp, counter);
                                    let file_path = output_folder.join(&filename);

                                    println!("📁 Writing to file: {:?}", file_path);

                                    // Write WAV file
                                    if let Ok(mut writer) = WavWriter::new(file_path.clone(), sample_rate, channels) {
                                        if writer.write_samples_f32(&speech_audio).is_ok() {
                                            if writer.finalize().is_ok() {
                                                println!("✅ VAD recording saved successfully: {:?}", file_path);
                                                info!("VAD recording saved: {:?}", file_path);

                                                // Emit event to frontend
                                                let _ = app_handle.emit("vad-speech-detected", AudioRecording {
                                                    audio_data: Vec::new(),
                                                    sample_rate,
                                                    channels,
                                                    duration_seconds,
                                                    file_path: Some(file_path.to_string_lossy().to_string()),
                                                });
                                            } else {
                                                println!("❌ Failed to finalize WAV file: {:?}", file_path);
                                            }
                                        } else {
                                            println!("❌ Failed to write audio samples to WAV file: {:?}", file_path);
                                        }
                                    } else {
                                        println!("❌ Failed to create WAV writer for: {:?}", file_path);
                                    }

                                    *vad_state.lock().unwrap() = VadState::Listening;
                                },
                                Ok(None) => {
                                    // No speech segment yet - this is normal
                                },
                                Err(e) => {
                                    println!("❌ RECORDER: VAD processing error: {}", e);
                                }
                            }
                        }
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| format!("Failed to build F32 VAD stream: {}", e))?
        }
        SampleFormat::I16 => {
            let vad_processor = vad_processor.clone();
            let vad_enabled = vad_enabled.clone();
            let vad_state = vad_state.clone();
            let output_folder = output_folder.clone();
            let app_handle = app_handle.clone();
            let recording_counter = recording_counter.clone();

            device
                .build_input_stream(
                    config,
                    move |data: &[i16], _: &_| {
                        let vad_is_enabled = vad_enabled.load(Ordering::Relaxed);
                        if vad_is_enabled {
                            // Convert i16 to f32
                            let f32_data: Vec<f32> = data.iter().map(|&s| s as f32 / 32767.0).collect();

                            // Process audio through VAD
                            match vad_processor.process_audio(&f32_data) {
                                Ok(Some(speech_audio)) => {
                                    println!("🎯 RECORDER I16: Received speech audio with {} samples!", speech_audio.len());
                                    *vad_state.lock().unwrap() = VadState::Processing;

                                    let duration_seconds = speech_audio.len() as f32 / (sample_rate * channels as u32) as f32;
                                    let audio_level = if !speech_audio.is_empty() {
                                        speech_audio.iter().map(|&s| s.abs()).sum::<f32>() / speech_audio.len() as f32
                                    } else {
                                        0.0
                                    };

                                    println!("💾 SAVING SPEECH SEGMENT I16: {} samples, {:.2}s duration, avg_level: {:.6}",
                                             speech_audio.len(), duration_seconds, audio_level);

                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_millis();
                                    let mut counter = recording_counter.lock().unwrap();
                                    *counter += 1;
                                    let filename = format!("vad_{}_{}.wav", timestamp, counter);
                                    let file_path = output_folder.join(&filename);

                                    println!("📁 Writing to file: {:?}", file_path);

                                    if let Ok(mut writer) = WavWriter::new(file_path.clone(), sample_rate, channels) {
                                        if writer.write_samples_f32(&speech_audio).is_ok() {
                                            if writer.finalize().is_ok() {
                                                println!("✅ VAD recording saved successfully: {:?}", file_path);
                                                info!("VAD recording saved: {:?}", file_path);

                                                let _ = app_handle.emit("vad-speech-detected", AudioRecording {
                                                    audio_data: Vec::new(),
                                                    sample_rate,
                                                    channels,
                                                    duration_seconds,
                                                    file_path: Some(file_path.to_string_lossy().to_string()),
                                                });
                                            } else {
                                                println!("❌ Failed to finalize WAV file: {:?}", file_path);
                                            }
                                        } else {
                                            println!("❌ Failed to write audio samples to WAV file: {:?}", file_path);
                                        }
                                    } else {
                                        println!("❌ Failed to create WAV writer for: {:?}", file_path);
                                    }

                                    *vad_state.lock().unwrap() = VadState::Listening;
                                },
                                Ok(None) => {
                                    // No speech segment yet - this is normal
                                },
                                Err(e) => {
                                    println!("❌ RECORDER I16: VAD processing error: {}", e);
                                }
                            }
                        }
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| format!("Failed to build I16 VAD stream: {}", e))?
        }
        SampleFormat::U16 => {
            let vad_processor = vad_processor.clone();
            let vad_enabled = vad_enabled.clone();
            let vad_state = vad_state.clone();
            let output_folder = output_folder.clone();
            let app_handle = app_handle.clone();
            let recording_counter = recording_counter.clone();

            device
                .build_input_stream(
                    config,
                    move |data: &[u16], _: &_| {
                        if vad_enabled.load(Ordering::Relaxed) {
                            // Convert u16 to f32
                            let f32_data: Vec<f32> = data.iter().map(|&s| (s as f32 - 32768.0) / 32767.0).collect();

                            // Process audio through VAD
                            if let Ok(Some(speech_audio)) = vad_processor.process_audio(&f32_data) {
                                *vad_state.lock().unwrap() = VadState::Processing;

                                let timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_millis();
                                let mut counter = recording_counter.lock().unwrap();
                                *counter += 1;
                                let filename = format!("vad_{}_{}.wav", timestamp, counter);
                                let file_path = output_folder.join(&filename);

                                if let Ok(mut writer) = WavWriter::new(file_path.clone(), sample_rate, channels) {
                                    if writer.write_samples_f32(&speech_audio).is_ok() {
                                        if writer.finalize().is_ok() {
                                            info!("VAD recording saved: {:?}", file_path);

                                            let _ = app_handle.emit("vad-speech-detected", AudioRecording {
                                                audio_data: Vec::new(),
                                                sample_rate,
                                                channels,
                                                duration_seconds: speech_audio.len() as f32 / (sample_rate * channels as u32) as f32,
                                                file_path: Some(file_path.to_string_lossy().to_string()),
                                            });
                                        }
                                    }
                                }

                                *vad_state.lock().unwrap() = VadState::Listening;
                            }
                        }
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| format!("Failed to build U16 VAD stream: {}", e))?
        }
        _ => return Err(format!("Unsupported sample format for VAD: {:?}", sample_format)),
    };

    Ok(stream)
}

impl Drop for RecorderState {
    fn drop(&mut self) {
        let _ = self.close_session();
        let _ = self.stop_vad_recording();
    }
}
