import type { VadState } from '$lib/constants/audio';
import { createTaggedError, extractErrorMessage } from 'wellcrafted/error';
import { Err, Ok, tryAsync } from 'wellcrafted/result';
import type { DeviceIdentifier } from './types';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getDefaultRecordingsFolder } from './recorder/utils';
import { settings } from '$lib/stores/settings.svelte';

const { NativeVadServiceError, NativeVadServiceErr } = createTaggedError(
	'NativeVadServiceError',
);
export type NativeVadServiceError = ReturnType<
	typeof NativeVadServiceError
>;

export interface VadRecording {
	audioData: number[];
	sampleRate: number;
	channels: number;
	durationSeconds: number;
	filePath?: string;
}

export function createNativeVadService() {
	let isListening = false;
	let vadState: VadState = 'IDLE';
	let unlistenFn: (() => void) | null = null;

	return {
		getVadState: (): VadState => {
			return vadState;
		},

		startActiveListening: async ({
			deviceId,
			onSpeechStart,
			onSpeechEnd,
			onVADMisfire,
			onSpeechRealStart,
		}: {
			deviceId: DeviceIdentifier | null;
			onSpeechStart: () => void;
			onSpeechEnd: (blob: Blob) => void;
			onVADMisfire?: () => void;
			onSpeechRealStart?: () => void;
		}) => {
			if (isListening) {
				return NativeVadServiceErr({
					message: 'VAD already active. Stop the current session before starting a new one.',
					context: { vadState },
					cause: undefined,
				});
			}

			vadState = 'LISTENING';

			// Get output folder and VAD settings
			const outputFolder = settings.value['recording.cpal.outputFolder'] ??
				await getDefaultRecordingsFolder();
			const aggressiveness = settings.value['recording.vad.aggressiveness'];
			const audioThreshold = settings.value['recording.vad.audioThreshold'];
			const silenceTimeoutMs = settings.value['recording.vad.silenceTimeoutMs'];

			// Set up event listener for VAD speech detection
			unlistenFn = await listen<VadRecording>('vad-speech-detected', async (event) => {

				// Show SPEECH_DETECTED state when speech event fires
				vadState = 'SPEECH_DETECTED';
				onSpeechStart();

				// Convert file path to blob if we have one
				if (event.payload.filePath) {
					try {
						// Read the file using Tauri command
						const audioBytes = await invoke<number[]>('read_vad_audio_file', {
							filePath: event.payload.filePath
						});

						// Convert bytes to Uint8Array and create blob
						const uint8Array = new Uint8Array(audioBytes);
						const blob = new Blob([uint8Array], { type: 'audio/wav' });

						vadState = 'LISTENING';
						onSpeechEnd(blob);
					} catch (error) {
						console.error('Failed to load VAD recording file:', error);
					}
				}
			});

			// Start VAD recording via Tauri command
			const { error } = await tryAsync({
				try: async () => {
					await invoke('start_vad_recording', {
						deviceIdentifier: deviceId || 'default',
						outputFolder,
						aggressiveness,
						audioThreshold,
						silenceTimeoutMs,
					});
				},
				catch: (error) =>
					NativeVadServiceErr({
						message: `Failed to start native VAD recording. ${extractErrorMessage(error)}`,
						context: { deviceId },
						cause: error,
					}),
			});

			if (error) {
				if (unlistenFn) {
					unlistenFn();
					unlistenFn = null;
				}
				return Err(error);
			}

			isListening = true;
			vadState = 'LISTENING';

			// Return success with device info
			return Ok({
				outcome: 'success',
				deviceId: deviceId || 'default' as DeviceIdentifier,
			});
		},

		stopActiveListening: async () => {
			if (!isListening) return Ok(undefined);

			// Reset state
			vadState = 'IDLE';

			// Clean up event listener
			if (unlistenFn) {
				unlistenFn();
				unlistenFn = null;
			}

			const { error } = await tryAsync({
				try: async () => {
					await invoke('stop_vad_recording');
				},
				catch: (error) =>
					NativeVadServiceErr({
						message: `Failed to stop native VAD recording. ${extractErrorMessage(error)}`,
						context: { vadState },
						cause: error,
					}),
			});

			isListening = false;
			vadState = 'IDLE';

			if (error) return Err(error);
			return Ok(undefined);
		},

		// Get device list from native recorder
		enumerateDevices: async () => {
			const { data, error } = await tryAsync({
				try: async () => {
					const devices = await invoke<string[]>('enumerate_recording_devices');
					// Convert to our device format
					return devices.map(device => ({
						id: device as DeviceIdentifier,
						label: device,
					}));
				},
				catch: (error) =>
					NativeVadServiceErr({
						message: 'Failed to enumerate recording devices',
						cause: error,
					}),
			});

			if (error) return Err(error);
			return Ok(data);
		},
	};
}

export type NativeVadService = ReturnType<typeof createNativeVadService>;

export const NativeVadServiceLive = createNativeVadService();