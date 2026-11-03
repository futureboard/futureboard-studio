// Uploads an Android App Bundle to a Google Play track through the Google
// Play Developer API (androidpublisher v3), signed in as the service account
// named in `.env` (see `.env.example`). No dependencies: Node 18+ only.
//
// Usage (from anywhere):
//   node apps/mobile/solfege_voxtune/scripts/play-upload.mjs
//   node .../play-upload.mjs --aab path/to/app.aab --track alpha \
//        --status draft --notes path/to/release-notes.txt
//   node .../play-upload.mjs --check      # sign in and read the track only
//   node .../play-upload.mjs --track alpha --code 3   # put an already
//        uploaded versionCode on a track (Play takes each code only once)
//
// Defaults come from `.env`: PLAY_TRACK, PLAY_RELEASE_STATUS,
// PLAY_PACKAGE_NAME. The bundle defaults to the one `flutter build appbundle`
// writes. The release is named "<versionName> (<versionCode>)" from
// pubspec.yaml, so releases of the same version name stay apart in Play
// Console. Release notes come from `play/release-notes.txt` (the notes of
// the release being made) unless `--notes` names another file, in Play
// Console's own paste format:
//   <th>
//   ...
//   </th>
//   <en-US>
//   ...
//   </en-US>
//
// Every change goes into one edit that is committed at the end, so a failed
// step leaves Play Console untouched.

import { createSign } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const app = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const API = "https://androidpublisher.googleapis.com/androidpublisher/v3";
const UPLOAD_API =
  "https://androidpublisher.googleapis.com/upload/androidpublisher/v3";
const SCOPE = "https://www.googleapis.com/auth/androidpublisher";

function readEnv(file) {
  if (!existsSync(file)) return {};
  const env = {};
  for (const raw of readFileSync(file, "utf8").split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith("#") || !line.includes("=")) continue;
    const at = line.indexOf("=");
    env[line.slice(0, at).trim()] = line
      .slice(at + 1)
      .trim()
      .replace(/^"(.*)"$/, "$1");
  }
  return env;
}

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  if (i < 0) return undefined;
  const value = process.argv[i + 1];
  if (!value || value.startsWith("--")) throw new Error(`--${name} needs a value`);
  return value;
}

/** `<th>…</th><en-US>…</en-US>` → [{ language, text }]. */
function parseNotes(file) {
  const text = readFileSync(file, "utf8");
  const notes = [];
  for (const m of text.matchAll(/<([A-Za-z-]+)>\s*([\s\S]*?)\s*<\/\1>/g)) {
    if (m[2].length > 500) {
      throw new Error(`release notes for ${m[1]} are ${m[2].length} characters; Play allows 500`);
    }
    notes.push({ language: m[1], text: m[2] });
  }
  if (!notes.length) throw new Error(`no <lang>…</lang> blocks in ${file}`);
  return notes;
}

/** An OAuth access token for the service account (JWT bearer grant). */
async function accessToken(keyFile) {
  const key = JSON.parse(readFileSync(keyFile, "utf8"));
  const now = Math.floor(Date.now() / 1000);
  const b64 = (o) => Buffer.from(JSON.stringify(o)).toString("base64url");
  const unsigned = `${b64({ alg: "RS256", typ: "JWT" })}.${b64({
    iss: key.client_email,
    scope: SCOPE,
    aud: key.token_uri,
    iat: now,
    exp: now + 3600,
  })}`;
  const signature = createSign("RSA-SHA256")
    .update(unsigned)
    .sign(key.private_key, "base64url");
  const res = await fetch(key.token_uri, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer",
      assertion: `${unsigned}.${signature}`,
    }),
  });
  const body = await res.json();
  if (!res.ok) {
    throw new Error(`sign-in as ${key.client_email} failed: ${body.error_description ?? body.error}`);
  }
  return { token: body.access_token, email: key.client_email };
}

async function call(token, method, url, body, contentType = "application/json") {
  const res = await fetch(url, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      ...(body !== undefined && { "Content-Type": contentType }),
    },
    body:
      body === undefined || contentType !== "application/json"
        ? body
        : JSON.stringify(body),
  });
  const text = await res.text();
  const json = text ? JSON.parse(text) : {};
  if (!res.ok) {
    throw new Error(`${method} ${url.replace(/\?.*/, "")}: ${res.status} ${json.error?.message ?? text}`);
  }
  return json;
}

async function main() {
  const env = { ...readEnv(path.join(app, ".env")), ...process.env };
  const pkg = env.PLAY_PACKAGE_NAME || "studio.futureboard.voxtune";
  const track = arg("track") ?? env.PLAY_TRACK ?? "internal";
  const status = arg("status") ?? env.PLAY_RELEASE_STATUS ?? "draft";
  const checkOnly = process.argv.includes("--check");
  const keyFile = env.PLAY_SERVICE_ACCOUNT_JSON;
  if (!keyFile) {
    throw new Error("PLAY_SERVICE_ACCOUNT_JSON is not set in .env (see .env.example)");
  }
  if (!existsSync(keyFile)) throw new Error(`service account key not found: ${keyFile}`);
  if (!["draft", "completed", "inProgress", "halted"].includes(status)) {
    throw new Error(`unknown release status "${status}"`);
  }
  // A bypassed build skips the purchase check: fine for testing tracks,
  // never for the store.
  if (track === "production" && String(env.PLAY_LICENSE_BYPASS).toLowerCase() === "true") {
    throw new Error(
      "PLAY_LICENSE_BYPASS=true builds skip the purchase check and cannot go to production; set it to false in .env and rebuild",
    );
  }

  const aab = path.resolve(
    arg("aab") ?? path.join(app, "build/app/outputs/bundle/release/app-release.aab"),
  );
  const defaultNotes = path.join(app, "play/release-notes.txt");
  const notesFile = arg("notes") ?? (existsSync(defaultNotes) ? defaultNotes : undefined);
  const notes = notesFile ? parseNotes(notesFile) : undefined;
  const versionName = readFileSync(path.join(app, "pubspec.yaml"), "utf8")
    .match(/^version:\s*([^+\s]+)/m)?.[1];
  const existingCode = arg("code");
  if (existingCode !== undefined && !/^\d+$/.test(existingCode)) {
    throw new Error(`--code must be a versionCode, got "${existingCode}"`);
  }
  if (!checkOnly && existingCode === undefined && !existsSync(aab)) {
    throw new Error(`bundle not found: ${aab}`);
  }

  const { token, email } = await accessToken(keyFile);
  console.log(`signed in as ${email}`);
  const appUrl = `${API}/applications/${pkg}`;
  const edit = await call(token, "POST", `${appUrl}/edits`, {});
  const editUrl = `${appUrl}/edits/${edit.id}`;

  if (checkOnly) {
    const current = await call(token, "GET", `${editUrl}/tracks/${track}`);
    console.log(`${pkg} · ${track}:`);
    for (const r of current.releases ?? []) {
      const langs = (r.releaseNotes ?? []).map((n) => n.language).join(", ") || "none";
      console.log(`  ${r.name ?? "(unnamed)"} · ${r.status} · codes ${(r.versionCodes ?? []).join(", ")} · notes ${langs}`);
    }
    await call(token, "DELETE", editUrl);
    return;
  }

  let bundle;
  if (existingCode !== undefined) {
    bundle = { versionCode: Number(existingCode) };
    console.log(`using uploaded versionCode ${existingCode}`);
  } else {
    console.log(`uploading ${path.basename(aab)} (${(readFileSync(aab).length / 1048576).toFixed(1)} MB)...`);
    bundle = await call(
      token,
      "POST",
      `${UPLOAD_API}/applications/${pkg}/edits/${edit.id}/bundles?uploadType=media`,
      readFileSync(aab),
      "application/octet-stream",
    );
    console.log(`uploaded versionCode ${bundle.versionCode}`);
  }

  await call(token, "PUT", `${editUrl}/tracks/${track}`, {
    track,
    releases: [
      {
        ...(versionName && { name: `${versionName} (${bundle.versionCode})` }),
        versionCodes: [String(bundle.versionCode)],
        status,
        ...(notes && { releaseNotes: notes }),
      },
    ],
  });
  await call(token, "POST", `${editUrl}:commit`);
  console.log(`committed: ${pkg} versionCode ${bundle.versionCode} → ${track} (${status})`);
}

main().catch((err) => {
  console.error(`play-upload: ${err.message}`);
  process.exit(1);
});
