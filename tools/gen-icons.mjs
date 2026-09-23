// Builds the Tauri icon set from tools/icon-master.png (1024x1024 RGBA master).
// ffmpeg decodes the master to raw pixels, the encoders below write PNG and ICO
// by hand, so this needs no image dependencies. Run: node tools/gen-icons.mjs
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { deflateSync } from "node:zlib";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const MASTER_PNG = join(ROOT, "tools", "icon-master.png");
const OUT = join(ROOT, "src-tauri", "icons");
mkdirSync(OUT, { recursive: true });

export const MASTER = 1024;

// ---------- master ----------
export function decodeMaster() {
  const buf = execFileSync(
    "ffmpeg",
    [
      "-v", "error",
      "-i", MASTER_PNG,
      "-vf", `crop='min(iw,ih)':'min(iw,ih)',scale=${MASTER}:${MASTER}:flags=lanczos`,
      "-f", "rawvideo", "-pix_fmt", "rgba", "-",
    ],
    { maxBuffer: 1 << 28 },
  );
  const expected = MASTER * MASTER * 4;
  if (buf.length !== expected) {
    throw new Error(`master decoded to ${buf.length} bytes, expected ${expected}`);
  }
  return buf;
}

// ---------- knock the white field out ----------
// A picture generator hands back a flat RGB image: the rounded plate sits on a
// white field, and a white field in a taskbar is a white square. Everything
// reachable from the border without crossing a dark pixel is field, so the fill
// walks inwards and turns the field transparent. The same walk gives the
// anti-aliased fringe its alpha, unblended from the white it was laid on, so
// the plate keeps a crisp edge instead of a halo.
const PLATE_MIN = 48;

export function unmatteWhite(px) {
  const N = MASTER * MASTER;
  const seen = new Uint8Array(N);
  const queue = new Int32Array(N);
  let head = 0, tail = 0;
  const offer = (i) => {
    if (seen[i] || px[i * 4 + 3] === 0) return;
    // A pixel as dark as the plate is the plate: it stops the walk.
    const m = Math.min(px[i * 4], px[i * 4 + 1], px[i * 4 + 2]);
    if (m <= PLATE_MIN) return;
    seen[i] = 1;
    queue[tail++] = i;
  };
  for (let k = 0; k < MASTER; k++) {
    offer(k);
    offer(N - MASTER + k);
    offer(k * MASTER);
    offer(k * MASTER + MASTER - 1);
  }
  while (head < tail) {
    const i = queue[head++];
    const x = i % MASTER;
    if (x > 0) offer(i - 1);
    if (x < MASTER - 1) offer(i + 1);
    if (i >= MASTER) offer(i - MASTER);
    if (i < N - MASTER) offer(i + MASTER);
  }
  for (let i = 0; i < N; i++) {
    const o = i * 4;
    if (!seen[i]) {
      px[o + 3] = 255;
      continue;
    }
    const m = Math.min(px[o], px[o + 1], px[o + 2]);
    const a = Math.min(1, Math.max(0, (255 - m) / (255 - PLATE_MIN)));
    px[o + 3] = Math.round(a * 255);
    if (a < 0.02) continue;
    // What the colour would have been with the white taken back out.
    for (let c = 0; c < 3; c++) {
      px[o + c] = Math.max(0, Math.min(255, Math.round((px[o + c] - 255 * (1 - a)) / a)));
    }
  }
}

// ---------- resize (box filter from master) ----------
// `keep` is the fraction of the master that survives, taken from the middle.
export function downscale(master, out, keep = 1) {
  const dst = new Uint8Array(out * out * 4);
  const span = MASTER * keep;
  const inset = (MASTER - span) / 2;
  const ratio = span / out;
  for (let y = 0; y < out; y++) {
    const y0 = Math.floor(inset + y * ratio), y1 = Math.max(y0 + 1, Math.floor(inset + (y + 1) * ratio));
    for (let x = 0; x < out; x++) {
      const x0 = Math.floor(inset + x * ratio), x1 = Math.max(x0 + 1, Math.floor(inset + (x + 1) * ratio));
      let r = 0, g = 0, b = 0, a = 0, n = 0;
      for (let sy = y0; sy < y1; sy++) {
        for (let sx = x0; sx < x1; sx++) {
          const o = (sy * MASTER + sx) * 4;
          // premultiply to avoid halos
          const w = master[o + 3] / 255;
          r += master[o] * w; g += master[o + 1] * w; b += master[o + 2] * w; a += master[o + 3];
          n++;
        }
      }
      const wSum = a / 255;
      const o = (y * out + x) * 4;
      dst[o] = Math.round(wSum > 0 ? r / wSum : 0);
      dst[o + 1] = Math.round(wSum > 0 ? g / wSum : 0);
      dst[o + 2] = Math.round(wSum > 0 ? b / wSum : 0);
      dst[o + 3] = Math.round(a / n);
    }
  }
  return dst;
}

// ---------- small-size legibility pass ----------
// The master is a soft drawing: gradients everywhere, and an amber hairline a
// couple of pixels wide at 1024. Averaged down to 16 px the cap and the plate run
// into one another and the accent turns grey, which is why this file used to
// hand-draw a different, flatter picture for the tray — and why the tray icon
// never matched the one in the window. So: keep the master's picture, and work
// harder as it shrinks.
//
// The blur runs on premultiplied pixels, so the transparent field cannot drag
// colour into the plate edge, and taps that fall off-canvas are dropped instead
// of counted, so that edge does not pick up a dark fringe from the empty field.
export function unsharp(src, size, amount) {
  const n = size * size * 4;
  const P = new Float32Array(n); // premultiplied original
  for (let i = 0; i < n; i += 4) {
    const a = src[i + 3] / 255;
    P[i] = src[i] * a;
    P[i + 1] = src[i + 1] * a;
    P[i + 2] = src[i + 2] * a;
    P[i + 3] = a;
  }
  const B = new Float32Array(n); // blurred
  const T = new Float32Array(n);
  const radius = 1; // 3-tap box: the widest blur that still leaves an edge here
  const pass = (input, output, alongX) => {
    for (let y = 0; y < size; y++) {
      for (let x = 0; x < size; x++) {
        for (let c = 0; c < 4; c++) {
          let sum = 0, taps = 0;
          for (let k = -radius; k <= radius; k++) {
            const u = alongX ? x + k : y + k;
            if (u < 0 || u >= size) continue;
            sum += input[(alongX ? y * size + u : u * size + x) * 4 + c];
            taps++;
          }
          output[(y * size + x) * 4 + c] = sum / taps;
        }
      }
    }
  };
  pass(P, T, true);
  pass(T, B, false);
  const dst = new Uint8Array(n);
  for (let i = 0; i < n; i += 4) {
    const a = P[i + 3];
    dst[i + 3] = Math.round(a * 255);
    if (a === 0) continue;
    for (let c = 0; c < 3; c++) {
      const sharp = P[i + c] + amount * (P[i + c] - B[i + c]);
      dst[i + c] = Math.round(Math.max(0, Math.min(255, sharp / a)));
    }
  }
  return dst;
}

/// Pushes the mid-tones apart around `pivot` so the cap separates from the plate,
/// and lifts the darkest values by `floor` so the plate keeps a silhouette on a
/// dark taskbar — the master's plate fades to near black at the bottom, which is
/// invisible against Windows' own chrome.
export function tone(src, size, pivot, gain, floor) {
  const dst = new Uint8Array(src.length);
  const p = pivot * 255;
  for (let i = 0; i < size * size; i++) {
    const o = i * 4;
    dst[o + 3] = src[o + 3];
    for (let c = 0; c < 3; c++) {
      const v = Math.max(0, Math.min(255, p + (src[o + c] - p) * gain));
      dst[o + c] = Math.round(v + floor * (1 - v / 255));
    }
  }
  return dst;
}

/// Separable Lanczos-3 over premultiplied RGBA, for the last 4:1 of a small
/// frame. Averaging the master straight down to 16 px is what made it soft, so
/// the box filter now stops at a four-times oversample and this finishes the
/// job with a filter that keeps an edge where a box can only smear it.
export function resample(src, from, to) {
  const f = from / to;
  const radius = Math.ceil(3 * f);
  const w = (x) => {
    if (x === 0) return 1;
    const t = Math.abs(x) / f;
    if (t >= 3) return 0;
    const p = Math.PI * t;
    return (3 * Math.sin(p) * Math.sin(p / 3)) / (p * p);
  };
  const pre = new Float32Array(from * from * 4);
  for (let i = 0; i < from * from; i++) {
    const a = src[i * 4 + 3] / 255;
    pre[i * 4] = src[i * 4] * a;
    pre[i * 4 + 1] = src[i * 4 + 1] * a;
    pre[i * 4 + 2] = src[i * 4 + 2] * a;
    pre[i * 4 + 3] = a;
  }
  const taps = (centre, count) => {
    const list = [];
    for (let k = -radius; k <= radius; k++) {
      const u = Math.round(centre) + k;
      if (u < 0 || u >= count) continue;
      list.push([u, w(u - centre)]);
    }
    return list;
  };
  // Horizontal, then vertical: two one-dimensional walks over the same kernel.
  const tmp = new Float32Array(to * from * 4);
  for (let y = 0; y < from; y++) {
    for (let x = 0; x < to; x++) {
      const list = taps((x + 0.5) * f - 0.5, from);
      const sum = list.reduce((a, [, ww]) => a + ww, 0) || 1;
      const o = (y * to + x) * 4;
      for (let ch = 0; ch < 4; ch++) tmp[o + ch] = 0;
      for (const [u, ww] of list) {
        const s = (y * from + u) * 4;
        for (let ch = 0; ch < 4; ch++) tmp[o + ch] += pre[s + ch] * ww / sum;
      }
    }
  }
  const acc = new Float32Array(to * to * 4);
  for (let y = 0; y < to; y++) {
    for (let x = 0; x < to; x++) {
      const list = taps((y + 0.5) * f - 0.5, from);
      const sum = list.reduce((a, [, ww]) => a + ww, 0) || 1;
      const o = (y * to + x) * 4;
      for (let ch = 0; ch < 4; ch++) acc[o + ch] = 0;
      for (const [v, ww] of list) {
        const s = (v * to + x) * 4;
        for (let ch = 0; ch < 4; ch++) acc[o + ch] += tmp[s + ch] * ww / sum;
      }
    }
  }
  const dst = new Uint8Array(to * to * 4);
  for (let i = 0; i < to * to; i++) {
    const a = acc[i * 4 + 3];
    dst[i * 4 + 3] = Math.round(Math.max(0, Math.min(1, a)) * 255);
    if (a <= 0) continue;
    for (let c = 0; c < 3; c++) {
      dst[i * 4 + c] = Math.round(Math.max(0, Math.min(255, acc[i * 4 + c] / a)));
    }
  }
  return dst;
}

/// How each frame is coaxed out of the master. Below 64 px it is the same
/// picture, just cropped in, sharpened and squeezed until it reads at tray size.
export const TWEAK = {
  16: { keep: 0.86, unsharp: 0.75, pivot: 0.33, gain: 1.18, floor: 46 },
  20: { keep: 0.88, unsharp: 0.65, pivot: 0.33, gain: 1.14, floor: 40 },
  24: { keep: 0.9, unsharp: 0.55, pivot: 0.35, gain: 1.1, floor: 32 },
  32: { keep: 0.94, unsharp: 0.45, pivot: 0.36, gain: 1.06, floor: 20 },
  48: { keep: 0.97, unsharp: 0.3, pivot: 0.38, gain: 1.02, floor: 8 },
};

export function frameFor(master, size) {
  const t = TWEAK[size];
  if (!t) return downscale(master, size);
  const shrunk = resample(downscale(master, size * 4, t.keep), size * 4, size);
  return tone(unsharp(shrunk, size, t.unsharp), size, t.pivot, t.gain, t.floor);
}

// ---------- PNG encoder ----------
const CRC_TABLE = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

function crc32(buf) {
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

export function pngEncode(rgba, size) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8;   // bit depth
  ihdr[9] = 6;   // RGBA
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    const ro = y * (size * 4 + 1);
    raw[ro] = 0; // filter: none
    Buffer.from(rgba.buffer, rgba.byteOffset + y * size * 4, size * 4).copy(raw, ro + 1);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---------- ICO encoder (BMP entries, 32bpp + AND mask) ----------
function bmpEntry(rgba, size) {
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(size, 4);
  header.writeInt32LE(size * 2, 8); // height doubled: XOR + AND
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);
  header.writeUInt32LE(0, 16);
  header.writeUInt32LE(size * size * 4, 20);

  const xor = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    const srcY = size - 1 - y; // bottom-up
    for (let x = 0; x < size; x++) {
      const so = (srcY * size + x) * 4;
      const dof = (y * size + x) * 4;
      xor[dof] = rgba[so + 2];
      xor[dof + 1] = rgba[so + 1];
      xor[dof + 2] = rgba[so];
      xor[dof + 3] = rgba[so + 3];
    }
  }

  const rowBytes = Math.ceil(size / 32) * 4;
  const and = Buffer.alloc(rowBytes * size);
  for (let y = 0; y < size; y++) {
    const srcY = size - 1 - y;
    for (let x = 0; x < size; x++) {
      const a = rgba[(srcY * size + x) * 4 + 3];
      if (a === 0) and[y * rowBytes + (x >> 3)] |= 0x80 >> (x & 7);
    }
  }
  return Buffer.concat([header, xor, and]);
}

export function icoEncode(sizes, images) {
  const entries = [];
  const blobs = [];
  let offset = 6 + sizes.length * 16;
  for (const s of sizes) {
    const b = bmpEntry(images[s], s);
    const e = Buffer.alloc(16);
    e[0] = s >= 256 ? 0 : s;
    e[1] = s >= 256 ? 0 : s;
    e[2] = 0; e[3] = 0;
    e.writeUInt16LE(1, 4);
    e.writeUInt16LE(32, 6);
    e.writeUInt32LE(b.length, 8);
    e.writeUInt32LE(offset, 12);
    entries.push(e);
    blobs.push(b);
    offset += b.length;
  }
  const head = Buffer.alloc(6);
  head.writeUInt16LE(0, 0);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(sizes.length, 4);
  return Buffer.concat([head, ...entries, ...blobs]);
}

// ---------- run ----------
export const pngSizes = [16, 20, 24, 32, 48, 64, 128, 256];
export const traySizes = [16, 20, 24, 32, 48, 64];

function main() {
console.log("decoding %s ...", MASTER_PNG);
const master = decodeMaster();
unmatteWhite(master);

const images = {};
for (const s of pngSizes) images[s] = frameFor(master, s);

// The tray wants an image that is already the pixel size Windows will draw it
// at, so the frames below are handed over as raw RGBA and the app picks one by
// scale factor. `Image::new` borrows them, so `include_bytes!` costs no copy.

const files = [
  ["16x16.png", pngEncode(images[16], 16)],
  ["20x20.png", pngEncode(images[20], 20)],
  ["24x24.png", pngEncode(images[24], 24)],
  ["32x32.png", pngEncode(images[32], 32)],
  ["48x48.png", pngEncode(images[48], 48)],
  ["64x64.png", pngEncode(images[64], 64)],
  ["128x128.png", pngEncode(images[128], 128)],
  ["128x128@2x.png", pngEncode(images[256], 256)],
  ["icon.png", pngEncode(images[256], 256)],
  ["icon.ico", icoEncode(pngSizes, images)],
  ...traySizes.map((s) => [`tray-${s}.rgba`, Buffer.from(images[s].buffer)]),
];
for (const [name, buf] of files) {
  writeFileSync(join(OUT, name), buf);
  console.log("wrote icons/%s  (%d bytes)", name, buf.length);
}
// The window paints the same plate in its header, and the only folder the
// webview can read from is the front end's own.
const uiIcon = pngEncode(images[256], 256);
writeFileSync(join(ROOT, "src", "icon.png"), uiIcon);
console.log("wrote src/icon.png  (%d bytes)", uiIcon.length);
console.log("done ->", OUT);
}

// Importing this file gives the pipeline; running it writes the icon set.
if (process.argv[1] && process.argv[1].endsWith("gen-icons.mjs")) main();
