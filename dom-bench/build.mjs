// Build the React and Svelte bundles into build/. (elm / alm-js / alm-wasm are
// built by build.sh with their own compilers.)
import * as esbuild from 'esbuild';
import { compile } from 'svelte/compiler';
import fs from 'node:fs';

fs.mkdirSync('build', { recursive: true });

// React: JSX bundle, minified, production. Naive + React.memo-optimized.
for (const [src, out] of [['App.jsx', 'build/react.bundle.js'], ['App_opt.jsx', 'build/react_opt.bundle.js']]) {
  await esbuild.build({
    entryPoints: [src], bundle: true, minify: true, format: 'iife', jsx: 'automatic',
    outfile: out, define: { 'process.env.NODE_ENV': '"production"' },
  });
}

// Svelte: compile the component, then bundle a tiny mount entry.
const { js } = compile(fs.readFileSync('App.svelte', 'utf8'), { generate: 'dom', css: 'injected' });
fs.writeFileSync('build/App_compiled.js', js.code);
fs.writeFileSync('build/svelte-main.js', "import App from './App_compiled.js';\nnew App({ target: document.getElementById('app') });\n");
await esbuild.build({ entryPoints: ['build/svelte-main.js'], bundle: true, minify: true, format: 'iife', outfile: 'build/svelte.bundle.js' });

console.log('built build/react.bundle.js + build/svelte.bundle.js');
