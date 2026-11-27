//! Tag suggestions learned from the owner's own tagged notes.
//!
//! A naive Bayes model over the words of a note and the tags already written on it. Raw naive Bayes scores are
//! overconfident (words are correlated), so the evidence and prior weights are fitted by leave-one-out on the
//! owner's own notes and a monotone (isotonic) map turns the score into a probability that can be read as
//! "about this often right".
//! Only tags the user wrote (or accepted from a suggestion, which writes them into the text) are training
//! labels; tags added by auto-tag rules are not, so a wrong rule cannot teach the model.
//!
//! Pure functions and plain data, no storage or network: any `NoteStore` can use it.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Below this confidence a suggestion is not shown.
pub const MIN_SHOW: f64 = 0.30;
/// At or above this confidence a suggestion is labelled "high".
pub const HIGH: f64 = 0.70;

const MIN_DOCS_TO_CALIBRATE: usize = 12;
const MIN_CALIBRATION_SAMPLES: usize = 30;
const UNCALIBRATED_CAP: f64 = 0.60;
const DEFAULT_TEMP: f64 = 0.5;
const SMOOTH: f64 = 1.0;
const LLR_CLAMP: f64 = 4.0;
/// A tag needs at least one feature this informative, or it would be suggested just for being common.
const MIN_EVIDENCE: f64 = 0.5;
const MAX_WORDS: usize = 60;
const CALIBRATION_SAMPLE: usize = 150;
const CALIBRATION_TAGS: usize = 80;
const TEMPS: [f64; 11] = [0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.7, 1.0, 1.5, 2.0];
/// Weight on the tag's base rate. Below 1 stops very common tags from outranking better-matching ones.
const PRIOR_WEIGHTS: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
const BIASES: [f64; 9] = [-3.0, -2.5, -2.0, -1.5, -1.0, -0.5, 0.0, 0.5, 1.0];
/// With at least this many leave-one-out samples the sigmoid is replaced by an isotonic map.
const MIN_ISO_SAMPLES: usize = 60;

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub tag: String,
    /// Estimated probability (0 to 1) that the tag fits.
    pub confidence: f64,
    /// "high" or "medium".
    pub tier: &'static str,
    /// The strongest evidence, for display ("because ...").
    pub why: Vec<String>,
}

/// A training document: the tags the user wrote on a note and the note text with the hashtags removed.
#[derive(Debug, Clone)]
pub struct Doc {
    pub labels: Vec<String>,
    pub text: String,
}

// ---------------------------------------------------------------- features

const STOP: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "have", "has", "had", "was", "were", "are", "but", "not", "you", "your",
    "about", "into", "need", "get", "got", "just", "then", "them", "they", "will", "would", "could", "should", "can", "its", "also",
    "what", "when", "where", "which", "who", "how", "why", "all", "any", "some", "more", "than", "too", "very", "our", "out", "off",
    "over", "per", "use", "using", "used", "like", "one", "two", "new", "now", "here", "there", "been", "being", "does", "did", "done",
];

/// Removes `#tag` tokens so a note's labels never appear among its own features.
pub fn strip_hashtags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut first = true;
        for tok in line.split_whitespace() {
            let is_tag = tok.starts_with('#') && tok.chars().nth(1).map_or(false, |c| c.is_alphanumeric());
            if is_tag {
                continue;
            }
            if !first {
                out.push(' ');
            }
            out.push_str(tok);
            first = false;
        }
    }
    out
}

fn stem(w: &str) -> String {
    if w.chars().count() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        w[..w.len() - 1].to_string()
    } else {
        w.to_string()
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3 && !w.chars().all(|c| c.is_ascii_digit()) && !STOP.contains(w))
        .map(|w| stem(w))
        .collect()
}

/// (distinct word stems, features): features are `w:<stem>`.
///
/// Adjacent word pairs were tried and removed: on a noisy synthetic corpus they lowered top-1 accuracy by 3 to 5
/// points. `chuck eval-tags` makes it cheap to try them again on real notes.
fn features(text: &str) -> (HashSet<String>, Vec<String>) {
    let mut words = HashSet::new();
    let mut base = Vec::new();
    for t in tokens(text) {
        if words.len() < MAX_WORDS && words.insert(t.clone()) {
            base.push(format!("w:{t}"));
        }
    }
    (words, base)
}

/// True if the tag's own name appears in the text ("aws" in a note that mentions AWS).
fn name_present(tag: &str, words: &HashSet<String>) -> bool {
    let parts: Vec<String> = tag.to_lowercase().split(|c| c == '-' || c == '_').filter(|p| !p.is_empty()).map(|p| stem(p)).collect();
    !parts.is_empty() && parts.iter().all(|p| words.contains(p))
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub labels: Vec<String>,
    words: HashSet<String>,
    base: Vec<String>,
}

impl Prepared {
    fn new(d: &Doc) -> Self {
        let (words, base) = features(&d.text);
        Self { labels: d.labels.clone(), words, base }
    }

    /// Features with the given written tags added as `t:<tag>` (what co-occurring tags say about each other).
    fn query_feats(&self, written: &[String]) -> Vec<String> {
        let mut f = self.base.clone();
        f.extend(written.iter().map(|t| format!("t:{t}")));
        f
    }
}

pub fn prepare(docs: &[Doc]) -> Vec<Prepared> {
    docs.iter().filter(|d| !d.labels.is_empty()).map(Prepared::new).collect()
}

// ---------------------------------------------------------------- model

fn bump(x: &mut u32, sign: i64) {
    *x = (*x as i64 + sign).max(0) as u32;
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

struct Scored {
    prior_logit: f64,
    /// Sum of clamped log-likelihood ratios over the query's features.
    sum: f64,
    /// Largest single log-likelihood ratio.
    best: f64,
    contribs: Vec<(String, f64)>,
}

#[derive(Default)]
pub struct Model {
    /// Number of training documents.
    n: u32,
    tag_n: HashMap<String, u32>,
    df: HashMap<String, u32>,
    /// tag -> feature -> documents with both.
    co: HashMap<String, HashMap<String, u32>>,
    /// tag -> (tagged documents mentioning the tag name, untagged documents mentioning it).
    name: HashMap<String, (u32, u32)>,
    /// Weight on the summed evidence.
    pub temp: f64,
    /// Weight on the tag's base rate.
    pub prior_w: f64,
    bias: f64,
    /// Isotonic map from raw score to probability, once there is enough data to fit one.
    iso: Option<Vec<Step>>,
    pub calibrated: bool,
    pub docs: usize,
}

impl Model {
    pub fn train(docs: &[Doc]) -> Model {
        Self::from_prepared(&prepare(docs))
    }

    pub fn from_prepared(prep: &[Prepared]) -> Model {
        let mut m = Self::counts(prep);
        m.calibrate(prep, None);
        m
    }

    /// Counts only, with default weights and no calibration.
    fn counts(prep: &[Prepared]) -> Model {
        let mut m = Model { temp: DEFAULT_TEMP, prior_w: 1.0, docs: prep.len(), ..Default::default() };
        for p in prep {
            for l in &p.labels {
                m.tag_n.entry(l.clone()).or_insert(0);
            }
        }
        for p in prep {
            m.apply(p, 1);
        }
        m
    }

    /// Adds (+1) or removes (-1) one document's counts. Symmetric, so leave-one-out is remove, predict, add.
    fn apply(&mut self, p: &Prepared, sign: i64) {
        bump(&mut self.n, sign);
        let mut feats = p.base.clone();
        feats.extend(p.labels.iter().map(|l| format!("t:{l}")));
        for f in &feats {
            bump(self.df.entry(f.clone()).or_insert(0), sign);
        }
        for t in &p.labels {
            bump(self.tag_n.entry(t.clone()).or_insert(0), sign);
            let m = self.co.entry(t.clone()).or_default();
            for f in &feats {
                bump(m.entry(f.clone()).or_insert(0), sign);
            }
        }
        let tags: Vec<String> = self.tag_n.keys().cloned().collect();
        for t in tags {
            if name_present(&t, &p.words) {
                let e = self.name.entry(t.clone()).or_insert((0, 0));
                if p.labels.contains(&t) {
                    bump(&mut e.0, sign);
                } else {
                    bump(&mut e.1, sign);
                }
            }
        }
    }

    fn score(&self, tag: &str, feats: &[String], words: &HashSet<String>) -> Option<Scored> {
        let nt = *self.tag_n.get(tag)? as f64;
        if nt < 1.0 {
            return None;
        }
        let n = self.n as f64;
        let prior = (nt + 0.5) / (n + 1.0);
        let prior_logit = (prior / (1.0 - prior)).ln();
        let own = format!("t:{tag}"); // a tag's own presence is never evidence for itself
        let co_t = self.co.get(tag);
        let mut sum = 0.0;
        let mut best = f64::MIN;
        let mut contribs = Vec::new();
        for f in feats {
            if *f == own {
                continue;
            }
            let df = *self.df.get(f).unwrap_or(&0) as f64;
            if df == 0.0 {
                continue; // never seen in training: no evidence either way
            }
            let c = co_t.and_then(|m| m.get(f)).copied().unwrap_or(0) as f64;
            let p1 = (c + SMOOTH) / (nt + 2.0 * SMOOTH);
            let p0 = ((df - c).max(0.0) + SMOOTH) / ((n - nt).max(0.0) + 2.0 * SMOOTH);
            let l = (p1 / p0).ln().clamp(-LLR_CLAMP, LLR_CLAMP);
            sum += l;
            best = best.max(l);
            contribs.push((f.clone(), l));
        }
        if name_present(tag, words) {
            let (a, b) = self.name.get(tag).copied().unwrap_or((0, 0));
            let p1 = (a as f64 + 0.5) / (nt + 1.0);
            let p0 = (b as f64 + 0.5) / ((n - nt).max(0.0) + 1.0);
            let l = (p1 / p0).ln().clamp(-LLR_CLAMP, LLR_CLAMP);
            sum += l;
            best = best.max(l);
            contribs.push(("name".to_string(), l));
        }
        if best == f64::MIN {
            best = 0.0;
        }
        Some(Scored { prior_logit, sum, best, contribs })
    }

    /// Score used for ranking. `offset` is the log-odds adjustment from accepted and dismissed suggestions.
    fn raw(&self, sc: &Scored, offset: f64) -> f64 {
        self.prior_w * sc.prior_logit + offset + self.temp * sc.sum
    }

    /// Calibrated probability, then shifted in log-odds by the feedback offset (so feedback always has an effect,
    /// whatever shape the calibration map has).
    fn prob(&self, sc: &Scored, offset: f64) -> f64 {
        let z = self.raw(sc, 0.0);
        let p = match &self.iso {
            Some(map) => lookup(map, z),
            None => sigmoid(z + self.bias),
        };
        let p = p.clamp(1e-4, 1.0 - 1e-4);
        sigmoid((p / (1.0 - p)).ln() + offset)
    }

    /// Fits the weights by leave-one-out log loss over a sample of the training notes, then an isotonic map.
    /// `exclude = Some((k, f))` leaves out notes with index % k == f, so an evaluation can calibrate on the
    /// other folds and be graded on the held-out one.
    fn calibrate(&mut self, prep: &[Prepared], exclude: Option<(usize, usize)>) {
        self.temp = DEFAULT_TEMP;
        self.prior_w = 1.0;
        self.bias = 0.0;
        self.iso = None;
        self.calibrated = false;
        if prep.len() < MIN_DOCS_TO_CALIBRATE {
            return;
        }
        let step = (prep.len() / CALIBRATION_SAMPLE).max(1);
        let mut samples: Vec<(f64, f64, bool)> = Vec::new(); // (prior logit, sum of llr, label)
        for (i, p) in prep.iter().enumerate().step_by(step) {
            if exclude.map_or(false, |(k, f)| i % k == f) {
                continue;
            }
            self.apply(p, -1);
            let mut tags: Vec<(String, u32)> = self.tag_n.iter().filter(|(_, c)| **c >= 1).map(|(t, c)| (t.clone(), *c)).collect();
            tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let mut cands: Vec<String> = tags.iter().take(CALIBRATION_TAGS).map(|(t, _)| t.clone()).collect();
            for l in &p.labels {
                if self.tag_n.get(l).copied().unwrap_or(0) >= 1 && !cands.contains(l) {
                    cands.push(l.clone());
                }
            }
            let feats = p.query_feats(&p.labels);
            for t in &cands {
                if let Some(sc) = self.score(t, &feats, &p.words) {
                    if sc.best >= MIN_EVIDENCE {
                        samples.push((sc.prior_logit, sc.sum, p.labels.contains(t)));
                    }
                }
            }
            self.apply(p, 1);
        }
        if samples.len() < MIN_CALIBRATION_SAMPLES || !samples.iter().any(|s| s.2) {
            return;
        }
        let loss = |t: f64, w: f64, b: f64| -> f64 {
            samples
                .iter()
                .map(|&(pl, s, y)| {
                    let p = sigmoid(w * pl + t * s + b).clamp(1e-6, 1.0 - 1e-6);
                    if y { -p.ln() } else { -(1.0 - p).ln() }
                })
                .sum::<f64>()
        };
        let mut best = (DEFAULT_TEMP, 1.0, 0.0, f64::MAX);
        for &t in &TEMPS {
            for &w in &PRIOR_WEIGHTS {
                for &b in &BIASES {
                    let l = loss(t, w, b);
                    if l < best.3 {
                        best = (t, w, b, l);
                    }
                }
            }
        }
        self.temp = best.0;
        self.prior_w = best.1;
        self.bias = best.2;
        self.calibrated = true;
        if samples.len() >= MIN_ISO_SAMPLES {
            let pts: Vec<(f64, bool)> = samples.iter().map(|&(pl, s, y)| (self.prior_w * pl + self.temp * s, y)).collect();
            self.iso = Some(isotonic(pts));
        }
    }

    /// Suggestions for a note being written. `taken` are tags already on it (written or added by a rule).
    /// `offsets` are per-tag log-odds adjustments learned from accepted and dismissed suggestions.
    pub fn suggest(&self, text: &str, taken: &[String], offsets: &HashMap<String, f64>) -> Vec<Suggestion> {
        if self.n == 0 {
            return Vec::new();
        }
        let (words, base) = features(&strip_hashtags(text));
        let written = crate::store::extract_tags(text);
        let mut feats = base;
        feats.extend(written.iter().map(|t| format!("t:{t}")));
        let mut out: Vec<(f64, Suggestion)> = Vec::new();
        for t in self.tag_n.keys() {
            if taken.contains(t) {
                continue;
            }
            let Some(sc) = self.score(t, &feats, &words) else { continue };
            if sc.best < MIN_EVIDENCE {
                continue;
            }
            let z = self.raw(&sc, offsets.get(t).copied().unwrap_or(0.0));
            let mut p = self.prob(&sc, offsets.get(t).copied().unwrap_or(0.0));
            if !self.calibrated {
                p = p.min(UNCALIBRATED_CAP);
            }
            if p < MIN_SHOW {
                continue;
            }
            let mut c = sc.contribs.clone();
            c.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let why = c
                .iter()
                .filter(|(_, l)| *l > 0.3)
                .take(3)
                .map(|(f, _)| match f.split_once(':') {
                    Some(("w", w)) => w.to_string(),
                    Some(("t", x)) => format!("#{x}"),
                    _ => "tag name".to_string(),
                })
                .collect();
            out.push((
                z,
                Suggestion {
                    tag: t.clone(),
                    confidence: ((p * 100.0).round() / 100.0).min(0.99), // never claim certainty
                    tier: if p >= HIGH { "high" } else { "medium" },
                    why,
                },
            ));
        }
        // Order by the raw score: the calibrated confidence is a step function, so many suggestions can share a value.
        out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.tag.cmp(&b.1.tag)));
        out.truncate(5);
        out.into_iter().map(|(_, s)| s).collect()
    }
}

/// One flat piece of the isotonic map: scores from `lo` to `hi` map to probability `p`.
#[derive(Debug, Clone, Copy)]
struct Step {
    lo: f64,
    hi: f64,
    p: f64,
}

/// Pool-adjacent-violators: the monotone non-decreasing step function closest to the labels. Neighbouring
/// blocks with equal rates are pooled too, so smoothing is applied to real counts and not to single samples.
fn isotonic(mut pts: Vec<(f64, bool)>) -> Vec<Step> {
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    struct Block {
        lo: f64,
        hi: f64,
        pos: f64,
        n: f64,
    }
    let mut st: Vec<Block> = Vec::new();
    for (z, y) in pts {
        st.push(Block { lo: z, hi: z, pos: y as u8 as f64, n: 1.0 });
        while st.len() >= 2 {
            let l = st.len();
            if st[l - 2].pos / st[l - 2].n >= st[l - 1].pos / st[l - 1].n {
                let b = st.pop().unwrap();
                let a = st.last_mut().unwrap();
                a.hi = b.hi;
                a.pos += b.pos;
                a.n += b.n;
            } else {
                break;
            }
        }
    }
    let mut out: Vec<Step> = st.iter().map(|b| Step { lo: b.lo, hi: b.hi, p: ((b.pos + 0.5) / (b.n + 1.0)).clamp(0.001, 0.999) }).collect();
    for i in 1..out.len() {
        out[i].p = out[i].p.max(out[i - 1].p); // smoothing must not break monotonicity
    }
    out
}

/// Inside a step: that step's probability. In the gap between two steps: interpolate in log-odds, which keeps
/// the map monotone and lets a short note (scores between the clusters seen in training) land between them.
fn lookup(map: &[Step], z: f64) -> f64 {
    let i = map.partition_point(|s| s.lo <= z); // first step starting above z
    if i == 0 {
        return map[0].p;
    }
    let a = map[i - 1];
    if z <= a.hi || i == map.len() {
        return a.p;
    }
    let b = map[i];
    let logit = |p: f64| (p / (1.0 - p)).ln();
    let t = (z - a.hi) / (b.lo - a.hi);
    sigmoid(logit(a.p) + t * (logit(b.p) - logit(a.p)))
}

/// Log-odds adjustment from feedback: tags you keep accepting rise, tags you keep dismissing fall.
pub fn feedback_offset(accepted: u32, dismissed: u32) -> f64 {
    (((accepted + 1) as f64) / ((dismissed + 1) as f64)).ln().clamp(-2.0, 1.5)
}

// ---------------------------------------------------------------- evaluation

pub struct ThresholdRow {
    pub theta: f64,
    pub shown: usize,
    pub correct: usize,
    pub precision: f64,
    pub recall: f64,
}

pub struct Bin {
    pub lo: f64,
    pub hi: f64,
    pub count: usize,
    pub mean_pred: f64,
    pub observed: f64,
}

pub struct Report {
    pub docs: usize,
    pub tags: usize,
    pub calibrated: bool,
    pub isotonic: bool,
    pub temp: f64,
    pub prior_w: f64,
    pub scenarios: usize,
    pub hit1: f64,
    pub hit3: f64,
    pub mrr: f64,
    pub legacy_hit1: f64,
    pub legacy_hit3: f64,
    pub legacy_mrr: f64,
    pub rows: Vec<ThresholdRow>,
    /// What the app shows with this model: top 5 above `MIN_SHOW`.
    pub ui: ThresholdRow,
    /// What the previous heuristic showed: top 5 above its 0.8 cutoff.
    pub legacy_ui: ThresholdRow,
    pub brier: f64,
    pub brier_prior: f64,
    pub logloss: f64,
    pub logloss_prior: f64,
    pub bins: Vec<Bin>,
}

/// The pre-calibration heuristic (tag name in text, plus P(tag|word) summed), kept only as a baseline.
fn legacy_score(m: &Model, tag: &str, feats: &[String], words: &HashSet<String>) -> f64 {
    let co_t = m.co.get(tag);
    let mut s = if name_present(tag, words) { 1.5 } else { 0.0 };
    for f in feats.iter().filter(|f| f.starts_with("w:")) {
        let df = *m.df.get(f).unwrap_or(&0) as f64;
        if df == 0.0 {
            continue;
        }
        let c = co_t.and_then(|x| x.get(f)).copied().unwrap_or(0) as f64;
        s += (c / df) * (df.min(2.0) / 2.0);
    }
    s
}

const FOLDS: usize = 5;

struct Row {
    tag: String,
    p: f64,
    z: f64,
    prior: f64,
    evidence: bool,
    held: bool,
    legacy: f64,
}

/// Leave-one-out evaluation: for each note and each of its tags, hide the note from the model, hide that one
/// tag, and see whether the model suggests it from the text and the note's other tags. Tags that appear on
/// only one note cannot be predicted and are skipped. The calibration map is fitted on the other folds of the
/// notes (5-fold), so the reliability table is not graded on the data it was fitted to.
pub fn evaluate(docs: &[Doc]) -> Report {
    let prep = prepare(docs);
    let full = Model::from_prepared(&prep);
    let (calibrated, isotonic, temp, prior_w, tags) = (full.calibrated, full.iso.is_some(), full.temp, full.prior_w, full.tag_n.len());
    let mut m = Model::counts(&prep);

    let mut cands: Vec<(f64, f64, bool, bool)> = Vec::new(); // (p, prior p, has evidence, is the held-out tag)
    let mut held: Vec<(f64, bool)> = Vec::new(); // held-out tag (p, has evidence)
    let mut lcands: Vec<(f64, bool)> = Vec::new();
    let mut lheld: Vec<f64> = Vec::new();
    let (mut hit1, mut hit3, mut mrr, mut l1, mut l3, mut lmrr, mut scenarios) = (0usize, 0usize, 0.0, 0usize, 0usize, 0.0, 0usize);
    // what the app shows: (shown, correct, trials where the hidden tag was among them)
    let (mut ui, mut lui) = ((0usize, 0usize, 0usize), (0usize, 0usize, 0usize));

    for fold in 0..FOLDS {
        m.calibrate(&prep, Some((FOLDS, fold)));
        for (i, p) in prep.iter().enumerate() {
            if i % FOLDS != fold {
                continue;
            }
            m.apply(p, -1);
            for h in &p.labels {
                if m.tag_n.get(h).copied().unwrap_or(0) < 1 {
                    continue;
                }
                let written: Vec<String> = p.labels.iter().filter(|l| *l != h).cloned().collect();
                let feats = p.query_feats(&written);
                let mut rows: Vec<Row> = Vec::new();
                for t in m.tag_n.keys() {
                    if written.contains(t) {
                        continue;
                    }
                    let Some(sc) = m.score(t, &feats, &p.words) else { continue };
                    let mut prob = m.prob(&sc, 0.0);
                    if !m.calibrated {
                        prob = prob.min(UNCALIBRATED_CAP);
                    }
                    rows.push(Row { tag: t.clone(), p: prob, z: m.raw(&sc, 0.0), prior: sigmoid(sc.prior_logit), evidence: sc.best >= MIN_EVIDENCE, held: t == h, legacy: legacy_score(&m, t, &feats, &p.words) });
                }
                scenarios += 1;
                let order = |key: &dyn Fn(&Row) -> f64| -> Vec<usize> {
                    let mut idx: Vec<usize> = (0..rows.len()).collect();
                    idx.sort_by(|&a, &b| key(&rows[b]).partial_cmp(&key(&rows[a])).unwrap_or(std::cmp::Ordering::Equal).then(rows[a].tag.cmp(&rows[b].tag)));
                    idx
                };
                let by_p = order(&|r| r.z);
                let by_legacy = order(&|r| r.legacy);
                let rank = |idx: &[usize]| idx.iter().position(|&i| rows[i].held).map_or(rows.len() + 1, |r| r + 1);
                let (r, lr) = (rank(&by_p), rank(&by_legacy));
                hit1 += (r == 1) as usize;
                hit3 += (r <= 3) as usize;
                mrr += 1.0 / r as f64;
                l1 += (lr == 1) as usize;
                l3 += (lr <= 3) as usize;
                lmrr += 1.0 / lr as f64;

                let shown: Vec<&Row> = by_p.iter().map(|&i| &rows[i]).filter(|x| x.evidence && x.p >= MIN_SHOW).take(5).collect();
                ui.0 += shown.len();
                ui.1 += shown.iter().filter(|x| x.held).count();
                ui.2 += shown.iter().any(|x| x.held) as usize;
                let lshown: Vec<&Row> = by_legacy.iter().map(|&i| &rows[i]).filter(|x| x.legacy >= 0.8).take(5).collect();
                lui.0 += lshown.len();
                lui.1 += lshown.iter().filter(|x| x.held).count();
                lui.2 += lshown.iter().any(|x| x.held) as usize;

                for row in &rows {
                    cands.push((row.p, row.prior, row.evidence, row.held));
                    lcands.push((row.legacy, row.held));
                    if row.held {
                        held.push((row.p, row.evidence));
                        lheld.push(row.legacy);
                    }
                }
            }
            m.apply(p, 1);
        }
    }

    let sc = scenarios.max(1) as f64;
    let row_of = |theta: f64, c: (usize, usize, usize)| ThresholdRow {
        theta,
        shown: c.0,
        correct: c.1,
        precision: if c.0 == 0 { f64::NAN } else { c.1 as f64 / c.0 as f64 },
        recall: c.2 as f64 / sc,
    };
    let threshold = |theta: f64| -> ThresholdRow {
        let shown: Vec<&(f64, f64, bool, bool)> = cands.iter().filter(|c| c.2 && c.0 >= theta).collect();
        let correct = shown.iter().filter(|c| c.3).count();
        let found = held.iter().filter(|h| h.1 && h.0 >= theta).count();
        row_of(theta, (shown.len(), correct, found))
    };
    let rows = [0.3, 0.5, 0.7, 0.9].iter().map(|&t| threshold(t)).collect();
    let _ = (&lcands, &lheld);

    // Calibration quality over the population a user would actually see (candidates with evidence).
    let seen: Vec<&(f64, f64, bool, bool)> = cands.iter().filter(|c| c.2).collect();
    let k = seen.len().max(1) as f64;
    let brier = seen.iter().map(|c| (c.0 - c.3 as u8 as f64).powi(2)).sum::<f64>() / k;
    let brier_prior = seen.iter().map(|c| (c.1 - c.3 as u8 as f64).powi(2)).sum::<f64>() / k;
    let ll = |p: f64, y: bool| -> f64 {
        let p = p.clamp(1e-6, 1.0 - 1e-6);
        if y { -p.ln() } else { -(1.0 - p).ln() }
    };
    let logloss = seen.iter().map(|c| ll(c.0, c.3)).sum::<f64>() / k;
    let logloss_prior = seen.iter().map(|c| ll(c.1, c.3)).sum::<f64>() / k;

    let edges = [0.0, 0.1, 0.3, 0.5, 0.7, 0.9, 1.0001];
    let bins = edges
        .windows(2)
        .map(|w| {
            let inb: Vec<&&(f64, f64, bool, bool)> = seen.iter().filter(|c| c.0 >= w[0] && c.0 < w[1]).collect();
            let n = inb.len();
            Bin {
                lo: w[0],
                hi: w[1].min(1.0),
                count: n,
                mean_pred: if n == 0 { f64::NAN } else { inb.iter().map(|c| c.0).sum::<f64>() / n as f64 },
                observed: if n == 0 { f64::NAN } else { inb.iter().filter(|c| c.3).count() as f64 / n as f64 },
            }
        })
        .collect();

    Report {
        docs: prep.len(),
        tags,
        calibrated,
        isotonic,
        temp,
        prior_w,
        scenarios,
        hit1: hit1 as f64 / sc,
        hit3: hit3 as f64 / sc,
        mrr: mrr / sc,
        legacy_hit1: l1 as f64 / sc,
        legacy_hit3: l3 as f64 / sc,
        legacy_mrr: lmrr / sc,
        rows,
        ui: row_of(MIN_SHOW, ui),
        legacy_ui: row_of(0.8, lui),
        brier,
        brier_prior,
        logloss,
        logloss_prior,
        bins,
    }
}

fn pct(x: f64) -> String {
    if x.is_nan() { "   n/a".to_string() } else { format!("{:5.1}%", x * 100.0) }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Tag suggestion evaluation (leave-one-out on your own notes)")?;
        writeln!(f, "  tagged notes: {}   distinct tags: {}   held-out tag trials: {}", self.docs, self.tags, self.scenarios)?;
        if self.calibrated {
            writeln!(
                f,
                "  calibration: evidence weight {:.2}, base-rate weight {:.2}, {} map",
                self.temp,
                self.prior_w,
                if self.isotonic { "isotonic" } else { "sigmoid" }
            )?;
        } else {
            writeln!(f, "  calibration: NOT fitted (needs {MIN_DOCS_TO_CALIBRATE}+ tagged notes with enough signal); confidences are capped at {:.0}%", UNCALIBRATED_CAP * 100.0)?;
        }
        if self.scenarios == 0 {
            writeln!(f, "\n  Not enough data: no tag appears on at least two notes.")?;
            return Ok(());
        }
        writeln!(f, "\nRanking: is the hidden tag among the top suggestions?")?;
        writeln!(f, "                      top-1    top-3    mean reciprocal rank")?;
        writeln!(f, "  calibrated model   {}  {}   {:.3}", pct(self.hit1), pct(self.hit3), self.mrr)?;
        writeln!(f, "  previous heuristic {}  {}   {:.3}", pct(self.legacy_hit1), pct(self.legacy_hit3), self.legacy_mrr)?;
        writeln!(f, "\nWhat the app shows (top 5 above the cutoff), per trial:")?;
        writeln!(f, "                           shown   precision   hidden tag among them")?;
        writeln!(f, "  calibrated (>= {:.2})    {:6}   {}      {}", self.ui.theta, self.ui.shown, pct(self.ui.precision), pct(self.ui.recall))?;
        writeln!(f, "  previous   (>= {:.2})    {:6}   {}      {}", self.legacy_ui.theta, self.legacy_ui.shown, pct(self.legacy_ui.precision), pct(self.legacy_ui.recall))?;
        writeln!(f, "\nIf only suggestions at or above a confidence are shown (no top-5 cap):")?;
        writeln!(f, "  threshold   shown   precision   recall of hidden tags")?;
        for r in &self.rows {
            writeln!(f, "  >= {:.2}    {:6}   {}      {}", r.theta, r.shown, pct(r.precision), pct(r.recall))?;
        }
        writeln!(f, "\nProbability quality (lower is better), suggestions with evidence only:")?;
        writeln!(f, "  Brier score   {:.4}   (always guessing the tag's base rate: {:.4})", self.brier, self.brier_prior)?;
        writeln!(f, "  log loss      {:.4}   (base rate: {:.4})", self.logloss, self.logloss_prior)?;
        writeln!(f, "\nReliability: when it says X%, how often is it right?")?;
        writeln!(f, "  confidence band    count   mean predicted   actually right")?;
        for b in &self.bins {
            writeln!(f, "  {:.1} to {:.1}      {:7}   {}           {}", b.lo, b.hi, b.count, pct(b.mean_pred), pct(b.observed))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(labels: &[&str], text: &str) -> Doc {
        Doc { labels: labels.iter().map(|s| s.to_string()).collect(), text: text.to_string() }
    }

    /// Deterministic pseudo-random numbers so the synthetic corpus is the same on every run.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[(self.next() % xs.len() as u64) as usize]
        }
    }

    fn corpus(n: usize) -> Vec<Doc> {
        let topics: [(&str, [&str; 6]); 5] = [
            ("aws", ["lambda", "bucket", "deploy", "cloudfront", "iam", "stack"]),
            ("home", ["milk", "garden", "plumber", "laundry", "paint", "garage"]),
            ("finance", ["invoice", "budget", "taxes", "expense", "ledger", "refund"]),
            ("reading", ["novel", "chapter", "author", "essay", "library", "review"]),
            ("fitness", ["run", "squat", "stretch", "marathon", "protein", "sleep"]),
        ];
        let noise = ["today", "remember", "tomorrow", "maybe", "check", "later", "important", "quick", "thing", "morning"];
        let mut r = Lcg(42);
        (0..n)
            .map(|_| {
                let a = (r.next() % 5) as usize;
                let mut labels = vec![topics[a].0];
                let mut words: Vec<&str> = (0..3).map(|_| r.pick(&topics[a].1)).collect();
                if r.next() % 4 == 0 {
                    let b = (r.next() % 5) as usize;
                    if b != a {
                        labels.push(topics[b].0);
                        words.push(r.pick(&topics[b].1));
                    }
                }
                for _ in 0..3 {
                    words.push(r.pick(&noise));
                }
                doc(&labels, &words.join(" "))
            })
            .collect()
    }

    #[test]
    fn tokenizing() {
        assert_eq!(strip_hashtags("buy milk #home and # heading #urgent"), "buy milk and # heading");
        assert_eq!(tokens("The Lambdas were 42 deploys"), vec!["lambda", "deploy"]);
        assert!(name_present("to-do", &["to".to_string()].into_iter().collect()) == false);
        assert!(name_present("aws", &["aws".to_string()].into_iter().collect()));
    }

    #[test]
    fn learns_associations_and_excludes_taken_tags() {
        let docs = corpus(120);
        let m = Model::train(&docs);
        assert!(m.calibrated);
        let s = m.suggest("fix the lambda deploy and the iam stack", &[], &HashMap::new());
        assert_eq!(s[0].tag, "aws");
        assert_eq!(s[0].tier, "high");
        assert!(s[0].why.iter().any(|w| w == "lambda" || w == "deploy" || w == "iam" || w == "stack"));
        assert!(m.suggest("fix the lambda deploy", &["aws".to_string()], &HashMap::new()).iter().all(|x| x.tag != "aws"));
        // a tag typed into the text is excluded by the caller and never counts as evidence for itself
        assert!(m.suggest("lambda #aws", &["aws".to_string()], &HashMap::new()).iter().all(|x| x.tag != "aws"));
        // nothing relevant: no suggestion
        assert!(m.suggest("zzz qqq", &[], &HashMap::new()).is_empty());
    }

    #[test]
    fn small_data_is_not_overconfident() {
        let docs = vec![doc(&["aws"], "lambda deploy"), doc(&["aws"], "lambda bucket"), doc(&["home"], "milk garden")];
        let m = Model::train(&docs);
        assert!(!m.calibrated);
        let s = m.suggest("lambda", &[], &HashMap::new());
        assert!(s.iter().all(|x| x.confidence <= UNCALIBRATED_CAP && x.tier == "medium"));
    }

    #[test]
    fn feedback_moves_confidence() {
        let m = Model::train(&corpus(120));
        let base = m.suggest("lambda bucket", &[], &HashMap::new())[0].confidence;
        let mut down = HashMap::new();
        down.insert("aws".to_string(), feedback_offset(0, 6));
        let after = m.suggest("lambda bucket", &[], &down).iter().find(|s| s.tag == "aws").map_or(0.0, |s| s.confidence);
        assert!(after < base, "{after} !< {base}");
        assert!(feedback_offset(5, 0) > 0.0 && feedback_offset(0, 5) < 0.0 && feedback_offset(0, 0) == 0.0);
    }

    #[test]
    fn evaluation_beats_base_rate_and_is_calibrated() {
        let r = evaluate(&corpus(200));
        assert!(r.calibrated);
        assert!(r.hit1 > 0.8, "top-1 {}", r.hit1);
        assert!(r.brier < r.brier_prior, "{} !< {}", r.brier, r.brier_prior);
        assert!(r.logloss < r.logloss_prior);
        // "it says 70% and is right about 70% of the time": every populated band within 0.15 of its prediction
        for b in r.bins.iter().filter(|b| b.count >= 30) {
            assert!((b.mean_pred - b.observed).abs() < 0.15, "band {}-{}: predicted {} observed {}", b.lo, b.hi, b.mean_pred, b.observed);
        }
        let high = r.rows.iter().find(|x| x.theta == 0.7).unwrap();
        assert!(high.precision > 0.8, "precision at 0.7 was {}", high.precision);
        assert!(format!("{r}").contains("Reliability"));
    }

    #[test]
    fn evaluation_with_no_repeated_tags_says_so() {
        let r = evaluate(&[doc(&["a"], "alpha beta"), doc(&["b"], "gamma delta")]);
        assert_eq!(r.scenarios, 0);
        assert!(format!("{r}").contains("Not enough data"));
    }
}
