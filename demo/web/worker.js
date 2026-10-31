// Encodes images to HEIC off the main thread, so that the page stays responsive.
const version = new URL(import.meta.url).searchParams.get("v") ?? "dev";
const module = await import(`./pkg/heifer_demo.js?v=${version}`);
await module.default({ module_or_path: `./pkg/heifer_demo_bg.wasm?v=${version}` });

self.onmessage = ({ data: { rgba, width, height, quality } }) => {
  const t0 = performance.now();
  try {
    const heic = module.encode(rgba, width, height, quality);
    self.postMessage({ heic, ms: Math.round(performance.now() - t0) }, [heic.buffer]);
  } catch (e) {
    self.postMessage({ error: e.message ?? String(e) });
  }
};
self.postMessage({ ready: true });
