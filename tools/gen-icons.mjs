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

const MASTER = 1024;

// ---------- master ----------
function decodeMaster() {
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

function unmatteWhite(px) {
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
// `keep` is the fraction of the master that survives: the tiny entries crop in a
// little so the keycap still reads in a 16 px tray.
function downscale(master, out, keep = 1) {
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

// ---------- hand-drawn 16 px frame ----------
// The master is all gradients, so at 16 px it collapses into a grey boulder with a
// detached amber speck (and the crop needed by the other sizes cuts its plate
// corners off). This draws the same design flat instead: slate plate, thick
// isometric keycap, amber accent on the front-left edge.
const TINY = {
  radius: 0.215,
  cx: 0.5, cy: 0.395, hw: 0.31, hh: 0.15,
  depth: 0.21,
  edge: 0.035,
};
const TINY_PALETTE = {
  plateTop: [0x25, 0x2e, 0x3b], plateBot: [0x11, 0x16, 0x1d],
  top: [0xd6, 0xdd, 0xe5], left: [0x79, 0x85, 0x92], right: [0x5b, 0x66, 0x73],
  amber: [0xf2, 0xa3, 0x3c],
};

function tinyFrame(size) {
  const SS = 4;
  const dst = new Uint8Array(size * size * 4);
  const edge = Math.max(TINY.edge, 1.15 / size);
  const p = TINY, c = TINY_PALETTE;

  const inPlate = (x, y) => {
    const px = Math.max(Math.abs(x - 0.5) - (0.5 - p.radius), 0);
    const py = Math.max(Math.abs(y - 0.5) - (0.5 - p.radius), 0);
    return Math.hypot(px, py) <= p.radius;
  };
  const inDiamond = (x, y, dy) =>
    Math.abs(x - p.cx) / p.hw + Math.abs(y - p.cy - dy) / p.hh <= 1;
  const wall = (x, y) => y >= p.cy && inDiamond(x, y, p.depth);
  // distance to the front-left rim, from the left vertex to the bottom vertex
  const railDist = (x, y) => {
    const ax = p.cx - p.hw, ay = p.cy, ex = p.hw, ey = p.hh;
    const t = Math.max(0, Math.min(1, ((x - ax) * ex + (y - ay) * ey) / (ex * ex + ey * ey)));
    return Math.hypot(x - (ax + t * ex), y - (ay + t * ey));
  };

  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const fx = (x + (sx + 0.5) / SS) / size, fy = (y + (sy + 0.5) / SS) / size;
          let col = null;
          if (inDiamond(fx, fy, 0)) col = c.top;
          else if (wall(fx, fy)) col = fx <= p.cx ? c.left : c.right;
          if (col && fx <= p.cx && railDist(fx, fy) <= edge / 2) col = c.amber;
          if (!col && inPlate(fx, fy)) {
            const t = Math.min(1, Math.max(0, (fy - 0.08) / 0.9));
            col = [0, 1, 2].map((i) =>
              Math.round(c.plateTop[i] + (c.plateBot[i] - c.plateTop[i]) * t),
            );
          }
          if (col) { r += col[0]; g += col[1]; b += col[2]; a += 255; }
        }
      }
      const n = SS * SS, o = (y * size + x) * 4;
      const cov = a / n / 255;
      dst[o] = cov > 0 ? Math.round(r / n / cov) : 0;
      dst[o + 1] = cov > 0 ? Math.round(g / n / cov) : 0;
      dst[o + 2] = cov > 0 ? Math.round(b / n / cov) : 0;
      dst[o + 3] = Math.round(a / n);
    }
  }
  return dst;
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

function pngEncode(rgba, size) {
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

function icoEncode(sizes, images) {
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
console.log("decoding %s ...", MASTER_PNG);
const master = decodeMaster();
unmatteWhite(master);

const pngSizes = [16, 32, 48, 64, 128, 256];
/// fraction of the master kept per size, see `downscale`
const KEEP = { 32: 0.88 };
const images = {};
for (const s of pngSizes) images[s] = s === 16 ? tinyFrame(s) : downscale(master, s, KEEP[s] || 1);

const files = [
  ["32x32.png", pngEncode(images[32], 32)],
  ["128x128.png", pngEncode(images[128], 128)],
  ["128x128@2x.png", pngEncode(images[256], 256)],
  ["icon.png", pngEncode(images[256], 256)],
  ["icon.ico", icoEncode([16, 32, 48, 64, 128, 256], images)],
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
