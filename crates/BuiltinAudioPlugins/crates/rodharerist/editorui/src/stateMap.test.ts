// Pins the TS half of the state-blob contract: `snapshotFromRodhareistState`
// must decode exactly what serde emits for `RodhareistState` (Rust twin:
// `state::tests::round_trips_through_json` + the wire/model pins).

import { describe, expect, test } from "bun:test";
import {
  AMP_VARIANT_TO_MODEL,
  CAB_VARIANT_TO_MODEL,
  DRIVE_VARIANT_TO_MODEL,
  EQ_VARIANT_TO_MODEL,
  MOD_VARIANT_TO_MODEL,
  DELAY_VARIANT_TO_MODEL,
  REVERB_VARIANT_TO_MODEL,
  STAGE_VARIANT_TO_CATEGORY,
  WAH_VARIANT_TO_MODEL,
  snapshotFromRodhareistState,
} from "./stateMap";
import { defaultValueFor, models, parameterDefaults } from "./data";
import {
  AMP_MODEL_INDEX,
  CAB_MODEL_INDEX,
  DRIVE_MODEL_INDEX,
  EQ_MODEL_INDEX,
  MOD_MODEL_INDEX,
} from "./bridge";

/** A serde-shaped `RodhareistState` fixture (variant-name enum strings). */
const FIXTURE = {
  schema_version: 2,
  params: {
    power: false,
    input_trim_db: 3.5,
    output_trim_db: -2,
    gate_on: true,
    drive_on: false,
    amp_on: true,
    mod_on: true,
    delay_on: false,
    reverb_on: true,
    cab_on: true,
    comp_on: false,
    eq_on: true,
    drive_model: "Rat",
    amp_model: "Recto",
    cab_model: "Tweed1x12",
    mic_model: "Ribbon",
    reverb_model: "Plate",
    tone_engine: "Classic",
    stage_order: ["Eq", "Amp", "Cab", null, null, null, null, null, null],
    gate_thresh_db: -40,
    drive_gain: 8.5,
    drive_tone: 3,
    drive_level: 7,
    amp_gain: 9,
    amp_bass: 6,
    amp_middle: 2,
    amp_treble: 7.5,
    amp_presence: 4,
    amp_master: 5,
    chorus_rate: 1,
    chorus_depth: 2,
    chorus_mix: 33,
    delay_model: "Analog",
    delay_time_ms: 640,
    delay_fb: 45,
    delay_mix: 25,
    delay_tone: 7.5,
    reverb_decay_s: 3.5,
    reverb_mix: 60,
    reverb_shimmer: 74,
    cab_mic: 80,
    cab_dist: 10,
    comp_thresh_db: -30,
    comp_ratio: 4,
    comp_attack_ms: 5,
    comp_release_ms: 200,
    comp_makeup_db: 3,
    eq_low_gain_db: 2,
    eq_mid1_freq_hz: 250,
    eq_mid1_gain_db: -3,
    eq_mid2_freq_hz: 3000,
    eq_mid2_gain_db: 4,
    eq_high_gain_db: -1,
    nam_input_trim_db: 1,
    nam_output_trim_db: -1,
    nam_mix: 90,
    nam_loudness_norm: false,
  },
};

function param(snap: NonNullable<ReturnType<typeof snapshotFromRodhareistState>>, modelId: string, id: string) {
  return snap.parameters[modelId]?.find((p) => p.id === id)?.val;
}

describe("snapshotFromRodhareistState", () => {
  test("invalid payloads return null (fresh insert path)", () => {
    expect(snapshotFromRodhareistState(null)).toBeNull();
    expect(snapshotFromRodhareistState({})).toBeNull();
    expect(snapshotFromRodhareistState("nope")).toBeNull();
    expect(snapshotFromRodhareistState({ schema_version: 2 })).toBeNull();
  });

  test("variant tables cover every Rust enum variant", () => {
    // 10 stages + the 5 second instances (`StageKind::Drive2` …).
    expect(Object.keys(STAGE_VARIANT_TO_CATEGORY)).toHaveLength(15);
    expect(Object.keys(AMP_VARIANT_TO_MODEL)).toHaveLength(11);
    expect(Object.keys(DRIVE_VARIANT_TO_MODEL)).toHaveLength(12);
    expect(Object.keys(CAB_VARIANT_TO_MODEL)).toHaveLength(14);
    expect(Object.keys(MOD_VARIANT_TO_MODEL)).toHaveLength(11);
    expect(Object.keys(EQ_VARIANT_TO_MODEL)).toHaveLength(3);
    expect(Object.keys(WAH_VARIANT_TO_MODEL)).toHaveLength(2);
    expect(Object.keys(REVERB_VARIANT_TO_MODEL)).toHaveLength(4);
    expect(Object.keys(DELAY_VARIANT_TO_MODEL)).toHaveLength(5);
  });

  // The mod slot's model list lives in four places that must agree: the Rust
  // `ModModel::ALL` order (pinned on the Rust side), the wire index map, the
  // variant table above, and the editor's model + parameter lists. Adding a
  // phaser voice to some but not all of them is silent — the model shows up in
  // the picker and then selects a different effect, or none.
  test("every mod model is wired end to end and its indices are contiguous", () => {
    const listed = models.mod.map((m) => m.id);
    const byIndex = Object.entries(MOD_MODEL_INDEX).sort((a, b) => a[1] - b[1]);

    expect(byIndex.map(([, i]) => i)).toEqual(listed.map((_, i) => i));
    expect(byIndex.map(([id]) => id)).toEqual(listed);
    expect(Object.values(MOD_VARIANT_TO_MODEL).sort()).toEqual([...listed].sort());

    for (const id of listed) {
      const table = parameterDefaults[id];
      expect(table, `no parameter table for mod model \`${id}\``).toBeDefined();
      expect(table?.map((p) => p.id)).toEqual([
        "chorus_rate",
        "chorus_depth",
        "chorus_mix",
      ]);
    }
  });

  // Same wiring hazard as the mod test above, for the two other stages with
  // per-model tuned voicings sharing one shared knob set.
  test("every classic amp model is wired end to end and its indices are contiguous", () => {
    // `models.amp` also lists `nam_capture`/`bypass` — special `ToneEngineKind`
    // engines, not `AmpModel` variants, so they carry no wire index/variant
    // entry here by design. `AMP_MODEL_INDEX`'s keys are the classic models only.
    const listed = Object.keys(AMP_MODEL_INDEX);
    const byIndex = Object.entries(AMP_MODEL_INDEX).sort((a, b) => a[1] - b[1]);

    expect(byIndex.map(([, i]) => i)).toEqual(listed.map((_, i) => i));
    expect(Object.values(AMP_VARIANT_TO_MODEL).sort()).toEqual([...listed].sort());

    for (const id of listed) {
      const table = parameterDefaults[id];
      expect(table, `no parameter table for amp model \`${id}\``).toBeDefined();
      expect(table?.map((p) => p.id)).toEqual([
        "amp_gain",
        "amp_bass",
        "amp_middle",
        "amp_treble",
        "amp_presence",
        "amp_master",
      ]);
    }
  });

  test("every drive model is wired end to end and its indices are contiguous", () => {
    const listed = models.dist.map((m) => m.id);
    const byIndex = Object.entries(DRIVE_MODEL_INDEX).sort((a, b) => a[1] - b[1]);

    expect(byIndex.map(([, i]) => i)).toEqual(listed.map((_, i) => i));
    expect(byIndex.map(([id]) => id)).toEqual(listed);
    expect(Object.values(DRIVE_VARIANT_TO_MODEL).sort()).toEqual([...listed].sort());

    for (const id of listed) {
      const table = parameterDefaults[id];
      expect(table, `no parameter table for drive model \`${id}\``).toBeDefined();
      expect(table?.map((p) => p.id)).toEqual([
        "drive_gain",
        "drive_tone",
        "drive_level",
      ]);
    }
  });

  test("every eq model is wired end to end and its indices are contiguous", () => {
    const listed = models.eq.map((m) => m.id);
    const byIndex = Object.entries(EQ_MODEL_INDEX).sort((a, b) => a[1] - b[1]);

    expect(byIndex.map(([, i]) => i)).toEqual(listed.map((_, i) => i));
    expect(byIndex.map(([id]) => id)).toEqual(listed);
    expect(Object.values(EQ_VARIANT_TO_MODEL).sort()).toEqual([...listed].sort());

    for (const id of listed) {
      const table = parameterDefaults[id];
      expect(table, `no parameter table for eq model \`${id}\``).toBeDefined();
      expect(table?.map((p) => p.id)).toEqual([
        "eq_low_gain",
        "eq_mid1_freq",
        "eq_mid1_gain",
        "eq_mid2_freq",
        "eq_mid2_gain",
        "eq_high_gain",
      ]);
    }
  });

  // Cab has no per-model parameter table (mic type/position/distance are
  // shared across every voicing), so this checks index/variant wiring only.
  test("every cab model is wired end to end and its indices are contiguous", () => {
    const listed = models.cab.map((m) => m.id);
    const byIndex = Object.entries(CAB_MODEL_INDEX).sort((a, b) => a[1] - b[1]);

    expect(byIndex.map(([, i]) => i)).toEqual(listed.map((_, i) => i));
    expect(byIndex.map(([id]) => id)).toEqual(listed);
    expect(Object.values(CAB_VARIANT_TO_MODEL).sort()).toEqual([...listed].sort());
  });

  test("maps a serde fixture field-for-field", () => {
    const snap = snapshotFromRodhareistState(FIXTURE);
    expect(snap).not.toBeNull();
    if (!snap) return;

    expect(snap.pathOrder).toEqual(["eq", "amp", "cab"]);
    expect(snap.stageModels.dist).toBe("rat");
    expect(snap.stageModels.amp).toBe("recto");
    expect(snap.stageModels.cab).toBe("tweed_1x12");
    expect(snap.stageModels.comp).toBe("softknee");
    expect(snap.stageModels.eq).toBe("parametric");

    expect(snap.bypassed).toEqual({ dist: true, delay: true, comp: true });

    expect(param(snap, "gate", "gate_thresh")).toBe(-40);
    expect(param(snap, "rat", "drive_gain")).toBe(8.5);
    expect(param(snap, "recto", "amp_master")).toBe(5);
    expect(snap.stageModels.delay).toBe("analog");
    expect(param(snap, "analog", "delay_time")).toBe(640);
    expect(param(snap, "analog", "delay_tone")).toBe(7.5);
    // The unselected tape voicing keeps its own default.
    expect(param(snap, "tape", "delay_time")).toBe(
      defaultValueFor("tape", "delay_time"),
    );
    expect(param(snap, "plate", "reverb_decay")).toBe(3.5);
    expect(param(snap, "tweed_1x12", "cab_mic")).toBe(80);
    expect(param(snap, "tweed_1x12", "cab_mic_type")).toBe(1);
    expect(param(snap, "softknee", "comp_ratio")).toBe(4);
    expect(param(snap, "parametric", "eq_mid2_gain")).toBe(4);
    expect(param(snap, "nam_capture", "nam_mix")).toBe(90);
    expect(param(snap, "nam_capture", "nam_slim_size")).toBe(100);
    // Non-selected models keep their defaults.
    expect(param(snap, "screamer", "drive_gain")).toBe(
      defaultValueFor("screamer", "drive_gain"),
    );

    expect(snap.globals).toEqual({
      inputTrim: 3.5,
      outputTrim: -2,
      globalBypass: true,
    });
    // Amp is in the path → focused.
    expect(snap.activeCat).toBe("amp");
    expect(snap.activeModelId).toBe("recto");
  });

  test("restores the dedicated Shimmer amount only on the Shimmer model", () => {
    const state = structuredClone(FIXTURE);
    state.params.reverb_model = "Shimmer";
    const snap = snapshotFromRodhareistState(state);
    expect(snap?.stageModels.verb).toBe("shimmer");
    expect(snap && param(snap, "shimmer", "reverb_shimmer")).toBe(74);
    expect(snap && param(snap, "plate", "reverb_shimmer")).toBeUndefined();
  });

  test("tone engine overrides the amp model id but not its knob values", () => {
    const nam = structuredClone(FIXTURE);
    nam.params.tone_engine = "NamCapture";
    const snap = snapshotFromRodhareistState(nam);
    expect(snap?.stageModels.amp).toBe("nam_capture");
    // Classic amp knobs still land on the underlying amp model.
    expect(snap && param(snap, "recto", "amp_gain")).toBe(9);

    const bypass = structuredClone(FIXTURE);
    bypass.params.tone_engine = "Bypass";
    expect(snapshotFromRodhareistState(bypass)?.stageModels.amp).toBe("bypass");
  });

  test("restores NAM A2 quality when present", () => {
    const state = {
      ...FIXTURE,
      params: { ...FIXTURE.params, nam_slim_size: 40 },
    };
    const snap = snapshotFromRodhareistState(state);
    expect(param(snap!, "nam_capture", "nam_slim_size")).toBe(40);
  });

  test("v3 mod/wah fields map onto the selected models", () => {
    const v3 = structuredClone(FIXTURE) as typeof FIXTURE & {
      params: Record<string, unknown>;
    };
    v3.schema_version = 3;
    v3.params.mod_model = "Phaser";
    v3.params.wah_model = "TouchWah";
    v3.params.wah_on = false;
    v3.params.wah_pos = 7.5;
    v3.params.wah_res = 8;
    v3.params.wah_sens = 3;
    v3.params.stage_order = [
      "Wah", "Eq", "Amp", "Cab", null, null, null, null, null, null,
    ];
    const snap = snapshotFromRodhareistState(v3);
    expect(snap).not.toBeNull();
    if (!snap) return;
    expect(snap.pathOrder).toEqual(["wah", "eq", "amp", "cab"]);
    expect(snap.stageModels.mod).toBe("phaser");
    expect(snap.stageModels.wah).toBe("touch_wah");
    expect(snap.bypassed.wah).toBe(true);
    // Shared chorus_* knobs land on the selected mod model, not "chorus".
    expect(param(snap, "phaser", "chorus_rate")).toBe(1);
    expect(param(snap, "chorus", "chorus_rate")).toBe(
      defaultValueFor("chorus", "chorus_rate"),
    );
    expect(param(snap, "touch_wah", "wah_pos")).toBe(7.5);
    expect(param(snap, "touch_wah", "wah_sens")).toBe(3);
  });

  test("a v2 blob (9 path slots, no mod/wah fields) still maps", () => {
    const snap = snapshotFromRodhareistState(FIXTURE);
    expect(snap).not.toBeNull();
    if (!snap) return;
    // Absent mod_model/wah_model fall back to each category's first model.
    expect(snap.stageModels.mod).toBe("chorus");
    expect(snap.stageModels.wah).toBe("cry_wah");
    expect(snap.bypassed.wah).toBeUndefined();
  });
});
