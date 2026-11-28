// Tag suggestions while typing.
// One of the page scripts, loaded in order by index.html; they share one global scope. See crates/chuck-http/src/assets.rs.

/* ---------- tag suggestions ----------
   The server returns calibrated confidences. Accepting (click) or dismissing (x) a suggestion is logged and
   nudges that tag's future confidence. */
function sendFeedback(tag, action, confidence) {
  api("/api/suggest/feedback", { method: "POST", body: JSON.stringify({ tag, action, confidence }) }).catch(() => {});
}
// Explains an empty suggestion list, so silence does not look like a broken feature.
function noSuggestionHint(res) {
  const n = res.tagged_notes, notes = n === 1 ? "1 tagged note" : `${n} tagged notes`;
  if (!n) return "No tag suggestions yet. Chuck suggests tags you have used before, so tag a few notes with #tags first.";
  if (res.suggestions.length) return "No other tag suggestions.";
  return `No matching tags from your ${notes}.` + (res.calibrated ? "" : ` Suggestions get more reliable after about ${res.calibrate_at}.`);
}
function attachSuggest(fields, target, box) {
  let timer, seq = 0;
  const dismissed = new Set();
  const run = async () => {
    const text = fields.map(f => f.value).join("\n").trim();
    const mine = ++seq;
    if (text.length < 3) { box.replaceChildren(); return; }
    let res;
    try { res = await api("/api/suggest", { method: "POST", body: JSON.stringify({ text }) }); } catch { return; }
    if (mine !== seq) return;
    const list = res.suggestions.filter(s => !dismissed.has(s.tag));
    if (!list.length) { box.replaceChildren(el("span", { className: "hint", textContent: noSuggestionHint(res) })); return; }
    box.replaceChildren(...(list.length ? [el("span", { className: "label", textContent: "Suggested" })] : []), ...list.map(s => {
      const pctText = Math.round(s.confidence * 100) + "%";
      const chip = el("button", { className: "chip" + (s.tier === "high" ? " high" : ""), type: "button",
        title: `${pctText} likely` + (s.why.length ? ` because of: ${s.why.join(", ")}` : ""),
        onclick: () => {
          const v = target.value.replace(/\s+$/, "");
          target.value = v + (v ? " " : "") + "#" + s.tag + " ";
          sendFeedback(s.tag, "accepted", s.confidence);
          target.focus(); run();
        } }, "#" + s.tag, el("span", { className: "pct", textContent: pctText }));
      paint(chip, s.tag);
      const x = el("button", { className: "chipx", type: "button", textContent: "×", title: "Not relevant",
        onclick: () => { dismissed.add(s.tag); sendFeedback(s.tag, "dismissed", s.confidence); run(); } });
      const wrap = el("span", { className: "chipwrap" }, chip, x);
      paint(wrap, s.tag);
      return wrap;
    }));
  };
  fields.forEach(f => f.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(run, 350); }));
}

