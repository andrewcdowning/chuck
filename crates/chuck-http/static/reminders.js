// Reminder alerts, while the tab is open.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- reminder alerts (fire while the tab is open) ---------- */
const fired = new Set();
function toast(n) {
  const t = el("div", { className: "toast" }, el("div", { textContent: "🔔 " + n.body.replace(/\s+/g, " ").slice(0, 140) }));
  const close = () => t.remove();
  t.append(el("div", { className: "row" },
    el("button", { textContent: "OK", onclick: close }),
    el("button", { textContent: "Snooze 10m", onclick: async () => { close(); await put(n, { remind_at: Math.floor(Date.now() / 1000) + 600 }); fired.delete(n.id); refresh(); } })));
  $("toasts").append(t);
}
async function checkReminders() {
  try { pendingReminders = (await api("/api/notes?reminders=true&limit=200")).notes; } catch { return; }
  const due = pendingReminders.filter(n => n.remind_at * 1000 <= Date.now() && !fired.has(n.id + ":" + n.remind_at));
  for (const n of due) {
    fired.add(n.id + ":" + n.remind_at);
    toast(n);
    if ("Notification" in window && Notification.permission === "granted") new Notification("Chuck reminder", { body: n.body.slice(0, 200) });
    try { await completeReminder(n); } catch {}
  }
  if (due.length) refresh();
}
function setupNotify() {
  const b = $("notify");
  if (!("Notification" in window) || Notification.permission !== "default") return;
  b.hidden = false;
  b.onclick = async () => { await Notification.requestPermission(); b.hidden = true; };
}

