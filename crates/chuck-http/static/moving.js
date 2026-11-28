// Moving cards: drag and keyboard.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- moving cards ----------
   Drag a card by its grip: it floats under the pointer, a dashed placeholder shows where it will land, and the board
   reflows (Columns re-packs). Dropping puts the card in the placeholder's place at once and saves in the background;
   if saving fails the card goes back. Open todos stay above the other cards, so a card only moves within its group.
   Keyboard: focus the grip, Space to pick up, arrow keys to move, Space to drop, Escape to cancel. */
const sameGroup = (a, b) => b && b.classList.contains("note") && !b.classList.contains("placeholder") && !b.classList.contains("dragging") && b.dataset.group === a.dataset.group;
function groupNeighbours(card) {
  let p = card.previousElementSibling, n = card.nextElementSibling;
  while (p && !sameGroup(card, p)) p = p.previousElementSibling;
  while (n && !sameGroup(card, n)) n = n.nextElementSibling;
  return { prev: p ? p.dataset.id : null, next: n ? n.dataset.id : null };
}
const say = text => { $("live").textContent = text; };
function originOf(card) { return { parent: card.parentElement, nextNode: card.nextSibling, ...groupNeighbours(card) }; }
function putBack(card, o) { o.parent.insertBefore(card, o.nextNode && o.nextNode.parentNode === o.parent ? o.nextNode : null); }

async function saveMove(card, o) {
  const { prev, next } = groupNeighbours(card);
  if (prev === o.prev && next === o.next) return;   // dropped where it started
  try { await api(`/api/notes/${card.dataset.id}/move`, { method: "POST", body: JSON.stringify({ prev, next }) }); }
  catch (e) { putBack(card, o); say("Move failed."); alert("Couldn't move the card: " + e.message); }
}

function startCardDrag(e, card) {
  if (e.button > 0 || card.classList.contains("picked")) return;
  e.preventDefault();
  const board = $("out"), rect = card.getBoundingClientRect();
  const o = originOf(card);
  const ph = el("div", { className: "note placeholder" + (card.classList.contains("compact") ? " compact" : "") });
  ph.dataset.group = card.dataset.group;
  ph.style.height = rect.height + "px";
  card.before(ph);
  Object.assign(card.style, { width: rect.width + "px", height: rect.height + "px", left: rect.left + "px", top: rect.top + "px" });
  card.classList.add("dragging");
  document.body.classList.add("moving-card");
  document.body.append(card);   // out of the board, so the board reflows around the placeholder
  const dx = e.clientX - rect.left, dy = e.clientY - rect.top;
  let x = e.clientX, y = e.clientY, raf = 0;

  const place = () => {
    const t = document.elementFromPoint(x, y)?.closest("#out > .note");
    if (!t || t === ph || t.dataset.group !== card.dataset.group) return;
    const r = t.getBoundingClientRect();
    const before = board.classList.contains("rows") ? x < r.left + r.width / 2 : y < r.top + r.height / 2;
    if (before) { if (t.previousElementSibling !== ph) t.before(ph); }
    else if (t.nextElementSibling !== ph) t.after(ph);
  };
  const tick = () => {   // scroll when the pointer is near the top or bottom of the window
    const edge = 70, speed = y < edge ? -(edge - y) / 3 : y > innerHeight - edge ? (y - innerHeight + edge) / 3 : 0;
    if (speed) { scrollBy(0, speed); place(); }
    raf = requestAnimationFrame(tick);
  };
  const move = ev => { x = ev.clientX; y = ev.clientY; card.style.left = x - dx + "px"; card.style.top = y - dy + "px"; place(); };
  const finish = cancel => {
    removeEventListener("pointermove", move);
    removeEventListener("pointerup", up);
    removeEventListener("pointercancel", cancelled);
    removeEventListener("keydown", esc, true);
    cancelAnimationFrame(raf);
    card.classList.remove("dragging");
    document.body.classList.remove("moving-card");
    card.style.width = card.style.height = card.style.left = card.style.top = "";
    if (cancel) { ph.remove(); putBack(card, o); return; }
    ph.replaceWith(card);
    saveMove(card, o);
  };
  const up = () => finish(false);
  const cancelled = () => finish(true);
  const esc = ev => { if (ev.key === "Escape") { ev.preventDefault(); finish(true); } };
  addEventListener("pointermove", move);
  addEventListener("pointerup", up);
  addEventListener("pointercancel", cancelled);
  addEventListener("keydown", esc, true);
  raf = requestAnimationFrame(tick);
}

function keyMoveCard(e, card) {
  const grip = () => card.querySelector(".grip");
  if (e.key === " " || e.key === "Enter") {
    e.preventDefault();
    if (!card.classList.contains("picked")) {
      card._origin = originOf(card);
      card.classList.add("picked");
      say("Card picked up. Arrow keys move it, Space drops it, Escape cancels.");
    } else {
      card.classList.remove("picked");
      saveMove(card, card._origin);
      say("Card dropped.");
    }
    return;
  }
  if (!card.classList.contains("picked")) return;
  if (e.key === "Escape") {
    e.preventDefault();
    card.classList.remove("picked");
    putBack(card, card._origin);
    grip()?.focus();
    say("Move cancelled.");
    return;
  }
  const back = e.key === "ArrowUp" || e.key === "ArrowLeft", fwd = e.key === "ArrowDown" || e.key === "ArrowRight";
  if (!back && !fwd) return;
  e.preventDefault();
  let t = back ? card.previousElementSibling : card.nextElementSibling;
  while (t && !sameGroup(card, t)) t = back ? t.previousElementSibling : t.nextElementSibling;
  if (!t) { say(back ? "Already first in its group." : "Already last in its group."); return; }
  back ? t.before(card) : t.after(card);
  grip()?.focus();   // moving the element can drop focus
  card.scrollIntoView({ block: "nearest" });
}

