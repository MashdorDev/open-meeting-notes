'use client';

import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface AudioLevelUpdate {
  levels: { device_name: string; rms_level: number; is_active: boolean }[];
}

/** How long a check keeps the microphone open. */
const CHECK_SECONDS = 10;

/** RMS to a 0..1 bar on a decibel scale: -60 dBFS is empty, 0 is full. */
function barWidth(rms: number): number {
  if (rms <= 0) return 0;
  const db = 20 * Math.log10(rms);
  return Math.min(1, Math.max(0, (db + 60) / 60));
}

/**
 * Opens the microphone for a few seconds and shows its real level, so a
 * muted or wrong microphone shows up before the call instead of after. The
 * microphone stays closed otherwise: a meter that is always on would keep
 * the system's microphone-in-use light on whenever the home page is open.
 */
export function MicCheck({ micId }: { micId: string | null }) {
  const [checking, setChecking] = useState(false);
  const [level, setLevel] = useState(0);
  const [heard, setHeard] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);
  const stopTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (!checking) return;
    let cancelled = false;
    const unlisten = listen<AudioLevelUpdate>('audio-levels', event => {
      const rms = event.payload.levels[0]?.rms_level ?? 0;
      setLevel(barWidth(rms));
      if (event.payload.levels[0]?.is_active) setHeard(true);
    });
    (async () => {
      try {
        if (cancelled) return;
        // No id: the backend opens the system default, as a recording would.
        await invoke('start_audio_level_monitoring', { deviceNames: micId ? [micId] : [] });
        stopTimer.current = setTimeout(() => setChecking(false), CHECK_SECONDS * 1000);
      } catch (error) {
        console.error('Microphone check failed:', error);
        if (!cancelled) {
          setFailed(String(error));
          setChecking(false);
        }
      }
    })();
    return () => {
      cancelled = true;
      if (stopTimer.current) clearTimeout(stopTimer.current);
      unlisten.then(fn => fn());
      invoke('stop_audio_level_monitoring').catch(error => console.error('Failed to stop the microphone check:', error));
      setLevel(0);
    };
  }, [checking, micId]);

  if (!checking) {
    return (
      <button
        type="button"
        onClick={() => { setHeard(false); setFailed(null); setChecking(true); }}
        className="text-sm text-blue-700 hover:underline"
      >
        {failed ? 'Microphone check failed, try again' : 'Check mic'}
      </button>
    );
  }
  return (
    <span className="inline-flex items-center gap-2 text-sm text-gray-600">
      <span className="inline-block h-2 w-24 rounded-full bg-gray-200 overflow-hidden" aria-hidden>
        <span className="block h-full bg-green-500 transition-[width] duration-100" style={{ width: `${level * 100}%` }} />
      </span>
      {heard ? 'Hearing you' : 'Say something…'}
      <button type="button" onClick={() => setChecking(false)} className="text-blue-700 hover:underline">Done</button>
    </span>
  );
}
