// Small helpers: DOM building, API calls, dates, checklists, reminder time parsing.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

const $ = id => document.getElementById(id);
const PAGE = 50;
let filter = "";            // "" | tag name | "@reminders"
let view = new URLSearchParams(location.search).get("view") || localStorage.getItem("chuck-view") || "feed";
let boardLayout = new URLSearchParams(location.search).get("layout") || localStorage.getItem("chuck-layout") || "cols";
let mode = "note";
let offset = 0;

/* ---------- helpers ---------- */
const PALETTE = ["#0ea5e9","#22c55e","#f59e0b","#ef4444","#f43f5e","#14b8a6","#f97316","#06b6d4","#84cc16","#d946ef","#94a3b8","#FB7185","#FDBA74","#A3E635","#C4B5FD"];
function accent(t) { let h = 5381; for (const c of t) h = ((h << 5) + h + c.codePointAt(0)) >>> 0; return PALETTE[h % PALETTE.length]; }
function paint(node, t) { node.style.setProperty("--accent", accent(t)); node.style.setProperty("--accent-ink", "#111"); }

async function api(path, opts) {
  const r = await fetch(path, { headers: { "content-type": "application/json" }, ...opts });
  if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.statusText);
  return r.status === 204 ? null : r.json();
}
// DOM is built with textContent only; note bodies are never injected as HTML.
function el(tag, props = {}, ...kids) { const e = Object.assign(document.createElement(tag), props); kids.forEach(k => e.append(k)); return e; }
const put = (n, patch) => api("/api/notes/" + n.id, { method: "PUT", body: JSON.stringify({ body: n.body, ...patch }) });

const pad = x => String(x).padStart(2, "0");
const toLocalInput = sec => { const d = new Date(sec * 1000); return `${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`; };
const fromLocalInput = v => v ? Math.floor(new Date(v).getTime() / 1000) : 0;
const toDateInput = sec => toLocalInput(sec).slice(0, 10);
const fromDateInput = v => v ? Math.floor(new Date(v + "T23:59:59").getTime() / 1000) : 0; // due at the end of that local day
const fmtDue = sec => {
  const d = new Date(sec * 1000);
  return d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric", ...(d.getFullYear() === new Date().getFullYear() ? {} : { year: "numeric" }) });
};
const fmtWhen = sec => new Date(sec * 1000).toLocaleString([], { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });

/* ---------- checklist parsing ---------- */
const ITEM = /^(\s*[-*]\s+\[)([ xX])(\]\s?)(.*)$/;
const isList = body => body.split("\n").some(l => ITEM.test(l));
function listStats(body) {
  let total = 0, done = 0;
  body.split("\n").forEach(l => { const m = ITEM.exec(l); if (m) { total++; if (m[2] !== " ") done++; } });
  return { total, done };
}

/* ---------- reminder time parsing and recurrence ---------- */
const DAYS = ["sunday","monday","tuesday","wednesday","thursday","friday","saturday"];
function nextOccurrence(atSec, repeat, nowMs = Date.now()) {
  const d = new Date(atSec * 1000);
  for (let i = 0; i < 4000 && d.getTime() <= nowMs; i++) {
    if (repeat === "daily") d.setDate(d.getDate() + 1);
    else if (repeat === "weekdays") { do d.setDate(d.getDate() + 1); while ([0, 6].includes(d.getDay())); }
    else if (repeat === "weekly") d.setDate(d.getDate() + 7);
    else if (repeat === "monthly") d.setMonth(d.getMonth() + 1);
    else break;
  }
  return Math.floor(d.getTime() / 1000);
}
// Understands: "in 30 minutes", "at 3pm", "tomorrow at 9am", "friday 2:30pm", "every monday at 8am", "every day", "daily".
function parseWhen(text) {
  const t = text.toLowerCase(), now = new Date();
  const rel = /\bin\s+(\d+)\s*(min|mins|minutes?|hours?|hrs?|days?|weeks?)\b/.exec(t);
  if (rel) {
    const n = +rel[1], u = rel[2];
    const ms = /^min/.test(u) ? n * 6e4 : /^(h)/.test(u) ? n * 36e5 : /^d/.test(u) ? n * 864e5 : n * 6048e5;
    return { at: Math.floor((now.getTime() + ms) / 1000), repeat: "" };
  }
  const tm = /\b(?:at\s+)?(\d{1,2})(?::(\d{2}))?\s*(am|pm)\b/.exec(t) || /\bat\s+(\d{1,2}):(\d{2})()\b/.exec(t);
  const every = /\bevery\s+(day|weekday|weekdays|week|month|sunday|monday|tuesday|wednesday|thursday|friday|saturday)\b/.exec(t);
  const dayName = every && DAYS.includes(every[1]) ? every[1] : (/\b(sunday|monday|tuesday|wednesday|thursday|friday|saturday)\b/.exec(t) || [])[1];
  const tomorrow = /\btomorrow\b/.test(t);
  const daily = /\bdaily\b/.test(t);
  if (!tm && !every && !dayName && !tomorrow && !daily) return null;

  let h = 9, m = 0;
  if (tm) {
    h = +tm[1]; m = tm[2] ? +tm[2] : 0;
    if (tm[3] === "pm" && h < 12) h += 12;
    if (tm[3] === "am" && h === 12) h = 0;
    if (h > 23 || m > 59) return null;
  }
  let repeat = "";
  if (daily || (every && every[1] === "day")) repeat = "daily";
  else if (every && /^weekdays?$/.test(every[1])) repeat = "weekdays";
  else if (every && (every[1] === "week" || DAYS.includes(every[1]))) repeat = "weekly";
  else if (every && every[1] === "month") repeat = "monthly";

  const d = new Date(now); d.setHours(h, m, 0, 0);
  if (tomorrow) d.setDate(d.getDate() + 1);
  if (dayName) {
    let ahead = (DAYS.indexOf(dayName) - now.getDay() + 7) % 7;
    if (ahead === 0 && d.getTime() <= now.getTime()) ahead = 7;
    d.setDate(d.getDate() + ahead);
  }
  let at = Math.floor(d.getTime() / 1000);
  if (at * 1000 <= now.getTime()) at = repeat ? nextOccurrence(at, repeat) : at + 86400;
  return { at, repeat };
}

/* ---------- reminder field group (used by composer and editor) ---------- */
function whenFields(note) {
  const when = el("input", { type: "datetime-local", title: "Remind at" });
  const rep = el("select", { title: "Repeat" });
  [["", "No repeat"], ["daily", "Daily"], ["weekdays", "Weekdays"], ["weekly", "Weekly"], ["monthly", "Monthly"]]
    .forEach(([v, l]) => rep.append(el("option", { value: v, textContent: l })));
  if (note && note.remind_at) { when.value = toLocalInput(note.remind_at); rep.value = note.repeat || ""; }
  const quick = (label, fn) => el("button", { className: "btn sm", type: "button", textContent: label, onclick: () => { when.value = toLocalInput(fn()); } });
  const inMin = n => () => Math.floor(Date.now() / 1000) + n * 60;
  const tomorrow9 = () => { const d = new Date(); d.setDate(d.getDate() + 1); d.setHours(9, 0, 0, 0); return Math.floor(d / 1000); };
  const node = el("div", { className: "row" }, when, rep, quick("+30m", inMin(30)), quick("+1h", inMin(60)), quick("Tomorrow 9am", tomorrow9));
  return { node, value: () => ({ remind_at: fromLocalInput(when.value), repeat: rep.value }), has: () => !!when.value, setFrom: p => { when.value = toLocalInput(p.at); rep.value = p.repeat; } };
}

