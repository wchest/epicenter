use std::sync::{Arc, Mutex};
use webrtc_vad::{Vad, SampleRate, VadMode};
use tracing::{debug, info};

/// VAD processing state that can be safely stored across threads
#[derive(Debug, Clone)]
pub struct VadState {
    pub sample_rate: u32,
    pub channels: u16,
    pub aggressiveness: u8,
    pub is_speech_active: bool,
    pub silence_frames: usize,
    pub required_silence_frames: usize,
    pub audio_threshold: f32,
}

impl VadState {
    pub fn new(sample_rate: u32, channels: u16, aggressiveness: u8) -> Self {
        Self::new_with_settings(sample_rate, channels, aggressiveness, 200.0, 800)
    }

    pub fn new_with_settings(sample_rate: u32, channels: u16, aggressiveness: u8, audio_threshold: f32, silence_timeout_ms: u32) -> Self {
        let required_silence_frames = silence_timeout_ms as usize / 30; // Convert ms to frames at 30ms per frame

        Self {
            sample_rate,
            channels,
            aggressiveness,
            is_speech_active: false,
            silence_frames: 0,
            required_silence_frames,
            audio_threshold,
        }
    }
}

/// Voice Activity Detector processor that creates VAD instances as needed
pub struct VadProcessor {
    state: VadState,
    /// Buffer to accumulate audio samples for processing
    processing_buffer: Vec<f32>,
    /// Buffer to accumulate speech audio for final output
    speech_buffer: Vec<f32>,
}

impl VadProcessor {
    /// Create a new VAD processor
    pub fn new(sample_rate: u32, channels: u16, aggressiveness: u8) -> Result<Self, String> {
        Self::new_with_settings(sample_rate, channels, aggressiveness, 200.0, 800)
    }

    /// Create a new VAD processor with custom settings
    pub fn new_with_settings(sample_rate: u32, channels: u16, aggressiveness: u8, audio_threshold: f32, silence_timeout_ms: u32) -> Result<Self, String> {
        info!("Creating VAD processor: {}Hz, {} channels, aggressiveness {}, audio_threshold: {}, silence_timeout: {}ms",
              sample_rate, channels, aggressiveness, audio_threshold, silence_timeout_ms);

        let state = VadState::new_with_settings(sample_rate, channels, aggressiveness, audio_threshold, silence_timeout_ms);

        Ok(Self {
            state,
            processing_buffer: Vec::new(),
            speech_buffer: Vec::new(),
        })
    }

    /// Create a WebRTC VAD instance (not stored, used locally)
    fn create_vad(&self) -> Result<Vad, String> {
        // Convert sample rate to WebRTC VAD format
        let vad_sample_rate = match self.state.sample_rate {
            8000 => SampleRate::Rate8kHz,
            16000 => SampleRate::Rate16kHz,
            32000 => SampleRate::Rate32kHz,
            48000 => SampleRate::Rate48kHz,
            _ => {
                // Default to 16kHz if not exact match
                info!("Sample rate {} not directly supported, using 16kHz for VAD", self.state.sample_rate);
                SampleRate::Rate16kHz
            }
        };

        let mut vad = Vad::new();
        vad.set_sample_rate(vad_sample_rate);

        // Set VAD mode based on aggressiveness (0-3)
        let mode = match self.state.aggressiveness {
            0 => VadMode::Quality,
            1 => VadMode::LowBitrate,
            2 => VadMode::Aggressive,
            3 => VadMode::VeryAggressive,
            _ => VadMode::Aggressive,
        };
        vad.set_mode(mode);

        Ok(vad)
    }

    /// Process audio samples and detect voice activity
    /// Returns true if speech is detected, false otherwise
    pub fn process_audio(&mut self, samples: &[f32]) -> Result<bool, String> {
        // Log incoming audio data with more detail
        let audio_level = samples.iter().map(|&s| s.abs()).sum::<f32>() / samples.len() as f32;
        let max_sample = samples.iter().map(|&s| s.abs()).fold(0.0, f32::max);
        let non_zero_samples = samples.iter().filter(|&&s| s.abs() > 0.0001).count();



        // Always accumulate audio samples for processing
        self.processing_buffer.extend_from_slice(samples);

        // Accumulate speech audio only when speech is active
        if self.state.is_speech_active {
            self.speech_buffer.extend_from_slice(samples);
        }

        // WebRTC VAD expects specific frame sizes based on sample rate
        let frame_size = match self.state.sample_rate {
            8000 => 240,  // 30ms at 8kHz
            16000 => 480, // 30ms at 16kHz
            32000 => 960, // 30ms at 32kHz
            48000 => 1440, // 30ms at 48kHz
            _ => 480, // Default to 16kHz
        };

        let mut speech_detected = false;

        // Process complete frames
        while self.processing_buffer.len() >= frame_size {
            // Extract one frame from processing buffer
            let frame: Vec<f32> = self.processing_buffer.drain(..frame_size).collect();

            // Convert to mono if needed (simple average)
            let mono_frame = if self.state.channels > 1 {
                frame.chunks(self.state.channels as usize)
                    .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
                    .collect::<Vec<f32>>()
            } else {
                frame
            };

            // Convert f32 samples to i16 for WebRTC VAD
            let i16_frame: Vec<i16> = mono_frame.iter()
                .map(|&sample| (sample * 32767.0).clamp(-32768.0, 32767.0) as i16)
                .collect();

            // Log frame details and detect speech
            let frame_level = i16_frame.iter().map(|&s| (s as f32).abs()).sum::<f32>() / i16_frame.len() as f32;
            let max_frame_sample = i16_frame.iter().map(|&s| (s as f32).abs()).fold(0.0, f32::max);

            // Create VAD instance for this frame (not stored)
            let mut vad = self.create_vad()?;

            // Detect speech in this frame, but only if audio level is above noise threshold
            let raw_vad_result = vad.is_voice_segment(&i16_frame)
                .map_err(|e| format!("VAD detection failed: {:?}", e))?;

            // Apply minimum audio level threshold to filter out background noise
            let min_audio_threshold = self.state.audio_threshold; // Use configurable threshold
            let is_speech = raw_vad_result && frame_level > min_audio_threshold;


            if is_speech {
                speech_detected = true;
                self.state.silence_frames = 0;

                if !self.state.is_speech_active {
                    info!("🎙️ SPEECH STARTED - VAD detected voice activity");
                    self.state.is_speech_active = true;
                }
            } else {
                if self.state.is_speech_active {
                    self.state.silence_frames += 1;
                    if self.state.silence_frames >= self.state.required_silence_frames {
                        info!("🔇 SPEECH ENDED - {}ms of silence detected", self.state.silence_frames * 30);
                        self.state.is_speech_active = false;
                        self.state.silence_frames = 0;
                    }
                }
            }
        }

        Ok(speech_detected)
    }

    /// Check if currently in speech
    pub fn is_speech_active(&self) -> bool {
        self.state.is_speech_active
    }

    /// Reset the VAD state
    pub fn reset(&mut self) {
        self.processing_buffer.clear();
        self.speech_buffer.clear();
        self.state.is_speech_active = false;
        self.state.silence_frames = 0;
    }

    /// Get accumulated speech audio and reset
    pub fn get_speech_audio(&mut self) -> Vec<f32> {
        let audio = self.speech_buffer.clone();
        let duration_seconds = audio.len() as f32 / (self.state.sample_rate * self.state.channels as u32) as f32;

        // Minimum duration threshold to avoid transcribing noise/short clips
        const MIN_DURATION_SECONDS: f32 = 1.5; // 1.5 seconds minimum

        if duration_seconds < MIN_DURATION_SECONDS {
            info!("🚫 Discarding speech audio: {:.2}s too short (min: {:.1}s)", duration_seconds, MIN_DURATION_SECONDS);
            self.speech_buffer.clear();
            return Vec::new(); // Return empty vector for short clips
        }

        let audio_level = if !audio.is_empty() {
            audio.iter().map(|&s| s.abs()).sum::<f32>() / audio.len() as f32
        } else {
            0.0
        };

        info!("🎵 Captured speech audio: {:.2}s duration, {} samples", duration_seconds, audio.len());

        self.speech_buffer.clear();
        audio
    }
}

/// Thread-safe handle for VAD processor
pub struct VadProcessorHandle {
    processor: Arc<Mutex<Option<VadProcessor>>>,
}

impl VadProcessorHandle {
    pub fn new() -> Self {
        Self {
            processor: Arc::new(Mutex::new(None)),
        }
    }

    /// Initialize VAD with given parameters
    pub fn init(&self, sample_rate: u32, channels: u16, aggressiveness: u8) -> Result<(), String> {
        self.init_with_settings(sample_rate, channels, aggressiveness, 500.0, 1000)
    }

    /// Initialize VAD with custom sensitivity settings
    pub fn init_with_settings(&self, sample_rate: u32, channels: u16, aggressiveness: u8, audio_threshold: f32, silence_timeout_ms: u32) -> Result<(), String> {
        let mut processor_guard = self.processor.lock()
            .map_err(|e| format!("Failed to lock VAD processor: {}", e))?;

        let processor = VadProcessor::new_with_settings(sample_rate, channels, aggressiveness, audio_threshold, silence_timeout_ms)?;
        *processor_guard = Some(processor);

        info!("🎛️  VAD INITIALIZED: {}Hz, {} channels, aggressiveness {}, audio_threshold: {}, silence_timeout: {}ms",
              sample_rate, channels, aggressiveness, audio_threshold, silence_timeout_ms);
        info!("🎯 VAD will detect speech with aggressiveness level {} (0=quality, 3=very aggressive)", aggressiveness);
        Ok(())
    }

    /// Process audio samples and return speech audio if a segment completed
    pub fn process_audio(&self, samples: &[f32]) -> Result<Option<Vec<f32>>, String> {
        let mut processor_guard = self.processor.lock()
            .map_err(|e| format!("Failed to lock VAD processor: {}", e))?;

        if let Some(ref mut processor) = *processor_guard {
            let was_active = processor.is_speech_active();
            let _speech_detected = processor.process_audio(samples)?;
            let is_active_now = processor.is_speech_active();

            // If speech just ended, return the accumulated audio
            if was_active && !is_active_now {
                let speech_audio = processor.get_speech_audio();
                if !speech_audio.is_empty() {
                    info!("🚀 Speech segment completed: {} samples", speech_audio.len());
                    return Ok(Some(speech_audio));
                }
            }
            Ok(None)
        } else {
            Err("VAD processor not initialized".to_string())
        }
    }

    /// Check if speech is active
    pub fn is_speech_active(&self) -> Result<bool, String> {
        let processor_guard = self.processor.lock()
            .map_err(|e| format!("Failed to lock VAD processor: {}", e))?;

        if let Some(ref processor) = *processor_guard {
            Ok(processor.is_speech_active())
        } else {
            Ok(false)
        }
    }

    /// Reset the VAD state
    pub fn reset(&self) -> Result<(), String> {
        let mut processor_guard = self.processor.lock()
            .map_err(|e| format!("Failed to lock VAD processor: {}", e))?;

        if let Some(ref mut processor) = *processor_guard {
            processor.reset();
        }
        Ok(())
    }

    /// Stop and cleanup VAD
    pub fn stop(&self) -> Result<(), String> {
        let mut processor_guard = self.processor.lock()
            .map_err(|e| format!("Failed to lock VAD processor: {}", e))?;

        *processor_guard = None;
        info!("VAD processor stopped");
        Ok(())
    }
}

// The handle is thread-safe because it only stores data that implements Send/Sync
unsafe impl Send for VadProcessorHandle {}
unsafe impl Sync for VadProcessorHandle {}