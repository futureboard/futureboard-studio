/** The ten stages the DSP has always had — one instance each. */
export type PrimaryCategoryId =
  | "dyn"
  | "comp"
  | "wah"
  | "dist"
  | "amp"
  | "eq"
  | "mod"
  | "delay"
  | "verb"
  | "cab";

/**
 * Second instances of the five stages a player actually doubles on a real
 * board (Rust `StageKind::Drive2` …). Independent blocks with their own model,
 * enable and knobs — a "B" block shares nothing with its "A" but the model
 * list it picks from.
 *
 * Amp, Cabinet, Reverb, Gate and Wah have no second instance: a second cabinet
 * convolver or NAM model would double the plugin's heaviest allocation for a
 * rig nobody builds.
 */
export type SecondCategoryId = "comp2" | "dist2" | "eq2" | "mod2" | "delay2";

export type CategoryId = PrimaryCategoryId | SecondCategoryId;

/** Which stage each second instance doubles. Mirrors Rust `StageKind::doubles`. */
export const doubles: Record<SecondCategoryId, PrimaryCategoryId> = {
  comp2: "comp",
  dist2: "dist",
  eq2: "eq",
  mod2: "mod",
  delay2: "delay",
};

export const secondCategoryIds = Object.keys(doubles) as SecondCategoryId[];

export function isSecondInstance(cat: CategoryId): cat is SecondCategoryId {
  return cat in doubles;
}

/**
 * The B-side id for an A-side one: the stage's own token gains a `2`.
 *
 * One rule covers models, params and node ids, so `drive_gain` → `drive2_gain`
 * and `chorus_rate` → `chorus2_rate` read as "which block, then which knob" —
 * and match the Rust `apply_to_params` arms exactly.
 */
export function secondInstanceParamId(id: string): string {
  const cut = id.indexOf("_");
  return cut < 0 ? `${id}2` : `${id.slice(0, cut)}2${id.slice(cut)}`;
}

export type Category = {
  name: string;
  short: string;
  color: string;
  rgb: string;
  node: string;
};

export type Preset = {
  id: string;
  name: string;
  category: CategoryId;
  model: string;
  /** Model selected in each populated stage. The focused category/model above
   * remains the editor landing point. */
  stageModels?: Partial<Record<CategoryId, string>>;
  values: Record<string, number>;
  /** Stages in the signal path (Helix order). Empty = empty path. */
  path?: CategoryId[];
  /** Stages bypassed (off) when loaded. */
  bypassed?: CategoryId[];
  /**
   * Per-preset output level in dB, loaded into the global Output Trim.
   *
   * A clean Twin and a saturated Recto do not arrive at the same loudness from
   * the same amp settings, and matching them by retuning Gain/Master would mean
   * writing settings no player would use on the real amp. The bank is levelled
   * here instead, so every preset keeps honest amp settings and still lands at
   * the same loudness. Measured, not estimated — see
   * `examples/preset_audit.rs`, which prints the value each preset needs.
   */
  outputTrim?: number;
};

export type Model = {
  id: string;
  name: string;
  /** Compact label for Path blocks */
  short: string;
  sub: string;
};

export type Param = {
  id: string;
  name: string;
  min: number;
  max: number;
  val: number;
  unit: string;
};

export const presetsData: Preset[] = [
  {
    id: "00A",
    name: "Empty",
    category: "amp",
    model: "mandarin",
    values: {},
    path: [],
    bypassed: ["dyn", "comp", "wah", "dist", "amp", "eq", "mod", "delay", "verb", "cab"],
  },
  {
    id: "01A",
    name: "US Studio Clean",
    outputTrim: 4.5,
    category: "amp",
    model: "twin",
    stageModels: { comp: "softknee", amp: "twin", verb: "room", cab: "american_2x12" },
    values: {
      // 2:1 with a slow-ish attack — a tracking comp that lets the pick through
      // and only rides the sustain. Makeup covers the ~3 dB it takes off.
      comp_thresh: -20,
      comp_ratio: 2,
      comp_attack: 18,
      comp_release: 140,
      comp_makeup: 3,
      // Twin Reverb studio clean: volume low, bright-forward tone stack.
      amp_gain: 3,
      amp_bass: 4.5,
      amp_middle: 5.5,
      amp_treble: 6.5,
      amp_presence: 5,
      amp_master: 7,
      reverb_decay: 1.6,
      reverb_mix: 18,
      cab_mic_type: 0,
      cab_mic: 42,
      cab_dist: 22,
    },
    path: ["comp", "amp", "verb", "cab"],
  },
  {
    id: "01B",
    name: "Warm Jazz Clean",
    outputTrim: 0.5,
    category: "amp",
    model: "twin",
    stageModels: { comp: "softknee", amp: "twin", eq: "parametric", verb: "room", cab: "open_back" },
    values: {
      comp_thresh: -18,
      comp_ratio: 2.5,
      comp_attack: 25,
      comp_release: 180,
      comp_makeup: 3,
      // Archtop-into-Twin voicing: bass and mids up, treble well back.
      amp_gain: 3,
      amp_bass: 5.8,
      amp_middle: 6.2,
      amp_treble: 3.8,
      amp_presence: 3,
      amp_master: 7.5,
      eq_low_gain: 1.5,
      eq_mid1_freq: 320,
      eq_mid1_gain: 1,
      eq_mid2_freq: 2800,
      eq_mid2_gain: -2,
      eq_high_gain: -1.5,
      reverb_decay: 1.4,
      reverb_mix: 13,
      cab_mic_type: 1,
      cab_mic: 28,
      cab_dist: 48,
    },
    path: ["comp", "amp", "eq", "verb", "cab"],
  },
  {
    id: "01C",
    name: "Country Slapback",
    outputTrim: 5,
    category: "delay",
    model: "tape",
    stageModels: { comp: "softknee", amp: "twin", delay: "tape", verb: "room", cab: "american_2x12" },
    values: {
      // Country squash: fast attack, hard ratio, and the makeup that costs.
      comp_thresh: -24,
      comp_ratio: 3.5,
      comp_attack: 8,
      comp_release: 90,
      comp_makeup: 4,
      amp_gain: 3.2,
      amp_bass: 4.2,
      amp_middle: 4.8,
      amp_treble: 7,
      amp_presence: 5.5,
      amp_master: 7.5,
      // ~105 ms with barely any regeneration is the classic single slapback.
      delay_time: 105,
      delay_fb: 11,
      delay_mix: 17,
      delay_tone: 6,
      reverb_decay: 1.3,
      reverb_mix: 12,
      cab_mic_type: 0,
      cab_mic: 58,
      cab_dist: 18,
    },
    path: ["comp", "amp", "delay", "verb", "cab"],
  },
  {
    id: "01D",
    name: "Funk Touch Wah",
    outputTrim: 3.5,
    category: "wah",
    model: "touch_wah",
    stageModels: { comp: "softknee", wah: "touch_wah", amp: "twin", cab: "american_2x12" },
    values: {
      comp_thresh: -22,
      comp_ratio: 3,
      comp_attack: 12,
      comp_release: 100,
      comp_makeup: 3,
      wah_pos: 2.2,
      wah_res: 5.5,
      wah_sens: 6.2,
      amp_gain: 2.6,
      amp_bass: 4,
      amp_middle: 5.2,
      amp_treble: 6.5,
      amp_presence: 5,
      amp_master: 8,
      cab_mic_type: 0,
      cab_mic: 48,
      cab_dist: 20,
    },
    path: ["comp", "wah", "amp", "cab"],
  },
  {
    id: "02A",
    name: "Top Boost Chime",
    outputTrim: 0,
    category: "amp",
    model: "topboost",
    stageModels: { amp: "topboost", verb: "room", cab: "open_back" },
    values: {
      // Top Boost chime: treble and Cut up, mids scooped, volume just under
      // where the amp starts to break up.
      amp_gain: 5,
      amp_bass: 4.5,
      amp_middle: 4,
      amp_treble: 7,
      amp_presence: 6.5,
      amp_master: 7,
      reverb_decay: 1.5,
      reverb_mix: 15,
      cab_mic_type: 0,
      cab_mic: 52,
      cab_dist: 30,
    },
    path: ["amp", "verb", "cab"],
  },
  {
    id: "02B",
    name: "Top Boost Tremolo",
    outputTrim: 1.5,
    category: "mod",
    model: "tremolo",
    stageModels: { amp: "topboost", mod: "tremolo", verb: "room", cab: "open_back" },
    values: {
      amp_gain: 4.5,
      amp_bass: 5,
      amp_middle: 4,
      amp_treble: 6.5,
      amp_presence: 6,
      amp_master: 7,
      // Rate 4.2 is 5.3 Hz on the tremolo's 0.5–12 Hz law — where an amp's own
      // opto tremolo sits. Mix is the Shape control here: low = sine, not chop.
      chorus_rate: 4.2,
      chorus_depth: 5.5,
      chorus_mix: 18,
      reverb_decay: 1.8,
      reverb_mix: 18,
      cab_mic_type: 1,
      cab_mic: 38,
      cab_dist: 42,
    },
    path: ["amp", "mod", "verb", "cab"],
  },
  {
    id: "02C",
    name: "Jangle Chorus",
    outputTrim: 5,
    category: "mod",
    model: "chorus",
    stageModels: { amp: "topboost", mod: "chorus", delay: "digital", verb: "room", cab: "open_back" },
    values: {
      amp_gain: 3.8,
      amp_bass: 4.5,
      amp_middle: 3.8,
      amp_treble: 7,
      amp_presence: 6.5,
      amp_master: 7.2,
      // 1.75 Hz on the chorus's linear 0.1–6 Hz law — a slow shimmer under the
      // chord, not a warble.
      chorus_rate: 2.8,
      chorus_depth: 4.5,
      chorus_mix: 28,
      delay_time: 360,
      delay_fb: 18,
      delay_mix: 14,
      delay_tone: 6.5,
      reverb_decay: 1.7,
      reverb_mix: 15,
      cab_mic_type: 2,
      cab_mic: 45,
      cab_dist: 45,
    },
    path: ["amp", "mod", "delay", "verb", "cab"],
  },
  {
    id: "03A",
    name: "Tweed Edge",
    outputTrim: -0.5,
    category: "amp",
    model: "bassman",
    stageModels: { amp: "bassman", verb: "room", cab: "tweed_1x12" },
    values: {
      // A tweed's edge lives at volume ~6, not at the top of the dial; the
      // master carries the level so the breakup stays where the name says.
      amp_gain: 6,
      amp_bass: 5,
      amp_middle: 6,
      amp_treble: 6,
      amp_presence: 4.5,
      amp_master: 5.5,
      reverb_decay: 1.2,
      reverb_mix: 10,
      cab_mic_type: 1,
      cab_mic: 30,
      cab_dist: 38,
    },
    path: ["amp", "verb", "cab"],
  },
  {
    id: "03B",
    name: "Tweed Blues Drive",
    outputTrim: -0.5,
    category: "dist",
    model: "breaker",
    stageModels: { dist: "breaker", amp: "bassman", verb: "room", cab: "tweed_1x12" },
    values: {
      drive_gain: 4,
      drive_tone: 5,
      drive_level: 6,
      amp_gain: 5,
      amp_bass: 5,
      amp_middle: 6,
      amp_treble: 5.8,
      amp_presence: 4.5,
      amp_master: 5.5,
      reverb_decay: 1.4,
      reverb_mix: 12,
      cab_mic_type: 1,
      cab_mic: 25,
      cab_dist: 42,
    },
    path: ["dist", "amp", "verb", "cab"],
  },
  {
    id: "04A",
    name: "Plexi Rhythm",
    outputTrim: -2,
    category: "amp",
    model: "plexi",
    stageModels: { amp: "plexi", cab: "brit_412" },
    values: {
      // Super Lead rhythm: bass back, mids and treble up, presence open —
      // the settings a Plexi is actually run at for a rhythm part.
      amp_gain: 6,
      amp_bass: 4,
      amp_middle: 6,
      amp_treble: 7,
      amp_presence: 6,
      amp_master: 5.5,
      cab_mic_type: 0,
      cab_mic: 38,
      cab_dist: 18,
    },
    path: ["amp", "cab"],
  },
  {
    id: "04B",
    name: "Plexi Lead",
    outputTrim: 0,
    category: "dist",
    model: "minotaur",
    stageModels: { dist: "minotaur", amp: "plexi", delay: "tape", verb: "plate", cab: "brit_412" },
    values: {
      // Transparent boost the way one is actually used in front of a Plexi:
      // gain low, output high, so the amp does the distorting.
      drive_gain: 2.5,
      drive_tone: 5.5,
      drive_level: 7.5,
      amp_gain: 7,
      amp_bass: 4,
      amp_middle: 6.5,
      amp_treble: 6.5,
      amp_presence: 6,
      amp_master: 6,
      delay_time: 360,
      delay_fb: 24,
      delay_mix: 18,
      delay_tone: 4.5,
      reverb_decay: 3.2,
      reverb_mix: 16,
      cab_mic_type: 1,
      cab_mic: 35,
      cab_dist: 28,
    },
    path: ["dist", "amp", "delay", "verb", "cab"],
  },
  {
    id: "05A",
    name: "JCM Clean",
    outputTrim: 1,
    category: "amp",
    model: "jcm",
    stageModels: { amp: "jcm", cab: "brit_412" },
    values: {
      // A master-volume Marshall's clean is preamp low / master up. The old
      // values (preamp 5, bass at 10) measured as crunch, not clean.
      amp_gain: 2,
      amp_bass: 5,
      amp_middle: 6,
      amp_treble: 6,
      amp_presence: 5,
      amp_master: 7,
      cab_mic_type: 0,
      cab_mic: 35,
      cab_dist: 20,
    },
    path: ["amp", "cab"],
  },
  {
    id: "05B",
    name: "JCM Crunch",
    outputTrim: -0.5,
    category: "amp",
    model: "jcm",
    stageModels: { amp: "jcm", cab: "brit_412" },
    values: {
      amp_gain: 5.5,
      amp_bass: 4.5,
      amp_middle: 6.5,
      amp_treble: 6.5,
      amp_presence: 5.5,
      amp_master: 5.5,
      cab_mic_type: 0,
      cab_mic: 42,
      cab_dist: 18,
    },
    path: ["amp", "cab"],
  },
  {
    id: "05C",
    name: "JCM Hot Rhythm",
    outputTrim: -3,
    category: "amp",
    model: "jcm",
    stageModels: { amp: "jcm", cab: "brit_412" },
    values: {
      // The hot end of the same amp — preamp up, tone stack still usable.
      // Bass at 10 with treble at 3 was a poster setting, not a Marshall one.
      amp_gain: 8,
      amp_bass: 4,
      amp_middle: 6.5,
      amp_treble: 6.5,
      amp_presence: 6,
      amp_master: 5.5,
      cab_mic_type: 1,
      cab_mic: 32,
      cab_dist: 24,
    },
    path: ["amp", "cab"],
  },
  {
    id: "06A",
    name: "Orange Crunch",
    outputTrim: -3,
    category: "amp",
    model: "mandarin",
    stageModels: { amp: "mandarin", cab: "vintage_212" },
    values: {
      amp_gain: 6,
      amp_bass: 5.5,
      amp_middle: 6.5,
      amp_treble: 5.5,
      amp_presence: 5,
      amp_master: 5.5,
      cab_mic_type: 1,
      cab_mic: 30,
      cab_dist: 28,
    },
    path: ["amp", "cab"],
  },
  {
    id: "06B",
    name: "Orange Fuzz",
    outputTrim: -5,
    category: "dist",
    model: "fuzz",
    stageModels: { dist: "fuzz", amp: "mandarin", verb: "room", cab: "vintage_212" },
    values: {
      drive_gain: 7.8,
      drive_tone: 3.8,
      drive_level: 5.5,
      // The fuzz is the distortion; the amp sits below its own breakup so the
      // two do not stack into mush.
      amp_gain: 4.5,
      amp_bass: 5,
      amp_middle: 6,
      amp_treble: 5,
      amp_presence: 4.5,
      amp_master: 5.5,
      reverb_decay: 1.4,
      reverb_mix: 10,
      cab_mic_type: 1,
      cab_mic: 24,
      cab_dist: 32,
    },
    path: ["dist", "amp", "verb", "cab"],
  },
  {
    id: "07A",
    name: "Recto Tight Rhythm",
    outputTrim: 1,
    category: "amp",
    model: "recto",
    stageModels: { dyn: "gate", dist: "screamer", amp: "recto", eq: "parametric", cab: "oversized_412" },
    values: {
      gate_thresh: -48,
      // Screamer in front of a Recto the way it is actually set: drive off,
      // level up. It tightens the low end rather than adding distortion.
      drive_gain: 1,
      drive_tone: 5.5,
      drive_level: 8.5,
      amp_gain: 7,
      amp_bass: 5,
      amp_middle: 3.5,
      amp_treble: 6,
      amp_presence: 6,
      amp_master: 5,
      eq_low_gain: -1.5,
      eq_mid1_freq: 300,
      eq_mid1_gain: -1,
      eq_mid2_freq: 1800,
      eq_mid2_gain: 1.5,
      eq_high_gain: 0,
      cab_mic_type: 0,
      cab_mic: 45,
      cab_dist: 14,
    },
    path: ["dyn", "dist", "amp", "eq", "cab"],
  },
  {
    id: "07B",
    name: "Recto Singing Lead",
    outputTrim: 2.5,
    category: "delay",
    model: "digital",
    stageModels: { dyn: "gate", dist: "screamer", amp: "recto", delay: "digital", verb: "plate", cab: "oversized_412" },
    values: {
      gate_thresh: -52,
      drive_gain: 1.5,
      drive_tone: 5.2,
      drive_level: 7.8,
      // Mids up against the rhythm preset — that is what makes a lead sing
      // through a mix instead of scooping out of it.
      amp_gain: 7.5,
      amp_bass: 4.8,
      amp_middle: 5,
      amp_treble: 5.8,
      amp_presence: 5.5,
      amp_master: 5,
      delay_time: 380,
      delay_fb: 28,
      delay_mix: 22,
      delay_tone: 5.5,
      reverb_decay: 3.5,
      reverb_mix: 16,
      cab_mic_type: 1,
      cab_mic: 38,
      cab_dist: 24,
    },
    path: ["dyn", "dist", "amp", "delay", "verb", "cab"],
  },
  {
    id: "08A",
    name: "Hot Rod Lead",
    outputTrim: 1,
    category: "amp",
    model: "slate",
    stageModels: { amp: "slate", delay: "analog", verb: "plate", cab: "slo_412" },
    values: {
      amp_gain: 6.5,
      amp_bass: 5,
      amp_middle: 6,
      amp_treble: 6,
      amp_presence: 6,
      amp_master: 5,
      delay_time: 340,
      delay_fb: 26,
      delay_mix: 20,
      delay_tone: 4.5,
      reverb_decay: 3.8,
      reverb_mix: 18,
      cab_mic_type: 1,
      cab_mic: 40,
      cab_dist: 26,
    },
    path: ["amp", "delay", "verb", "cab"],
  },
  {
    id: "08B",
    name: "80s Rack Lead",
    outputTrim: 5,
    category: "mod",
    model: "chorus",
    stageModels: { dyn: "gate", amp: "slate", mod: "chorus", delay: "dual", verb: "hall", cab: "slo_412" },
    values: {
      gate_thresh: -55,
      amp_gain: 6.8,
      amp_bass: 4.5,
      amp_middle: 5.8,
      amp_treble: 6.2,
      amp_presence: 6.2,
      amp_master: 5,
      // 1.4 Hz — the slow, wide rack chorus the era is built on.
      chorus_rate: 2.2,
      chorus_depth: 5,
      chorus_mix: 24,
      delay_time: 430,
      delay_fb: 30,
      delay_mix: 24,
      delay_tone: 6,
      reverb_decay: 5.5,
      reverb_mix: 22,
      cab_mic_type: 2,
      cab_mic: 45,
      cab_dist: 40,
    },
    path: ["dyn", "amp", "mod", "delay", "verb", "cab"],
  },
  {
    id: "09A",
    name: "Rat Clean Platform",
    outputTrim: -2.5,
    category: "dist",
    model: "rat",
    stageModels: { dist: "rat", amp: "twin", verb: "room", cab: "american_2x12" },
    values: {
      drive_gain: 5.8,
      drive_tone: 4.2,
      drive_level: 5.8,
      // The "platform" half of the name: the Twin stays under its own breakup
      // so everything heard here is the pedal.
      amp_gain: 2.5,
      amp_bass: 4.5,
      amp_middle: 5.5,
      amp_treble: 6,
      amp_presence: 5,
      amp_master: 7.5,
      reverb_decay: 1.5,
      reverb_mix: 12,
      cab_mic_type: 0,
      cab_mic: 34,
      cab_dist: 24,
    },
    path: ["dist", "amp", "verb", "cab"],
  },
  {
    id: "09B",
    name: "DS-1 Hard Rock",
    outputTrim: -5,
    category: "dist",
    model: "ds_one",
    stageModels: { dist: "ds_one", amp: "jcm", cab: "brit_412" },
    values: {
      drive_gain: 6.5,
      drive_tone: 4.8,
      drive_level: 6,
      amp_gain: 4.5,
      amp_bass: 5,
      amp_middle: 6,
      amp_treble: 6,
      amp_presence: 5,
      amp_master: 5.5,
      cab_mic_type: 0,
      cab_mic: 40,
      cab_dist: 18,
    },
    path: ["dist", "amp", "cab"],
  },
  {
    id: "10A",
    name: "Modern Tight",
    outputTrim: 1.5,
    category: "dist",
    model: "tight_rift",
    stageModels: { dyn: "gate", dist: "tight_rift", amp: "recto", eq: "parametric", cab: "uber_412" },
    values: {
      gate_thresh: -44,
      drive_gain: 6.2,
      drive_tone: 5.8,
      drive_level: 5.5,
      amp_gain: 5.5,
      amp_bass: 4.5,
      amp_middle: 4,
      amp_treble: 6,
      amp_presence: 6,
      amp_master: 4.5,
      eq_low_gain: -2,
      eq_mid1_freq: 250,
      eq_mid1_gain: -1.5,
      eq_mid2_freq: 1600,
      eq_mid2_gain: 2,
      eq_high_gain: 0.5,
      cab_mic_type: 0,
      cab_mic: 50,
      cab_dist: 12,
    },
    path: ["dyn", "dist", "amp", "eq", "cab"],
  },
  {
    id: "11A",
    name: "Bass Foundation",
    outputTrim: 0.5,
    category: "amp",
    model: "bassman",
    stageModels: { comp: "softknee", amp: "bassman", eq: "parametric", cab: "bass_cabinet" },
    values: {
      comp_thresh: -20,
      comp_ratio: 4,
      comp_attack: 28,
      comp_release: 160,
      comp_makeup: 4,
      amp_gain: 3.8,
      amp_bass: 6.5,
      amp_middle: 5,
      amp_treble: 4.2,
      amp_presence: 3.8,
      amp_master: 6,
      eq_low_gain: 1,
      eq_mid1_freq: 220,
      eq_mid1_gain: -1.5,
      eq_mid2_freq: 1200,
      eq_mid2_gain: 1,
      eq_high_gain: -1,
      cab_mic_type: 1,
      cab_mic: 45,
      cab_dist: 32,
    },
    path: ["comp", "amp", "eq", "cab"],
  },
  {
    id: "12A",
    name: "Phin Drive Echo",
    outputTrim: 1.5,
    category: "delay",
    model: "tape",
    stageModels: { dist: "super_drive", amp: "twin", eq: "parametric", delay: "tape", verb: "room", cab: "open_back" },
    values: {
      drive_gain: 4.5,
      drive_tone: 6,
      drive_level: 6.5,
      amp_gain: 3.5,
      amp_bass: 3.8,
      amp_middle: 6,
      amp_treble: 6.5,
      amp_presence: 5.5,
      amp_master: 7,
      eq_low_gain: -2,
      eq_mid1_freq: 500,
      eq_mid1_gain: 1.5,
      eq_mid2_freq: 2400,
      eq_mid2_gain: 2,
      eq_high_gain: 0.5,
      delay_time: 285,
      delay_fb: 32,
      delay_mix: 24,
      delay_tone: 4.5,
      reverb_decay: 1.8,
      reverb_mix: 14,
      cab_mic_type: 0,
      cab_mic: 48,
      cab_dist: 26,
    },
    path: ["dist", "amp", "eq", "delay", "verb", "cab"],
  },
  {
    id: "12B",
    name: "Molam Swirl",
    outputTrim: 10,
    category: "mod",
    model: "molam_swirl",
    stageModels: { dist: "breaker", amp: "twin", mod: "molam_swirl", delay: "analog", verb: "room", cab: "open_back" },
    values: {
      drive_gain: 3.2,
      drive_tone: 5.8,
      drive_level: 6.2,
      amp_gain: 3.2,
      amp_bass: 4,
      amp_middle: 5.8,
      amp_treble: 6.2,
      amp_presence: 5,
      amp_master: 7.5,
      // The phaser voices take Rate on a *cubic* law (`rate_hz_from_knob`),
      // scaled 0.55 for this voice: 6.8 is 0.70 Hz, a Uni-Vibe throb. The old
      // 2.2 was 0.05 Hz — one sweep every twenty seconds, i.e. standing still.
      chorus_rate: 6.8,
      chorus_depth: 7,
      chorus_mix: 62,
      delay_time: 330,
      delay_fb: 28,
      delay_mix: 20,
      delay_tone: 4,
      reverb_decay: 1.8,
      reverb_mix: 14,
      cab_mic_type: 1,
      cab_mic: 38,
      cab_dist: 36,
    },
    path: ["dist", "amp", "mod", "delay", "verb", "cab"],
  },
  {
    id: "12C",
    name: "Khaen Wide",
    outputTrim: 10.5,
    category: "mod",
    model: "khaen_swirl",
    stageModels: { amp: "twin", mod: "khaen_swirl", delay: "ping_pong", verb: "hall", cab: "open_back" },
    values: {
      amp_gain: 2.8,
      amp_bass: 4.2,
      amp_middle: 5.2,
      amp_treble: 5.8,
      amp_presence: 4.8,
      amp_master: 7.8,
      // Same cubic law, same 0.55 scale: 5.6 is 0.40 Hz — the slow, continuous
      // drift this voice is for. The old 1.6 was 0.04 Hz, effectively frozen.
      chorus_rate: 5.6,
      chorus_depth: 7.5,
      chorus_mix: 58,
      delay_time: 420,
      delay_fb: 34,
      delay_mix: 26,
      delay_tone: 4.5,
      reverb_decay: 4.8,
      reverb_mix: 22,
      cab_mic_type: 2,
      cab_mic: 42,
      cab_dist: 52,
    },
    path: ["amp", "mod", "delay", "verb", "cab"],
  },
  {
    id: "13A",
    name: "Singing Sustain",
    outputTrim: -1.5,
    category: "amp",
    model: "boutique",
    stageModels: { amp: "boutique", verb: "room", cab: "vintage_212" },
    values: {
      // Boutique territory: gain stays low enough that the amp's own touch
      // compression — not clipping — is doing the sustaining, mids pushed for
      // the singing upper-mid this voice leads with.
      amp_gain: 3.5,
      amp_bass: 5,
      amp_middle: 6.8,
      amp_treble: 5,
      amp_presence: 5.2,
      amp_master: 7.5,
      reverb_decay: 1.6,
      reverb_mix: 14,
      cab_mic_type: 0,
      cab_mic: 40,
      cab_dist: 26,
    },
    path: ["amp", "verb", "cab"],
  },
  {
    id: "13B",
    name: "Invader Chug",
    outputTrim: 4.5,
    category: "amp",
    model: "invader",
    stageModels: { dyn: "gate", amp: "invader", eq: "parametric", cab: "uber_412" },
    values: {
      gate_thresh: -50,
      amp_gain: 8.2,
      amp_bass: 4.5,
      amp_middle: 3,
      amp_treble: 6,
      amp_presence: 6.5,
      amp_master: 4.5,
      eq_low_gain: -1.5,
      eq_mid1_freq: 280,
      eq_mid1_gain: -1.5,
      eq_mid2_freq: 1700,
      eq_mid2_gain: 1.5,
      eq_high_gain: 0.5,
      cab_mic_type: 0,
      cab_mic: 48,
      cab_dist: 12,
    },
    path: ["dyn", "amp", "eq", "cab"],
  },
  {
    id: "13C",
    name: "Tweed Breakup",
    outputTrim: -4.0,
    category: "amp",
    model: "tweed_combo",
    stageModels: { amp: "tweed_combo", verb: "room", cab: "tweed_1x12" },
    values: {
      // A small combo's own power stage is the breakup here — gain sits at
      // the point where picking harder pushes it over, not maxed out.
      amp_gain: 6.5,
      amp_bass: 5.5,
      amp_middle: 5.5,
      amp_treble: 6,
      amp_presence: 4,
      amp_master: 6.5,
      reverb_decay: 1.3,
      reverb_mix: 11,
      cab_mic_type: 1,
      cab_mic: 30,
      cab_dist: 34,
    },
    path: ["amp", "verb", "cab"],
  },
];

const primaryCategories: Record<PrimaryCategoryId, Category> = {
  dyn: {
    name: "Gate",
    short: "Gate",
    color: "var(--c-dyn)",
    rgb: "91, 124, 250",
    node: "gate",
  },
  comp: {
    name: "Compressor",
    short: "Comp",
    color: "var(--c-comp)",
    rgb: "240, 200, 80",
    node: "comp",
  },
  wah: {
    name: "Wah",
    short: "Wah",
    color: "var(--c-wah)",
    rgb: "170, 200, 80",
    node: "wah",
  },
  dist: {
    name: "Distortion",
    short: "Dist",
    color: "var(--c-dist)",
    rgb: "232, 148, 42",
    node: "drive",
  },
  amp: {
    name: "Amp",
    short: "Amp",
    color: "var(--c-amp)",
    rgb: "232, 92, 92",
    node: "amp",
  },
  eq: {
    name: "Equalizer",
    short: "EQ",
    color: "var(--c-eq)",
    rgb: "120, 220, 200",
    node: "eq",
  },
  mod: {
    name: "Modulation",
    short: "Mod",
    color: "var(--c-mod)",
    rgb: "61, 184, 232",
    node: "mod",
  },
  delay: {
    name: "Delay",
    short: "Delay",
    color: "var(--c-delay)",
    rgb: "61, 214, 140",
    node: "delay",
  },
  verb: {
    name: "Reverb",
    short: "Verb",
    color: "var(--c-verb)",
    rgb: "168, 120, 240",
    node: "reverb",
  },
  cab: {
    name: "Cabinet",
    short: "Cab",
    color: "var(--c-cab)",
    rgb: "224, 112, 176",
    node: "cab",
  },
};

/**
 * A second instance carries its stage's colour and icon — it is the same kind
 * of gear — but says "B" in its name and label so the two blocks in the path
 * are never confused for one another. `node` is the DSP-side stage id, which
 * is what makes `drive2_on` and `drive2_gain` reach the right block.
 */
export const categories: Record<CategoryId, Category> = {
  ...primaryCategories,
  ...(Object.fromEntries(
    secondCategoryIds.map((cat) => {
      const base = primaryCategories[doubles[cat]];
      return [
        cat,
        {
          ...base,
          name: `${base.name} B`,
          short: `${base.short} B`,
          node: `${base.node}2`,
        } satisfies Category,
      ];
    }),
  ) as Record<SecondCategoryId, Category>),
};

const primaryModels: Record<PrimaryCategoryId, Model[]> = {
  dyn: [
    {
      id: "gate",
      name: "Noise Gate",
      short: "Gate",
      sub: "Dynamic threshold noise reduction",
    },
  ],
  comp: [
    {
      id: "softknee",
      name: "Studio Comp",
      short: "Comp",
      sub: "Stereo-linked soft-knee compressor",
    },
  ],
  eq: [
    {
      id: "parametric",
      name: "Studio EQ",
      short: "EQ",
      sub: "4-band parametric tone shaping",
    },
    {
      id: "vintage_eq",
      name: "Vintage EQ",
      short: "Vintage",
      sub: "Passive-console character, wide smooth bells",
    },
    {
      id: "modern_eq",
      name: "Modern EQ",
      short: "Modern",
      sub: "Surgical digital character, narrow precise bells",
    },
  ],
  dist: [
    {
      id: "screamer",
      name: "Green Screamer",
      short: "Screamer",
      sub: "Tube drive mid-boost pedal",
    },
    {
      id: "minotaur",
      name: "Minotaur Boost",
      short: "Minotaur",
      sub: "Buffered analog clean boost",
    },
    {
      id: "rat",
      name: "Rats Nest",
      short: "Rat",
      sub: "Hard-clipping filthy distortion",
    },
    {
      id: "breaker",
      name: "Breaker Blues",
      short: "Breaker",
      sub: "Soft low-gain overdrive",
    },
    {
      id: "fuzz",
      name: "Face Fuzz",
      short: "Fuzz",
      sub: "Gated asymmetric fuzz",
    },
    {
      id: "centurion",
      name: "Centurion OD",
      short: "Centurion",
      sub: "Transparent mid-forward overdrive",
    },
    {
      id: "ds_one",
      name: "DS Classic",
      short: "DS-1",
      sub: "Raw orange-box hard clipper",
    },
    {
      id: "super_drive",
      name: "Super Drive",
      short: "SuperDrv",
      sub: "Asymmetric smooth overdrive",
    },
    {
      id: "metal_core",
      name: "Metal Core",
      short: "Metal",
      sub: "Huge-gain scooped metal distortion",
    },
    {
      id: "tight_rift",
      name: "Tight Rift",
      short: "Rift",
      sub: "Modern tight high-gain, djent-ready",
    },
    {
      id: "amber_crunch",
      name: "Amber Crunch",
      short: "Amber",
      sub: "Bright silicon blues/rock rhythm crunch",
    },
    {
      id: "copper_fuzz",
      name: "Copper Fuzz",
      short: "Copper",
      sub: "Tight aggressive silicon fuzz",
    },
  ],
  amp: [
    {
      id: "mandarin",
      name: "Mandarin 80",
      short: "Mandarin",
      sub: "1980 vintage British Orange tube head",
    },
    {
      id: "plexi",
      name: "Brit Plexi 100",
      short: "Plexi",
      sub: "Super Lead 1959 plexiglass Marshall",
    },
    {
      id: "twin",
      name: "Twin Clean",
      short: "Twin",
      sub: "High-headroom American clean combo",
    },
    {
      id: "topboost",
      name: "Top Boost",
      short: "TopBoost",
      sub: "Chiming British class-A combo",
    },
    {
      id: "recto",
      name: "Recto Modern",
      short: "Recto",
      sub: "Tight modern high-gain rectifier",
    },
    {
      id: "jcm",
      name: "JCM Crunch",
      short: "JCM",
      sub: "Classic British stack crunch",
    },
    {
      id: "slate",
      name: "Lead Slate",
      short: "Slate",
      sub: "Hot-rodded saturated lead amp",
    },
    {
      id: "bassman",
      name: "Bassman",
      short: "Bassman",
      sub: "Loose American bass-heavy head",
    },
    {
      id: "boutique",
      name: "Overdrive Special",
      short: "Boutique",
      sub: "Smooth boutique low/mid-gain sustain",
    },
    {
      id: "invader",
      name: "Invader 5150",
      short: "Invader",
      sub: "Tight, scooped modern high-gain stack",
    },
    {
      id: "tweed_combo",
      name: "Tweed Deluxe",
      short: "Tweed",
      sub: "Small, early-breakup single-speaker combo",
    },
    {
      id: "nam_capture",
      name: "NAM A2 Capture",
      short: "NAM A2",
      sub: "Neural Amp Modeler A2 (.nam / TONE3000)",
    },
    {
      id: "bypass",
      name: "Bypass",
      short: "Bypass",
      sub: "Pass the Tone/Amp slot through unprocessed",
    },
  ],
  wah: [
    {
      id: "cry_wah",
      name: "Cry Wah",
      short: "Cry",
      sub: "Pedal-position resonant sweep",
    },
    {
      id: "touch_wah",
      name: "Touch Wah",
      short: "Touch",
      sub: "Envelope-following auto wah",
    },
  ],
  mod: [
    {
      id: "chorus",
      name: "70s Analog Chorus",
      short: "Chorus",
      sub: "Warm analog modulated chorus",
    },
    {
      id: "phaser",
      name: "Vibe Phase 90",
      short: "Phaser",
      sub: "Swept 4-stage analog phaser",
    },
    {
      id: "flanger",
      name: "Jet Flanger",
      short: "Flanger",
      sub: "Short-delay jet-sweep flanger",
    },
    {
      id: "tremolo",
      name: "Opto Tremolo",
      short: "Trem",
      sub: "Amp-style optical tremolo",
    },
    {
      id: "molam_swirl",
      name: "Molam Swirl",
      short: "Molam",
      sub: "Slow Uni-Vibe throb with vowels — Isan / luk-thung lead swirl",
    },
    {
      id: "phin_vibe",
      name: "Phin Vibe",
      short: "PhinVibe",
      sub: "Staggered stages, no feedback — a throb, not a sweep",
    },
    {
      id: "khaen_swirl",
      name: "Khaen Swirl",
      short: "Khaen",
      sub: "8-stage, four notches — lush and continuous",
    },
    {
      id: "bi_lam",
      name: "Bi-Lam",
      short: "BiLam",
      sub: "12-stage cascade, slow and very wide",
    },
    {
      id: "isan_jet",
      name: "Isan Jet",
      short: "IsanJet",
      sub: "6-stage, hard regeneration — lively jet, not a helicopter",
    },
    {
      id: "soft_phase",
      name: "Soft Phase",
      short: "SoftPh",
      sub: "2-stage, one gentle notch — the subtle end of the range",
    },
    {
      id: "wide_vibe",
      name: "Wide Vibe",
      short: "WideV",
      sub: "Real linked stereo spread — lush and wide, not mono",
    },
  ],
  delay: [
    { id: "tape", name: "Tape Echo", short: "Tape", sub: "Warm saturated tape delay" },
    {
      id: "digital",
      name: "Digital Delay",
      short: "Digital",
      sub: "Clean full-bandwidth repeats",
    },
    {
      id: "analog",
      name: "Analog BBD",
      short: "Analog",
      sub: "Dark thinning bucket-brigade repeats",
    },
    {
      id: "ping_pong",
      name: "Ping Pong",
      short: "Ping",
      sub: "Repeats alternating left and right",
    },
    {
      id: "dual",
      name: "Dual Delay",
      short: "Dual",
      sub: "Quarter note left, dotted eighth right",
    },
  ],
  verb: [
    {
      id: "plate",
      name: "Studio Plate",
      short: "Plate",
      sub: "Sustained metallic plate resonance",
    },
    {
      id: "room",
      name: "Tracking Room",
      short: "Room",
      sub: "Short, damped early reflections",
    },
    {
      id: "hall",
      name: "Concert Hall",
      short: "Hall",
      sub: "Long predelay and spacious tail",
    },
    {
      id: "shimmer",
      name: "Shimmer",
      short: "Shimmer",
      sub: "Octave-up voice ascending in the tail",
    },
  ],
  cab: [
    {
      id: "vintage_cab",
      name: "1960v Vintage 4x12",
      short: "4x12",
      sub: "Celestion vintage cabinet sim",
    },
    {
      id: "american_2x12",
      name: "American 2x12",
      short: "2x12",
      sub: "Bright, tight open-back combo",
    },
    {
      id: "tweed_1x12",
      name: "Tweed 1x12",
      short: "Tweed",
      sub: "Small, boxy single-speaker combo",
    },
    {
      id: "modern_412",
      name: "Modern 4x12",
      short: "Modern",
      sub: "Tight, scooped, extended highs",
    },
    {
      id: "open_back",
      name: "Open Back",
      short: "Open",
      sub: "Wide front/rear cancellation and open dynamics",
    },
    {
      id: "vintage_212",
      name: "Vintage 2x12",
      short: "Vint 2x12",
      sub: "Warm paired speakers with damped cone breakup",
    },
    {
      id: "oversized_412",
      name: "Oversized 4x12",
      short: "Oversized",
      sub: "Deep closed-box modes and tight upper mids",
    },
    {
      id: "bass_cabinet",
      name: "Bass Cabinet",
      short: "Bass",
      sub: "Extended low-end radiation with controlled breakup",
    },
    {
      id: "brit_412",
      name: "British Stack 4x12",
      short: "Brit",
      sub: "Mid-forward greenback-voiced closed 4x12",
    },
    {
      id: "uber_412",
      name: "Uberkab 4x12",
      short: "Uber",
      sub: "Tight, scooped German high-gain 4x12",
    },
    {
      id: "slo_412",
      name: "SLO Custom 4x12",
      short: "SLO",
      sub: "Smooth, singing high-gain 4x12",
    },
    {
      id: "ir",
      name: "Impulse Response",
      short: "IR",
      sub: "Convolution with a loaded .wav cabinet IR",
    },
    {
      id: "modern_212",
      name: "Modern 2x12",
      short: "Mod 2x12",
      sub: "Tight closed-back pair, scaled-down modern scoop",
    },
    {
      id: "american_1x12",
      name: "American 1x12 Combo",
      short: "Am 1x12",
      sub: "Small, bright, open single speaker",
    },
  ],
};

/**
 * Model id for the second instance of a stage.
 *
 * `parameterDefaults` and the editor's live `parameters` map are keyed by model
 * id, so the two instances of a stage must not share one — otherwise dialling
 * Drive B's Gain would move Drive A's. The bridge strips the suffix again
 * before the id reaches the DSP, which knows only `screamer`.
 */
export function secondInstanceModelId(modelId: string): string {
  return `${modelId}_2`;
}

/** Strip `secondInstanceModelId`. Safe to call on an A-side id. */
export function baseModelId(modelId: string): string {
  return modelId.endsWith("_2") ? modelId.slice(0, -2) : modelId;
}

export const models: Record<CategoryId, Model[]> = {
  ...primaryModels,
  ...(Object.fromEntries(
    secondCategoryIds.map((cat) => [
      cat,
      primaryModels[doubles[cat]].map((m) => ({
        ...m,
        id: secondInstanceModelId(m.id),
      })),
    ]),
  ) as Record<SecondCategoryId, Model[]>),
};

const primaryParameterDefaults: Record<string, Param[]> = {
  gate: [
    {
      id: "gate_thresh",
      name: "Threshold",
      min: -80,
      max: 0,
      val: -55,
      unit: "dB",
    },
  ],
  softknee: [
    { id: "comp_thresh", name: "Threshold", min: -60, max: 0, val: -24, unit: "dB" },
    { id: "comp_ratio", name: "Ratio", min: 1, max: 20, val: 2, unit: ":1" },
    { id: "comp_attack", name: "Attack", min: 0.1, max: 100, val: 10, unit: "ms" },
    { id: "comp_release", name: "Release", min: 10, max: 1000, val: 120, unit: "ms" },
    { id: "comp_makeup", name: "Makeup", min: 0, max: 24, val: 0, unit: "dB" },
  ],
  // All three EQ models share this exact param set — the model only changes
  // the fixed shelf corners and bell Q inside the DSP (`eq::EqProfile`), not
  // what these six knobs mean or where they default.
  parametric: [
    { id: "eq_low_gain", name: "Low", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_mid1_freq", name: "Mid1 Freq", min: 100, max: 1000, val: 400, unit: "Hz" },
    { id: "eq_mid1_gain", name: "Mid1", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_mid2_freq", name: "Mid2 Freq", min: 600, max: 6000, val: 2000, unit: "Hz" },
    { id: "eq_mid2_gain", name: "Mid2", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_high_gain", name: "High", min: -15, max: 15, val: 0, unit: "dB" },
  ],
  vintage_eq: [
    { id: "eq_low_gain", name: "Low", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_mid1_freq", name: "Mid1 Freq", min: 100, max: 1000, val: 400, unit: "Hz" },
    { id: "eq_mid1_gain", name: "Mid1", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_mid2_freq", name: "Mid2 Freq", min: 600, max: 6000, val: 2000, unit: "Hz" },
    { id: "eq_mid2_gain", name: "Mid2", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_high_gain", name: "High", min: -15, max: 15, val: 0, unit: "dB" },
  ],
  modern_eq: [
    { id: "eq_low_gain", name: "Low", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_mid1_freq", name: "Mid1 Freq", min: 100, max: 1000, val: 400, unit: "Hz" },
    { id: "eq_mid1_gain", name: "Mid1", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_mid2_freq", name: "Mid2 Freq", min: 600, max: 6000, val: 2000, unit: "Hz" },
    { id: "eq_mid2_gain", name: "Mid2", min: -15, max: 15, val: 0, unit: "dB" },
    { id: "eq_high_gain", name: "High", min: -15, max: 15, val: 0, unit: "dB" },
  ],
  screamer: [
    {
      id: "drive_gain",
      name: "Drive",
      min: 0,
      max: 10,
      val: 6.0,
      unit: "",
    },
    {
      id: "drive_tone",
      name: "Tone",
      min: 0,
      max: 10,
      val: 5.5,
      unit: "",
    },
    {
      id: "drive_level",
      name: "Level",
      min: 0,
      max: 10,
      val: 6.5,
      unit: "",
    },
  ],
  minotaur: [
    {
      id: "drive_gain",
      name: "Gain",
      min: 0,
      max: 10,
      val: 3.5,
      unit: "",
    },
    {
      id: "drive_tone",
      name: "Tone",
      min: 0,
      max: 10,
      val: 5.0,
      unit: "",
    },
    {
      id: "drive_level",
      name: "Output",
      min: 0,
      max: 10,
      val: 7.0,
      unit: "",
    },
  ],
  rat: [
    { id: "drive_gain", name: "Distortion", min: 0, max: 10, val: 7.5, unit: "" },
    { id: "drive_tone", name: "Filter", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "drive_level", name: "Volume", min: 0, max: 10, val: 6.0, unit: "" },
  ],
  breaker: [
    { id: "drive_gain", name: "Drive", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 6.5, unit: "" },
  ],
  fuzz: [
    { id: "drive_gain", name: "Fuzz", min: 0, max: 10, val: 8.5, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 3.5, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 5.5, unit: "" },
  ],
  centurion: [
    { id: "drive_gain", name: "Gain", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "drive_level", name: "Output", min: 0, max: 10, val: 6.5, unit: "" },
  ],
  ds_one: [
    { id: "drive_gain", name: "Dist", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 6.0, unit: "" },
  ],
  super_drive: [
    { id: "drive_gain", name: "Drive", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 6.0, unit: "" },
  ],
  metal_core: [
    { id: "drive_gain", name: "Dist", min: 0, max: 10, val: 7.5, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 5.5, unit: "" },
  ],
  tight_rift: [
    { id: "drive_gain", name: "Gain", min: 0, max: 10, val: 7.0, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 5.5, unit: "" },
  ],
  amber_crunch: [
    { id: "drive_gain", name: "Gain", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 6.5, unit: "" },
  ],
  copper_fuzz: [
    { id: "drive_gain", name: "Fuzz", min: 0, max: 10, val: 7.5, unit: "" },
    { id: "drive_tone", name: "Tone", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "drive_level", name: "Level", min: 0, max: 10, val: 5.5, unit: "" },
  ],
  mandarin: [
    {
      id: "amp_gain",
      name: "Drive",
      min: 0,
      max: 10,
      val: 5.0,
      unit: "",
    },
    {
      id: "amp_bass",
      name: "Bass",
      min: 0,
      max: 10,
      val: 5.0,
      unit: "",
    },
    {
      id: "amp_middle",
      name: "Mid",
      min: 0,
      max: 10,
      val: 5.5,
      unit: "",
    },
    {
      id: "amp_treble",
      name: "Treble",
      min: 0,
      max: 10,
      val: 5.0,
      unit: "",
    },
    {
      id: "amp_presence",
      name: "Presence",
      min: 0,
      max: 10,
      val: 5.0,
      unit: "",
    },
    {
      id: "amp_master",
      name: "Master",
      min: 0,
      max: 10,
      val: 5.0,
      unit: "",
    },
  ],
  plexi: [
    {
      id: "amp_gain",
      name: "Pre Gain",
      min: 0,
      max: 10,
      val: 7.5,
      unit: "",
    },
    {
      id: "amp_bass",
      name: "Bass",
      min: 0,
      max: 10,
      val: 4.0,
      unit: "",
    },
    {
      id: "amp_middle",
      name: "Middle",
      min: 0,
      max: 10,
      val: 6.2,
      unit: "",
    },
    {
      id: "amp_treble",
      name: "Treble",
      min: 0,
      max: 10,
      val: 6.5,
      unit: "",
    },
    {
      id: "amp_presence",
      name: "Presence",
      min: 0,
      max: 10,
      val: 6.0,
      unit: "",
    },
    {
      id: "amp_master",
      name: "Master",
      min: 0,
      max: 10,
      val: 5.0,
      unit: "",
    },
  ],
  twin: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 2.5, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 8.5, unit: "" },
  ],
  topboost: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 3.5, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 7.0, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 6.0, unit: "" },
  ],
  recto: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 7.5, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 3.0, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 6.5, unit: "" },
  ],
  jcm: [
    { id: "amp_gain", name: "Pre Gain", min: 0, max: 10, val: 7.0, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "amp_middle", name: "Middle", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 5.5, unit: "" },
  ],
  slate: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 6.5, unit: "" },
  ],
  bassman: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 7.0, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 4.0, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 6.0, unit: "" },
  ],
  boutique: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 3.5, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 6.5, unit: "" },
  ],
  invader: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 8.0, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 3.5, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 5.0, unit: "" },
  ],
  tweed_combo: [
    { id: "amp_gain", name: "Drive", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "amp_bass", name: "Bass", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_middle", name: "Mid", min: 0, max: 10, val: 5.5, unit: "" },
    { id: "amp_treble", name: "Treble", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "amp_presence", name: "Presence", min: 0, max: 10, val: 4.0, unit: "" },
    { id: "amp_master", name: "Master", min: 0, max: 10, val: 6.0, unit: "" },
  ],
  nam_capture: [
    { id: "nam_input_trim", name: "Input Trim", min: -24, max: 24, val: 0, unit: "dB" },
    { id: "nam_output_trim", name: "Output Trim", min: -24, max: 24, val: 0, unit: "dB" },
    { id: "nam_mix", name: "Mix", min: 0, max: 100, val: 100, unit: "%" },
    { id: "nam_slim_size", name: "Quality", min: 0, max: 100, val: 100, unit: "%" },
  ],
  bypass: [],
  ir: [],
  cry_wah: [
    { id: "wah_pos", name: "Position", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "wah_res", name: "Resonance", min: 0, max: 10, val: 5.0, unit: "" },
  ],
  touch_wah: [
    { id: "wah_pos", name: "Base Freq", min: 0, max: 10, val: 2.0, unit: "" },
    { id: "wah_res", name: "Resonance", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "wah_sens", name: "Sensitivity", min: 0, max: 10, val: 5.0, unit: "" },
  ],
  // Mix reaches its deepest notch at 100%. Rate defaults sit mid-slow: the DSP
  // curve is cubic and voice-scaled, so these knobs land in the pedal swirl
  // range rather than a jet.
  phaser: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 3.5, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 7.0, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 60, unit: "%" },
  ],
  molam_swirl: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 3.0, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 7.5, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 70, unit: "%" },
  ],
  phin_vibe: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 4.0, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 75, unit: "%" },
  ],
  khaen_swirl: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 2.5, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 7.5, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 65, unit: "%" },
  ],
  bi_lam: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 2.0, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 8.0, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 55, unit: "%" },
  ],
  isan_jet: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 7.0, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 6.5, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 65, unit: "%" },
  ],
  soft_phase: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 3.0, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 5.0, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 45, unit: "%" },
  ],
  wide_vibe: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 3.2, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 7.0, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 62, unit: "%" },
  ],
  flanger: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 2.5, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "chorus_mix", name: "Mix", min: 0, max: 100, val: 50, unit: "%" },
  ],
  tremolo: [
    { id: "chorus_rate", name: "Rate", min: 0, max: 10, val: 4.5, unit: "" },
    { id: "chorus_depth", name: "Depth", min: 0, max: 10, val: 6.0, unit: "" },
    { id: "chorus_mix", name: "Shape", min: 0, max: 100, val: 20, unit: "%" },
  ],
  chorus: [
    {
      id: "chorus_rate",
      name: "Rate",
      min: 0,
      max: 10,
      val: 4.0,
      unit: "",
    },
    {
      id: "chorus_depth",
      name: "Depth",
      min: 0,
      max: 10,
      val: 5.5,
      unit: "",
    },
    {
      id: "chorus_mix",
      name: "Mix",
      min: 0,
      max: 100,
      val: 40,
      unit: "%",
    },
  ],
  tape: [
    {
      id: "delay_time",
      name: "Time",
      min: 40,
      max: 1200,
      val: 420,
      unit: "ms",
    },
    {
      id: "delay_fb",
      name: "Feedback",
      min: 0,
      max: 100,
      val: 35,
      unit: "%",
    },
    {
      id: "delay_mix",
      name: "Mix",
      min: 0,
      max: 100,
      val: 30,
      unit: "%",
    },
    { id: "delay_tone", name: "Tone", min: 0, max: 10, val: 5, unit: "" },
  ],
  // The Delay slot's voicings share Time/Feedback/Mix/Tone (the voicing is the
  // model, as in the Mod and Reverb slots); Tone is centred on each voicing's
  // own feedback colour, so 5 means something different on each.
  digital: [
    { id: "delay_time", name: "Time", min: 40, max: 1200, val: 380, unit: "ms" },
    { id: "delay_fb", name: "Feedback", min: 0, max: 100, val: 30, unit: "%" },
    { id: "delay_mix", name: "Mix", min: 0, max: 100, val: 28, unit: "%" },
    { id: "delay_tone", name: "Tone", min: 0, max: 10, val: 5, unit: "" },
  ],
  analog: [
    { id: "delay_time", name: "Time", min: 40, max: 1200, val: 320, unit: "ms" },
    { id: "delay_fb", name: "Feedback", min: 0, max: 100, val: 45, unit: "%" },
    { id: "delay_mix", name: "Mix", min: 0, max: 100, val: 32, unit: "%" },
    { id: "delay_tone", name: "Tone", min: 0, max: 10, val: 5, unit: "" },
  ],
  ping_pong: [
    { id: "delay_time", name: "Time", min: 40, max: 1200, val: 300, unit: "ms" },
    { id: "delay_fb", name: "Feedback", min: 0, max: 100, val: 42, unit: "%" },
    { id: "delay_mix", name: "Mix", min: 0, max: 100, val: 35, unit: "%" },
    { id: "delay_tone", name: "Tone", min: 0, max: 10, val: 5, unit: "" },
  ],
  dual: [
    { id: "delay_time", name: "Time", min: 40, max: 1200, val: 500, unit: "ms" },
    { id: "delay_fb", name: "Feedback", min: 0, max: 100, val: 38, unit: "%" },
    { id: "delay_mix", name: "Mix", min: 0, max: 100, val: 33, unit: "%" },
    { id: "delay_tone", name: "Tone", min: 0, max: 10, val: 5, unit: "" },
  ],
  plate: [
    {
      id: "reverb_decay",
      name: "Decay",
      min: 0.5,
      max: 15,
      val: 8.5,
      unit: "s",
    },
    {
      id: "reverb_mix",
      name: "Mix",
      min: 0,
      max: 100,
      val: 55,
      unit: "%",
    },
  ],
  // Room/Hall share the Decay/Mix knobs (the voicing is the model, as in the
  // Mod slot) — distinct defaults suit each space. Shimmer adds a dedicated
  // octave-up feedback amount.
  room: [
    { id: "reverb_decay", name: "Decay", min: 0.5, max: 15, val: 1.8, unit: "s" },
    { id: "reverb_mix", name: "Mix", min: 0, max: 100, val: 30, unit: "%" },
  ],
  hall: [
    { id: "reverb_decay", name: "Decay", min: 0.5, max: 15, val: 11, unit: "s" },
    { id: "reverb_mix", name: "Mix", min: 0, max: 100, val: 45, unit: "%" },
  ],
  shimmer: [
    { id: "reverb_decay", name: "Decay", min: 0.5, max: 15, val: 12, unit: "s" },
    { id: "reverb_mix", name: "Mix", min: 0, max: 100, val: 40, unit: "%" },
    { id: "reverb_shimmer", name: "Shimmer", min: 0, max: 100, val: 62, unit: "%" },
  ],
  vintage_cab: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 0, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 20, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 40, unit: "%" },
  ],
  american_2x12: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 0, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 35, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 30, unit: "%" },
  ],
  tweed_1x12: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 1, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 15, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 55, unit: "%" },
  ],
  modern_412: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 0, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 45, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 20, unit: "%" },
  ],
  open_back: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 2, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 55, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 45, unit: "%" },
  ],
  vintage_212: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 1, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 40, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 30, unit: "%" },
  ],
  oversized_412: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 0, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 32, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 18, unit: "%" },
  ],
  bass_cabinet: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 1, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 60, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 35, unit: "%" },
  ],
  brit_412: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 0, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 38, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 25, unit: "%" },
  ],
  uber_412: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 0, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 48, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 18, unit: "%" },
  ],
  slo_412: [
    { id: "cab_mic_type", name: "Mic Type", min: 0, max: 2, val: 0, unit: "" },
    { id: "cab_mic", name: "Mic Pos", min: 0, max: 100, val: 42, unit: "%" },
    { id: "cab_dist", name: "Distance", min: 0, max: 100, val: 28, unit: "%" },
  ],
};

/**
 * The B blocks' knobs are the A blocks' knobs on the B ids, so the parameter
 * schema stays one table: adding a control to the Delay adds it to Delay B
 * too, and neither can drift.
 */
export const parameterDefaults: Record<string, Param[]> = {
  ...primaryParameterDefaults,
  ...Object.fromEntries(
    secondCategoryIds.flatMap((cat) =>
      models[cat].map((model) => [
        model.id,
        (primaryParameterDefaults[baseModelId(model.id)] ?? []).map((param) => ({
          ...param,
          id: secondInstanceParamId(param.id),
        })),
      ]),
    ),
  ),
};

/** Stage order as the rack lists it: each B block sits beside its A block. */
export const chainOrder: CategoryId[] = [
  "dyn",
  "comp",
  "comp2",
  "wah",
  "dist",
  "dist2",
  "amp",
  "eq",
  "eq2",
  "mod",
  "mod2",
  "delay",
  "delay2",
  "verb",
  "cab",
];

/** Number of DSP path slots (mirrors Rust `PATH_SLOTS`). */
export const PATH_SLOTS = 15;

/** Index used by DSP `path_slot_*` / `StageKind`. Append-only — these values
 * are the Rust `StageKind` discriminants (comp/eq appended as 7/8, wah as 9,
 * the second instances as 10-14). */
export const stageIndex: Record<CategoryId, number> = {
  dyn: 0,
  dist: 1,
  amp: 2,
  mod: 3,
  delay: 4,
  verb: 5,
  cab: 6,
  comp: 7,
  eq: 8,
  wah: 9,
  dist2: 10,
  mod2: 11,
  delay2: 12,
  eq2: 13,
  comp2: 14,
};

export const stageByIndex: CategoryId[] = [
  "dyn",
  "dist",
  "amp",
  "mod",
  "delay",
  "verb",
  "cab",
  "comp",
  "eq",
  "wah",
  "dist2",
  "mod2",
  "delay2",
  "eq2",
  "comp2",
];

/** Pack a path into the DSP slots (empty = -1). */
export function pathToSlotValues(path: CategoryId[]): number[] {
  const slots = Array.from({ length: PATH_SLOTS }, () => -1);
  path.forEach((cat, i) => {
    if (i < PATH_SLOTS) slots[i] = stageIndex[cat];
  });
  return slots;
}

/** Factory default path. The wah is never tonally neutral, so it starts in
 * the rack and joins the path only when the user places it — and a doubled
 * block is always a choice, never a default. */
export function defaultPath(): CategoryId[] {
  return chainOrder.filter((c) => c !== "wah" && !isSecondInstance(c));
}

export function emptyPath(): CategoryId[] {
  return [];
}

export function rackFromPath(path: CategoryId[]): CategoryId[] {
  const inPath = new Set(path);
  return chainOrder.filter((c) => !inPath.has(c));
}

export const icons: Record<string, string> = {
  gate: '<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
  drive:
    '<polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/>',
  amp: '<rect x="2" y="3" width="20" height="14" rx="2"/><line x1="2" y1="10" x2="22" y2="10"/><circle cx="6" cy="14" r="1"/><circle cx="10" cy="14" r="1"/>',
  comp: '<path d="M3 18c3 0 3-8 6-8s3 4 6 4 3-2 6-2"/><line x1="3" y1="6" x2="21" y2="6"/>',
  eq: '<line x1="6" y1="4" x2="6" y2="20"/><line x1="12" y1="4" x2="12" y2="20"/><line x1="18" y1="4" x2="18" y2="20"/><circle cx="6" cy="14" r="2"/><circle cx="12" cy="8" r="2"/><circle cx="18" cy="16" r="2"/>',
  mod: '<path d="M2 12s2-6 5-6 5 12 10 12 5-6 5-6"/>',
  delay: '<circle cx="12" cy="12" r="9"/><polyline points="12 7 12 12 16 14"/>',
  reverb: '<path d="M12 3v18M17 6v12M22 10v4M7 6v12M2 10v4"/>',
  cab: '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14c0 1.66 4 3 9 3s9-1.34 9-3V5"/>',
  wah: '<path d="M5 20 L9 4 L15 4 L19 20 Z"/><line x1="7" y1="15" x2="17" y2="15"/>',
};

// A second instance is the same kind of gear, so it wears the same icon; the
// "B" in its name and label is what tells the two blocks apart.
for (const cat of secondCategoryIds) {
  icons[categories[cat].node] = icons[categories[doubles[cat]].node] ?? "";
}

export function fmt(val: number, unit: string): string {
  if (unit === "" || unit === "s") return `${val.toFixed(1)}${unit}`;
  return `${Math.round(val)}${unit}`;
}

/** Canonical default for a parameter within a given model, if defined. */
export function defaultValueFor(
  modelId: string,
  paramId: string,
): number | undefined {
  return parameterDefaults[modelId]?.find((p) => p.id === paramId)?.val;
}

export function cloneParameters(): Record<string, Param[]> {
  const out: Record<string, Param[]> = {};
  for (const [modelId, params] of Object.entries(parameterDefaults)) {
    out[modelId] = params.map((p) => ({ ...p }));
  }
  return out;
}

/** Overlay saved values onto the current complete model schema.
 * Legacy presets may omit inactive models or controls added in newer builds. */
export function completeParameters(
  source: Record<string, Param[]>,
): Record<string, Param[]> {
  const out = cloneParameters();
  for (const [modelId, saved] of Object.entries(source)) {
    const current = out[modelId];
    if (!current) {
      out[modelId] = saved.map((param) => ({ ...param }));
      continue;
    }
    out[modelId] = current.map((fallback) => {
      const loaded = saved.find((param) => param.id === fallback.id);
      return loaded ? { ...fallback, val: loaded.val } : fallback;
    });
  }
  return out;
}

/** Complete per-stage model selection for a factory preset. */
export function stageModelsForPreset(
  preset: Preset,
): Record<CategoryId, string> {
  const stageModels = {} as Record<CategoryId, string>;
  for (const cat of chainOrder) {
    stageModels[cat] = models[cat][0]?.id ?? "";
  }
  Object.assign(stageModels, preset.stageModels);
  stageModels[preset.category] = preset.model;
  return stageModels;
}

/**
 * Build the complete parameter state for a factory preset. Presets contain
 * only their intentional overrides, so never layer one over the currently
 * edited state: that would leak controls from the previously selected preset.
 */
export function parametersForPreset(preset: Preset): Record<string, Param[]> {
  const parameters = cloneParameters();
  const selectedModels = new Set(Object.values(stageModelsForPreset(preset)));
  for (const modelId of selectedModels) {
    const modelParams = parameters[modelId];
    if (!modelParams) continue;
    parameters[modelId] = modelParams.map((param) => {
      const value = preset.values[param.id];
      return value === undefined ? param : { ...param, val: value };
    });
  }
  return parameters;
}
