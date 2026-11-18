// Starts the wasm bundle from an external, content-hashed module, so the page
// needs no inline script. Trunk writes the bundle paths into
// <meta name="fond-bundle">.
const bundle = document.querySelector('meta[name="fond-bundle"]');
if (bundle) {
  const { default: init } = await import(bundle.dataset.js);
  await init({ module_or_path: bundle.dataset.wasm });
}
