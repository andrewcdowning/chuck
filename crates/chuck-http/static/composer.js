// The composer: the form for a new card, its due date and reminder fields.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- due date field (todo cards) ---------- */
function dueFields(note) {
  const d = el("input", { type: "date", title: "Due date" });
  if (note && note.due_at) d.value = toDateInput(note.due_at);
  const set = days => { const x = new Date(); x.setDate(x.getDate() + days); d.value = `${x.getFullYear()}-${pad(x.getMonth() + 1)}-${pad(x.getDate())}`; };
  const q = (label, fn) => el("button", { className: "btn sm", type: "button", textContent: label, onclick: fn });
  const node = el("div", { className: "row" }, el("span", { className: "label", textContent: "Due" }), d,
    q("Today", () => set(0)), q("Tomorrow", () => set(1)), q("Next week", () => set(7)), q("Clear", () => { d.value = ""; }));
  return { node, value: () => fromDateInput(d.value) };
}

/* ---------- composer ----------
   Card types: note, todo, reminder. Pick one with the buttons, or type #todo / #reminder / #note followed by a
   space and the composer switches to that type (the token is removed and your text is kept). */
const TYPE_TOKEN = /(^|\s)#(todo|reminder|note)(?=\s)/i;
const TYPE_TOKEN_END = /(^|\s)#(todo|reminder|note)\s*$/i;
let composerText = () => "";   // merged text of the active composer, carried across a type switch

function stripToken(text, re) {
  const m = re.exec(text);
  if (!m) return null;
  const rest = (text.slice(0, m.index) + m[1] + text.slice(m.index + m[0].length))
    .replace(/[ \t]{2,}/g, " ").replace(/[ \t]+$/gm, "").replace(/^\s+/, "");
  return { type: m[2].toLowerCase(), text: rest };
}

function switchType(type, text) {
  mode = type;
  mark("modes", "m", type);
  if (type === "todo") {
    // One line becomes the first item; several lines make the first one the title.
    const lines = text.split("\n").map(l => l.trim()).filter(Boolean);
    renderComposer(lines.length > 1 ? { title: lines[0], items: lines.slice(1).join("\n") } : { items: lines[0] || "" });
  } else renderComposer({ text });
  const f = $("composer").querySelector("textarea");
  if (f) { f.focus(); f.setSelectionRange(f.value.length, f.value.length); }
}

function openComposerRecorder(autostart) {
  if (!composerRecorder) {
    composerRecorder = recorderEl({
      onAttach: p => { pendingMedia.push({ name: p.name, status: "ready", prepared: p, audio: true }); renderTray(); },
      onClose: () => { composerRecorder = null; },
      autostart,
    });
    $("tray").before(composerRecorder);
  }
  composerRecorder.querySelector("button")?.focus();
}

function renderComposer(carry = {}) {
  const box = $("composer");
  const msg = el("div", { className: "err" });
  const sugg = el("div", { className: "sugg" });
  let ta, title, items, extra, due, fields, kids;
  if (mode === "todo") {
    title = el("input", { placeholder: "Title (optional), e.g. Groceries #home", value: carry.title || "" });
    items = el("textarea", { placeholder: "One item per line", value: carry.items || "" });
    due = dueFields();
    fields = [title, items];
    kids = [title, el("div", { style: "height:8px" }), items, due.node];
    composerText = () => [title.value, items.value].filter(v => v.trim()).join("\n");
  } else {
    ta = el("textarea", { value: carry.text || "", placeholder: mode === "reminder"
      ? "e.g. remind me at 3pm to call Bob, or every Monday at 8am standup. Or pick a time below."
      : "Capture something. Use #tags. #todo or #reminder changes the card type, #photo attaches pictures, #voice records. Drop or paste files here. Ctrl/Cmd+Enter to save." });
    fields = [ta];
    kids = [ta];
    if (mode === "reminder") {
      extra = whenFields();
      kids.push(extra.node, el("div", { className: "hint", textContent: "Reminders alert while Chuck is open in a browser tab." }));
    }
    composerText = () => ta.value;
  }
  attachSuggest(fields, fields[0], sugg);
  fields.forEach(f => f.addEventListener("input", () => {
    if (/(^|\s)#voice(?=\s)/i.test(f.value)) {
      f.value = f.value.replace(/(^|\s)#voice\s/i, "$1");
      openComposerRecorder(true);
    }
    if (/(^|\s)#photo(?=\s)/i.test(f.value)) {
      f.value = f.value.replace(/(^|\s)#photo\s/i, "$1");
      picker.click();
    }
    const r = stripToken(composerText(), TYPE_TOKEN);
    if (r) switchType(r.type, r.text);
  }));

  const save = async () => {
    msg.textContent = "";
    // A type token left at the very end (no space typed after it) switches the type instead of saving.
    const r = stripToken(composerText(), TYPE_TOKEN_END);
    if (r) { switchType(r.type, r.text); return; }
    let body, patch = {};
    if (mode === "todo") {
      const lines = items.value.split("\n").map(l => l.trim()).filter(Boolean);
      if (!lines.length) { msg.textContent = "Add at least one item."; return; }
      body = [title.value.trim(), ...lines.map(l => "- [ ] " + l.replace(/^[-*]\s*(\[[ xX]\]\s*)?/, ""))].filter(Boolean).join("\n");
      const d = due.value();
      if (d) patch.due_at = d;
    } else {
      body = ta.value.trim();
      if (!body && !pendingMedia.some(x => x.status === "ready")) return;
      if (composerRecorder && composerRecorder.querySelector(".recdot")) { msg.textContent = "Stop the recording first."; return; }
    }
    if (pendingMedia.some(x => x.status === "preparing")) { msg.textContent = "Still preparing images…"; return; }
    if (mode === "reminder") {
      let v = extra.value();
      if (!v.remind_at) {
        const p = parseWhen(body);
        if (!p) { msg.textContent = "Couldn't read a time. Pick one below."; return; }
        v = { remind_at: p.at, repeat: p.repeat };
      }
      patch = { ...patch, ...v };
    }
    const ready = pendingMedia.filter(x => x.status === "ready");
    let card;
    try { card = await api("/api/notes" + (ready.length ? "?media=true" : ""), { method: "POST", body: JSON.stringify({ body, ...patch }) }); }
    catch (e) { msg.textContent = e.message; return; }
    const failed = [];
    for (const [k, it] of ready.entries()) {
      try { await uploadImage(card.id, it.prepared, f => { msg.textContent = `Uploading ${k + 1} of ${ready.length}: ${Math.round(f * 100)}%`; }); }
      catch (e) { failed.push(`${it.name}: ${e.message}`); }
    }
    if (ready.length && failed.length === ready.length && !body) await api("/api/notes/" + card.id, { method: "DELETE" }).catch(() => {});
    pendingMedia.forEach(x => x.url && URL.revokeObjectURL(x.url));
    pendingMedia = [];
    renderComposer();
    if (failed.length) $("composer").querySelector(".err").textContent = "Not attached: " + failed.join("; ");
    refresh();
  };
  fields.forEach(f => f.addEventListener("keydown", e => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) save(); }));
  const attach = el("button", { className: "btn", type: "button", textContent: "Attach", title: "Attach pictures or audio (or drop or paste them here)", onclick: () => picker.click() });
  const record = el("button", { className: "btn", type: "button", textContent: "Record", title: "Record a voice note (or type #voice)", onclick: () => openComposerRecorder(false) });
  box.replaceChildren(...kids, ...(composerRecorder ? [composerRecorder] : []), el("div", { className: "tray", id: "tray" }), sugg,
    el("div", { className: "row" }, el("button", { className: "btn primary", textContent: "Save", onclick: save }), attach, record, msg));
  renderTray();
}

