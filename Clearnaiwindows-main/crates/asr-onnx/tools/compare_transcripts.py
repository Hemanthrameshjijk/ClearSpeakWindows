"""Compare our on-PC transcript (diarize_file -> <audio>.ours.json) with another one (Deepgram / Citrus IQ).

    python compare_transcripts.py call.ours.json theirs.json|theirs.txt [--out report.txt]

Theirs can be:
- Deepgram JSON (results.channels[*].alternatives[0].words with "speaker", or one channel per speaker), or
  results.utterances.
- Text, one line per turn: "[00:12] Speaker 1: text", "Speaker 0: text", "Agent: text" or "00:12 Caller - text".

Prints:
- How far apart the words are (word difference rate, taking theirs as the reference; it is not "who is right").
- On the words both have, how often the speaker agrees, after matching speaker names (their "0" may be our "2").
- The places the speaker disagrees, to check by ear.
"""
import json, re, sys, itertools, argparse

ONES = "zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen " \
       "seventeen eighteen nineteen".split()
TENS = "_ _ twenty thirty forty fifty sixty seventy eighty ninety".split()


ORDINAL = {1: "first", 2: "second", 3: "third", 4: "fourth", 5: "fifth", 6: "sixth", 7: "seventh", 8: "eighth",
           9: "ninth", 10: "tenth", 11: "eleventh", 12: "twelfth", 13: "thirteenth", 14: "fourteenth", 15: "fifteenth",
           16: "sixteenth", 17: "seventeenth", 18: "eighteenth", 19: "nineteenth", 20: "twentieth", 21: "twenty first",
           22: "twenty second", 23: "twenty third", 24: "twenty fourth", 25: "twenty fifth", 26: "twenty sixth",
           27: "twenty seventh", 28: "twenty eighth", 29: "twenty ninth", 30: "thirtieth", 31: "thirty first"}
SAME = [("alright", "all right"), ("gonna", "going to"), ("wanna", "want to"), ("gotta", "got to"),
        ("directv", "direct tv"), ("mr", "mister"), ("mrs", "missus"), ("ok", "okay"), ("mm-hmm", "mm hmm"),
        ("uh-huh", "uh huh"), ("e-mail", "email"), ("dot com", "com")]


def num_words(n):
    if n < 20:
        return [ONES[n]]
    if n < 100:
        return [TENS[n // 10]] + ([ONES[n % 10]] if n % 10 else [])
    if n < 1000:
        return [ONES[n // 100], "hundred"] + (num_words(n % 100) if n % 100 else [])
    if n < 1_000_000:
        return num_words(n // 1000) + ["thousand"] + (num_words(n % 1000) if n % 1000 else [])
    return [ONES[int(d)] for d in str(n)]


def norm(text):
    """Lowercase words without punctuation; digits spelled out (Deepgram writes "25", our model "twenty five")."""
    text = text.lower().replace("%", " percent").replace("&", " and ")
    # Same words, different spelling style (Deepgram's smart formatting vs our plain words).
    text = re.sub(r"\$(\d[\d,]*)(?:\.(\d\d))?", lambda m: f"{m.group(1)} dollars" + (f" {m.group(2)} cents" if m.group(2) and m.group(2) != "00" else ""), text)
    text = re.sub(r"\b(\d+)(st|nd|rd|th)\b", lambda m: ORDINAL.get(int(m.group(1)), m.group(1)), text)
    text = re.sub(r"\b([ap])\.?m\.?(?=\W|$)", r"\1 m", text)
    for a, b in SAME:
        text = re.sub(rf"\b{a}\b", b, text)
    out = []
    for tok in re.findall(r"[a-z']+|\d[\d,]*", text):
        if tok[0].isdigit():
            digits = tok.replace(",", "")
            # long numbers (phone, account) are read digit by digit
            out += num_words(int(digits)) if len(digits) <= 3 else [ONES[int(d)] for d in digits]
        else:
            out.append(tok.replace("'", ""))
    # Letters said one by one ("t v", "s e y m o u r") vs written as one word ("TV"): join runs of single letters
    # on both sides ("a" and "i" are real words, so they never start or extend a run).
    joined = []
    for w in out:
        if len(w) == 1 and w not in "ai" and joined and joined[-1][0] == "#":
            joined[-1] += w
        elif len(w) == 1 and w not in "ai":
            joined.append("#" + w)
        else:
            joined.append(w)
    # "$29.99" is said "twenty nine ninety nine": the unit words are formatting, not speech, so neither side keeps them.
    return [w.lstrip("#") for w in joined if w and w not in ("dollars", "dollar", "cents")]


def load(path):
    """-> list of (word, speaker label) in time order."""
    raw = open(path, encoding="utf-8-sig").read()
    if raw.lstrip().startswith("{"):
        j = json.loads(raw)
        if "words" in j and "lines" in j:  # ours
            return [(w, str(x["speaker"])) for x in j["words"] for w in norm(x["word"])]
        res = j.get("results", j)
        chans = res.get("channels") or []
        words = []
        for ci, ch in enumerate(chans):
            for x in ch["alternatives"][0].get("words", []):
                spk = x.get("speaker", ci if len(chans) > 1 else 0)
                for w in norm(x.get("punctuated_word") or x["word"]):
                    words.append((x["start"], w, str(spk)))
        if not words and res.get("utterances"):
            for u in res["utterances"]:
                for w in norm(u["transcript"]):
                    words.append((u["start"], w, str(u.get("speaker", u.get("channel", 0)))))
        words.sort(key=lambda t: t[0])
        return [(w, s) for _, w, s in words]
    words = []
    # Citrus IQ report: a header block, then "Name [mm:ss]" on its own line and the text on the lines after it.
    head = re.compile(r"^\s*(\S.{0,30}?)\s*\[\d+:\d+(?::\d+)?\]\s*$")
    if any(head.match(l) for l in raw.splitlines()):
        spk = None
        for line in raw.splitlines():
            m = head.match(line)
            if m:
                spk = m.group(1).strip()
            elif spk is not None and line.strip() and not line.startswith("==="):
                words += [(w, spk) for w in norm(line)]
        return words
    for line in raw.splitlines():
        m = re.match(r"\s*(?:\[?[\d:.]+\]?\s*)?([A-Za-z][\w ]{0,20}?)\s*[:\-]\s+(.*)", line)
        if m:
            words += [(w, m.group(1).strip()) for w in norm(m.group(2))]
        elif line.strip():
            words += [(w, "?") for w in norm(line)]
    return words


def align(a, b):
    """Edit-distance alignment of two word lists -> (errors, [(i, j) of equal words])."""
    n, m = len(a), len(b)
    d = [[0] * (m + 1) for _ in range(n + 1)]
    for i in range(n + 1):
        d[i][0] = i
    for j in range(m + 1):
        d[0][j] = j
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            d[i][j] = min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + (a[i - 1] != b[j - 1]))
    pairs, i, j = [], n, m
    while i > 0 and j > 0:
        if a[i - 1] == b[j - 1] and d[i][j] == d[i - 1][j - 1]:
            pairs.append((i - 1, j - 1)); i -= 1; j -= 1
        elif d[i][j] == d[i - 1][j - 1] + 1:
            i -= 1; j -= 1
        elif d[i][j] == d[i - 1][j] + 1:
            i -= 1
        else:
            j -= 1
    return d[n][m], pairs[::-1]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ours"); ap.add_argument("theirs"); ap.add_argument("--out")
    a = ap.parse_args()
    ours, theirs = load(a.ours), load(a.theirs)
    ow, tw = [w for w, _ in ours], [w for w, _ in theirs]
    errs, pairs = align(tw, ow)
    rep = [f"words: theirs {len(tw)}, ours {len(ow)}",
           f"word difference (theirs as reference): {100 * errs / max(len(tw), 1):.1f}%",
           f"speakers: theirs {sorted(set(s for _, s in theirs))}, ours {sorted(set(s for _, s in ours))}"]
    ts = sorted(set(theirs[j][1] for j, _ in pairs))
    os_ = sorted(set(ours[i][1] for _, i in pairs))
    best, best_map = -1, {}
    if ts and os_:
        names = os_ + [None] * max(0, len(ts) - len(os_))
        for perm in itertools.permutations(names, len(ts)):
            mp = dict(zip(ts, perm))
            ok = sum(1 for j, i in pairs if mp[theirs[j][1]] == ours[i][1])
            if ok > best:
                best, best_map = ok, mp
        rep.append(f"speaker agreement on {len(pairs)} shared words: {100 * best / len(pairs):.1f}%  "
                   f"(their -> our speaker: {best_map})")
        # disagreement stretches, with some context
        bad = [k for k, (j, i) in enumerate(pairs) if best_map[theirs[j][1]] != ours[i][1]]
        runs, start = [], None
        for k in range(len(pairs) + 1):
            if k < len(pairs) and k in set(bad):
                start = k if start is None else start
            elif start is not None:
                runs.append((start, k)); start = None
        if runs:
            rep.append(f"\n{len(runs)} place(s) where the speaker differs:")
        for s, e in runs[:40]:
            words = " ".join(tw[pairs[k][0]] for k in range(s, e))
            rep.append(f"  theirs {theirs[pairs[s][0]][1]} / ours {ours[pairs[s][1]][1]}: \"{words}\"")
    text = "\n".join(rep)
    print(text)
    if a.out:
        open(a.out, "w", encoding="utf-8").write(text + "\n")


if __name__ == "__main__":
    main()
