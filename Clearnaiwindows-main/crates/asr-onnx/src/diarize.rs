//! Who spoke when (diarization) for a recording where all voices share one channel.
//!
//! - The caller cuts speech into short overlapping windows ([`windows`]) and takes a voice fingerprint (ECAPA, unit
//!   length) of each.
//! - [`cluster`] groups the fingerprints into speakers: average-linkage merging by cosine similarity until either
//!   the requested number of speakers is left or no two groups sound alike any more, then a few rounds of moving each
//!   window to its closest speaker. Speaker 0 is whoever speaks first.
//! - [`label_words`] gives each word the speaker of the window centred nearest to it, and [`turns`] joins words into
//!   lines that change when the speaker changes or after a pause.
//!
//! A recording with the agent and the caller on separate channels needs none of this: the channel says who spoke.

use crate::model::Word;

/// Fingerprint window and step (seconds). 1.5 s is enough for a steady fingerprint and short enough to catch "yes".
pub const WIN_S: f32 = 1.5;
pub const HOP_S: f32 = 0.75;
/// Shorter speech than this gets no fingerprint of its own (its words take the nearest window's speaker).
pub const MIN_WIN_S: f32 = 0.5;
/// Two groups whose average similarity is below this are different people. Tuned on 11 made-up LibriSpeech
/// conversations (2-3 speakers, same and mixed gender): 0.15 labelled every word right; 0.10 merged two women and
/// 0.20 split some voices in two. The window is narrow, so when the number of speakers is known (a 1:1 call: 2),
/// pass it instead.
pub const SAME_SPEAKER: f32 = 0.15;
/// A group with fewer windows than this is noise or a cough, not a speaker: it joins the closest real speaker.
const MIN_GROUP: usize = 3;
/// A pause this long starts a new line even when the same person goes on.
const TURN_GAP_S: f32 = 1.0;

/// (start, end) in seconds of each fingerprint window inside one stretch of speech running `s0..s1`.
pub fn windows(s0: f32, s1: f32) -> Vec<(f32, f32)> {
    let len = s1 - s0;
    if len < MIN_WIN_S {
        return Vec::new();
    }
    if len <= WIN_S {
        return vec![(s0, s1)];
    }
    let mut out = Vec::new();
    let mut t = s0;
    while t + WIN_S < s1 {
        out.push((t, t + WIN_S));
        t += HOP_S;
    }
    out.push((s1 - WIN_S, s1)); // the last window ends with the speech
    out
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn unit(v: &[f32]) -> Vec<f32> {
    let n = dot(v, v).sqrt().max(1e-12);
    v.iter().map(|x| x / n).collect()
}

/// Speaker number for each fingerprint (all unit length). `speakers`: the exact number when known, else decided by
/// [`SAME_SPEAKER`] (`threshold` overrides it).
pub fn cluster(embs: &[Vec<f32>], speakers: Option<usize>, threshold: Option<f32>) -> Vec<usize> {
    let n = embs.len();
    if n == 0 {
        return Vec::new();
    }
    let thr = threshold.unwrap_or(SAME_SPEAKER);
    // Group by voice until no two groups sound alike.
    let (mut group, mut size, _) = merge(embs, |s, _| s < thr);
    let big = |size: &[usize]| size.iter().filter(|&&s| s >= MIN_GROUP).count();
    if let Some(want) = speakers.map(|w| w.max(1)) {
        // Known count, but the voices sound more alike than usual (e.g. two men on one phone line). Merging on down
        // to exactly that many groups left one huge group and a few scraps (a real 3-person call came out as one
        // speaker); instead take the most-merged point where `want` groups each still hold a real share of the
        // speech. On 8 real calls: that call 73% -> 87% agreement with Citrus IQ, the others unchanged.
        if big(&size) < want {
            (group, size) = coarsest_with(embs, want);
        }
    }
    // Real speakers are the groups with enough windows; tiny ones (a cough, a click) join the closest of them. With a
    // known count, only the biggest groups are speakers and the rest (noise, breathing, a lone "ah") join them too.
    // Forcing the merge down to the count instead merged the two people of a real call into one, because they
    // sounded more alike than either did to a group of noise.
    let mut real: Vec<usize> = (0..n).filter(|&i| size[i] >= MIN_GROUP).collect();
    if real.is_empty() {
        real = (0..n).filter(|&i| size[i] > 0).collect();
    }
    real.sort_by_key(|&i| std::cmp::Reverse(size[i]));
    if let Some(want) = speakers {
        real.truncate(want.max(1));
    }
    let ids = real;
    // Windows of tiny groups start unlabelled (usize::MAX); the first refine round gives them a speaker.
    let mut label: Vec<usize> = group.iter().map(|g| ids.iter().position(|x| x == g).unwrap_or(usize::MAX)).collect();
    // Refine: move every window to its closest speaker average, a few times.
    for _ in 0..5 {
        let mut sums: Vec<Option<Vec<f32>>> = vec![None; ids.len()];
        for (e, &l) in embs.iter().zip(&label) {
            if let Some(slot) = sums.get_mut(l) {
                let s = slot.get_or_insert_with(|| vec![0.0; e.len()]);
                for (a, x) in s.iter_mut().zip(e) {
                    *a += x;
                }
            }
        }
        let cents: Vec<Vec<f32>> = sums.into_iter().flatten().map(|s| unit(&s)).collect();
        let new: Vec<usize> = embs
            .iter()
            .map(|e| (0..cents.len()).max_by(|&a, &b| dot(e, &cents[a]).total_cmp(&dot(e, &cents[b]))).unwrap_or(0))
            .collect();
        let done = new == label;
        label = new;
        if done {
            break;
        }
    }
    // Number speakers by first appearance.
    let mut order: Vec<usize> = Vec::new();
    for &l in &label {
        if !order.contains(&l) {
            order.push(l);
        }
    }
    label.iter().map(|l| order.iter().position(|o| o == l).unwrap()).collect()
}

/// A group needs this share of all windows to count as a person when looking for a known number of speakers.
const MIN_SHARE: f32 = 0.02;

/// The grouping, along the full merge order, with the fewest groups that still has `want` groups of at least
/// [`MIN_SHARE`] of the windows (and [`MIN_GROUP`]). If none has, merging down to `want` groups.
fn coarsest_with(embs: &[Vec<f32>], want: usize) -> (Vec<usize>, Vec<usize>) {
    let n = embs.len();
    let big_min = MIN_GROUP.max((MIN_SHARE * n as f32) as usize);
    let (_, _, order) = merge(embs, |_, _| false);
    // Replay the merges, counting groups that are big enough; remember the last step that still has `want`.
    let mut size = vec![1usize; n];
    let mut count = if big_min <= 1 { n } else { 0 };
    let mut last: Option<usize> = None;
    for (t, &(a, b)) in order.iter().enumerate() {
        count -= (size[a] >= big_min) as usize + (size[b] >= big_min) as usize;
        size[a] += size[b];
        size[b] = 0;
        count += (size[a] >= big_min) as usize;
        if count >= want {
            last = Some(t);
        }
    }
    let Some(last) = last else {
        let (g, s, _) = merge(embs, |_, left| left <= want);
        return (g, s);
    };
    let mut group: Vec<usize> = (0..n).collect();
    let mut size = vec![1usize; n];
    for &(a, b) in &order[..=last] {
        size[a] += size[b];
        size[b] = 0;
        for g in group.iter_mut() {
            if *g == b {
                *g = a;
            }
        }
    }
    (group, size)
}

/// Average-linkage merging of the fingerprints, best-matching pair first, until `stop(similarity of that pair,
/// groups left)` says so. Returns each window's group (an index), each group's size (0 = merged away) and the merges
/// made, in order (kept group, merged-away group).
fn merge(embs: &[Vec<f32>], stop: impl Fn(f32, usize) -> bool) -> (Vec<usize>, Vec<usize>, Vec<(usize, usize)>) {
    let n = embs.len();
    // sim[i][j] = mean pairwise similarity between groups i and j.
    let mut sim: Vec<Vec<f32>> = (0..n).map(|i| (0..n).map(|j| dot(&embs[i], &embs[j])).collect()).collect();
    let mut size = vec![1usize; n];
    let mut group: Vec<usize> = (0..n).collect();
    let mut left = n;
    let mut order = Vec::with_capacity(n);
    while left > 1 {
        let mut best = (f32::NEG_INFINITY, 0, 0);
        for i in 0..n {
            if size[i] == 0 {
                continue;
            }
            for j in i + 1..n {
                if size[j] > 0 && sim[i][j] > best.0 {
                    best = (sim[i][j], i, j);
                }
            }
        }
        let (s, a, b) = best;
        if stop(s, left) {
            break;
        }
        for k in 0..n {
            if size[k] > 0 && k != a && k != b {
                let v = (sim[a][k] * size[a] as f32 + sim[b][k] * size[b] as f32) / (size[a] + size[b]) as f32;
                sim[a][k] = v;
                sim[k][a] = v;
            }
        }
        size[a] += size[b];
        size[b] = 0;
        for g in group.iter_mut() {
            if *g == b {
                *g = a;
            }
        }
        order.push((a, b));
        left -= 1;
    }
    (group, size, order)
}

/// Speaker of each word: the window (start, end, speaker) whose centre is nearest the word's middle.
pub fn label_words(words: &[Word], wins: &[(f32, f32, usize)]) -> Vec<usize> {
    words
        .iter()
        .map(|w| {
            let mid = 0.5 * (w.start + w.end);
            wins.iter()
                .min_by(|a, b| {
                    let da = (0.5 * (a.0 + a.1) - mid).abs();
                    let db = (0.5 * (b.0 + b.1) - mid).abs();
                    da.total_cmp(&db)
                })
                .map(|w| w.2)
                .unwrap_or(0)
        })
        .collect()
}

/// One line of the transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub speaker: usize,
    pub start: f32,
    pub end: f32,
    pub text: String,
}

/// Words (in time order) with their speakers -> lines.
pub fn turns(words: &[Word], speakers: &[usize]) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    for (w, &s) in words.iter().zip(speakers) {
        match out.last_mut() {
            Some(t) if t.speaker == s && w.start - t.end < TURN_GAP_S => {
                t.text.push(' ');
                t.text.push_str(&w.text);
                t.end = w.end;
            }
            _ => out.push(Turn { speaker: s, start: w.start, end: w.end, text: w.text.clone() }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit vector near axis `a` with a little of axis `b` (noise).
    fn v(a: usize, b: usize, noise: f32) -> Vec<f32> {
        let mut x = vec![0.0; 8];
        x[a] = 1.0;
        x[b] += noise;
        unit(&x)
    }

    #[test]
    fn windows_cover_the_speech() {
        assert!(windows(0.0, 0.3).is_empty(), "too short for a fingerprint");
        assert_eq!(windows(1.0, 2.0), vec![(1.0, 2.0)]);
        let w = windows(0.0, 4.0);
        assert_eq!(w.first(), Some(&(0.0, 1.5)));
        assert_eq!(w.last(), Some(&(2.5, 4.0)));
        assert!(w.windows(2).all(|p| p[1].0 - p[0].0 <= HOP_S + 1e-6));
    }

    #[test]
    fn two_voices_are_told_apart_and_numbered_by_who_spoke_first() {
        // B speaks first, then A, then B again.
        let mut e = Vec::new();
        for i in 0..5 {
            e.push(v(1, 3, 0.1 * i as f32));
        }
        for i in 0..6 {
            e.push(v(0, 4, 0.1 * i as f32));
        }
        for i in 0..3 {
            e.push(v(1, 5, 0.1 * i as f32));
        }
        let want: Vec<usize> = [vec![0; 5], vec![1; 6], vec![0; 3]].concat();
        assert_eq!(cluster(&e, None, None), want, "number of speakers found by itself");
        assert_eq!(cluster(&e, Some(2), None), want, "number of speakers given");
    }

    #[test]
    fn one_voice_stays_one_speaker() {
        let e: Vec<Vec<f32>> = (0..10).map(|i| v(2, 6, 0.05 * i as f32)).collect();
        assert!(cluster(&e, None, None).iter().all(|&l| l == 0));
    }

    #[test]
    fn a_single_odd_window_does_not_become_a_speaker() {
        let mut e: Vec<Vec<f32>> = (0..8).map(|i| v(0, 4, 0.05 * i as f32)).collect();
        e.insert(3, v(7, 6, 0.0)); // a cough
        let l = cluster(&e, None, None);
        assert!(l.iter().all(|&x| x == 0), "{l:?}");
    }

    #[test]
    fn with_a_known_count_a_lone_odd_window_does_not_take_a_speaker_slot() {
        // Two people plus one "ah" that sounds like neither (a real call did this: the two people were merged).
        let mut e: Vec<Vec<f32>> = Vec::new();
        for i in 0..6 {
            e.push(v(0, 4, 0.05 * i as f32));
            e.push(v(1, 5, 0.05 * i as f32));
        }
        // A few windows of noise that sound like nothing else (a real call had this; the two men sounded more alike
        // than either did to the noise, so merging down to 2 merged the two men).
        for i in 0..4 {
            e.push(v(7, 6, 0.05 * i as f32));
        }
        // ...and the two people sound somewhat alike (similarity ~0.11: told apart, but closer than to the noise).
        for x in e.iter_mut().take(12) {
            x[2] = 0.35;
            *x = unit(x);
        }
        let l = cluster(&e, Some(2), None);
        let want: Vec<usize> = (0..12).map(|i| i % 2).collect();
        assert_eq!(&l[..12], &want[..], "{l:?}");
        assert!(l[12..].iter().all(|&x| x < 2), "the noise joins one of the two speakers");
    }

    #[test]
    fn a_known_count_with_two_alike_voices_still_finds_everyone() {
        // A and B sound alike (similarity 0.6, so the plain threshold joins them), C is different, plus two stray
        // windows. Merging down to 3 groups kept the strays as "speakers" and A+B as one (a real call: 1 speaker).
        let mix = |i: usize| {
            let mut x = vec![0.0; 8];
            x[0] = 0.6;
            x[1] = 0.8;
            x[5] = 0.05 * i as f32;
            unit(&x)
        };
        let mut e: Vec<Vec<f32>> = Vec::new();
        for i in 0..6 {
            e.push(v(0, 4, 0.05 * i as f32)); // A
            e.push(mix(i)); // B
            e.push(v(2, 3, 0.05 * i as f32)); // C
        }
        e.push(v(7, 6, 0.0));
        e.push(v(6, 7, 0.0));
        let l = cluster(&e, Some(3), None);
        assert_eq!(&l[..18], &(0..18).map(|i| i % 3).collect::<Vec<_>>()[..], "{l:?}");
        assert!(l[18..].iter().all(|&x| x < 3));
    }

    #[test]
    fn words_take_the_nearest_window_and_lines_split_on_speaker_or_pause() {
        let w = |t: &str, s: f32| Word { text: t.into(), start: s, end: s + 0.3 };
        let words = [w("hello", 0.1), w("there", 0.5), w("hi", 2.0), w("again", 4.0)];
        let wins = [(0.0, 1.5, 0), (1.5, 3.0, 1), (3.5, 5.0, 1)];
        let sp = label_words(&words, &wins);
        assert_eq!(sp, [0, 0, 1, 1]);
        let t = turns(&words, &sp);
        let texts: Vec<_> = t.iter().map(|t| (t.speaker, t.text.as_str())).collect();
        assert_eq!(texts, [(0, "hello there"), (1, "hi"), (1, "again")], "a 1.7 s pause starts a new line");
    }
}
