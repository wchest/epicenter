pub mod commands;
pub mod recorder;
pub mod wav_writer;
pub mod vad;

// Export everything from commands for easy access
pub use commands::{
    cancel_recording, close_recording_session, enumerate_recording_devices,
    get_current_recording_id, init_recording_session, start_recording, stop_recording, AppData,
};

// Export native VAD commands
pub use commands::{init_vad_recording_session, stop_vad_recording_session};

// Export key types from recorder
pub use recorder::AudioRecording;
