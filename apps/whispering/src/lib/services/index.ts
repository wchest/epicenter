import { AnalyticsServiceLive } from './analytics';
import { CommandServiceLive } from './command';
import * as completions from './completion';
import { DbServiceLive } from './db';
import { DownloadServiceLive } from './download';
import { FfmpegServiceLive } from './ffmpeg';
import { FsServiceLive } from './fs';
import { GlobalShortcutManagerLive } from './global-shortcut-manager';
import { LocalShortcutManagerLive } from './local-shortcut-manager';
import { NotificationServiceLive } from './notifications';
import { OsServiceLive } from './os';
import { PermissionsServiceLive } from './permissions';
import { CpalRecorderServiceLive } from './recorder/cpal';
import { NavigatorRecorderServiceLive } from './recorder/navigator';
import { FfmpegRecorderServiceLive } from './recorder/ffmpeg';
import { PlaySoundServiceLive } from './sound';
import { TextServiceLive } from './text';
import { ToastServiceLive } from './toast';
import * as transcriptions from './transcription';
import { TrayIconServiceLive } from './tray';
import { VadServiceLive } from './vad-recorder';
import { NativeVadServiceLive } from './native-vad';
import type { VadService } from './vad-recorder';

/**
 * Get the appropriate VAD service based on settings.
 * Returns native VAD if enabled in settings, otherwise returns web VAD.
 */
export function getVadService(): VadService {
	// Import settings dynamically to avoid circular dependencies
	const { settings } = require('$lib/stores/settings.svelte');
	const useNative = settings.value['recording.vad.useNative'];

	return useNative ? NativeVadServiceLive : VadServiceLive;
}

/**
 * Unified services object providing consistent access to all services.
 */
export {
	AnalyticsServiceLive as analytics,
	TextServiceLive as text,
	CommandServiceLive as command,
	completions,
	TrayIconServiceLive as tray,
	DbServiceLive as db,
	DownloadServiceLive as download,
	FfmpegServiceLive as ffmpeg,
	FsServiceLive as fs,
	GlobalShortcutManagerLive as globalShortcutManager,
	LocalShortcutManagerLive as localShortcutManager,
	NotificationServiceLive as notification,
	CpalRecorderServiceLive as cpalRecorder,
	NavigatorRecorderServiceLive as navigatorRecorder,
	FfmpegRecorderServiceLive as ffmpegRecorder,
	PermissionsServiceLive as permissions,
	ToastServiceLive as toast,
	OsServiceLive as os,
	PlaySoundServiceLive as sound,
	transcriptions,
	// Export both VAD services individually for direct access if needed
	VadServiceLive as webVad,
	NativeVadServiceLive as nativeVad,
};
