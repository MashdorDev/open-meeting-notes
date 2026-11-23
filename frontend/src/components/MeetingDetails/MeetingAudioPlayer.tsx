"use client";

import { useEffect, useState } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';

interface MeetingAudioPlayerProps {
  meetingId: string;
}

/** Plays the meeting's saved recording; renders nothing when none was saved. */
export function MeetingAudioPlayer({ meetingId }: MeetingAudioPlayerProps) {
  const [src, setSrc] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setSrc(null);
    invoke<string | null>('get_meeting_audio_path', { meetingId })
      .then(path => {
        if (!cancelled && path) setSrc(convertFileSrc(path));
      })
      .catch(error => console.error('Failed to find the meeting recording:', error));
    return () => {
      cancelled = true;
    };
  }, [meetingId]);

  if (!src) return null;
  return <audio controls preload="metadata" src={src} className="mt-3 w-full h-9" />;
}
