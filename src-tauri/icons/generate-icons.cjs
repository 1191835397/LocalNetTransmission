// Original LanLink mark. Uses only Node built-ins; no downloads or image tools.
// Run: node src-tauri/icons/generate-icons.cjs
const fs = require('node:fs');
const path = require('node:path');
const zlib = require('node:zlib');

const shapes = [
  { x: 4, y: 4, w: 120, h: 120, r: 28, color: '#eef1fc' },
  { x: 23, y: 28, w: 49, h: 49, r: 13, color: '#273a63' },
  { x: 29, y: 34, w: 37, h: 37, r: 8, color: '#eef1fc' },
  { x: 56, y: 53, w: 49, h: 49, r: 13, color: '#6176d9' },
  { x: 62, y: 59, w: 37, h: 37, r: 8, color: '#eef1fc' },
  { points: [[43, 49], [75, 49], [75, 43], [88, 53], [75, 63], [75, 57], [43, 57]], color: '#eb8759' },
  { points: [[85, 76], [53, 76], [53, 70], [40, 80], [53, 90], [53, 84], [85, 84]], color: '#273a63' },
];

function inside(shape, x, y) {
  if (shape.points) {
    let hit = false;
    for (let i = 0, j = shape.points.length - 1; i < shape.points.length; j = i++) {
      const [ax, ay] = shape.points[i];
      const [bx, by] = shape.points[j];
      if ((ay > y) !== (by > y) && x < ((bx - ax) * (y - ay)) / (by - ay) + ax) hit = !hit;
    }
    return hit;
  }
  if (x < shape.x || x > shape.x + shape.w || y < shape.y || y > shape.y + shape.h) return false;
  const cx = Math.max(shape.x + shape.r, Math.min(x, shape.x + shape.w - shape.r));
  const cy = Math.max(shape.y + shape.r, Math.min(y, shape.y + shape.h - shape.r));
  return (x - cx) ** 2 + (y - cy) ** 2 <= shape.r ** 2;
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, content) {
  const name = Buffer.from(type);
  const header = Buffer.alloc(4);
  header.writeUInt32BE(content.length);
  const checksum = Buffer.alloc(4);
  checksum.writeUInt32BE(crc32(Buffer.concat([name, content])));
  return Buffer.concat([header, name, content, checksum]);
}

function png(size) {
  const raw = Buffer.alloc(size * (1 + size * 4));
  const samples = 2;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const color = [0, 0, 0, 0];
      for (let sy = 0; sy < samples; sy++) {
        for (let sx = 0; sx < samples; sx++) {
          const px = ((x + (sx + 0.5) / samples) * 128) / size;
          const py = ((y + (sy + 0.5) / samples) * 128) / size;
          let top;
          for (const shape of shapes) if (inside(shape, px, py)) top = shape;
          if (top) {
            for (let channel = 0; channel < 3; channel++) color[channel] += parseInt(top.color.slice(1 + channel * 2, 3 + channel * 2), 16);
            color[3] += 255;
          }
        }
      }
      const offset = y * (1 + size * 4) + 1 + x * 4;
      const covered = color[3] / 255;
      for (let channel = 0; channel < 3; channel++) raw[offset + channel] = covered ? Math.round(color[channel] / covered) : 0;
      raw[offset + 3] = Math.round(color[3] / (samples * samples));
    }
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8;
  header[9] = 6; // RGBA, required by Tauri.
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', header), chunk('IDAT', zlib.deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]);
}

function save(name, bytes) {
  fs.writeFileSync(path.join(__dirname, name), bytes);
}

const svgShapes = shapes.map((shape) => shape.points
  ? `<polygon points="${shape.points.map((point) => point.join(',')).join(' ')}" fill="${shape.color}"/>`
  : `<rect x="${shape.x}" y="${shape.y}" width="${shape.w}" height="${shape.h}" rx="${shape.r}" fill="${shape.color}"/>`).join('\n  ');
save('icon.svg', `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128">\n  ${svgShapes}\n</svg>\n`);
const sizes = [16, 32, 48, 128, 256, 512, 1024];
const images = new Map(sizes.map((size) => [size, png(size)]));
save('32x32.png', images.get(32));
save('128x128.png', images.get(128));
save('128x128@2x.png', images.get(256));

const icoSizes = [16, 32, 48, 256];
const icoHeader = Buffer.alloc(6 + icoSizes.length * 16);
icoHeader.writeUInt16LE(1, 2);
icoHeader.writeUInt16LE(icoSizes.length, 4);
let offset = icoHeader.length;
icoSizes.forEach((size, index) => {
  const entry = 6 + index * 16;
  icoHeader[entry] = size === 256 ? 0 : size;
  icoHeader[entry + 1] = size === 256 ? 0 : size;
  icoHeader.writeUInt16LE(1, entry + 4);
  icoHeader.writeUInt16LE(32, entry + 6);
  icoHeader.writeUInt32LE(images.get(size).length, entry + 8);
  icoHeader.writeUInt32LE(offset, entry + 12);
  offset += images.get(size).length;
});
save('icon.ico', Buffer.concat([icoHeader, ...icoSizes.map((size) => images.get(size))]));
const icnsEntries = [['ic07', 128], ['ic08', 256], ['ic09', 512], ['ic10', 1024]].map(([type, size]) => {
  const header = Buffer.alloc(8);
  header.write(type, 0, 4, 'ascii');
  header.writeUInt32BE(images.get(size).length + 8, 4);
  return Buffer.concat([header, images.get(size)]);
});
const icnsHeader = Buffer.alloc(8);
icnsHeader.write('icns', 0, 4, 'ascii');
icnsHeader.writeUInt32BE(8 + icnsEntries.reduce((sum, entry) => sum + entry.length, 0), 4);
save('icon.icns', Buffer.concat([icnsHeader, ...icnsEntries]));
console.log('Generated original SVG, RGBA PNG, ICO and ICNS assets in src-tauri/icons.');
