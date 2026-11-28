// Themes, views and modes, then start-up.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- chrome: themes, views, modes ---------- */
function mark(groupId, attr, val) { document.querySelectorAll(`#${groupId} button`).forEach(b => b.classList.toggle("on", b.dataset[attr] === val)); }
function setTheme(t) { document.documentElement.dataset.theme = t; localStorage.setItem("chuck-theme", t); mark("themes", "t", t); }
function setView(v) { view = v; localStorage.setItem("chuck-view", v); mark("views", "v", v); $("layouts").hidden = v !== "board"; refresh(); }
function setLayout(l) {
  boardLayout = l;
  localStorage.setItem("chuck-layout", l);
  mark("layouts", "l", l);
  // Switching only changes the board's class; the packer re-measures every card.
  if (view === "board") {
    $("out").className = "grid " + l;
    document.querySelectorAll("#out > .note").forEach(packCard);
  }
}
function setMode(m) { switchType(m, composerText()); }
document.querySelectorAll("#themes button").forEach(b => b.onclick = () => setTheme(b.dataset.t));
document.querySelectorAll("#views button").forEach(b => b.onclick = () => setView(b.dataset.v));
document.querySelectorAll("#modes button").forEach(b => b.onclick = () => setMode(b.dataset.m));
setTheme(document.documentElement.dataset.theme); mark("views", "v", view); mark("modes", "m", mode);
mark("layouts", "l", boardLayout); $("layouts").hidden = view !== "board";
document.querySelectorAll("#layouts button").forEach(b => b.onclick = () => setLayout(b.dataset.l));

let timer;
["q", "from", "to"].forEach(id => $(id).addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(refresh, 200); }));
$("more").onclick = () => loadFeed(false);
setupMedia(); renderComposer(); setupNotify(); setupRules(); refresh();
checkReminders(); setInterval(checkReminders, 30000);
