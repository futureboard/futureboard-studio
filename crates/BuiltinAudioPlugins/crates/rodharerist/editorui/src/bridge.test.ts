// Pins the TS half of the cross-language param contract. The Rust half is
// `wire::tests::model_select_wire_values_match_editor_ids` in
// `rodharerist/src/wire.rs` — both must agree on the numeric model-select
// values and on the coalescing semantics native relies on.

import { beforeEach, describe, expect, test } from "bun:test";
import {
  AMP_MODEL_INDEX,
  CAB_MODEL_INDEX,
  DRIVE_MODEL_INDEX,
  DELAY_MODEL_INDEX,
  EQ_MODEL_INDEX,
  REVERB_MODEL_INDEX,
  TONE_ENGINE_INDEX,
  __flushParamEditsForTest,
  postClearClip,
  postEnabled,
  postLoadNamCapture,
  postModel,
  postParam,
  postPathOrder,
} from "./bridge";
import {
  clearActiveParamBinding,
  postTone3000LoadTone,
  postTone3000Search,
  postTone3000Status,
  setActiveParamBinding,
} from "./instanceBridge";
import { PATH_SLOTS } from "./data";

/** Capture `futureboard.setParams` POST bodies fired by a flush. */
function captureBatches(): { id: string; value: number }[][] {
  const batches: { id: string; value: number }[][] = [];
  globalThis.fetch = ((_url: unknown, init?: { body?: unknown }) => {
    const body = JSON.parse(String(init?.body ?? "{}"));
    if (body.type === "futureboard.setParams") batches.push(body.params);
    return Promise.resolve(new Response("{}"));
  }) as typeof fetch;
  return batches;
}

beforeEach(() => {
  // Drain anything a previous test queued, then bind a fresh instance.
  clearActiveParamBinding();
  __flushParamEditsForTest();
  setActiveParamBinding({
    pluginId: "rodharerist",
    instanceId: "track-1::insert-1",
    bindingGeneration: 1,
  });
});

describe("model-select wire values", () => {
  test("amp map mirrors AmpModel::ALL order", () => {
    expect(AMP_MODEL_INDEX).toEqual({
      mandarin: 0,
      plexi: 1,
      twin: 2,
      topboost: 3,
      recto: 4,
      jcm: 5,
      slate: 6,
      bassman: 7,
      boutique: 8,
      invader: 9,
      tweed_combo: 10,
    });
  });

  test("drive map mirrors DriveModel::ALL order", () => {
    expect(DRIVE_MODEL_INDEX).toEqual({
      screamer: 0,
      minotaur: 1,
      rat: 2,
      breaker: 3,
      fuzz: 4,
      centurion: 5,
      ds_one: 6,
      super_drive: 7,
      metal_core: 8,
      tight_rift: 9,
      amber_crunch: 10,
      copper_fuzz: 11,
    });
  });

  test("cab map mirrors CabModel::ALL order", () => {
    expect(CAB_MODEL_INDEX).toEqual({
      vintage_cab: 0,
      american_2x12: 1,
      tweed_1x12: 2,
      modern_412: 3,
      open_back: 4,
      vintage_212: 5,
      oversized_412: 6,
      bass_cabinet: 7,
      brit_412: 8,
      uber_412: 9,
      slo_412: 10,
      ir: 11,
      modern_212: 12,
      american_1x12: 13,
    });
  });

  test("reverb map mirrors ReverbModel::ALL order", () => {
    expect(REVERB_MODEL_INDEX).toEqual({
      plate: 0,
      room: 1,
      hall: 2,
      shimmer: 3,
    });
  });

  test("delay map mirrors DelayModel::ALL order", () => {
    expect(DELAY_MODEL_INDEX).toEqual({
      tape: 0,
      digital: 1,
      analog: 2,
      ping_pong: 3,
      dual: 4,
    });
  });

  test("eq map mirrors EqModel::ALL order", () => {
    expect(EQ_MODEL_INDEX).toEqual({
      parametric: 0,
      vintage_eq: 1,
      modern_eq: 2,
    });
  });

  test("postModel forwards an eq voicing as eq_model", () => {
    const batches = captureBatches();
    postModel("eq", "modern_eq");
    __flushParamEditsForTest();
    expect(batches).toEqual([[{ id: "eq_model", value: 2 }]]);
  });

  test("postModel forwards a delay voicing as delay_model", () => {
    const batches = captureBatches();
    postModel("delay", "ping_pong");
    __flushParamEditsForTest();
    expect(batches).toEqual([[{ id: "delay_model", value: 3 }]]);
  });

  test("postModel accepts the reverb node id used by the editor", () => {
    const batches = captureBatches();
    postModel("reverb", "room");
    __flushParamEditsForTest();
    postModel("reverb", "hall");
    __flushParamEditsForTest();
    postModel("reverb", "shimmer");
    __flushParamEditsForTest();
    expect(batches).toEqual([
      [{ id: "reverb_model", value: 1 }],
      [{ id: "reverb_model", value: 2 }],
      [{ id: "reverb_model", value: 3 }],
    ]);
  });

  test("tone engine indices match ToneEngineKind", () => {
    expect(TONE_ENGINE_INDEX).toEqual({ classic: 0, nam_capture: 1, bypass: 2 });
  });
});

describe("param edit coalescing", () => {
  test("timeout fallback flushes even when CEF animation frames stall", async () => {
    const batches = captureBatches();
    const originalRequestAnimationFrame = globalThis.requestAnimationFrame;
    const originalCancelAnimationFrame = globalThis.cancelAnimationFrame;
    globalThis.requestAnimationFrame = (() => 42) as typeof requestAnimationFrame;
    globalThis.cancelAnimationFrame = (() => {}) as typeof cancelAnimationFrame;
    try {
      postParam("drive_gain", 6.4);
      await new Promise((resolve) => setTimeout(resolve, 50));
      expect(batches).toEqual([[{ id: "drive_gain", value: 6.4 }]]);
    } finally {
      globalThis.requestAnimationFrame = originalRequestAnimationFrame;
      globalThis.cancelAnimationFrame = originalCancelAnimationFrame;
      __flushParamEditsForTest();
    }
  });

  test("rebinding cancels a stalled frame and lets the new instance schedule", async () => {
    const batches = captureBatches();
    const originalRequestAnimationFrame = globalThis.requestAnimationFrame;
    const originalCancelAnimationFrame = globalThis.cancelAnimationFrame;
    globalThis.requestAnimationFrame = (() => 42) as typeof requestAnimationFrame;
    globalThis.cancelAnimationFrame = (() => {}) as typeof cancelAnimationFrame;
    try {
      postParam("drive_gain", 3.3);
      setActiveParamBinding({
        pluginId: "rodharerist",
        instanceId: "track-2::insert-7",
        bindingGeneration: 2,
      });
      postParam("drive_gain", 8.8);
      await new Promise((resolve) => setTimeout(resolve, 50));
      expect(batches).toEqual([[{ id: "drive_gain", value: 8.8 }]]);
    } finally {
      globalThis.requestAnimationFrame = originalRequestAnimationFrame;
      globalThis.cancelAnimationFrame = originalCancelAnimationFrame;
      __flushParamEditsForTest();
    }
  });

  test("repeated edits to one id flush as a single last-value entry", () => {
    const batches = captureBatches();
    postParam("drive_gain", 1.0);
    postParam("drive_gain", 4.2);
    postParam("drive_gain", 9.9);
    __flushParamEditsForTest();
    expect(batches).toEqual([[{ id: "drive_gain", value: 9.9 }]]);
  });

  test("distinct ids keep insertion order in one batch", () => {
    const batches = captureBatches();
    postEnabled("amp", false);
    postModel("drive", "rat");
    postParam("delay_time", 500);
    __flushParamEditsForTest();
    expect(batches).toEqual([
      [
        { id: "amp_on", value: 0 },
        { id: "drive_model", value: 2 },
        { id: "delay_time", value: 500 },
      ],
    ]);
  });

  test("amp special engines ride tone_engine", () => {
    const batches = captureBatches();
    postModel("amp", "bypass");
    __flushParamEditsForTest();
    postModel("amp", "nam_capture");
    __flushParamEditsForTest();
    postModel("amp", "plexi");
    __flushParamEditsForTest();
    expect(batches).toEqual([
      [{ id: "tone_engine", value: 2 }],
      [{ id: "tone_engine", value: 1 }],
      [{ id: "amp_model", value: 1 }],
    ]);
  });

  // Every slot on every publish, including the ones past the end of the path:
  // this is what makes removing a block actually stop the DSP running it.
  test("path order publishes all slots with -1 for empty", () => {
    const batches = captureBatches();
    postPathOrder(["amp", "comp", "eq", "wah"]);
    __flushParamEditsForTest();
    expect(batches).toEqual([
      [
        { id: "path_slot_0", value: 2 },
        { id: "path_slot_1", value: 7 },
        { id: "path_slot_2", value: 8 },
        { id: "path_slot_3", value: 9 },
        ...Array.from({ length: PATH_SLOTS - 4 }, (_, i) => ({
          id: `path_slot_${i + 4}`,
          value: -1,
        })),
      ],
    ]);
  });

  // A doubled block travels on its own StageKind discriminant, so the DSP runs
  // the second instance rather than the first a second time.
  test("second instances take the path slots the DSP knows them by", () => {
    const batches = captureBatches();
    postPathOrder(["dist", "dist2", "amp", "delay", "delay2"]);
    __flushParamEditsForTest();
    expect(batches[0]?.slice(0, 5)).toEqual([
      { id: "path_slot_0", value: 1 },
      { id: "path_slot_1", value: 10 },
      { id: "path_slot_2", value: 2 },
      { id: "path_slot_3", value: 4 },
      { id: "path_slot_4", value: 12 },
    ]);
  });

  // The `_2` suffix is an editor-side key so the two blocks' knobs stay apart;
  // the DSP shares one model enum and must not see it.
  test("second-instance model selects strip the editor suffix", () => {
    const batches = captureBatches();
    postModel("drive2", "rat_2");
    __flushParamEditsForTest();
    postModel("mod2", "phaser_2");
    __flushParamEditsForTest();
    postModel("delay2", "ping_pong_2");
    __flushParamEditsForTest();
    expect(batches).toEqual([
      [{ id: "drive2_model", value: 2 }],
      [{ id: "mod2_model", value: 1 }],
      [{ id: "delay2_model", value: 3 }],
    ]);
  });

  test("mod and wah model selects ride their model params", () => {
    const batches = captureBatches();
    postModel("mod", "phaser");
    __flushParamEditsForTest();
    postModel("mod", "tremolo");
    __flushParamEditsForTest();
    postModel("wah", "touch_wah");
    __flushParamEditsForTest();
    expect(batches).toEqual([
      [{ id: "mod_model", value: 1 }],
      [{ id: "mod_model", value: 3 }],
      [{ id: "wah_model", value: 1 }],
    ]);
  });

  test("comp/eq enables and clear_clip ride the param wire", () => {
    const batches = captureBatches();
    postEnabled("comp", false);
    postEnabled("eq", true);
    postClearClip();
    __flushParamEditsForTest();
    expect(batches).toEqual([
      [
        { id: "comp_on", value: 0 },
        { id: "eq_on", value: 1 },
        { id: "clear_clip", value: 1 },
      ],
    ]);
  });

  test("loadNamCapture posts the bound instance and full file text", () => {
    const posts: Record<string, unknown>[] = [];
    globalThis.fetch = ((_url: unknown, init?: { body?: unknown }) => {
      posts.push(JSON.parse(String(init?.body ?? "{}")));
      return Promise.resolve(new Response("{}"));
    }) as typeof fetch;
    postLoadNamCapture('{"weights":[1]}', {
      name: "MyCapture",
      stereo: true,
      fullRig: false,
    });
    expect(posts).toEqual([
      {
        type: "futureboard.loadNamCapture",
        protocolVersion: 1,
        pluginId: "rodharerist",
        instanceId: "track-1::insert-1",
        bindingGeneration: 1,
        name: "MyCapture",
        json: '{"weights":[1]}',
        stereo: true,
        fullRig: false,
      },
    ]);
  });

  test("TONE3000 posts never carry a token, only search query and tone id", () => {
    const posts: Record<string, unknown>[] = [];
    globalThis.fetch = ((_url: unknown, init?: { body?: unknown }) => {
      posts.push(JSON.parse(String(init?.body ?? "{}")));
      return Promise.resolve(new Response("{}"));
    }) as typeof fetch;
    postTone3000Status();
    postTone3000Search("twin", 2);
    postTone3000LoadTone(42, { stereo: true, fullRig: false, size: "lite" });
    expect(posts).toEqual([
      {
        type: "futureboard.tone3000Status",
        protocolVersion: 1,
        pluginId: "rodharerist",
      },
      {
        type: "futureboard.tone3000Search",
        protocolVersion: 1,
        pluginId: "rodharerist",
        query: "twin",
        page: 2,
      },
      {
        type: "futureboard.tone3000LoadTone",
        protocolVersion: 1,
        pluginId: "rodharerist",
        instanceId: "track-1::insert-1",
        bindingGeneration: 1,
        toneId: 42,
        size: "lite",
        stereo: true,
        fullRig: false,
      },
    ]);
    for (const body of posts) {
      expect(JSON.stringify(body)).not.toMatch(/t3k_|apiKey|accessToken|oauth/i);
    }
  });

  test("rebinding drops edits queued under the old instance", () => {
    const batches = captureBatches();
    postParam("drive_gain", 3.3);
    setActiveParamBinding({
      pluginId: "rodharerist",
      instanceId: "track-2::insert-7",
      bindingGeneration: 2,
    });
    __flushParamEditsForTest();
    expect(batches).toEqual([]);
  });

  test("no binding means no POST at all", () => {
    const batches = captureBatches();
    clearActiveParamBinding();
    postParam("drive_gain", 3.3);
    __flushParamEditsForTest();
    expect(batches).toEqual([]);
  });
});
