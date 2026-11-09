import { describe, it, expect, afterEach } from 'vitest';
import { render, screen, cleanup, fireEvent } from '@testing-library/react';
import { RecordingPlayer } from './RecordingPlayer';
import { LiveCaptions } from './LiveCaptions';
import type { RecordingInfo } from '../types';

function recording(overrides: Partial<RecordingInfo> = {}): RecordingInfo {
  return {
    id: 'rec1',
    stream_id: 's1',
    host_user_id: '@host:example.com',
    media_type: 'video',
    duration_ms: 60_000,
    status: 'ready',
    created_at: '2026-11-09T00:00:00Z',
    ...overrides,
  };
}

describe('RecordingPlayer captions', () => {
  afterEach(cleanup);

  it('renders a subtitle track per caption url', () => {
    const { container } = render(
      <RecordingPlayer
        recording={recording({
          caption_urls: { en: 'https://cdn/r.en.vtt', de: 'https://cdn/r.de.vtt' },
        })}
        recordingUrl="https://cdn/r.mp4"
      />,
    );
    const tracks = container.querySelectorAll('video > track');
    expect(tracks).toHaveLength(2);
    expect(tracks[0]?.getAttribute('kind')).toBe('subtitles');
    expect(tracks[0]?.getAttribute('srclang')).toBe('de');
    expect(tracks[0]?.getAttribute('src')).toBe('https://cdn/r.de.vtt');
  });

  it('offers a language picker defaulting to English and switching off', () => {
    render(
      <RecordingPlayer
        recording={recording({
          caption_urls: { en: 'https://cdn/r.en.vtt', de: 'https://cdn/r.de.vtt' },
        })}
        recordingUrl="https://cdn/r.mp4"
      />,
    );
    const picker = screen.getByLabelText('Subtitle language') as HTMLSelectElement;
    expect(picker.value).toBe('en');
    expect(screen.getByRole('option', { name: 'German' })).toBeDefined();
    fireEvent.change(picker, { target: { value: '' } });
    expect(picker.value).toBe('');
  });

  it('renders no picker when there are no caption urls', () => {
    render(<RecordingPlayer recording={recording()} recordingUrl="https://cdn/r.mp4" />);
    expect(screen.queryByLabelText('Subtitle language')).toBeNull();
  });
});

describe('LiveCaptions', () => {
  afterEach(cleanup);

  it('renders nothing without segments', () => {
    const { container } = render(<LiveCaptions segments={[]} />);
    expect(container.firstChild).toBeNull();
  });

  it('renders final segments solid and interim lighter', () => {
    render(
      <LiveCaptions
        segments={[
          { id: 'f1', text: 'hello world', final: true },
          { id: 'i1', text: 'and more', final: false },
        ]}
      />,
    );
    expect(screen.getByText('hello world').className).toContain('mm-live-captions__final');
    expect(screen.getByText('and more').className).toContain('mm-live-captions__interim');
  });
});
