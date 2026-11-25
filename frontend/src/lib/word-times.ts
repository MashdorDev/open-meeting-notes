import type { TranscriptWord } from '@/types';

/** How far ahead a shown word may look for its timed match: the shown text
 * drops filler words ("um") that the engine still timed. */
const MATCH_LOOKAHEAD = 3;

const normalize = (word: string) => word.toLowerCase().replace(/[^\p{L}\p{N}]/gu, '');

/**
 * Times every word of a line as shown. Words the engine timed keep their
 * times; the rest (older meetings, Whisper, words that don't match) share
 * the time between their timed neighbours by length.
 */
export function timeWords(text: string, start: number, end: number, words?: TranscriptWord[]): TranscriptWord[] {
  const shown = text.split(/\s+/).filter(Boolean);
  const timed: (TranscriptWord | null)[] = shown.map(() => null);
  let next = 0;
  shown.forEach((word, i) => {
    if (!words) return;
    const key = normalize(word);
    for (let j = next; j < Math.min(words.length, next + MATCH_LOOKAHEAD); j++) {
      if (normalize(words[j].text) === key) {
        timed[i] = { text: word, start: words[j].start, end: words[j].end };
        next = j + 1;
        return;
      }
    }
  });

  let i = 0;
  while (i < shown.length) {
    if (timed[i]) { i++; continue; }
    let j = i;
    while (j < shown.length && !timed[j]) j++;
    const from = i > 0 ? timed[i - 1]!.end : start;
    const to = Math.max(from, j < shown.length ? timed[j]!.start : end);
    const run = shown.slice(i, j);
    const total = run.reduce((sum, w) => sum + w.length, 0);
    let at = from;
    run.forEach((word, k) => {
      const length = ((to - from) * word.length) / total;
      timed[i + k] = { text: word, start: at, end: at + length };
      at += length;
    });
    i = j;
  }
  return timed as TranscriptWord[];
}

/** The word being said at `seconds`, or -1 between and outside words. */
export function wordAt(words: TranscriptWord[], seconds: number): number {
  for (let i = words.length - 1; i >= 0; i--) {
    if (words[i].start <= seconds) return seconds < words[i].end ? i : -1;
  }
  return -1;
}
