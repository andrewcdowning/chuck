// Pictures: preparing, uploading and showing them.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- media ----------
   Photos are redrawn on a canvas before upload: that applies the camera's rotation, caps the size, makes a small
   preview, and drops all metadata (GPS location included). The server strips metadata again and checks every file.
   GIFs are kept as they are so animation survives. */
const MEDIA_ACCEPT = "image/jpeg,image/png,image/gif,image/webp,image/avif,image/heic,image/heif,.heic,.heif," +
  "audio/webm,audio/ogg,audio/mp4,audio/x-m4a,audio/mpeg,audio/wav,audio/x-wav,.m4a,.mp3,.ogg,.oga,.opus,.wav,.weba";
const MAX_SIDE = 2560, THUMB_SIDE = 640;
let pendingMedia = [];   // images attached in the composer, waiting for Save: { name, status, prepared, url, error }
let picker;              // the hidden file input behind "Attach" and #photo
let composerRecorder = null;   // the composer's recorder panel; kept across type switches so a recording survives

const isHeic = f => /image\/hei[cf]/i.test(f.type) || /\.hei[cf]$/i.test(f.name);
const looksLikeImage = f => /^image\//.test(f.type) || /\.(jpe?g|png|gif|webp|avif|hei[cf])$/i.test(f.name);
const looksLikeAudio = f => /^audio\//.test(f.type) || /\.(m4a|mp3|ogg|oga|opus|wav|weba)$/i.test(f.name);
const looksLikeMedia = f => looksLikeImage(f) || looksLikeAudio(f);
const fmtDur = ms => { const t = Math.max(0, Math.round((ms || 0) / 1000)); return `${Math.floor(t / 60)}:${String(t % 60).padStart(2, "0")}`; };
const hasFiles = e => !!e.dataTransfer && [...e.dataTransfer.types].includes("Files");
// Encoding never waits forever: some browsers stall instead of failing on a format they cannot write.
const canvasBlob = (c, type, q) => Promise.race([
  new Promise(res => c.toBlob(res, type, q)),
  new Promise(res => setTimeout(() => res(null), 15000)),
]);
// Synchronous check: an unsupported type makes toDataURL return PNG instead.
const canEncode = (() => {
  const seen = {};
  return type => seen[type] ??= (() => { const c = document.createElement("canvas"); c.width = c.height = 1; return c.toDataURL(type).startsWith("data:" + type); })();
})();

async function decodeImage(file) {
  try { return await createImageBitmap(file); } catch {}
  const url = URL.createObjectURL(file);   // fallback through <img>, which some browsers decode more formats with
  try { const img = new Image(); img.src = url; await img.decode(); return await createImageBitmap(img); }
  finally { URL.revokeObjectURL(url); }
}

async function encodeImage(bitmap, maxSide, type) {
  const scale = Math.min(1, maxSide / Math.max(bitmap.width, bitmap.height));
  const w = Math.max(1, Math.round(bitmap.width * scale)), h = Math.max(1, Math.round(bitmap.height * scale));
  const c = document.createElement("canvas");
  c.width = w; c.height = h;
  c.getContext("2d").drawImage(bitmap, 0, 0, w, h);
  let blob = canEncode(type) ? await canvasBlob(c, type, 0.88) : null;
  if (!blob || blob.type !== type) blob = await canvasBlob(c, "image/png");   // this browser cannot encode `type`
  if (!blob) throw new Error("this browser could not encode the image");
  return { blob, w, h };
}

async function prepareImage(file) {
  if (!looksLikeImage(file)) throw new Error(`${file.name}: not a picture or audio file. Chuck accepts JPEG, PNG, GIF, WebP, AVIF, HEIC, and WebM, Ogg, M4A, MP3 and WAV audio.`);
  if (file.size > 60 * 1024 * 1024) throw new Error(`${file.name}: larger than 60 MB.`);
  let bitmap;
  try { bitmap = await decodeImage(file); }
  catch { throw new Error(isHeic(file) ? `${file.name}: this browser can't read HEIC photos. Save it as JPEG and try again.` : `${file.name}: couldn't read this image.`); }
  const gif = file.type === "image/gif" || /\.gif$/i.test(file.name);
  const photo = /jpe?g|hei[cf]/i.test(file.type) || /\.(jpe?g|hei[cf])$/i.test(file.name);
  const type = photo ? "image/jpeg" : "image/webp";
  const full = gif ? { blob: file, w: bitmap.width, h: bitmap.height } : await encodeImage(bitmap, MAX_SIDE, type);
  const thumb = gif ? null : await encodeImage(bitmap, THUMB_SIDE, type);
  bitmap.close?.();
  if (full.blob.size > 20 * 1024 * 1024) throw new Error(`${file.name}: still larger than 20 MB after resizing.`);
  const ext = { "image/jpeg": "jpg", "image/webp": "webp", "image/png": "png", "image/gif": "gif" }[full.blob.type] || "img";
  const name = gif ? file.name : file.name.replace(/\.[^.]+$/, "") + "." + ext;
  return { full: full.blob, thumb: thumb && thumb.blob, width: full.w, height: full.h, name };
}

function sendBlob(method, url, blob, onProgress) {
  return new Promise((resolve, reject) => {
    const x = new XMLHttpRequest();
    x.open(method, url);
    x.upload.onprogress = e => { if (e.lengthComputable && onProgress) onProgress(e.loaded / e.total); };
    x.onload = () => {
      if (x.status < 300) return resolve(x.responseText ? JSON.parse(x.responseText) : null);
      let m = x.statusText || "upload failed";
      try { m = JSON.parse(x.responseText).error || m; } catch {}
      reject(new Error(m));
    };
    x.onerror = () => reject(new Error("network error"));
    x.send(blob);
  });
}

// Audio is uploaded as it is (the server checks and cleans it); its length is measured here, because files
// recorded in a browser often do not state their own duration.
function audioDuration(blob) {
  return new Promise(res => {
    const a = new Audio(), url = URL.createObjectURL(blob);
    let done = false;
    const fin = v => { if (done) return; done = true; URL.revokeObjectURL(url); res(Number.isFinite(v) && v > 0 ? Math.round(v * 1000) : null); };
    a.preload = "metadata";
    a.onloadedmetadata = () => {
      if (a.duration === Infinity) { a.ondurationchange = () => { if (Number.isFinite(a.duration)) fin(a.duration); }; a.currentTime = 1e101; }
      else fin(a.duration);
    };
    a.onerror = () => fin(null);
    setTimeout(() => fin(null), 8000);
    a.src = url;
  });
}

async function prepareMedia(file) {
  if (!looksLikeAudio(file) || looksLikeImage(file)) return prepareImage(file);
  if (file.size > 25 * 1024 * 1024) throw new Error(`${file.name}: audio larger than 25 MB.`);
  return { full: file, thumb: null, name: file.name, audio: true, duration_ms: await audioDuration(file) };
}

async function uploadImage(noteId, p, onProgress) {
  const dur = p.duration_ms ? `&duration_ms=${p.duration_ms}` : "";
  const a = await sendBlob("POST", `/api/notes/${noteId}/attachments?name=${encodeURIComponent(p.name)}${dur}`, p.full, onProgress);
  // Without a preview the card simply shows the full image, so a failed preview is not an error.
  if (p.thumb) await sendBlob("PUT", `/api/media/${a.id}/thumb`, p.thumb).catch(() => {});
  return a;
}

function addPending(files) {
  for (const f of files) {
    const item = { name: f.name, status: "preparing" };
    pendingMedia.push(item);
    prepareMedia(f)
      .then(p => { item.prepared = p; item.audio = !!p.audio; item.url = p.audio ? null : URL.createObjectURL(p.thumb || p.full); item.status = "ready"; })
      .catch(e => { item.status = "error"; item.error = e.message; })
      .finally(renderTray);
  }
  renderTray();
}

function dropPending(item) {
  if (item.url) URL.revokeObjectURL(item.url);
  pendingMedia = pendingMedia.filter(x => x !== item);
  renderTray();
}

function renderTray() {
  const tray = $("tray");
  if (!tray) return;
  tray.replaceChildren(...pendingMedia.map(it => {
    const x = el("button", { className: "x", type: "button", textContent: "×", title: "Remove", onclick: () => dropPending(it) });
    if (it.status === "error") return el("div", { className: "tile error", textContent: it.error }, x);
    const tile = el("div", { className: "tile" + (it.audio ? " audio" : ""), title: it.name });
    if (it.url) tile.append(el("img", { src: it.url, alt: it.name }));
    if (it.audio) tile.append("♪ " + fmtDur(it.prepared?.duration_ms));
    if (it.status !== "ready") tile.append(el("span", { className: "st", textContent: it.status }));
    tile.append(x);
    return tile;
  }));
}

// Uploads images straight onto an existing card (drop on the card).
async function addToCard(n, files, card) {
  const note = el("div", { className: "upnote", textContent: "Preparing…" });
  card.querySelector(".content").prepend(note);
  const errors = [];
  let done = 0;
  for (const f of files) {
    try {
      const p = await prepareMedia(f);
      await uploadImage(n.id, p, frac => { note.textContent = `Adding ${done + 1} of ${files.length}: ${Math.round(frac * 100)}%`; });
      done++;
    } catch (e) { errors.push(e.message); }
  }
  if (errors.length) { note.className = "upnote err"; note.textContent = errors.join(" "); setTimeout(refresh, 4000); }
  else refresh();
}

// Every picture URL carries the attachment's version: a new picture never reuses a cached URL.
const mediaUrl = (a, small) => `/api/media/${a.id}?v=${a.v}` + (small && a.has_thumb ? "&thumb=true" : "");

function lightbox(imgs, start) {
  let i = start;
  const pic = el("img", { alt: "" });
  const cap = el("div", { className: "cap" });
  const box = el("div", { className: "lightbox", role: "dialog", "aria-label": "Image viewer" }, pic, cap);
  const show = k => {
    i = (k + imgs.length) % imgs.length;
    pic.src = mediaUrl(imgs[i], false);
    pic.alt = imgs[i].name || "image";
    cap.textContent = (imgs[i].name || "") + (imgs.length > 1 ? `   ${i + 1} / ${imgs.length}` : "");
  };
  const onKey = e => {
    if (e.key === "Escape") close();
    else if (e.key === "ArrowRight") show(i + 1);
    else if (e.key === "ArrowLeft") show(i - 1);
  };
  const close = () => { box.remove(); document.removeEventListener("keydown", onKey); };
  box.onclick = close;
  document.addEventListener("keydown", onKey);
  document.body.append(box);
  show(i);
}

