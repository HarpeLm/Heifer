// Runs HEIC/HEICS decoding, encoding and animation exports off the main thread.
const version = new URL(import.meta.url).searchParams.get("v") ?? "dev";
const mod = await import(`./pkg/heifer_demo.js?v=${version}`);
await mod.default({ module_or_path: `./pkg/heifer_demo_bg.wasm?v=${version}` });

function replySequence(id, seq, t0) {
  const width = seq.width;
  const height = seq.height;
  const frameCount = seq.frame_count;
  const durationsMs = seq.durations_ms;
  const info = seq.info;
  const rgba = seq.take_rgba();
  seq.free();
  self.postMessage(
    {
      id,
      width,
      height,
      frameCount,
      durationsMs,
      info,
      rgba,
      ms: Math.round(performance.now() - t0),
    },
    [rgba.buffer, durationsMs.buffer],
  );
}

self.onmessage = ({ data }) => {
  const { id, op } = data;
  const t0 = performance.now();
  try {
    if (op === "encode") {
      const heic = mod.encode(data.rgba, data.width, data.height, data.quality);
      self.postMessage({ id, heic, ms: Math.round(performance.now() - t0) }, [heic.buffer]);
    } else if (op === "decodeSequence") {
      replySequence(id, mod.decode_sequence(data.bytes), t0);
    } else if (op === "decodeAnimation") {
      replySequence(id, mod.decode_animation(data.bytes), t0);
    } else if (op === "encodeSequence") {
      const heics = mod.encode_sequence(
        data.rgba,
        data.width,
        data.height,
        data.durationsMs,
        data.quality,
        data.speed,
      );
      self.postMessage({ id, heics, ms: Math.round(performance.now() - t0) }, [heics.buffer]);
    } else if (op === "exportGif") {
      const bytes = mod.export_gif(data.rgba, data.width, data.height, data.durationsMs);
      self.postMessage({ id, bytes, ms: Math.round(performance.now() - t0) }, [bytes.buffer]);
    } else if (op === "exportApng") {
      const bytes = mod.export_apng(data.rgba, data.width, data.height, data.durationsMs);
      self.postMessage({ id, bytes, ms: Math.round(performance.now() - t0) }, [bytes.buffer]);
    } else if (op === "exportPngZip") {
      const bytes = mod.export_png_frames_zip(data.rgba, data.width, data.height, data.stem);
      self.postMessage({ id, bytes, ms: Math.round(performance.now() - t0) }, [bytes.buffer]);
    } else {
      throw new Error(`unknown worker op: ${op}`);
    }
  } catch (e) {
    self.postMessage({ id, error: e.message ?? String(e) });
  }
};
self.postMessage({ ready: true });
