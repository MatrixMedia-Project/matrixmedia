import { describe, expect, it } from 'vitest';
import {
  captionLanguageLabel,
  captionsOptionLabel,
  captionsShouldWrite,
  captionsStatusLine,
  CAPTIONS_OPT_INS,
  parseTranscriptionPayload,
  pickCaptionTrack,
  reduceSegments,
  TRANSCRIPTION_TOPIC,
  type TranscriptionSegment,
} from './captions';
import type { StreamCaptions } from './types';

function setting(overrides: Partial<StreamCaptions> = {}): StreamCaptions {
  return {
    opt_in: 'inherit',
    default_opt_in: true,
    wants_captions: true,
    stt: null,
    ...overrides,
  };
}

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

describe('host opt-in view model', () => {
  it('offers inherit/on/off with default-aware labels', () => {
    expect(CAPTIONS_OPT_INS).toEqual(['inherit', 'on', 'off']);
    expect(captionsOptionLabel('inherit', true)).toBe('Follow my default (on)');
    expect(captionsOptionLabel('inherit', false)).toBe('Follow my default (off)');
    expect(captionsOptionLabel('on', false)).toBe('On for this broadcast');
    expect(captionsOptionLabel('off', true)).toBe('Off for this broadcast');
  });

  it('explains what the stored setting means', () => {
    expect(captionsStatusLine(setting({ wants_captions: true }))).toContain('On.');
    expect(captionsStatusLine(setting({ wants_captions: false }))).toContain('Off.');
  });

  it('only writes when the choice differs from the stored option', () => {
    const current = setting({ opt_in: 'inherit' });
    expect(captionsShouldWrite(current, 'inherit')).toBe(false);
    expect(captionsShouldWrite(current, 'on')).toBe(true);
  });
});
