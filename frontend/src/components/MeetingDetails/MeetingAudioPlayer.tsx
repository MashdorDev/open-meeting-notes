"use client";

import { useEffect, useState, type RefObject } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';

interface MeetingAudioPlayerProps {
  meetingId: string;
  audioRef: RefObject<HTMLAudioElement>;
  /** Whether a playable recording is on show, so lines can offer seeking. */
  onAvailable: (available: boolean) => void;
  /** The playback position in seconds, on every time update and seek. */
  onPlayhead: (seconds: number) => void;
  /** On every play and seek, so the transcript follows playback again. */
  onPlayOrSeek: () => void;
}

/** How far playback moves before the transcript hears of it, while playing. */
const PLAYHEAD_STEP_SECONDS = 0.05;

/** Plays the meeting's saved recording; renders nothing when none was saved. */
export function MeetingAudioPlayer({ meetingId, audioRef, onAvailable, onPlayhead, onPlayOrSeek }: MeetingAudioPlayerProps) {
  const [src, setSrc] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [playing, setPlaying] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setSrc(null);
    setFailed(false);
    setPlaying(false);
    invoke<string | null>('get_meeting_audio_path', { meetingId })
      .then(path => {
        if (!cancelled && path) setSrc(convertFileSrc(path));
      })
      .catch(error => console.error('Failed to find the meeting recording:', error));
    return () => {
      cancelled = true;
    };
  }, [meetingId]);

  useEffect(() => onAvailable(!!src && !failed), [src, failed, onAvailable]);

  // timeupdate fires about four times a second, too coarse to follow single
  // words, so while playing the position is read every frame instead.
  useEffect(() => {
    if (!playing) return;
    let frame = 0;
    let sent = -1;
    const tick = () => {
      const seconds = audioRef.current?.currentTime;
      if (seconds !== undefined && Math.abs(seconds - sent) >= PLAYHEAD_STEP_SECONDS) {
        sent = seconds;
        onPlayhead(seconds);
      }
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [playing, audioRef, onPlayhead]);

  if (!src) return null;
  // Linux WebKitGTK cannot always stream from the asset scheme
  // (tauri-apps/tauri#3725); the file itself is still in the folder.
  if (failed) {
    return (
      <p className="mt-3 text-sm text-gray-500">
        The recording can&apos;t play here. Use Recording to open its folder.
      </p>
    );
  }
  return (
    <audio
      ref={audioRef}
      controls
      preload="metadata"
      src={src}
      onError={() => setFailed(true)}
      onTimeUpdate={e => onPlayhead(e.currentTarget.currentTime)}
      onSeeked={e => {
        onPlayhead(e.currentTarget.currentTime);
        onPlayOrSeek();
      }}
      onPlay={() => {
        setPlaying(true);
        onPlayOrSeek();
      }}
      onPause={() => setPlaying(false)}
      onEnded={() => setPlaying(false)}
      className="mt-3 w-full h-9"
    />
  );
}
