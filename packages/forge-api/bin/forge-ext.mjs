#!/usr/bin/env node
// forge-ext build [dir]           bundles a Forge extension (TS/TSX) into <dir>/dist/extension.js
// forge-ext watch [dir]           the same, again on every change
// forge-ext pack  [dir] [-o out]  builds it and packs it into a .forgeext file (a zip)
//
// React and @forge-ide/api are provided by the host runtime, so they are mapped to its shared
// modules instead of being bundled: every extension renders through the same React.
//
// A package holds what the extension needs at run time: package.json, dist/, its pages and
// assets, and its sidecars (bin/<platform>/<name>, declared in `forge.sidecars`). The rules
// match crates/forge-extension-host/src/package.rs, which installs and exports packages.
import { build, context } from 'esbuild';
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';
import { deflateRawSync } from 'node:zlib';

const argv = process.argv.slice(2);
const cmd = argv[0] ?? 'build';
const outFlag = argv.indexOf('-o');
const out = outFlag >= 0 ? argv[outFlag + 1] : null;
const dir = argv.slice(1).find((a, i, all) => !a.startsWith('-') && all[i - 1] !== '-o') ?? '.';
if (!['build', 'watch', 'pack'].includes(cmd)) {
  console.error('usage: forge-ext build|watch|pack [extension-dir] [-o file.forgeext]');
  process.exit(1);
}
const root = resolve(dir);
const pkg = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
const entry = join(root, pkg.forge?.entry ?? 'src/extension.tsx');

// `@forge/api`: the API's name before it was published as `@forge-ide/api`.
const shared = ['react', 'react/jsx-runtime', '@forge-ide/api', '@forge/api'];
const hostModules = {
  name: 'forge-host-modules',
  setup(b) {
    b.onResolve({ filter: new RegExp(`^(${shared.map((s) => s.replace('/', '\\/')).join('|')})$`) }, (args) => ({ path: args.path, namespace: 'forge-host' }));
    b.onLoad({ filter: /.*/, namespace: 'forge-host' }, (args) => ({
      contents: `module.exports = globalThis.__forge.modules[${JSON.stringify(args.path)}];`,
      loader: 'js',
    }));
  },
};

const options = {
  entryPoints: [entry],
  outfile: join(root, pkg.forge?.main ?? 'dist/extension.js'),
  bundle: true,
  format: 'iife',
  globalName: '__forgeExtension',
  platform: 'neutral',
  target: 'es2020',
  jsx: 'automatic',
  plugins: [hostModules],
  define: { 'process.env.NODE_ENV': '"production"' },
  logLevel: 'info',
};

// ------------------------------------------------------------------------------- packing

function pack() {
  const defaults = ['package.json', 'dist', 'webview', 'assets', 'media', 'bin', 'README.md', 'CHANGELOG.md', 'LICENSE', 'LICENSE.md', 'icon.png'];
  const skipped = new Set(['node_modules', '.git', '.DS_Store']);
  const main = pkg.forge?.main ?? 'dist/extension.js';
  const roots = [...new Set([...(pkg.forge?.files ?? defaults).map((f) => f.replace(/^\.\//, '').replace(/\/$/, '')), 'package.json', main])].sort();
  const sidecars = pkg.forge?.sidecars ?? [];

  const files = [];
  const walk = (path, name) => {
    const stat = statSync(path);
    if (stat.isDirectory()) {
      for (const child of readdirSync(path).sort()) if (!skipped.has(child)) walk(join(path, child), `${name}/${child}`);
    } else if (stat.isFile()) {
      const sidecar = name.startsWith('bin/') && sidecars.some((s) => name.endsWith(`/${s}`) || name.endsWith(`/${s}.exe`));
      files.push({ name, path, mode: sidecar ? 0o755 : stat.mode & 0o777 });
    }
  };
  for (const r of roots) {
    if (r.split('/').some((c) => c === '..' || c === '')) throw new Error(`\`${r}\` in forge.files must be a path inside the extension`);
    if (existsSync(join(root, r))) walk(join(root, r), r);
  }
  files.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  files.splice(0, files.length, ...files.filter((f, i) => i === 0 || f.name !== files[i - 1].name));

  for (const sidecar of sidecars) {
    const platforms = files.filter((f) => f.name.startsWith('bin/') && (f.name.endsWith(`/${sidecar}`) || f.name.endsWith(`/${sidecar}.exe`))).map((f) => f.name.split('/')[1]);
    if (platforms.length === 0) {
      console.error(`sidecar \`${sidecar}\` is not built: put it in bin/<platform>/${sidecar} (e.g. bin/darwin-arm64/${sidecar})`);
      process.exit(1);
    }
    console.log(`  ${sidecar}: ${platforms.join(', ')}`);
  }

  const name = pkg.name.replace(/^@/, '').replace(/\//g, '-');
  const target = resolve(out ?? join(root, `${name}${pkg.version ? `-${pkg.version}` : ''}.forgeext`));
  writeFileSync(target, zip(files));
  console.log(`packed ${files.length} files into ${relative(process.cwd(), target) || target}`);
}

/** A zip archive of `files` (deflated, with unix permissions). */
function zip(files) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  const { time, date } = dosTime(new Date());
  for (const f of files) {
    const data = readFileSync(f.path);
    const compressed = deflateRawSync(data, { level: 9 });
    const stored = compressed.length >= data.length;
    const body = stored ? data : compressed;
    const name = Buffer.from(f.name.split(sep).join('/'), 'utf8');
    const crc = crc32(data);

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4); // version needed
    local.writeUInt16LE(0x0800, 6); // UTF-8 names
    local.writeUInt16LE(stored ? 0 : 8, 8);
    local.writeUInt16LE(time, 10);
    local.writeUInt16LE(date, 12);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(body.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(name.length, 26);
    local.writeUInt16LE(0, 28);
    locals.push(local, name, body);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE((3 << 8) | 20, 4); // made by: unix
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(0x0800, 8);
    central.writeUInt16LE(stored ? 0 : 8, 10);
    central.writeUInt16LE(time, 12);
    central.writeUInt16LE(date, 14);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(body.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(name.length, 28);
    central.writeUInt32LE(((0o100000 | f.mode) << 16) >>> 0, 38); // regular file + permissions
    central.writeUInt32LE(offset, 42);
    centrals.push(central, name);

    offset += local.length + name.length + body.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(files.length, 8);
  end.writeUInt16LE(files.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

function dosTime(d) {
  return {
    time: (d.getHours() << 11) | (d.getMinutes() << 5) | Math.floor(d.getSeconds() / 2),
    date: ((d.getFullYear() - 1980) << 9) | ((d.getMonth() + 1) << 5) | d.getDate(),
  };
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(buffer) {
  let c = 0xffffffff;
  for (const byte of buffer) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

// Last, so the helpers above are initialised.
if (cmd === 'watch') {
  const ctx = await context(options);
  await ctx.watch();
} else {
  await build(options);
  if (cmd === 'pack') pack();
}
