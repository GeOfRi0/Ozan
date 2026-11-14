// Rust tarafındaki Tauri komutları için tipli sarmalayıcılar.
import { invoke } from "@tauri-apps/api/core";

export type ReplayMode = "restart" | "overlap";

export interface HotkeyBinding {
  code: string;
  label: string;
  passthrough: boolean;
}

export interface CapturedKey {
  code: string;
  label: string;
}

export interface Sound {
  id: string;
  name: string;
  file: string;
  gain: number;
  userGain: number;
  normalized: boolean;
  favorite: boolean;
  duration: number;
  color: string;
  replayMode: ReplayMode;
  hotkey: HotkeyBinding | null;
}

export interface Folder {
  id: string;
  name: string;
  sounds: string[];
}

export interface Settings {
  volume: number;
  localPreview: boolean;
  micDevice: string | null;
  broadcastDevice: string | null;
  monitorDevice: string | null;
  normalizeChoice: boolean | null;
  panicHotkey: HotkeyBinding;
  onboardingDone: boolean;
  pttEnabled: boolean;
  pttHotkey: HotkeyBinding | null;
  muteMicOnPlay: boolean;
}

export interface LibraryData {
  version: number;
  sounds: Sound[];
  folders: Folder[];
  settings: Settings;
}

export interface DeviceList {
  inputs: string[];
  outputs: string[];
  defaultInput: string | null;
  defaultOutput: string | null;
}

export interface VoiceProgress {
  id: string;
  position: number;
  duration: number;
  progress: number;
}

export interface AudioStatus {
  mic: "off" | "active" | "muted" | "waiting" | "error";
  micLevel: number;
  playing: number;
  voices: VoiceProgress[];
  apoSampleRate: number;
  apoChannels: number;
  apoHeartbeat: number;
}

export type InjectionMode = "apoActive" | "fallbackDirectOutput";

export interface ApoHealthReport {
  mode: InjectionMode;
  modeLabel: string;
  dllPath: string;
  dllExists: boolean;
  dllLoadable: boolean;
  dllSha256: string;
  expectedSha256: string;
  sha256Valid: boolean;
  comRegistered: boolean;
  fxChainIntact: boolean;
  activeEndpointId: string | null;
  activeEndpointName: string | null;
  originalChainedClsids: string[];
  shmConnected: boolean;
  shmNamespace: string;
  apoSampleRate: number;
  apoChannels: number;
  apoHeartbeat: number;
  needsRepair: boolean;
  statusMessage: string;
  fallbackReason: string | null;
}

export interface WaveformData {
  peaks: number[];
  duration: number;
}

export interface ImportResult {
  added: Sound[];
  errors: string[];
  skipped: number;
}

export interface ImportProgress {
  done: number;
  total: number;
  name: string;
}

export type DeviceKind = "mic" | "monitor" | "broadcast";

export const DEFAULT_DEVICE = "__default__";
export const SUPPORTED_EXTS = ["opus", "mp3", "wav", "ogg", "flac"];

export const COLOR_PALETTE = [
  { label: "Varsayılan", value: "" },
  { label: "Mor", value: "#7c6cff" },
  { label: "Mavi", value: "#3b82f6" },
  { label: "Camgöbeği", value: "#06b6d4" },
  { label: "Yeşil", value: "#22c55e" },
  { label: "Sarı", value: "#eab308" },
  { label: "Turuncu", value: "#f97316" },
  { label: "Kırmızı", value: "#ef4444" },
  { label: "Pembe", value: "#ec4899" },
];

export const api = {
  getLibrary: () => invoke<LibraryData>("get_library"),
  importFiles: (paths: string[], normalize: boolean, remember: boolean) =>
    invoke<ImportResult>("import_files", { paths, normalize, remember }),
  deleteSounds: (ids: string[]) => invoke<LibraryData>("delete_sounds", { ids }),
  renameSound: (id: string, name: string) => invoke<void>("rename_sound", { id, name }),
  setFavorite: (id: string, favorite: boolean) => invoke<void>("set_favorite", { id, favorite }),
  setSoundGain: (id: string, userGain: number, persist: boolean) =>
    invoke<void>("set_sound_gain", { id, userGain, persist }),
  setSoundColor: (id: string, color: string) => invoke<void>("set_sound_color", { id, color }),
  setSoundReplayMode: (id: string, replayMode: ReplayMode) =>
    invoke<void>("set_sound_replay_mode", { id, replayMode }),
  setSoundHotkey: (id: string, hotkey: HotkeyBinding | null) =>
    invoke<void>("set_sound_hotkey", { id, hotkey }),
  startHotkeyCapture: () => invoke<void>("start_hotkey_capture"),
  cancelHotkeyCapture: () => invoke<void>("cancel_hotkey_capture"),
  setPanicHotkey: (hotkey: HotkeyBinding) => invoke<void>("set_panic_hotkey", { hotkey }),
  setPttConfig: (enabled: boolean, hotkey: HotkeyBinding | null) =>
    invoke<void>("set_ptt_config", { enabled, hotkey }),
  setMuteMicOnPlay: (enabled: boolean) => invoke<void>("set_mute_mic_on_play", { enabled }),
  completeOnboarding: (pttEnabled: boolean, pttHotkey: HotkeyBinding | null, muteMicOnPlay: boolean) =>
    invoke<void>("complete_onboarding", { pttEnabled, pttHotkey, muteMicOnPlay }),
  reorderSounds: (folderId: string | null, ids: string[]) =>
    invoke<void>("reorder_sounds", { folderId, ids }),
  createFolder: (name: string) => invoke<Folder>("create_folder", { name }),
  renameFolder: (id: string, name: string) => invoke<void>("rename_folder", { id, name }),
  deleteFolder: (id: string) => invoke<void>("delete_folder", { id }),
  addToFolder: (folderId: string, ids: string[]) => invoke<void>("add_to_folder", { folderId, ids }),
  removeFromFolder: (folderId: string, ids: string[]) =>
    invoke<void>("remove_from_folder", { folderId, ids }),
  playSound: (id: string) => invoke<void>("play_sound", { id }),
  playSoundRange: (id: string, startSec: number, endSec: number) =>
    invoke<void>("play_sound_range", { id, startSec, endSec }),
  getWaveform: (id: string) => invoke<WaveformData>("get_waveform", { id }),
  saveTrimmedSound: (id: string, startSec: number, endSec: number) =>
    invoke<Sound>("save_trimmed_sound", { id, startSec, endSec }),
  exportProfileBackup: () => invoke<string | null>("export_profile_backup"),
  importProfileBackup: () => invoke<LibraryData | null>("import_profile_backup"),
  stopAll: () => invoke<void>("stop_all"),
  setVolume: (volume: number, persist: boolean) => invoke<void>("set_volume", { volume, persist }),
  setLocalPreview: (enabled: boolean) => invoke<void>("set_local_preview", { enabled }),
  setNormalizeChoice: (choice: boolean | null) => invoke<void>("set_normalize_choice", { choice }),
  listDevices: () => invoke<DeviceList>("list_devices"),
  setDevice: (kind: DeviceKind, name: string | null) => invoke<void>("set_device", { kind, name }),
  audioStatus: () => invoke<AudioStatus>("audio_status"),
  getApoStatus: () => invoke<ApoHealthReport>("get_apo_status"),
  repairApo: () => invoke<ApoHealthReport>("repair_apo"),
  openSoundsFolder: () => invoke<void>("open_sounds_folder"),
};

export function errMsg(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}
