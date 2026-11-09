import { describe, expect, it } from 'vitest';
import {
  captionLanguageLabel,
  parseTranscriptionPayload,
  pickCaptionTrack,
  reduceSegments,
  TRANSCRIPTION_TOPIC,
  type TranscriptionSegment,
} from './captions';

function payload(obj: unknown): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(obj));
}

function seg(partial: Partial<TranscriptionSegment> & { text: string }): TranscriptionSegment {
  return { id: 's1', final: false, ...partial };
}

describe('TRANSCRIPTION_TOPIC', () => {
  it('uses the mm.* namespace', () => {
    expect(TRANSCRIPTION_TOPIC).toBe('mm.transcription');
  });
});

describe('parseTranscriptionPayload', () => {
  it('parses a full final segment', () => {
    const out = parseTranscriptionPayload(
      payload({ id: 'a', text: 'hello', final: true, language: 'en', start_time: 1, end_time: 2 }),
    );
    expect(out).toEqual({
      id: 'a',
      text: 'hello',
      final: true,
      language: 'en',
      start_time: 1,
      end_time: 2,
    });
  });

  it('coerces a missing final flag to interim', () => {
    const out = parseTranscriptionPayload(payload({ id: 'a', text: 'hel' }));
    expect(out?.final).toBe(false);
  });

  it('rejects non-JSON payloads', () => {
    expect(parseTranscriptionPayload(new TextEncoder().encode('not json'))).toBeNull();
  });

  it('rejects payloads without text', () => {
    expect(parseTranscriptionPayload(payload({ id: 'a', final: true }))).toBeNull();
    expect(parseTranscriptionPayload(payload({ text: '' }))).toBeNull();
    expect(parseTranscriptionPayload(payload(['text']))).toBeNull();
  });
});

describe('pickCaptionTrack', () => {
  const urls = { en: 'https://cdn/r.en.vtt', de: 'https://cdn/r.de.vtt' };

  it('returns null when there are no tracks', () => {
    expect(pickCaptionTrack(undefined, ['en'])).toBeNull();
    expect(pickCaptionTrack({}, ['en'])).toBeNull();
  });

  it('matches exact and case-insensitive tags', () => {
    expect(pickCaptionTrack(urls, ['de'])).toBe('de');
    expect(pickCaptionTrack({ 'EN': 'u' }, ['en'])).toBe('EN');
  });

  it('falls back from region to base language', () => {
    expect(pickCaptionTrack(urls, ['en-US', 'de'])).toBe('en');
    expect(pickCaptionTrack(urls, ['fr', 'de-AT'])).toBe('de');
  });

  it('falls back to English, then first sorted track', () => {
    expect(pickCaptionTrack(urls, ['fr'])).toBe('en');
    expect(pickCaptionTrack({ fr: 'u', de: 'v' }, ['ja'])).toBe('de');
  });
});

describe('reduceSegments', () => {
  it('appends interim segments and updates them in place by id', () => {
    let state: TranscriptionSegment[] = [];
    state = reduceSegments(state, seg({ id: 'a', text: 'hel' }));
    state = reduceSegments(state, seg({ id: 'a', text: 'hello' }));
    expect(state).toHaveLength(1);
    expect(state[0]!.text).toBe('hello');
    expect(state[0]!.final).toBe(false);
  });

  it('promotes a final segment and retires its interim', () => {
    let state = [seg({ id: 'a', text: 'hello' })];
    state = reduceSegments(state, seg({ id: 'a', text: 'hello', final: true }));
    expect(state).toHaveLength(1);
    expect(state[0]!.final).toBe(true);
  });

  it('keeps a final scrollback capped at maxFinals', () => {
    let state: TranscriptionSegment[] = [];
    for (let i = 0; i < 5; i++) {
      state = reduceSegments(state, seg({ id: `f${i}`, text: `t${i}`, final: true }), 3);
    }
    expect(state.map((s) => s.id)).toEqual(['f2', 'f3', 'f4']);
  });

  it('renders interim distinctly: interim never counted as final', () => {
    const state = reduceSegments([], seg({ id: 'a', text: 'hel' }));
    expect(state.filter((s) => s.final)).toHaveLength(0);
  });
});

describe('captionLanguageLabel', () => {
  it('labels known languages and regions', () => {
    expect(captionLanguageLabel('en')).toBe('English');
    expect(captionLanguageLabel('de')).toBe('German');
    expect(captionLanguageLabel('en-US')).toBe('English (US)');
  });

  it('falls back to the raw key', () => {
    expect(captionLanguageLabel('xx')).toBe('xx');
  });
});
