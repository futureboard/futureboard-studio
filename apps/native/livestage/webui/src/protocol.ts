// The server's wire format: the engine's session and commands as serde
// writes them (crates/LiveStageEngine/src/session.rs, engine.rs), plus the
// messages the server pushes (apps/native/livestage/src/server.rs).

export type Id = number

export type StripRef = { kind: 'channel'; id: Id } | { kind: 'bus'; id: Id } | { kind: 'master' }
export type StripOutput = { kind: 'master' } | { kind: 'bus'; id: Id } | { kind: 'none' }
export type PatchSource = { kind: 'master' } | { kind: 'bus'; id: Id } | { kind: 'channel'; id: Id }

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

export interface StripCore {
  fader_db: number
  pan: number
  mute: boolean
  solo: boolean
  inserts: InsertSlot[]
}

export interface SendSlot {
  bus: Id
  level_db: number
  pre_fader: boolean
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

export interface BusStrip extends StripCore {
  id: Id
  name: string
  output: StripOutput
  record_arm: boolean
}

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

export interface Session {
  name: string
  audio: AudioSettings
  channels: ChannelStrip[]
  buses: BusStrip[]
  master: MasterStrip
  outputs: OutputPatch[]
  recording: RecordSettings
  next_id: Id
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
}

export type ServerMessage =
  | Hello
  | { type: 'session'; session: Session }
  | StatusMessage
  | { type: 'meters'; meters: { strip: StripRef; levels: StripLevels }[] }
  | { type: 'telemetry'; insert: Id; frame: TelemetryFrame }
  | ({ type: 'reply'; id?: number; ok: boolean; error?: string } & Record<string, unknown>)

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
  | { cmd: 'add_bus'; name: string }
  | { cmd: 'remove_bus'; bus: Id }
  | { cmd: 'set_send'; channel: Id; bus: Id; level_db: number; pre_fader: boolean }
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
  // The server's own.
  | { cmd: 'save' }
  | { cmd: 'devices'; host?: string | null }
  | { cmd: 'installed' }

export const MIN_FADER_DB = -90
export const MAX_FADER_DB = 10

export function stripKey(strip: StripRef | PatchSource): string {
  return strip.kind === 'master' ? 'master' : `${strip.kind}:${strip.id}`
}

export function sameStrip(a: StripRef | PatchSource, b: StripRef | PatchSource): boolean {
  return stripKey(a) === stripKey(b)
}
