// The server's wire format: the engine's session and commands as serde
// writes them (crates/LiveStageEngine/src/session.rs, engine.rs), plus the
// messages the server pushes (apps/native/livestage/src/server.rs).

export type Id = number

export type StripRef =
  | { kind: 'channel'; id: Id }
  | { kind: 'bus'; id: Id }
  /** Phase 3: a matrix strip (fader, mute, solo, processing, inserts…). */
  | { kind: 'matrix'; id: Id }
  | { kind: 'master' }
export type StripOutput = { kind: 'master' } | { kind: 'bus'; id: Id } | { kind: 'none' }
export type PatchSource =
  | { kind: 'master' }
  | { kind: 'bus'; id: Id }
  | { kind: 'channel'; id: Id }
  /** The solo/monitor bus (PFL/AFL, or the master while nothing is soloed). */
  | { kind: 'monitor' }
  /** A matrix's output (Phase 3). */
  | { kind: 'matrix'; id: Id }

export interface InputPatch {
  left: number | null
  right: number | null
}

export type InsertPlugin =
  | { type: 'builtin'; stem: string; params: [number, number][] }
  | {
      type: 'external'
      format: string
      path: string
      class_id: string
      name: string
      state?: [string, string] | null
    }

export interface InsertSlot {
  id: Id
  bypass: boolean
  plugin: InsertPlugin
}

// ── The strip's processing section (crates/LiveStageEngine/src/processing.rs) ──

export interface Hpf {
  on: boolean
  /** 20 … 600 Hz. */
  hz: number
  /** 12, 18 or 24 dB/oct. */
  slope_db: number
}

export interface Gate {
  on: boolean
  /** −80 … 0 dB. */
  threshold_db: number
  /** Attenuation when closed, −80 (a full mute) … 0 dB. */
  range_db: number
  /** 0.05 … 100 ms. */
  attack_ms: number
  /** 0 … 2000 ms. */
  hold_ms: number
  /** 5 … 4000 ms. */
  release_ms: number
}

export type EqKind = 'low_shelf' | 'bell' | 'high_shelf'

export interface EqBand {
  kind: EqKind
  /** 20 … 20 000 Hz. */
  hz: number
  /** −18 … +18 dB. */
  gain_db: number
  /** 0.1 … 10. */
  q: number
}

export interface Eq {
  on: boolean
  /** Low, low-mid, high-mid, high. */
  bands: EqBand[]
}

export interface Comp {
  on: boolean
  /** −60 … 0 dB. */
  threshold_db: number
  /** 1 … 20 (20 is a limiter). */
  ratio: number
  /** 0.1 … 200 ms. */
  attack_ms: number
  /** 10 … 2000 ms. */
  release_ms: number
  /** 0 … 24 dB. */
  knee_db: number
  /** 0 … 24 dB. */
  makeup_db: number
}

export interface Delay {
  on: boolean
  /** 0 … 1000 ms. */
  ms: number
}

export type ProcessingOrder = 'eq_then_comp' | 'comp_then_eq'

export interface Processing {
  hpf: Hpf
  gate: Gate
  eq: Eq
  comp: Comp
  delay: Delay
  order: ProcessingOrder
}

/** What every channel, bus and master carries. The console-core fields are
 *  filled with the engine's defaults by the store when an older server's
 *  session lacks them (processing.ts `normalizeSession`). */
export interface StripCore {
  fader_db: number
  pan: number
  mute: boolean
  solo: boolean
  inserts: InsertSlot[]
  processing: Processing
  /** Index into Studio's 12 track colours; null: the default. */
  color: number | null
  /** DCA indices this strip follows (channels and buses only). */
  dcas: number[]
  /** Mute-group indices (channels and buses only). */
  mute_groups: number[]
  /** Never silenced by solo-in-place. */
  solo_safe: boolean
  /** A scene recall leaves this strip entirely alone. False from a server
   *  that predates scenes. */
  recall_safe: boolean
}

export interface SendSlot {
  bus: Id
  level_db: number
  pre_fader: boolean
  /** −1…1, effective on a stereo bus (Phase 3; 0 from an older server). */
  pan: number
  /** Use the channel's own pan instead of `pan` (true from an older server). */
  pan_follow: boolean
}

export interface ChannelStrip extends StripCore {
  id: Id
  name: string
  input: InputPatch
  trim_db: number
  phase_invert: boolean
  sends: SendSlot[]
  output: StripOutput
  record_arm: boolean
}

/** What a bus is for (Phase 3): a monitor mix, a subgroup, or an effect
 *  send with its return. */
export type BusRole = 'aux' | 'group' | 'fx'

export interface BusStrip extends StripCore {
  id: Id
  name: string
  output: StripOutput
  record_arm: boolean
  /** Filled by the store for an older server: `group` when it feeds the
   *  master, else `aux` (the engine's own rule for old shows). */
  role: BusRole
  /** False: a mono bus (its sum on both sides; sends have no pan). */
  stereo: boolean
}

/** What feeds a matrix: the master or a bus, post-fader. */
export type MatrixSource = { kind: 'master' } | { kind: 'bus'; id: Id }

export interface MatrixSend {
  source: MatrixSource
  level_db: number
  /** −1…1, into a stereo matrix. */
  pan: number
}

/** A matrix (Phase 3): sums the master and buses, each at its own level,
 *  then processing → inserts → fader → its output patch. */
export interface MatrixStrip extends StripCore {
  id: Id
  name: string
  stereo: boolean
  sources: MatrixSend[]
  record_arm: boolean
}

/** Where talkback and the oscillator are added in. */
export type Destination =
  | { kind: 'bus'; id: Id }
  | { kind: 'matrix'; id: Id }
  | { kind: 'master' }
  | { kind: 'monitor' }

export interface Talkback {
  /** The interface input the talkback mic is on. */
  input: number | null
  level_db: number
  /** A 100 Hz high-pass. */
  hpf: boolean
  to: Destination[]
}

export type OscillatorKind = 'sine' | 'pink' | 'white'

export interface Oscillator {
  kind: OscillatorKind
  /** Sine only. */
  hz: number
  /** −90 (off) … 0 dBFS. */
  level_db: number
  to: Destination[]
}

/** A custom fader bank. */
export interface Layer {
  name: string
  strips: StripRef[]
}

export const LAYER_COUNT = 8

export interface MasterStrip extends StripCore {
  record_arm: boolean
}

export interface OutputPatch {
  source: PatchSource
  left: number
  right: number | null
}

export type RecordFormat = 'wav' | 'flac'
export type RecordTap = 'input' | 'post_inserts'

export interface RecordSettings {
  folder: string | null
  format: RecordFormat
  bit_depth: number
  tap: RecordTap
}

export interface AudioSettings {
  host: string | null
  input_device: string | null
  output_device: string | null
  sample_rate: number
  buffer_frames: number
}

export interface Dca {
  name: string
  level_db: number
  mute: boolean
  color: number | null
}

export interface MuteGroup {
  name: string
  active: boolean
}

export type SoloMode = 'pfl' | 'afl' | 'sip'

/** What the monitor plays while nothing is soloed. */
export type MonitorSource = { kind: 'master' } | { kind: 'bus'; id: Id } | { kind: 'matrix'; id: Id }

export interface Monitor {
  solo_mode: SoloMode
  level_db: number
  dim: boolean
  /** Phase 3; the master from an older server. */
  source: MonitorSource
}

export const DCA_COUNT = 8
export const MUTE_GROUP_COUNT = 8
/** Studio's track colours (`--fb-track-color-1` … `-12`). */
export const STRIP_COLORS = 12

// ── Scenes (Phase 2: the console's scene memory) ──

/** What a scene's recall touches ("focus"); every part defaults to true. */
export interface RecallScope {
  /** Trim, polarity, input patch. */
  input: boolean
  /** The processing section. */
  processing: boolean
  /** Insert parameters and bypass. */
  inserts: boolean
  /** Strip faders and DCA levels. */
  faders: boolean
  /** Strip mutes, DCA mutes, mute-group active. */
  mutes: boolean
  pan: boolean
  /** Send levels and pre/post. */
  sends: boolean
  /** Strip outputs and the output patch. */
  routing: boolean
  /** DCA and mute-group membership. */
  assign: boolean
  /** Strip, DCA and mute-group names and colours. */
  names: boolean
}

/** A scene as the session message carries it: no mix body. */
export interface SceneSummary {
  id: Id
  name: string
  note: string
  scope: RecallScope
}

/** What a strip copy carries; every part optional. */
export interface StripSettings {
  processing?: Processing
  inserts?: { bypass: boolean; plugin: InsertPlugin }[]
  sends?: SendSlot[]
  fader_db?: number
  pan?: number
  trim_db?: number
  phase_invert?: boolean
  name?: string
  color?: number | null
}

export type Section =
  | 'processing'
  | 'hpf'
  | 'gate'
  | 'eq'
  | 'comp'
  | 'delay'
  | 'inserts'
  | 'sends'
  | 'fader_pan'
  | 'input'
  | 'name_color'

export interface LibraryItem {
  id: string
  name: string
  category: string
  sections: Section[]
  settings: StripSettings
  /** Ships with LiveStage: read-only. */
  factory: boolean
}

export interface History {
  /** The step Undo would take back ("Fader Kick"), or null. */
  undo: string | null
  redo: string | null
  undo_depth: number
  redo_depth: number
}

export interface Session {
  name: string
  audio: AudioSettings
  channels: ChannelStrip[]
  buses: BusStrip[]
  master: MasterStrip
  outputs: OutputPatch[]
  recording: RecordSettings
  next_id: Id
  /** Always 8 (padded by the store for an older server). */
  dcas: Dca[]
  /** Always 8 (padded by the store for an older server). */
  mute_groups: MuteGroup[]
  monitor: Monitor
  /** In list order; empty from a server that predates scenes. */
  scenes: SceneSummary[]
  /** The last recalled or stored scene. */
  current_scene: Id | null
  // Phase 3 ("Bus & Monitor"): filled with the engine's defaults by the
  // store for an older server.
  matrices: MatrixStrip[]
  talkback: Talkback
  oscillator: Oscillator
  /** Up to 8. */
  layers: Layer[]
  /** Phase 4: the loaded take for virtual soundcheck (filled by the store
   *  for an older server: nothing loaded). */
  playback: PlaybackSetup
}

// ── Phase 4: users, roles and lock (apps/native/livestage/src/users.rs) ──

export type Role = 'admin' | 'engineer' | 'musician' | 'viewer'

/** A mix a musician may change: an aux bus or a matrix. */
export type MixRef = { kind: 'bus'; id: Id } | { kind: 'matrix'; id: Id }

/** A user as the server lists them (never with the PIN). */
export interface UserSummary {
  name: string
  role: Role
  /** Present in the admin's `users` list; absent from `auth`'s. */
  mixes?: MixRef[]
}

/** The logged-in user. */
export interface AuthUser {
  name: string
  role: Role
  mixes: MixRef[]
}

/** `{"type":"auth",…}`: sent on connect and whenever it changes. */
export interface AuthMessage {
  type: 'auth'
  /** `open`: no users exist, every client is an admin without a login. */
  mode: 'open' | 'users'
  users: UserSummary[]
  user: AuthUser | null
  locked: boolean
}

// ── Phase 4: MIDI and OSC remote (apps/native/livestage/src/remote.rs) ──

export type MidiKind = 'cc' | 'note' | 'pc'
export type MapMode = 'absolute' | 'toggle' | 'momentary'

export interface MidiMessageRef {
  /** 1…16. */
  channel: number
  kind: MidiKind
  number: number
}

export interface MidiMap {
  midi: MidiMessageRef
  /** An OSC-style address (`/ch/1/fader`). */
  target: string
  mode: MapMode
}

export interface RemoteSettings {
  osc: { enabled: boolean; port: number; feedback: boolean }
  midi: { inputs: string[]; outputs: string[]; feedback: boolean; maps: MidiMap[] }
}

export interface RemotePortStatus {
  name: string
  open: boolean
  error: string | null
}

/** How the remote is doing (remote.rs `Remote::status`). */
export interface RemoteStatus {
  osc: {
    /** The address the OSC socket is bound to; null: not listening. */
    listening: string | null
    error: string | null
    subscribers: number
    received: number
    /** Packets that were not OSC. */
    unreadable: number
  }
  midi: { inputs: RemotePortStatus[]; outputs: RemotePortStatus[]; error: string | null }
  /** Remote events dropped because the queue was full. */
  dropped: number
  /** Unknown addresses and wrong types. */
  ignored: number
  /** Commands the remote sent that were refused or failed. */
  refused: number
  /** A MIDI learn waiting for a message. */
  learning: { target: string; mode: MapMode } | null
}

// ── Phase 4: playback and virtual soundcheck (crates/LiveStageEngine/src/playback.rs) ──

export interface PlaybackTrack {
  file: string
  channels: number
  /** The channel this file plays into in virtual soundcheck; null: none. */
  channel: Id | null
}

/** Saved with the show; `virtual_soundcheck` is false after loading one. */
export interface PlaybackSetup {
  folder: string | null
  tracks: PlaybackTrack[]
  virtual_soundcheck: boolean
}

export type PlaybackState = 'stopped' | 'playing' | 'paused'

/** The engine status's `playback`. */
export interface PlaybackStatus {
  state: PlaybackState
  position: number
  duration: number
  underruns: number
  error: string | null
  /** Whole-take loop (absent from a server that does not report it). */
  loop?: boolean
}

export interface TakeFile {
  name: string
  channels: number
  rate: number
  seconds: number
}

/** A take folder under the recordings folder in use. */
export interface Take {
  name: string
  path: string
  files: TakeFile[]
}

export interface EngineStatus {
  running: boolean
  sample_rate: number
  in_channels: number
  out_channels: number
  input_device: string | null
  output_device: string | null
  load: number
  input_underruns: number
  error: string | null
  recording_seconds: number | null
  recording_dropped: number
  /** Phase 3: talkback is open now / the oscillator is sounding. Absent
   *  from an older server. */
  talkback_active?: boolean
  oscillator_on?: boolean
  /** Phase 4: the take's transport. Absent from an older server. */
  playback?: PlaybackStatus
}

export type InsertState = 'ready' | 'loading' | { failed: string }

export interface RecordingSummary {
  folder: string
  files: string[]
  seconds: number
  dropped_samples: number
  errors: string[]
}

export interface BuiltinParam {
  index: number
  id: string
  name: string
  min: number
  max: number
  default: number
  unit: string
}

export interface BuiltinPreset {
  name: string
  /** Every wire value, by index. */
  values: number[]
}

/** What an editor is drawn from: the effect's whole wire table (a
 *  parameter's index is its position in `ids`), the DSP's own defaults, and
 *  its factory presets. */
export interface BuiltinSpec {
  ids: string[]
  defaults: number[]
  presets: BuiltinPreset[]
}

export interface BuiltinEffect {
  stem: string
  name: string
  category: string
  params: BuiltinParam[]
  spec: BuiltinSpec | null
}

/** A built-in's own meters (linear levels; reduction in dB, positive). */
export interface LevelFrame {
  in_peak: number
  in_rms: number
  out_peak: number
  out_rms: number
  gain_reduction_db: number
  in_clip: boolean
  out_clip: boolean
  slot_in_peak: number[]
  slot_out_peak: number[]
}

export interface ImageFrame {
  correlation: number
  band_correlation: number[]
  band_level: number[]
  /** Interleaved left/right, oldest first. */
  scope: number[]
}

/** What a watched insert measured since the last frame. */
export interface TelemetryFrame {
  levels?: LevelFrame
  /** 128 log-spaced bins 20 Hz–20 kHz, 0 (−100 dB) … 255 (0 dB): the signal
   *  arriving at the insert. */
  spectrum?: number[]
  band_reduction?: number[]
  image?: ImageFrame
  /** WhiteSharp's pitch readings block. */
  pitch?: number[]
}

export interface InstalledEffect {
  name: string
  vendor: string
  format: string
  path: string
  class_id: string
}

export interface DeviceInfo {
  name: string
  channels: number
  default_sample_rate: number
  default: boolean
}

export interface Devices {
  hosts: string[]
  inputs: DeviceInfo[]
  outputs: DeviceInfo[]
}

export interface StripLevels {
  output: [number, number]
  input: [number, number]
  /** The processing section's gate is open (or off). Absent from an older server. */
  gate_open?: boolean
  /** What the gate takes off now, dB, ≥ 0. */
  gate_db?: number
  /** What the compressor takes off now, dB, ≥ 0. */
  comp_db?: number
}

export interface Hello {
  type: 'hello'
  version: string
  external_plugins: boolean
  session_path: string | null
  effects: BuiltinEffect[]
}

export interface StatusMessage {
  type: 'status'
  status: EngineStatus
  recording: boolean
  /** Inserts that are not simply running, by id. */
  inserts: Record<string, InsertState>
  last_recording: RecordingSummary | null
  /** When the session file was last written (Phase 2; absent from an older
   *  server). */
  last_saved?: { path: string; at: string; auto: boolean } | null
  /** The mix moved since the session file was written. */
  unsaved?: boolean
}

// ── Storage (the LiveStage appliance; apps/native/livestage/src/storage_api.rs) ──

export interface StorageTarget {
  /** `internal` or a volume's UUID: what the appliance records to. */
  id: string
  /** The volume's label, "Internal", or the UUID while it is not plugged in. */
  label: string
  /** False: an external volume that is not plugged in or not mountable. */
  available: boolean
  /** Where recordings go now (the internal folder while falling back). */
  recordings_dir: string
}

export interface StorageVolume {
  /** `internal`, the filesystem's UUID, or "" when it has none. */
  id: string
  /** "" when the filesystem has none. */
  label: string
  fs: string | null
  device: string
  disk: string
  model: string | null
  size_bytes: number
  /** Null when not mounted. */
  free_bytes: number | null
  mounted: 'rw' | 'ro' | null
  mount_path: string | null
  /** Can be a recording target. */
  supported: boolean
  ejected: boolean
}

export interface StorageDisk {
  disk: string
  model: string | null
  size_bytes: number
  removable: boolean
  /** Holds the system: never formatted. */
  system: boolean
}

export interface StorageState {
  target: StorageTarget
  volumes: StorageVolume[]
  disks: StorageDisk[]
}

/** A change under way, as the server asked the storage service. */
export type StorageRequest =
  | { op: 'list' }
  | { op: 'use'; id: string }
  | { op: 'eject'; id: string }
  | { op: 'format'; disk: string; label: string }

export interface StorageMessage {
  type: 'storage'
  /** Whether this machine's storage service answers (the appliance only). */
  available: boolean
  /** Why it does not. */
  reason: string | null
  storage: StorageState | null
  busy: StorageRequest | null
  /** The folder the recorder writes takes into now. */
  folder: string
}

export type ServerMessage =
  | Hello
  | { type: 'session'; session: Session }
  | StatusMessage
  | StorageMessage
  | {
      type: 'meters'
      meters: { strip: StripRef; levels: StripLevels }[]
      /** The monitor bus, peak, linear. Absent from an older server. */
      monitor?: [number, number]
      /** The talkback input after its level, peak, linear (Phase 3). */
      talkback?: number[]
    }
  | { type: 'telemetry'; insert: Id; frame: TelemetryFrame }
  | ({ type: 'history' } & History)
  | { type: 'scene_state'; current: Id | null; modified: boolean }
  | { type: 'library'; items: LibraryItem[] }
  | { type: 'saved'; path: string; at: string; auto: boolean; revision?: number }
  | ({ type: 'reply'; id?: number; ok: boolean; error?: string } & Record<string, unknown>)
  | AuthMessage
  /** The remote settings changed (sent to admins), or a MIDI learn took a
   *  message (`learned`; null with `error` when the map was refused). */
  | {
      type: 'remote'
      learned?: MidiMap | null
      error?: string
      remote?: RemoteSettings
      status?: RemoteStatus
    }

/** Every command the engine takes (engine::Command), tagged by `cmd`. */
export type Command =
  | { cmd: 'set_fader'; strip: StripRef; db: number }
  | { cmd: 'set_pan'; strip: StripRef; pan: number }
  | { cmd: 'set_mute'; strip: StripRef; mute: boolean }
  | { cmd: 'set_solo'; strip: StripRef; solo: boolean }
  | { cmd: 'set_trim'; channel: Id; db: number }
  | { cmd: 'set_phase_invert'; channel: Id; invert: boolean }
  | { cmd: 'add_channel'; name: string; input: InputPatch }
  | { cmd: 'remove_channel'; channel: Id }
  | { cmd: 'rename_strip'; strip: StripRef; name: string }
  | { cmd: 'set_channel_input'; channel: Id; input: InputPatch }
  | { cmd: 'set_strip_output'; strip: StripRef; output: StripOutput }
  /** `role`/`stereo`: Phase 3 (an older server ignores them: a stereo bus). */
  | { cmd: 'add_bus'; name: string; role?: BusRole; stereo?: boolean }
  | { cmd: 'remove_bus'; bus: Id }
  /** `pan`/`pan_follow` omitted: unchanged. */
  | {
      cmd: 'set_send'
      channel: Id
      bus: Id
      level_db: number
      /** Omitted: unchanged (a new send: the bus role's default). A
       *  musician may not send it at all. */
      pre_fader?: boolean
      pan?: number
      pan_follow?: boolean
    }
  | { cmd: 'remove_send'; channel: Id; bus: Id }
  | { cmd: 'add_insert'; strip: StripRef; plugin: InsertPlugin; index: number | null }
  | { cmd: 'remove_insert'; strip: StripRef; insert: Id }
  | { cmd: 'move_insert'; strip: StripRef; insert: Id; to: number }
  | { cmd: 'set_insert_bypass'; insert: Id; bypass: boolean }
  | { cmd: 'set_insert_param'; insert: Id; index: number; value: number }
  | { cmd: 'set_insert_params'; insert: Id; values: [number, number][] }
  | { cmd: 'watch_insert'; insert: Id; watch: boolean }
  | { cmd: 'set_output_patch'; patches: OutputPatch[] }
  | { cmd: 'set_record_arm'; strip: StripRef; arm: boolean }
  | { cmd: 'set_record_settings'; settings: RecordSettings }
  | { cmd: 'set_audio'; settings: AudioSettings }
  | { cmd: 'start_recording' }
  | { cmd: 'stop_recording' }
  // The console core.
  | { cmd: 'set_processing'; strip: StripRef; processing: Processing }
  | { cmd: 'set_strip_color'; strip: StripRef; color: number | null }
  | { cmd: 'set_solo_safe'; strip: StripRef; safe: boolean }
  | { cmd: 'clear_solo' }
  | { cmd: 'assign_dca'; strip: StripRef; dca: number; assigned: boolean }
  | { cmd: 'set_dca_level'; dca: number; db: number }
  | { cmd: 'set_dca_mute'; dca: number; mute: boolean }
  | { cmd: 'rename_dca'; dca: number; name: string }
  | { cmd: 'set_dca_color'; dca: number; color: number | null }
  | { cmd: 'assign_mute_group'; strip: StripRef; group: number; assigned: boolean }
  | { cmd: 'set_mute_group'; group: number; active: boolean }
  | { cmd: 'rename_mute_group'; group: number; name: string }
  | { cmd: 'set_monitor'; monitor: Monitor }
  | { cmd: 'move_channel'; channel: Id; index: number }
  | { cmd: 'move_bus'; bus: Id; index: number }
  // Scenes, undo, paste (Phase 2).
  | { cmd: 'undo' }
  | { cmd: 'redo' }
  /** Without `id` (or `null`): a new scene after the current one. */
  | { cmd: 'scene_store'; id?: Id | null; name?: string }
  | { cmd: 'scene_recall'; id: Id }
  | { cmd: 'scene_rename'; id: Id; name: string }
  | { cmd: 'scene_note'; id: Id; note: string }
  | { cmd: 'scene_scope'; id: Id; scope: RecallScope }
  | { cmd: 'scene_delete'; id: Id }
  | { cmd: 'scene_move'; id: Id; index: number }
  | { cmd: 'set_recall_safe'; strip: StripRef; safe: boolean }
  | { cmd: 'paste_strip'; targets: StripRef[]; settings: StripSettings; sections: Section[] }
  // The library (the server's own file).
  | { cmd: 'library_save'; name: string; category: string; settings: StripSettings; sections: Section[] }
  | { cmd: 'library_apply'; item: string; targets: StripRef[] }
  | { cmd: 'library_delete'; item: string }
  | { cmd: 'library_rename'; item: string; name: string; category: string }
  // Bus & Monitor (Phase 3).
  | { cmd: 'set_bus_role'; bus: Id; role: BusRole }
  | { cmd: 'set_bus_stereo'; bus: Id; stereo: boolean }
  | { cmd: 'add_matrix'; name: string; stereo: boolean }
  | { cmd: 'remove_matrix'; matrix: Id }
  | { cmd: 'set_matrix_send'; matrix: Id; source: MatrixSource; level_db: number; pan: number }
  | { cmd: 'move_matrix'; matrix: Id; index: number }
  | { cmd: 'set_matrix_stereo'; matrix: Id; stereo: boolean }
  | { cmd: 'set_talkback'; talkback: Talkback }
  | { cmd: 'talk'; active: boolean }
  | { cmd: 'set_oscillator'; oscillator: Oscillator }
  | { cmd: 'oscillator_on'; on: boolean }
  | { cmd: 'set_layer'; index: number; name: string; strips: StripRef[] }
  | { cmd: 'remove_layer'; index: number }
  // The server's own.
  | { cmd: 'save' }
  | { cmd: 'devices'; host?: string | null }
  | { cmd: 'installed' }
  // The appliance's disks (`id` is the request's, so the volume is `volume`).
  | { cmd: 'storage'; op: 'use' | 'eject'; volume: string }
  | { cmd: 'storage'; op: 'format'; disk: string; label: string }
  // Users, roles and lock (Phase 4).
  | { cmd: 'auth' }
  | { cmd: 'login'; name: string; pin: string }
  | { cmd: 'resume'; token: string }
  | { cmd: 'logout' }
  | { cmd: 'users' }
  | { cmd: 'user_add'; name: string; role: Role; pin: string; mixes: MixRef[] }
  | {
      cmd: 'user_set'
      name: string
      role?: Role
      pin?: string
      old_pin?: string
      mixes?: MixRef[]
      new_name?: string
    }
  | { cmd: 'user_remove'; name: string }
  | { cmd: 'lock'; pin?: string }
  | { cmd: 'unlock'; pin: string }
  // MIDI and OSC remote (Phase 4).
  | { cmd: 'remote_settings' }
  | { cmd: 'set_remote'; remote: RemoteSettings }
  | { cmd: 'midi_ports' }
  | { cmd: 'midi_learn'; target: string; mode: MapMode }
  | { cmd: 'midi_learn_cancel' }
  // Playback and virtual soundcheck (Phase 4).
  | { cmd: 'takes' }
  | { cmd: 'playback_load'; folder: string }
  | { cmd: 'playback_assign'; file: string; channel: Id | null }
  | { cmd: 'playback_unload' }
  | { cmd: 'playback'; action: 'play' | 'pause' | 'stop' }
  | { cmd: 'playback_locate'; seconds: number }
  | { cmd: 'playback_loop'; on: boolean }
  | { cmd: 'set_virtual_soundcheck'; on: boolean }

/** What changes nothing (contract §1): sent even by a viewer, and while
 *  locked. Everything else "changes something". */
export const READ_ONLY_COMMANDS: ReadonlySet<string> = new Set([
  'status',
  'session',
  'meters',
  'devices',
  'effects',
  'inserts',
  'installed',
  'library',
  'takes',
  'auth',
  'users',
  'login',
  'resume',
  'logout',
  'unlock',
  'remote_settings',
  'midi_ports',
  'watch_insert',
])

export const MIN_FADER_DB = -90
export const MAX_FADER_DB = 10

type Keyed = StripRef | PatchSource | Destination | MatrixSource | MonitorSource

export function stripKey(strip: Keyed): string {
  return strip.kind === 'master' || strip.kind === 'monitor' ? strip.kind : `${strip.kind}:${strip.id}`
}

/** The meters' key for the monitor bus (`stripKey({kind: 'monitor'})`). */
export const MONITOR_KEY = 'monitor'
/** The meters' key for the talkback input. */
export const TALKBACK_KEY = 'talkback'

export function sameStrip(a: Keyed, b: Keyed): boolean {
  return stripKey(a) === stripKey(b)
}
