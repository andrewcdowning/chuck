// The sidebar panels: auto-tag rules and the account (export, delete, administrators).
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- auto-tag rules ---------- */
async function loadRules() {
  const box = $("rulelist");
  let rules;
  try { rules = (await api("/api/rules")).rules; } catch (e) { box.textContent = e.message; return; }
  box.replaceChildren(...(rules.length ? rules.map(r => {
    const chip = el("span", { className: "tag", textContent: "#" + r.tag }); paint(chip, r.tag);
    return el("div", { className: "rule" }, el("span", { className: "kw", textContent: r.pattern }), "→", chip,
      el("button", { className: "linkbtn", type: "button", textContent: "Remove", onclick: async () => { await api("/api/rules/" + r.id, { method: "DELETE" }); loadRules(); } }));
  }) : [el("div", { className: "hint", textContent: "No rules yet." })]));
}
function setupRules() {
  const msg = $("rule-msg");
  $("rule-add").onclick = async () => {
    msg.textContent = "";
    try {
      await api("/api/rules", { method: "POST", body: JSON.stringify({ pattern: $("rule-kw").value, tag: $("rule-tag").value }) });
      $("rule-kw").value = ""; $("rule-tag").value = ""; loadRules();
    } catch (e) { msg.textContent = e.message; }
  };
  $("rule-apply").onclick = async () => {
    msg.textContent = "";
    try { const r = await api("/api/rules/apply", { method: "POST" }); msg.textContent = r.changed + " note(s) updated."; refresh(); }
    catch (e) { msg.textContent = e.message; }
  };
  loadRules();
  setupAccount();
}

/* ---------- account ---------- */
const fmtBytes = n => n < 1024 ? n + " B" : n < 1048576 ? (n / 1024).toFixed(0) + " KB" : (n / 1048576).toFixed(1) + " MB";
async function loadUsers() {
  const box = $("userlist");
  let users;
  try { users = (await api("/api/admin/users")).users; } catch (e) { box.textContent = e.message; return; }
  const post = async (path, body) => { try { await api(path, { method: "POST", body: JSON.stringify(body) }); } catch (e) { $("acct-msg").textContent = e.message; } loadUsers(); };
  box.replaceChildren(...users.map(u => {
    const label = (u.display_name ? u.display_name + " (" + u.id + ")" : u.id) + (u.role === "admin" ? " [admin]" : "") + (u.status !== "active" ? " [" + u.status + "]" : "");
    const row = el("div", { className: "rule" }, el("span", { textContent: label }),
      el("span", { className: "hint", textContent: u.notes + " notes, " + u.attachments + " files, " + fmtBytes(u.storage_bytes) }));
    if (u.id !== ME.id) {
      row.append(
        ...(u.status === "deleting" ? [] : [el("button", { className: "linkbtn", type: "button", textContent: u.status === "active" ? "Disable" : "Enable", onclick: () => post("/api/admin/users/status", { id: u.id, active: u.status !== "active" }) })]),
        el("button", { className: "linkbtn", type: "button", textContent: "Delete", onclick: () => { if (confirm("Delete " + u.id + " and everything they own? This cannot be undone.")) post("/api/admin/users/delete", { id: u.id }); } }));
    }
    return row;
  }));
}
let ME = null;
async function setupAccount() {
  const msg = $("acct-msg");
  try { ME = await api("/api/me"); } catch (e) { $("acct-who").textContent = e.message; return; }
  $("acct-who").textContent = "Signed in as " + (ME.display_name ? ME.display_name + " (" + ME.id + ")" : ME.id) + (ME.role === "admin" ? ", administrator" : "");
  $("acct-delete").onclick = async () => {
    msg.textContent = "";
    if ($("acct-confirm").value !== ME.id) { msg.textContent = "Type your account id exactly to confirm."; return; }
    try { await api("/api/me", { method: "DELETE", body: JSON.stringify({ confirm: ME.id }) }); location.reload(); }
    catch (e) { msg.textContent = e.message; }
  };
  if (ME.role === "admin") { $("admin").hidden = false; loadUsers(); }
}

