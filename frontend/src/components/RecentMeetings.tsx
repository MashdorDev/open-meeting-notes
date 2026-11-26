'use client';

import { useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useRouter } from 'next/navigation';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import { useConfig } from '@/contexts/ConfigContext';
import { resolveDeviceLabel } from '@/lib/audioDevices';

interface RecentMeeting {
  id: string;
  title: string;
  created_at: string;
  duration_seconds: number | null;
  names: string[];
}

const RECENT_LIMIT = 12;

function dayLabel(date: Date, now: Date): string {
  const startOfDay = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((startOfDay(now) - startOfDay(date)) / 86_400_000);
  if (days === 0) return 'Today';
  if (days === 1) return 'Yesterday';
  return date.toLocaleDateString(undefined, { weekday: 'long', day: 'numeric', month: 'long' });
}

function lengthLabel(seconds: number | null): string | null {
  if (seconds === null) return null;
  const minutes = Math.round(seconds / 60);
  if (minutes < 1) return 'under 1 min';
  if (minutes < 60) return `${minutes} min`;
  const rest = minutes % 60;
  return rest === 0 ? `${Math.floor(minutes / 60)} h` : `${Math.floor(minutes / 60)} h ${rest} min`;
}

function modelLabel(provider: string, model: string): string {
  if (provider === 'parakeet') return 'Parakeet';
  if (provider === 'localWhisper') return `Whisper ${model}`;
  return model || provider;
}

/**
 * The home page while nothing is recording: what the next recording will use,
 * and the latest meetings grouped by day, one click from each.
 */
export function RecentMeetings({ hasMicrophone, hasSystemAudio }: { hasMicrophone: boolean; hasSystemAudio: boolean }) {
  const router = useRouter();
  // The sidebar's list changes when a meeting is saved, renamed or deleted.
  const { meetings: sidebarMeetings, setCurrentMeeting } = useSidebar();
  const { selectedDevices, transcriptModelConfig } = useConfig();
  const [meetings, setMeetings] = useState<RecentMeeting[] | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [micLabel, setMicLabel] = useState<string | null>(null);
  // Today and Yesterday move at midnight even when no meeting changes.
  const [today, setToday] = useState(() => new Date().toDateString());

  useEffect(() => {
    let current = true;
    invoke<RecentMeeting[]>('api_get_recent_meetings', { limit: RECENT_LIMIT })
      .then(found => {
        if (!current) return;
        setMeetings(found);
        setLoadFailed(false);
      })
      .catch(error => {
        console.error('Failed to load recent meetings:', error);
        if (current) setLoadFailed(true);
      });
    return () => { current = false; };
  }, [sidebarMeetings]);

  useEffect(() => {
    const timer = setInterval(() => setToday(new Date().toDateString()), 60_000);
    return () => clearInterval(timer);
  }, []);

  // Device ids are not for display (on Linux they read like
  // "alsa_input.pci-0000_00_1f.3.analog-stereo").
  useEffect(() => {
    const id = selectedDevices.micDevice;
    if (!id) { setMicLabel(null); return; }
    let current = true;
    resolveDeviceLabel(id).then(label => { if (current) setMicLabel(label); });
    return () => { current = false; };
  }, [selectedDevices.micDevice]);

  const groups = useMemo(() => {
    const now = new Date(today);
    const byDay: { label: string; items: RecentMeeting[] }[] = [];
    for (const m of meetings ?? []) {
      const label = dayLabel(new Date(m.created_at), now);
      const last = byDay[byDay.length - 1];
      if (last?.label === label) last.items.push(m);
      else byDay.push({ label, items: [m] });
    }
    return byDay;
  }, [meetings, today]);

  const ready = [
    micLabel ?? 'Default microphone',
    hasSystemAudio ? 'computer audio' : 'no computer audio',
    modelLabel(transcriptModelConfig.provider, transcriptModelConfig.model),
  ].join(' · ');

  const open = (m: RecentMeeting) => {
    setCurrentMeeting({ id: m.id, title: m.title });
    router.push(`/meeting-details?id=${m.id}`);
  };

  return (
    <div className="pt-6 text-left">
      {hasMicrophone && <p className="text-sm text-gray-500 mb-8">Ready to record · {ready}</p>}

      {loadFailed && meetings === null && (
        <p className="text-sm text-gray-500">Recent meetings could not be loaded. They are still in the sidebar.</p>
      )}

      {meetings !== null && meetings.length === 0 && (
        <div className="text-gray-500">
          <p className="text-lg font-semibold text-gray-800">No meetings yet</p>
          <p className="text-sm mt-1">Press record when a call starts. The transcript and summary land here.</p>
        </div>
      )}

      {groups.map(group => (
        <section key={group.label} className="mb-6">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-gray-500 mb-1">{group.label}</h2>
          <ul>
            {group.items.map(m => {
              const time = new Date(m.created_at).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
              const details = [time, lengthLabel(m.duration_seconds), m.names.join(', ') || null].filter(Boolean).join(' · ');
              return (
                <li key={m.id}>
                  <button
                    type="button"
                    onClick={() => open(m)}
                    className="w-full flex items-baseline justify-between gap-4 rounded-md px-2 py-2 -mx-2 text-left hover:bg-gray-100"
                  >
                    <span className="truncate text-gray-900">{m.title}</span>
                    <span className="shrink-0 text-sm text-gray-500">{details}</span>
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      ))}
    </div>
  );
}
