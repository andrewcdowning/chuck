// Audio: the player and the recorder.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- audio ----------
   A minimal player over a hidden <audio>: play/pause, a seek bar, elapsed and total time, and speed. Only one
   clip plays at a time. The recorder records with whatever format the browser can make, shows a timer and a level
   meter, and lets the take be played back before it is attached. The microphone is released as soon as recording
   stops, is cancelled, or its card is redrawn. */
let nowPlaying = null;
function audioPlayer(src, durationMs) {
  const audio = el("audio", { preload: "metadata", src });
  const btn = el("button", { className: "play", type: "button", textContent: "▶", "aria-label": "Play" });
  const bar = el("input", { type: "range", min: 0, max: 1000, value: 0, step: 1, className: "seek", "aria-label": "Position" });
  const time = el("span", { className: "time" });
  const speed = el("button", { className: "speed", type: "button", textContent: "1×", title: "Playback speed" });
  let seeking = false;
  const total = () => (Number.isFinite(audio.duration) && audio.duration > 0 ? audio.duration * 1000 : durationMs || 0);
  const show = () => {
    time.textContent = `${fmtDur(audio.currentTime * 1000)} / ${fmtDur(total())}`;
    if (!seeking) bar.value = total() ? Math.min(1000, Math.round(audio.currentTime * 1e6 / total())) : 0;
  };
  btn.onclick = () => { audio.paused ? audio.play().catch(() => {}) : audio.pause(); };
  audio.onplay = () => {
    if (nowPlaying && nowPlaying !== audio) nowPlaying.pause();
    nowPlaying = audio;
    btn.textContent = "❚❚"; btn.setAttribute("aria-label", "Pause");
  };
  audio.onpause = audio.onended = () => { btn.textContent = "▶"; btn.setAttribute("aria-label", "Play"); };
  audio.ontimeupdate = audio.onloadedmetadata = audio.ondurationchange = show;
  bar.oninput = () => { seeking = true; time.textContent = `${fmtDur(bar.value / 1000 * total())} / ${fmtDur(total())}`; };
  bar.onchange = () => { audio.currentTime = bar.value / 1000 * total() / 1000; seeking = false; show(); };
  const rates = [1, 1.5, 2];
  speed.onclick = () => { const r = rates[(rates.indexOf(audio.playbackRate) + 1) % rates.length]; audio.playbackRate = r; speed.textContent = r + "×"; };
  show();
  return el("div", { className: "player" }, btn, bar, time, speed, audio);
}

const MAX_RECORDING_MS = 10 * 60 * 1000;
const liveRecorders = new Set();
function recordingType() {
  for (const t of ["audio/webm;codecs=opus", "audio/ogg;codecs=opus", "audio/mp4", "audio/webm"]) if (window.MediaRecorder?.isTypeSupported?.(t)) return t;
  return "";
}
function recorderEl({ onAttach, onClose, autostart }) {
  const box = el("div", { className: "recorder" });
  let stream, rec, chunks, started, timer, raf, actx, blob, durMs, url;
  const release = () => {
    clearInterval(timer); cancelAnimationFrame(raf);
    stream?.getTracks().forEach(t => t.stop());
    actx?.close?.().catch?.(() => {});
    stream = null; actx = null;
  };
  const cleanup = () => {
    asking++;   // a permission answer that arrives after this is ignored, and its microphone released at once
    if (rec && rec.state !== "inactive") { rec.onstop = null; try { rec.stop(); } catch {} }
    release();
    if (url) { URL.revokeObjectURL(url); url = null; }
    liveRecorders.delete(box);
  };
  box.cleanup = cleanup;
  const btn = (text, onclick, primary) => el("button", { className: "btn sm" + (primary ? " primary" : ""), type: "button", textContent: text, onclick });
  const close = () => { cleanup(); box.remove(); onClose?.(); };
  const idle = () => box.replaceChildren(btn("● Record", start, true), btn("Close", close));
  const fail = m => { release(); box.replaceChildren(el("span", { className: "err", textContent: m }), btn("Close", close)); };

  let asking = 0;   // id of the current permission request; a Cancel while waiting makes a late answer ignored
  async function start() {
    if (!window.isSecureContext) return fail("Recording needs a secure page (localhost or HTTPS).");
    if (!navigator.mediaDevices?.getUserMedia || !window.MediaRecorder) return fail("This browser can't record audio.");
    const mine = ++asking;
    // The browser may be showing a permission prompt; until it is answered nothing else can happen, so say so.
    box.replaceChildren(el("span", { className: "hint", textContent: "Waiting for microphone permission. Look for the prompt next to the address bar." }),
      btn("Cancel", () => { asking++; idle(); say("Recording cancelled."); }));
    let s;
    try { s = await navigator.mediaDevices.getUserMedia({ audio: true }); }
    catch (e) {
      if (mine !== asking) return;
      return fail({
        NotAllowedError: "Microphone access was blocked. Allow it for this site in the browser (the icon next to the address bar). On a Mac, also check System Settings > Privacy & Security > Microphone for your browser.",
        NotFoundError: "No microphone was found. Connect one and try again.",
        NotReadableError: "The microphone could not be opened. Another app may be using it, or the system is blocking it.",
      }[e.name] || `The microphone could not be opened (${e.name}).`);
    }
    if (mine !== asking) { s.getTracks().forEach(t => t.stop()); return; }   // cancelled while waiting
    stream = s;
    liveRecorders.add(box);
    const type = recordingType();
    rec = new MediaRecorder(stream, type ? { mimeType: type } : {});
    chunks = [];
    rec.ondataavailable = e => { if (e.data.size) chunks.push(e.data); };
    rec.onstop = () => { durMs = Date.now() - started; release(); blob = new Blob(chunks, { type: rec.mimeType || type || "audio/webm" }); review(); };
    rec.start(1000);
    started = Date.now();
    const clock = el("span", { className: "time", textContent: "0:00" });
    const meter = el("span", { className: "meter", title: "Input level" }, el("i"));
    try {
      actx = new AudioContext();
      const an = actx.createAnalyser();
      an.fftSize = 512;
      actx.createMediaStreamSource(stream).connect(an);
      const buf = new Uint8Array(an.fftSize);
      const loop = () => {
        an.getByteTimeDomainData(buf);
        let peak = 0;
        for (const v of buf) peak = Math.max(peak, Math.abs(v - 128));
        meter.firstChild.style.width = Math.min(100, peak / 128 * 160) + "%";
        raf = requestAnimationFrame(loop);
      };
      loop();
    } catch {}
    timer = setInterval(() => {
      const ms = Date.now() - started;
      clock.textContent = fmtDur(ms);
      if (ms >= MAX_RECORDING_MS && rec.state === "recording") rec.stop();
    }, 250);
    box.replaceChildren(el("span", { className: "recdot", "aria-hidden": "true" }), clock, meter,
      btn("■ Stop", () => { if (rec.state !== "inactive") rec.stop(); }, true),
      btn("Cancel", () => { cleanup(); idle(); say("Recording cancelled."); }));
    say("Recording. Press Stop when you are done.");
  }

  function review() {
    if (url) URL.revokeObjectURL(url);
    url = URL.createObjectURL(blob);
    const ext = /ogg/.test(blob.type) ? "ogg" : /mp4/.test(blob.type) ? "m4a" : "webm";
    const when = new Date().toLocaleString([], { dateStyle: "medium", timeStyle: "short" }).replace(/[/:]/g, "-");
    const name = `Voice note ${when}.${ext}`;
    box.replaceChildren(audioPlayer(url, durMs), el("div", { className: "row" },
      btn("Attach", () => { const p = { full: blob, thumb: null, name, audio: true, duration_ms: durMs }; cleanup(); idle(); onAttach(p); }, true),
      btn("Record again", start),
      btn("Discard", () => { cleanup(); idle(); say("Recording discarded."); })));
    say("Recording stopped. Play it back, then attach or discard it.");
  }

  idle();
  if (autostart) start();
  return box;
}

// Pictures in a card: one image keeps its proportions (space reserved before it loads, never upscaled, height
// capped); two sit side by side; three or more form a grid, with "+N" when there are more than four.
function mediaEl(n, compact) {
  const pics = imageEl(n, compact);
  const clips = n.attachments.filter(a => a.mime.startsWith("audio/")).map(a => el("div", { className: "audio" },
    ...(a.name && !/^Voice note /.test(a.name) ? [el("div", { className: "aname", textContent: a.name, title: a.name })] : []),
    audioPlayer(mediaUrl(a, false), a.duration_ms)));
  if (!pics && !clips.length) return null;
  return el("div", { className: "media-all" }, ...(pics ? [pics] : []), ...clips);
}

function imageEl(n, compact) {
  const imgs = n.attachments.filter(a => a.mime.startsWith("image/"));
  if (!imgs.length) return null;
  const src = mediaUrl;
  const pic = (a, small) => el("img", { src: src(a, small), alt: a.name || "image", loading: "lazy", decoding: "async", width: a.width, height: a.height });
  if (imgs.length === 1) {
    const a = imgs[0], img = pic(a, compact || a.width > 1280);
    img.style.maxWidth = `min(100%, ${a.width}px)`;
    img.onclick = () => lightbox(imgs, 0);
    return el("div", { className: "media one" }, img);
  }
  const grid = el("div", { className: "media grid g" + Math.min(imgs.length, 4) });
  imgs.slice(0, 4).forEach((a, k) => {
    const tile = el("button", { className: "tile", type: "button", title: a.name || "image", onclick: () => lightbox(imgs, k) }, pic(a, true));
    if (k === 3 && imgs.length > 4) tile.append(el("span", { className: "more", textContent: "+" + (imgs.length - 4) }));
    grid.append(tile);
  });
  return grid;
}

function setupMedia() {
  picker = el("input", { type: "file", multiple: true, accept: MEDIA_ACCEPT, hidden: true });
  picker.addEventListener("change", () => { addPending([...picker.files]); picker.value = ""; });
  document.body.append(picker);
  const comp = $("composer");
  comp.addEventListener("dragover", e => { if (hasFiles(e)) { e.preventDefault(); comp.classList.add("drop-on"); } });
  comp.addEventListener("dragleave", e => { if (!comp.contains(e.relatedTarget)) comp.classList.remove("drop-on"); });
  comp.addEventListener("drop", e => {
    if (!hasFiles(e)) return;
    e.preventDefault(); e.stopPropagation();
    comp.classList.remove("drop-on");
    addPending([...e.dataTransfer.files]);
  });
  comp.addEventListener("paste", e => {
    const files = [...(e.clipboardData?.files || [])].filter(looksLikeMedia);
    if (files.length) { e.preventDefault(); addPending(files); }
  });
  // Show every drop target while files are dragged over the page; never let the browser open a dropped file.
  // Browsers fire dragover continuously while a drag is over the page, but they do not reliably say when a drag
  // leaves the window or is cancelled (Firefox in particular). So the outlines stay on only while dragover keeps
  // arriving, and clear themselves shortly after it stops.
  let dragTimer;
  const endDrag = () => {
    clearTimeout(dragTimer);
    document.body.classList.remove("dragging-files");
    document.querySelectorAll(".drop-on").forEach(x => x.classList.remove("drop-on"));
  };
  window.addEventListener("dragover", e => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    document.body.classList.add("dragging-files");
    clearTimeout(dragTimer);
    dragTimer = setTimeout(endDrag, 300);
  });
  window.addEventListener("drop", e => { if (hasFiles(e)) e.preventDefault(); endDrag(); });
  window.addEventListener("dragend", endDrag);
  window.addEventListener("blur", endDrag);
}

