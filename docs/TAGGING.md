# Tag suggestions

Chuck suggests tags you already use while you type. Everything runs locally from your own notes; nothing is sent anywhere.

## How it works

- **Training data:** notes that carry at least one tag you wrote. Hashtags are stripped from the text before it is used as
  features, so a note's tags never leak into its own evidence. Tags added by auto-tag rules are not labels, so a wrong rule
  cannot teach the model. Accepting a suggestion writes the tag into the note, so accepted tags do become labels.
- **Model:** naive Bayes. For each tag it weighs the words of the note (stemmed, common words removed), the tags already
  written on it, and whether the tag's own name appears in the text. A tag is only suggested if at least one feature is
  informative, so a very common tag is not suggested on every note.
- **Confidence:** raw naive Bayes scores are overconfident because words are correlated. Chuck fits the evidence and base-rate
  weights by leave-one-out on your own notes (each note is hidden from the model, then predicted), then maps the score to a
  probability with an isotonic (monotone step) map. A confidence of 0.8 means "about 80% of suggestions like this are right".
  Suggestions are ranked by the underlying score, and the confidence is what you see.
- **Tiers:** below 30% is hidden, 30% to 70% is shown as a normal chip, 70% and above is shown filled ("high"). Confidence is
  never displayed above 99%.
- **Small collections:** until there are 12 tagged notes with enough signal (roughly 30 usable training examples) the model is
  not calibrated and every confidence is capped at 60%, so nothing is labelled "high" on thin evidence.
- **Why:** hover a chip to see the strongest evidence ("because of: lambda, deploy").

## Feedback

Clicking a chip accepts it (the tag is written into the note). The **x** on a chip dismisses it. Both are logged in the
`suggestion_events` table. A tag you keep accepting rises, and one you keep dismissing falls, by shifting its log-odds by
`ln((accepted + 1) / (dismissed + 1))`, limited to between -2 and +1.5. This nudges borderline suggestions. It cannot override a
suggestion the model is very sure about; for that, use the dismiss button on the specific suggestion, or add an auto-tag rule.

## Measuring it: `chuck eval-tags`

    chuck eval-tags [--db PATH] [--owner ID]

For every note and every tag on it, the command hides the note from the model, hides that one tag, and checks whether the
model suggests it. It prints:

- how often the hidden tag is the top suggestion or in the top three, next to the previous heuristic;
- what the app would show (top five above the cutoff): how many suggestions, how many are right, how often the hidden tag is
  among them;
- precision and recall at each confidence cutoff;
- Brier score and log loss against always guessing a tag's base rate;
- a reliability table: when the model says X%, how often it is right. The calibration map used for this table is fitted on
  other notes (5-fold), so it is not graded on its own training data.

Use it before and after any change to the model. Tags that appear on only one note cannot be predicted and are skipped.

## Things tried and left out

- **Word pairs (bigrams):** lowered top-1 accuracy by 3 to 5 points on a noisy synthetic corpus, because they are mostly
  one-off noise at small sizes. Worth re-testing on real notes with `eval-tags`.
- **Ignoring words seen in only one note:** no improvement.

## Limits

- Accuracy depends on how many notes you have tagged and how consistent your tagging is. The numbers in pull requests come
  from a synthetic corpus, not your notes; run `eval-tags` on your own data.
- The model is rebuilt from your notes after any note changes (milliseconds for thousands of notes).
- Lexical only: "deploy" and "release" are unrelated words to it.
