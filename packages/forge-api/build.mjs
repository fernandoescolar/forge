// Bundles src/runtime.ts (React + reconciler + API) into dist/runtime.js, the single script
// the Rust extension host evaluates before any extension.
import { build } from 'esbuild';

await build({
  entryPoints: ['src/runtime.ts'],
  outfile: 'dist/runtime.js',
  bundle: true,
  format: 'iife',
  platform: 'neutral',
  mainFields: ['main', 'module'],
  target: 'es2020',
  minify: process.env.FORGE_MINIFY === '1',
  define: { 'process.env.NODE_ENV': '"production"' },
  legalComments: 'none',
  logLevel: 'info',
});
