// Maps a native `RodhareistState` blob (serde-serialized Rust `Params`,
// delivered through `selectInstance.state`) into the editor's `RigSnapshot`
// shape, so switching instances shows that insert's real state without
// posting anything back to the DSP (the DSP is the authority — it already
// holds these values).
//
// Serde emits Rust enum *variant names* as strings (`"drive_model":
// "Screamer"`, `"stage_order": ["Gate", null, ...]`) — the tables below are
// the variant→editor-id halves of the cross-language contract; the Rust side
// pins the same pairings in `wire.rs` / model `from_model_id` tests.

import {
  chainOrder,
  cloneParameters,
  models,
  secondInstanceModelId,
  type CategoryId,
  type Param,
} from "./data";
import type { GlobalState, RigSnapshot } from "./Editor";

/** Rust `StageKind` variant → editor category. */
export const STAGE_VARIANT_TO_CATEGORY: Record<string, CategoryId> = {
  Gate: "dyn",
  Drive: "dist",
  Amp: "amp",
  Mod: "mod",
  Delay: "delay",
  Reverb: "verb",
  Cab: "cab",
  Comp: "comp",
  Eq: "eq",
  Wah: "wah",
  Drive2: "dist2",
  Mod2: "mod2",
  Delay2: "delay2",
  Eq2: "eq2",
  Comp2: "comp2",
};

/** Rust `AmpModel` variant → editor model id. */
export const AMP_VARIANT_TO_MODEL: Record<string, string> = {
  Mandarin: "mandarin",
  Plexi: "plexi",
  Twin: "twin",
  TopBoost: "topboost",
  Recto: "recto",
  Jcm: "jcm",
  Slate: "slate",
  Bassman: "bassman",
  Boutique: "boutique",
  Invader: "invader",
  TweedCombo: "tweed_combo",
};

/** Rust `DriveModel` variant → editor model id. */
export const DRIVE_VARIANT_TO_MODEL: Record<string, string> = {
  Screamer: "screamer",
  Minotaur: "minotaur",
  Rat: "rat",
  Breaker: "breaker",
  Fuzz: "fuzz",
  Centurion: "centurion",
  DsOne: "ds_one",
  SuperDrive: "super_drive",
  MetalCore: "metal_core",
  TightRift: "tight_rift",
  AmberCrunch: "amber_crunch",
  CopperFuzz: "copper_fuzz",
};

/** Rust `CabModel` variant → editor model id. */
export const CAB_VARIANT_TO_MODEL: Record<string, string> = {
  Vintage4x12: "vintage_cab",
  American2x12: "american_2x12",
  Tweed1x12: "tweed_1x12",
  Modern4x12: "modern_412",
  OpenBack: "open_back",
  Vintage2x12: "vintage_212",
  Oversized4x12: "oversized_412",
  BassCabinet: "bass_cabinet",
  Brit4x12: "brit_412",
  Uber4x12: "uber_412",
  Slo4x12: "slo_412",
  Ir: "ir",
  Modern2x12: "modern_212",
  American1x12: "american_1x12",
};

/** Rust `ReverbModel` variant → editor model id. */
export const REVERB_VARIANT_TO_MODEL: Record<string, string> = {
  Plate: "plate",
  Room: "room",
  Hall: "hall",
  Shimmer: "shimmer",
};

/** Rust `ModModel` variant → editor model id. */
export const MOD_VARIANT_TO_MODEL: Record<string, string> = {
  Chorus: "chorus",
  Phaser: "phaser",
  Flanger: "flanger",
  Tremolo: "tremolo",
  MolamSwirl: "molam_swirl",
  PhinVibe: "phin_vibe",
  KhaenSwirl: "khaen_swirl",
  BiLam: "bi_lam",
  IsanJet: "isan_jet",
  SoftPhase: "soft_phase",
  WideVibe: "wide_vibe",
};

/** Rust `DelayModel` variant → editor model id. */
export const DELAY_VARIANT_TO_MODEL: Record<string, string> = {
  Tape: "tape",
  Digital: "digital",
  Analog: "analog",
  PingPong: "ping_pong",
  Dual: "dual",
};

/** Rust `WahModel` variant → editor model id. */
export const WAH_VARIANT_TO_MODEL: Record<string, string> = {
  CryWah: "cry_wah",
  TouchWah: "touch_wah",
};

/**
 * Rust `EqModel` variant → editor model id. `Studio` maps to `parametric`,
 * the editor's original (and, until the model select was added, only) EQ
 * id — kept rather than renamed, so every existing preset keeps its exact
 * voicing.
 */
export const EQ_VARIANT_TO_MODEL: Record<string, string> = {
  Studio: "parametric",
  Vintage: "vintage_eq",
  Modern: "modern_eq",
};

type ParamsJson = Record<string, unknown>;

function num(p: ParamsJson, key: string): number | undefined {
  const v = p[key];
  return typeof v === "number" && Number.isFinite(v) ? v : undefined;
}

function bool(p: ParamsJson, key: string, fallback: boolean): boolean {
  const v = p[key];
  return typeof v === "boolean" ? v : fallback;
}

/** Set `paramId` in `list` (a cloned `Param[]`) when the blob has a value. */
function setVal(list: Param[] | undefined, paramId: string, value: number | undefined) {
  if (list === undefined || value === undefined) return;
  const param = list.find((x) => x.id === paramId);
  if (param) param.val = value;
}

/**
 * Build a `RigSnapshot` from a `selectInstance.state` payload. Returns `null`
 * for anything that is not a valid `RodhareistState` (fresh insert `{}`,
 * corrupt blob, future schema) — the caller falls back to factory defaults.
 */
export function snapshotFromRodhareistState(state: unknown): RigSnapshot | null {
  if (!state || typeof state !== "object") return null;
  const root = state as { schema_version?: unknown; params?: unknown };
  if (typeof root.schema_version !== "number") return null;
  if (!root.params || typeof root.params !== "object") return null;
  const p = root.params as ParamsJson;

  // Path: stage_order variant names → categories, in order, holes skipped.
  const rawOrder = Array.isArray(p.stage_order) ? p.stage_order : [];
  const pathOrder: CategoryId[] = [];
  for (const slot of rawOrder) {
    if (typeof slot !== "string") continue;
    const cat = STAGE_VARIANT_TO_CATEGORY[slot];
    if (cat && !pathOrder.includes(cat)) pathOrder.push(cat);
  }

  // Models per category. Single-algorithm stages are fixed; amp folds in the
  // tone engine (nam_capture / bypass override the classic model id).
  const stageModels = {} as Record<CategoryId, string>;
  for (const cat of chainOrder) {
    stageModels[cat] = models[cat][0]?.id ?? "";
  }
  const driveVariant = typeof p.drive_model === "string" ? p.drive_model : "";
  if (DRIVE_VARIANT_TO_MODEL[driveVariant]) {
    stageModels.dist = DRIVE_VARIANT_TO_MODEL[driveVariant]!;
  }
  const cabVariant = typeof p.cab_model === "string" ? p.cab_model : "";
  if (CAB_VARIANT_TO_MODEL[cabVariant]) {
    stageModels.cab = CAB_VARIANT_TO_MODEL[cabVariant]!;
  }
  const modVariant = typeof p.mod_model === "string" ? p.mod_model : "";
  if (MOD_VARIANT_TO_MODEL[modVariant]) {
    stageModels.mod = MOD_VARIANT_TO_MODEL[modVariant]!;
  }
  const wahVariant = typeof p.wah_model === "string" ? p.wah_model : "";
  if (WAH_VARIANT_TO_MODEL[wahVariant]) {
    stageModels.wah = WAH_VARIANT_TO_MODEL[wahVariant]!;
  }
  const eqVariant = typeof p.eq_model === "string" ? p.eq_model : "";
  if (EQ_VARIANT_TO_MODEL[eqVariant]) {
    stageModels.eq = EQ_VARIANT_TO_MODEL[eqVariant]!;
  }
  const delayVariant = typeof p.delay_model === "string" ? p.delay_model : "";
  if (DELAY_VARIANT_TO_MODEL[delayVariant]) {
    stageModels.delay = DELAY_VARIANT_TO_MODEL[delayVariant]!;
  }
  const reverbVariant = typeof p.reverb_model === "string" ? p.reverb_model : "";
  if (REVERB_VARIANT_TO_MODEL[reverbVariant]) {
    stageModels.verb = REVERB_VARIANT_TO_MODEL[reverbVariant]!;
  }
  // Second instances live in their own `stage_b` block. Absent from a pre-v4
  // blob, in which case every B stage keeps its editor default — which is the
  // default of the stage it doubles, so nothing on screen changes.
  const b: ParamsJson =
    p.stage_b && typeof p.stage_b === "object"
      ? (p.stage_b as ParamsJson)
      : {};
  const driveBVariant = typeof b.drive_model === "string" ? b.drive_model : "";
  if (DRIVE_VARIANT_TO_MODEL[driveBVariant]) {
    stageModels.dist2 = secondInstanceModelId(DRIVE_VARIANT_TO_MODEL[driveBVariant]!);
  }
  const modBVariant = typeof b.mod_model === "string" ? b.mod_model : "";
  if (MOD_VARIANT_TO_MODEL[modBVariant]) {
    stageModels.mod2 = secondInstanceModelId(MOD_VARIANT_TO_MODEL[modBVariant]!);
  }
  const delayBVariant = typeof b.delay_model === "string" ? b.delay_model : "";
  if (DELAY_VARIANT_TO_MODEL[delayBVariant]) {
    stageModels.delay2 = secondInstanceModelId(DELAY_VARIANT_TO_MODEL[delayBVariant]!);
  }
  const eqBVariant = typeof b.eq_model === "string" ? b.eq_model : "";
  if (EQ_VARIANT_TO_MODEL[eqBVariant]) {
    stageModels.eq2 = secondInstanceModelId(EQ_VARIANT_TO_MODEL[eqBVariant]!);
  }
  const ampVariant = typeof p.amp_model === "string" ? p.amp_model : "";
  const toneEngine = typeof p.tone_engine === "string" ? p.tone_engine : "Classic";
  if (toneEngine === "NamCapture") {
    stageModels.amp = "nam_capture";
  } else if (toneEngine === "Bypass") {
    stageModels.amp = "bypass";
  } else if (AMP_VARIANT_TO_MODEL[ampVariant]) {
    stageModels.amp = AMP_VARIANT_TO_MODEL[ampVariant]!;
  }

  // Bypasses: editor "bypassed" is the inverse of the DSP's `*_on`.
  const bypassed: Partial<Record<CategoryId, boolean>> = {};
  const enables: [CategoryId, string][] = [
    ["dyn", "gate_on"],
    ["comp", "comp_on"],
    ["wah", "wah_on"],
    ["dist", "drive_on"],
    ["amp", "amp_on"],
    ["eq", "eq_on"],
    ["mod", "mod_on"],
    ["delay", "delay_on"],
    ["verb", "reverb_on"],
    ["cab", "cab_on"],
  ];
  for (const [cat, key] of enables) {
    if (!bool(p, key, true)) bypassed[cat] = true;
  }
  const bEnables: [CategoryId, string][] = [
    ["comp2", "comp_on"],
    ["dist2", "drive_on"],
    ["eq2", "eq_on"],
    ["mod2", "mod_on"],
    ["delay2", "delay_on"],
  ];
  for (const [cat, key] of bEnables) {
    if (!bool(b, key, true)) bypassed[cat] = true;
  }

  // Knob values: only each category's *selected* model receives blob values
  // (Params holds one value per shared id); other models keep defaults.
  const parameters = cloneParameters();
  setVal(parameters.gate, "gate_thresh", num(p, "gate_thresh_db"));
  const dist = parameters[stageModels.dist];
  setVal(dist, "drive_gain", num(p, "drive_gain"));
  setVal(dist, "drive_tone", num(p, "drive_tone"));
  setVal(dist, "drive_level", num(p, "drive_level"));
  // Amp knobs live on the classic amp models; NAM has its own ids.
  const ampModelForParams =
    AMP_VARIANT_TO_MODEL[ampVariant] ?? stageModels.amp;
  const amp = parameters[ampModelForParams];
  setVal(amp, "amp_gain", num(p, "amp_gain"));
  setVal(amp, "amp_bass", num(p, "amp_bass"));
  setVal(amp, "amp_middle", num(p, "amp_middle"));
  setVal(amp, "amp_treble", num(p, "amp_treble"));
  setVal(amp, "amp_presence", num(p, "amp_presence"));
  setVal(amp, "amp_master", num(p, "amp_master"));
  const nam = parameters.nam_capture;
  setVal(nam, "nam_input_trim", num(p, "nam_input_trim_db"));
  setVal(nam, "nam_output_trim", num(p, "nam_output_trim_db"));
  setVal(nam, "nam_mix", num(p, "nam_mix"));
  setVal(nam, "nam_slim_size", num(p, "nam_slim_size"));
  const softknee = parameters.softknee;
  setVal(softknee, "comp_thresh", num(p, "comp_thresh_db"));
  setVal(softknee, "comp_ratio", num(p, "comp_ratio"));
  setVal(softknee, "comp_attack", num(p, "comp_attack_ms"));
  setVal(softknee, "comp_release", num(p, "comp_release_ms"));
  setVal(softknee, "comp_makeup", num(p, "comp_makeup_db"));
  // The Eq slot's models share the eq_* ids; only the selected model
  // receives the blob values (same rule as dist/cab/mod above).
  const eqParams = parameters[stageModels.eq];
  setVal(eqParams, "eq_low_gain", num(p, "eq_low_gain_db"));
  setVal(eqParams, "eq_mid1_freq", num(p, "eq_mid1_freq_hz"));
  setVal(eqParams, "eq_mid1_gain", num(p, "eq_mid1_gain_db"));
  setVal(eqParams, "eq_mid2_freq", num(p, "eq_mid2_freq_hz"));
  setVal(eqParams, "eq_mid2_gain", num(p, "eq_mid2_gain_db"));
  setVal(eqParams, "eq_high_gain", num(p, "eq_high_gain_db"));
  // The Mod slot's models share the chorus_* ids; only the selected model
  // receives the blob values (same rule as dist/cab above).
  const modParams = parameters[stageModels.mod];
  setVal(modParams, "chorus_rate", num(p, "chorus_rate"));
  setVal(modParams, "chorus_depth", num(p, "chorus_depth"));
  setVal(modParams, "chorus_mix", num(p, "chorus_mix"));
  const wahParams = parameters[stageModels.wah];
  setVal(wahParams, "wah_pos", num(p, "wah_pos"));
  setVal(wahParams, "wah_res", num(p, "wah_res"));
  setVal(wahParams, "wah_sens", num(p, "wah_sens"));
  // The Delay slot's voicings share the delay_* ids; only the selected model
  // receives the blob values (same rule as dist/cab/mod above).
  const delayParams = parameters[stageModels.delay];
  setVal(delayParams, "delay_time", num(p, "delay_time_ms"));
  setVal(delayParams, "delay_fb", num(p, "delay_fb"));
  setVal(delayParams, "delay_mix", num(p, "delay_mix"));
  setVal(delayParams, "delay_tone", num(p, "delay_tone"));
  // Reverb voicings share Decay/Mix; Shimmer alone also exposes its octave-up
  // feedback amount. Write only into the active model's param set.
  const verbParams = parameters[stageModels.verb];
  setVal(verbParams, "reverb_decay", num(p, "reverb_decay_s"));
  setVal(verbParams, "reverb_mix", num(p, "reverb_mix"));
  setVal(verbParams, "reverb_shimmer", num(p, "reverb_shimmer"));
  const cab = parameters[stageModels.cab];
  const micVariant = typeof p.mic_model === "string" ? p.mic_model : "Dynamic";
  const micIndex: number | undefined = (
    { Dynamic: 0, Ribbon: 1, Condenser: 2 } as Record<string, number>
  )[micVariant];
  setVal(cab, "cab_mic_type", micIndex);
  setVal(cab, "cab_mic", num(p, "cab_mic"));
  setVal(cab, "cab_dist", num(p, "cab_dist"));

  // Second-instance knobs, on the same rule: only the B block's *selected*
  // model receives them.
  const distB = parameters[stageModels.dist2];
  setVal(distB, "drive2_gain", num(b, "drive_gain"));
  setVal(distB, "drive2_tone", num(b, "drive_tone"));
  setVal(distB, "drive2_level", num(b, "drive_level"));
  const softkneeB = parameters.softknee_2;
  setVal(softkneeB, "comp2_thresh", num(b, "comp_thresh_db"));
  setVal(softkneeB, "comp2_ratio", num(b, "comp_ratio"));
  setVal(softkneeB, "comp2_attack", num(b, "comp_attack_ms"));
  setVal(softkneeB, "comp2_release", num(b, "comp_release_ms"));
  setVal(softkneeB, "comp2_makeup", num(b, "comp_makeup_db"));
  const eqB = parameters[stageModels.eq2];
  setVal(eqB, "eq2_low_gain", num(b, "eq_low_gain_db"));
  setVal(eqB, "eq2_mid1_freq", num(b, "eq_mid1_freq_hz"));
  setVal(eqB, "eq2_mid1_gain", num(b, "eq_mid1_gain_db"));
  setVal(eqB, "eq2_mid2_freq", num(b, "eq_mid2_freq_hz"));
  setVal(eqB, "eq2_mid2_gain", num(b, "eq_mid2_gain_db"));
  setVal(eqB, "eq2_high_gain", num(b, "eq_high_gain_db"));
  const modB = parameters[stageModels.mod2];
  setVal(modB, "chorus2_rate", num(b, "chorus_rate"));
  setVal(modB, "chorus2_depth", num(b, "chorus_depth"));
  setVal(modB, "chorus2_mix", num(b, "chorus_mix"));
  const delayB = parameters[stageModels.delay2];
  setVal(delayB, "delay2_time", num(b, "delay_time_ms"));
  setVal(delayB, "delay2_fb", num(b, "delay_fb"));
  setVal(delayB, "delay2_mix", num(b, "delay_mix"));
  setVal(delayB, "delay2_tone", num(b, "delay_tone"));

  const globals: GlobalState = {
    inputTrim: num(p, "input_trim_db") ?? 0,
    outputTrim: num(p, "output_trim_db") ?? 0,
    globalBypass: !bool(p, "power", true),
  };

  const activeCat: CategoryId = pathOrder.includes("amp")
    ? "amp"
    : (pathOrder[0] ?? "amp");

  return {
    activeCat,
    activeModelId: stageModels[activeCat],
    stageModels,
    pathOrder,
    bypassed,
    parameters,
    globals,
  };
}
