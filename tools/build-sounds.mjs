// Turns the real mechanical switch recordings from tplai/kbsim (MIT) into the
// 48 kHz mono 16 bit WAVs that are compiled into the binary.
//
// The source takes are raw microphone recordings: they carry room noise before
// the strike and a long decaying tail after it. Both cost latency or bytes, so
// every take is trimmed sample accurately, normalised and faded here, once, at
// build time. Run:  node tools/build-sounds.mjs
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(ROOT, "src-tauri", "assets", "sounds");
const CACHE = join(ROOT, "tools", ".cache", "kbsim");
const SRC = join(CACHE, "src", "assets", "audio");
const TARBALL = "https://codeload.github.com/tplai/kbsim/tar.gz/refs/heads/master";

const RATE = 48_000;
/// Peak the press takes are scaled to (-1 dBFS).
const DOWN_PEAK = 0.891;
/// Releases are scaled lower: they are a much softer sound in real life, and
/// the hook already plays them at a reduced gain.
const UP_PEAK = 0.708;
const FADE_IN = 0.0004;
const FADE_OUT = 0.003;
/// A take starts here: -52 dBFS. Everything before is room noise.
const START_THRESHOLD = 0.0025;
/// The tail is kept down to here, plus a little room so it decays naturally.
const END_THRESHOLD = 0.0008;
const TAIL_KEEP = 0.004;

/// id = profile id in config.json and the asset directory name.
/// dir = the switch set inside kbsim. The famous boards of the hobby:
/// Cherry's MX trio, the tactile Holy Panda, Topre, NovelKeys Cream, Alpaca,
/// Gateron Inks, Kailh Box Navy, vintage Alps Blue and the buckling springs
/// of the IBM Model M.
const SETS = [
  { id: "mx-blue", dir: "mxblue" },
  { id: "mx-brown", dir: "mxbrown" },
  { id: "mx-black", dir: "mxblack" },
  { id: "holy-panda", dir: "holypanda" },
  { id: "topre", dir: "topre" },
  { id: "cream", dir: "cream" },
  { id: "alpaca", dir: "alpaca" },
  { id: "ink-black", dir: "blackink" },
  { id: "ink-red", dir: "redink" },
  { id: "box-navy", dir: "boxnavy" },
  { id: "alps-blue", dir: "bluealps" },
  { id: "model-m", dir: "buckling" },
];

/// Every slot is a real recording. Where a set has no dedicated caps for a key
/// the nearest row take stands in, which no ear can tell apart.
const SLOTS = [
  { out: "down_r0", files: ["press/GENERIC_R0.mp3"], peak: DOWN_PEAK },
  { out: "down_r1", files: ["press/GENERIC_R1.mp3"], peak: DOWN_PEAK },
  { out: "down_r2", files: ["press/GENERIC_R2.mp3"], peak: DOWN_PEAK },
  { out: "down_r3", files: ["press/GENERIC_R3.mp3"], peak: DOWN_PEAK },
  { out: "down_r4", files: ["press/GENERIC_R4.mp3"], peak: DOWN_PEAK },
  { out: "down_space", files: ["press/SPACE.mp3", "press/GENERIC_R4.mp3"], peak: DOWN_PEAK },
  { out: "down_enter", files: ["press/ENTER.mp3", "press/GENERIC_R0.mp3"], peak: DOWN_PEAK },
  { out: "down_backspace", files: ["press/BACKSPACE.mp3", "press/GENERIC_R1.mp3"], peak: DOWN_PEAK },
  { out: "up_generic", files: ["release/GENERIC.mp3"], peak: UP_PEAK },
  { out: "up_space", files: ["release/SPACE.mp3", "release/GENERIC.mp3"], peak: UP_PEAK },
  { out: "up_enter", files: ["release/ENTER.mp3", "release/GENERIC.mp3"], peak: UP_PEAK },
  { out: "up_backspace", files: ["release/BACKSPACE.mp3", "release/GENERIC.mp3"], peak: UP_PEAK },
];

function fetchSources() {
  if (existsSync(join(SRC, "mxbrown", "press", "GENERIC_R0.mp3"))) return;
  console.log("downloading kbsim (MIT) into tools/.cache ...");
  mkdirSync(CACHE, { recursive: true });
  const tar = join(CACHE, "kbsim.tar.gz");
  execFileSync("curl", ["-sL", "--fail", "-o", tar, TARBALL], { stdio: "inherit" });
  execFileSync("tar", ["-xzf", tar, "-C", CACHE, "--strip-components=1"], { stdio: "inherit" });
}

/// Decodes to mono f32 at the embed rate using ffmpeg, so no mp3 decoder has to
/// ship inside the app.
function decode(file) {
  const buf = execFileSync(
    "ffmpeg",
    ["-v", "error", "-i", file, "-f", "f32le", "-acodec", "pcm_f32le", "-ac", "1", "-ar", String(RATE), "-"],
    { maxBuffer: 1 << 28 },
  );
  return new Float32Array(buf.buffer, buf.byteOffset, buf.length / 4);
}

function trimNormalise(samples, peakTarget) {
  let start = -1;
  for (let i = 0; i < samples.length; i++) {
    if (Math.abs(samples[i]) >= START_THRESHOLD) {
      start = Math.max(0, i - 2);
      break;
    }
  }
  if (start < 0) return null;

  let end = samples.length - 1;
  while (end > start && Math.abs(samples[end]) < END_THRESHOLD) end--;
  end = Math.min(samples.length, end + Math.round(TAIL_KEEP * RATE));

  const seg = samples.subarray(start, end);
  let peak = 0;
  let peakAt = 0;
  for (let i = 0; i < seg.length; i++) {
    const a = Math.abs(seg[i]);
    if (a > peak) {
      peak = a;
      peakAt = i;
    }
  }
  let attack = 0;
  for (let i = 0; i < Math.min(seg.length, Math.round(0.001 * RATE)); i++) attack += seg[i] * seg[i];
  attack = Math.sqrt(attack / Math.max(1, Math.min(seg.length, Math.round(0.001 * RATE))));
  const gain = peakTarget / peak;

  const fi = Math.round(FADE_IN * RATE);
  const fo = Math.round(FADE_OUT * RATE);
  const out = new Float32Array(seg.length);
  for (let i = 0; i < seg.length; i++) {
    let f = 1;
    if (i < fi) f *= i / fi;
    const left = seg.length - 1 - i;
    if (left < fo) f *= left / fo;
    out[i] = seg[i] * gain * f;
  }
  return {
    samples: out,
    leadMs: (start / RATE) * 1000,
    peakMs: (peakAt / RATE) * 1000,
    peakDb: 20 * Math.log10(peak),
    gainDb: 20 * Math.log10(gain),
    attackDb: 20 * Math.log10(attack + 1e-9),
  };
}

function wav16(samples) {
  const head = Buffer.alloc(44);
  head.write("RIFF", 0);
  head.writeUInt32LE(36 + samples.length * 2, 4);
  head.write("WAVE", 8);
  head.write("fmt ", 12);
  head.writeUInt32LE(16, 16);
  head.writeUInt16LE(1, 20);
  head.writeUInt16LE(1, 22);
  head.writeUInt32LE(RATE, 24);
  head.writeUInt32LE(RATE * 2, 28);
  head.writeUInt16LE(2, 32);
  head.writeUInt16LE(16, 34);
  head.write("data", 36);
  head.writeUInt32LE(samples.length * 2, 40);
  const body = Buffer.alloc(samples.length * 2);
  for (let i = 0; i < samples.length; i++) {
    const v = Math.max(-1, Math.min(1, samples[i]));
    body.writeInt16LE(Math.round(v * 32767), i * 2);
  }
  return Buffer.concat([head, body]);
}

fetchSources();
let total = 0;
for (const set of SETS) {
  const dir = join(OUT, set.id);
  mkdirSync(dir, { recursive: true });
  for (const slot of SLOTS) {
    const file = slot.files.map((f) => join(SRC, set.dir, f)).find(existsSync);
    if (!file) throw new Error(`missing source ${set.dir}/${slot.files.join(" | ")}`);
    const res = trimNormalise(decode(file), slot.peak);
    if (!res) throw new Error(`silent take: ${file}`);
    const buf = wav16(res.samples);
    writeFileSync(join(dir, slot.out + ".wav"), buf);
    total += buf.length;
    const used = slot.files.length > 1 && !file.endsWith(slot.files[0]) ? " (row take)" : "";
    console.log(
      `${(set.id + "/" + slot.out).padEnd(26)} ${(res.samples.length / RATE * 1000).toFixed(1).padStart(6)} ms  ` +
        `lead ${res.leadMs.toFixed(2).padStart(6)}  peak@ ${res.peakMs.toFixed(2).padStart(6)} ms  ` +
        `attack ${res.attackDb.toFixed(1).padStart(6)} dB  gain ${res.gainDb >= 0 ? "+" : ""}${res.gainDb.toFixed(1).padStart(5)} dB  ` +
        `${(buf.length / 1024).toFixed(1).padStart(6)} kB${used}`,
    );
  }
}
console.log(`\n${SETS.length * SLOTS.length} files, ${(total / 1024).toFixed(0)} kB total -> ${OUT}`);
