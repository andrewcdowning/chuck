// Cards: the renderer for every kind of card, and the Markdown renderer.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- cards ----------
   Everything on the feed or board is a card with a `kind`. Notes, todos and reminders share one renderer
   (`noteEl`); a new card type can register its own renderer here. Layouts only place cards. */
const CARD_RENDERERS = {};
function cardEl(card, compact) { return (CARD_RENDERERS[card.kind] || noteEl)(card, compact); }

/* ---------- note card ---------- */
/* ---------- Markdown ----------
   Rendered as DOM nodes with textContent only, never innerHTML, so note text cannot inject markup or scripts.
   Raw HTML in a note is shown as plain text. Links must be http, https or mailto. Images are not rendered. */
const INLINE = new RegExp([
  "`([^`\\n]+)`",                                            // 1 inline code
  "\\[([^\\]\\n]+)\\]\\(([^)\\s]+)\\)",                      // 2,3 [text](url)
  "\\*\\*(?!\\s)([^\\n]+?)(?<!\\s)\\*\\*",                   // 4 bold
  "~~(?!\\s)([^\\n]+?)(?<!\\s)~~",                           // 5 strikethrough
  "(?<![\\w*])\\*(?![\\s*])([^*\\n]+?)(?<!\\s)\\*(?![\\w*])", // 6 italic *x*
  "(?<!\\w)_(?!\\s)([^_\\n]+?)(?<!\\s)_(?!\\w)",             // 7 italic _x_
  "(https?:\\/\\/[^\\s<>()]*[^\\s<>().,;:!?'\"])",           // 8 bare URL
  "(?<=^|\\s)#([\\p{L}\\p{N}_-]+)",                          // 9 #tag
].join("|"), "gu");
const SAFE_URL = /^(https?:|mailto:)/i;

function mdLink(href, content) {
  const a = el("a", { href, target: "_blank", rel: "noopener noreferrer nofollow" });
  a.append(content);
  return a;
}

function renderInline(text, inLink = false) {
  const frag = document.createDocumentFragment();
  let last = 0;
  for (const m of text.matchAll(INLINE)) {
    if (m.index > last) frag.append(text.slice(last, m.index));
    last = m.index + m[0].length;
    if (m[1] !== undefined) frag.append(el("code", { textContent: m[1] }));
    else if (m[2] !== undefined) {
      if (SAFE_URL.test(m[3]) && !inLink) frag.append(mdLink(m[3], renderInline(m[2], true)));
      else frag.append(m[0]);
    } else if (m[4] !== undefined) frag.append(el("strong", {}, renderInline(m[4], inLink)));
    else if (m[5] !== undefined) frag.append(el("s", {}, renderInline(m[5], inLink)));
    else if (m[6] !== undefined) frag.append(el("em", {}, renderInline(m[6], inLink)));
    else if (m[7] !== undefined) frag.append(el("em", {}, renderInline(m[7], inLink)));
    else if (m[8] !== undefined) frag.append(inLink ? m[8] : mdLink(m[8], m[8]));
    else if (m[9] !== undefined) {
      if (inLink) { frag.append(m[0]); continue; }
      const t = m[9].toLowerCase();
      const chip = el("span", { className: "tag", textContent: "#" + m[9], onclick: () => pick(t) });
      paint(chip, t);
      frag.append(chip);
    }
  }
  if (last < text.length) frag.append(text.slice(last));
  return frag;
}

const MD_HR = /^\s*([-*_])(\s*\1){2,}\s*$/;
const MD_UL = /^\s*[-*+]\s+/;
const MD_OL = /^\s*\d+[.)]\s+/;
const MD_BLOCK = line => /^```/.test(line) || /^#{1,6}\s/.test(line) || /^>/.test(line) || MD_HR.test(line) || MD_UL.test(line) || MD_OL.test(line);

function renderMarkdown(text) {
  const frag = document.createDocumentFragment();
  const lines = text.split("\n");
  let i = 0, m;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    if (/^```/.test(line)) {
      const buf = [];
      for (i++; i < lines.length && !/^```/.test(lines[i]); i++) buf.push(lines[i]);
      i++; // closing fence
      frag.append(el("pre", {}, el("code", { textContent: buf.join("\n") })));
    } else if ((m = /^(#{1,6})\s+(.*)$/.exec(line))) {
      frag.append(el("h" + m[1].length, {}, renderInline(m[2])));
      i++;
    } else if (MD_HR.test(line)) {
      frag.append(el("hr"));
      i++;
    } else if (/^>/.test(line)) {
      const buf = [];
      for (; i < lines.length && /^>/.test(lines[i]); i++) buf.push(lines[i].replace(/^>\s?/, ""));
      frag.append(el("blockquote", {}, renderInline(buf.join("\n"))));
    } else if (MD_UL.test(line) || MD_OL.test(line)) {
      const ordered = MD_OL.test(line), re = ordered ? MD_OL : MD_UL;
      const list = el(ordered ? "ol" : "ul");
      for (; i < lines.length && re.test(lines[i]); i++) list.append(el("li", {}, renderInline(lines[i].replace(re, ""))));
      frag.append(list);
    } else {
      const buf = [line];
      for (i++; i < lines.length && lines[i].trim() && !MD_BLOCK(lines[i]); i++) buf.push(lines[i]);
      frag.append(el("div", { className: "md-p" }, renderInline(buf.join("\n"))));
    }
  }
  return frag;
}

function bodyEl(n, card) {
  const box = el("div", { className: "body" });
  const lines = n.body.split("\n");
  let pending = [];
  const flush = () => { if (pending.length) { box.append(el("div", { className: "txtblock" }, renderMarkdown(pending.join("\n")))); pending = []; } };
  lines.forEach((line, i) => {
    const m = ITEM.exec(line);
    if (!m) { pending.push(line); return; }
    flush();
    const done = m[2] !== " ";
    const cb = el("button", { className: "box", type: "button", textContent: done ? "✓" : "", title: done ? "Mark not done" : "Mark done", "aria-pressed": String(done),
      onclick: async () => {
        const l = n.body.split("\n");
        l[i] = l[i].replace(/\[( |x|X)\]/, done ? "[ ]" : "[x]");
        try { const up = await put({ ...n, body: l.join("\n") }, {}); card.replaceWith(noteEl(up, card.classList.contains("compact"))); refresh(); }
        catch (e) { alert(e.message); }
      } });
    box.append(el("div", { className: "item" + (done ? " done" : "") }, cb, el("span", { className: "txt" }, renderInline(m[4]))));
  });
  flush();
  return box;
}

function reminderChip(n, card) {
  const now = Date.now(), due = n.remind_at * 1000 <= now && !n.reminder_done;
  const label = n.reminder_done ? "Reminded " + fmtWhen(n.remind_at) : (due ? "Due " : "") + fmtWhen(n.remind_at);
  const chip = el("div", { className: "rem-chip" + (due ? " due" : "") + (n.reminder_done ? " sent" : "") });
  chip.append("🔔 " + label + (n.repeat ? " · " + n.repeat : ""));
  const act = (text, fn) => chip.append(el("button", { type: "button", textContent: text, onclick: async () => {
    try { card.replaceWith(noteEl(await fn(), card.classList.contains("compact"))); loadFilters(); } catch (e) { alert(e.message); } } }));
  if (!n.reminder_done) {
    act("Done", () => completeReminder(n));
    act("Snooze 10m", () => put(n, { remind_at: Math.floor(Date.now() / 1000) + 600 }));
  } else act("Re-arm", () => put(n, { remind_at: Math.floor(Date.now() / 1000) + 3600 }));
  return chip;
}

// Finish a reminder: repeating ones move to the next occurrence, one-offs are marked done.
function completeReminder(n) {
  return n.repeat ? put(n, { remind_at: nextOccurrence(n.remind_at, n.repeat) }) : put(n, { reminder_done: true });
}

function dueChip(n, open) {
  const now = Date.now(), sameDay = new Date(n.due_at * 1000).toDateString() === new Date().toDateString();
  let cls = "rem-chip", label = "Due " + fmtDue(n.due_at);
  if (!open) cls += " sent";
  else if (n.due_at * 1000 < now) { cls += " due"; label = "Overdue " + fmtDue(n.due_at); }
  else if (sameDay) { cls += " due"; label = "Due today"; }
  return el("div", { className: cls, textContent: "📅 " + label });
}

function noteEl(n, compact) {
  const card = el("div", { className: "note" + (compact ? " compact" : "") });
  if (n.tags.length) paint(card, n.tags[0]); else card.classList.add("plain");
  const stats = isList(n.body) ? listStats(n.body) : null;
  card.dataset.id = n.id;
  // Same rule as the server: a card with any unchecked item is in the pinned group above the rest.
  card.dataset.group = stats && stats.total > stats.done ? "pin" : "rest";
  const created = new Date(n.created_at * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
  const acts = el("div", { className: "acts" },
    el("button", { textContent: "Record", title: "Record a voice note on this card", onclick: () => {
      if (card.querySelector(".recorder")) return;
      card.querySelector(".content").prepend(recorderEl({
        onAttach: async p => {
          try { await uploadImage(n.id, p); } catch (e) { alert("Couldn't attach the recording: " + e.message); }
          refresh();
        },
        autostart: true,
      }));
    } }),
    el("button", { textContent: "Edit", onclick: () => edit(card, n, compact) }),
    el("button", { textContent: "Delete", onclick: async () => {
      if (confirm("Delete this note?")) { await api("/api/notes/" + n.id, { method: "DELETE" }); refresh(); }
    } }));
  const lead = el("span", { className: "lead" });
  if (filter !== "@reminders") {   // the reminders view is sorted by time, so cards are not moved there
    const grip = el("button", { className: "grip", type: "button", textContent: "⠿", title: "Drag to move. Or press Space, then the arrow keys.",
      "aria-label": "Move card", "aria-roledescription": "drag handle" });
    grip.addEventListener("pointerdown", e => startCardDrag(e, card));
    grip.addEventListener("keydown", e => keyMoveCard(e, card));
    lead.append(grip);
  }
  lead.append(el("span", { textContent: created }));
  const head = el("div", { className: "head" }, lead);
  if (stats) {
    const pct = stats.total ? Math.round(100 * stats.done / stats.total) : 0;
    const bar = el("span", { className: "bar2" }, el("i", { style: `width:${pct}%` }));
    head.append(el("span", { className: "progress", title: pct + "% done" }, bar, `${stats.done}/${stats.total}`));
  }
  head.append(acts);
  const content = el("div", { className: "content" });
  if (n.remind_at) content.append(reminderChip(n, card));
  if (n.due_at && stats) content.append(dueChip(n, stats.total - stats.done > 0));
  const media = mediaEl(n, compact);
  if (media) content.append(media);
  const body = bodyEl(n, card);
  content.append(body);
  // Every tag is shown on the card (written in the text or added by a rule); click one to filter.
  if (n.tags.length) {
    content.append(el("div", { className: "autotags" },
      ...n.tags.map(t => { const c = el("span", { className: "tag", textContent: "#" + t, title: "Show #" + t, onclick: () => pick(t) }); paint(c, t); return c; })));
  }
  if (compact && (n.body.length > 400 || n.body.split("\n").length > 10)) {
    body.classList.add("clamp");
    const more = el("button", { className: "linkbtn", type: "button", textContent: "Show more", onclick: () => {
      const open = body.classList.toggle("clamp"); more.textContent = open ? "Show more" : "Show less"; } });
    content.append(more);
  }
  if (stats) {
    const add = el("input", { className: "additem", placeholder: "Add item, press Enter" });
    add.addEventListener("keydown", async e => {
      if (e.key !== "Enter" || !add.value.trim()) return;
      try { const up = await put({ ...n, body: n.body.replace(/\s+$/, "") + "\n- [ ] " + add.value.trim() }, {}); card.replaceWith(noteEl(up, compact)); refresh(); }
      catch (err) { alert(err.message); }
    });
    content.append(add);
  }
  card.append(head, content);
  // Each card is a drop target: dropped images are added to it.
  card.addEventListener("dragover", e => { if (hasFiles(e)) { e.preventDefault(); card.classList.add("drop-on"); } });
  card.addEventListener("dragleave", e => { if (!card.contains(e.relatedTarget)) card.classList.remove("drop-on"); });
  card.addEventListener("drop", e => {
    if (!hasFiles(e)) return;
    e.preventDefault(); e.stopPropagation();
    card.classList.remove("drop-on");
    document.body.classList.remove("dragging-files");
    addToCard(n, [...e.dataTransfer.files], card);
  });
  return card;
}

function edit(card, n, compact) {
  const ta = el("textarea", { value: n.body });
  const wf = whenFields(n);
  const isTodo = n.kind === "todo";
  const df = isTodo ? dueFields(n) : null;
  const msg = el("div", { className: "err" });
  const sugg = el("div", { className: "sugg" });
  attachSuggest([ta], ta, sugg);
  const save = el("button", { className: "btn primary", textContent: "Save", onclick: async () => {
    const v = wf.value();
    const patch = { body: ta.value, remind_at: v.remind_at || -1, repeat: v.repeat };
    if (df) patch.due_at = df.value() || -1;
    try { await api("/api/notes/" + n.id, { method: "PUT", body: JSON.stringify(patch) }); }
    catch (e) { msg.textContent = e.message; return; }
    refresh();
  } });
  const mediaEdit = n.attachments.length ? el("div", { className: "media-edit" }, ...n.attachments.map(a => {
    const isAudio = a.mime.startsWith("audio/");
    const tile = el("span", { className: "tile" + (isAudio ? " audio" : ""), title: a.name || "" },
      isAudio ? "♪ " + fmtDur(a.duration_ms) : el("img", { src: mediaUrl(a, true), alt: a.name || "image" }));
    tile.append(el("button", { className: "x", type: "button", textContent: "×", title: "Remove from card", onclick: async () => {
      if (!confirm(isAudio ? "Remove this recording from the card?" : "Remove this image from the card?")) return;
      try { await api("/api/media/" + a.id, { method: "DELETE" }); tile.remove(); } catch (e) { msg.textContent = e.message; }
    } }));
    return tile;
  })) : null;
  card.replaceChildren(el("div", { className: "content" }, ta, ...(mediaEdit ? [mediaEdit] : []), sugg,
    ...(df ? [df.node] : []),
    el("div", { className: "label", style: "margin-top:10px", textContent: "Reminder (leave blank for none)" }), wf.node,
    el("div", { className: "row" }, save, el("button", { className: "btn", textContent: "Cancel", onclick: () => refresh() }), msg)));
  ta.focus();
}

