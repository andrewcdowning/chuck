// Loading and laying out the feed and the board.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- loading: feed and board ---------- */
function params(extra = {}) {
  const p = new URLSearchParams();
  if ($("q").value.trim()) p.set("q", $("q").value.trim());
  if ($("from").value) p.set("from", Math.floor(new Date($("from").value + "T00:00:00").getTime() / 1000));
  if ($("to").value) p.set("to", Math.floor(new Date($("to").value + "T23:59:59").getTime() / 1000));
  Object.entries(extra).forEach(([k, v]) => p.set(k, v));
  return p;
}

/* Column packing: each card's grid span follows its real height. A ResizeObserver keeps it right as images load,
   cards are edited or items ticked; a MutationObserver picks up cards added or replaced anywhere in the board. */
const PACK_ROW = 2, PACK_GAP = 14;
function packCard(card) {
  if (!card.parentElement || !card.parentElement.classList.contains("cols")) { card.style.gridRowEnd = ""; return; }
  const h = card.getBoundingClientRect().height;
  card.style.gridRowEnd = `span ${Math.max(1, Math.ceil((h + PACK_GAP) / PACK_ROW))}`;
}
const packer = new ResizeObserver(entries => entries.forEach(e => packCard(e.target)));
// Cards are re-packed as soon as they change, without waiting for a frame: when added or replaced, when anything
// inside them changes (editing, a new checklist item, "Show more"), and when an image in them finishes loading.
// The ResizeObserver above is the backstop for anything else (for example resizing a text box by hand).
const boardEl = document.getElementById("out");
new MutationObserver(muts => {
  const touched = new Set();
  for (const m of muts) {
    if (m.target === boardEl) {
      m.addedNodes.forEach(n => { if (n.classList?.contains("note")) { touched.add(n); packer.observe(n); } });
      m.removedNodes.forEach(n => { if (n.classList?.contains("note")) packer.unobserve(n); });
    } else {
      const card = m.target.closest?.("#out > .note") || m.target.parentElement?.closest("#out > .note");
      if (card) touched.add(card);
    }
  }
  touched.forEach(packCard);
}).observe(boardEl, { childList: true, subtree: true, attributes: true, attributeFilter: ["class"] });
boardEl.addEventListener("load", e => { if (e.target.tagName === "IMG") { const c = e.target.closest("#out > .note"); if (c) packCard(c); } }, true);

let feedSeq = 0;   // only the newest load may write to the board (fast view switches or typing start several)
async function loadFeed(reset) {
  const board = view === "board";
  const mine = ++feedSeq;
  const extra = { limit: PAGE, offset: reset ? 0 : offset };
  if (filter === "@reminders") extra.reminders = "true"; else if (filter) extra.tag = filter;
  const { notes } = await api("/api/notes?" + params(extra));
  if (mine !== feedSeq) return;
  if (reset) {
    liveRecorders.forEach(r => { if (r !== composerRecorder) r.cleanup(); });
    offset = 0;
    $("out").replaceChildren();
  }
  $("out").className = board ? "grid " + boardLayout : "";
  notes.forEach(n => $("out").append(cardEl(n, board)));
  offset += notes.length;
  if (reset && !notes.length) $("out").append(el("div", { className: "empty", textContent: filter === "@reminders" ? "No pending reminders" : "Nothing here yet" }));
  $("more").hidden = notes.length < PAGE;
}

async function loadFilters() {
  const [{ tags }, { notes: rem }] = await Promise.all([api("/api/tags"), api("/api/notes?reminders=true&limit=200")]);
  pendingReminders = rem;
  const box = $("tags");
  const mk = (cls, key, label, count, dot) => {
    const b = el("button", { className: "pill " + cls + (filter === key ? " on" : ""), onclick: () => pick(key) },
      ...(dot ? [el("span", { className: "dot" })] : []), el("span", { className: "t", textContent: label }), el("span", { className: "n", textContent: count }));
    return b;
  };
  box.replaceChildren(mk("", "", "All", ""), mk("rem", "@reminders", "🔔 Reminders", rem.length));
  tags.forEach(t => { const b = mk("", t.tag, "#" + t.tag, t.count, true); paint(b, t.tag); box.append(b); });
}

let pendingReminders = [];
function pick(f) { filter = f; refresh(); }
function syncExport() {
  const p = params();
  p.delete("limit"); p.delete("offset");
  if (filter && filter !== "@reminders") p.set("tag", filter);
  if (filter === "@reminders") p.set("reminders", "true");
  $("export").href = "/api/export" + (p.toString() ? "?" + p : "");
}
async function refresh() {
  syncExport();
  try {
    await loadFeed(true);
    await loadFilters();
  } catch (e) { $("out").replaceChildren(el("div", { className: "err", textContent: e.message })); }
}

