'use client';

import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useRouter } from 'next/navigation';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import { useConfig } from '@/contexts/ConfigContext';
import { resolveDeviceLabel } from '@/lib/audioDevices';
import { Button } from '@/components/ui/button';
import { MicCheck } from '@/components/MicCheck';
import { useIsLinux } from '@/hooks/usePlatform';

interface HomeMeeting {
  id: string;
  title: string;
  created_at: string;
  duration_seconds: number | null;
  names: string[];
  has_summary: boolean;
  summary_line: string | null;
  action_items: number | null;
}

interface Attention {
  meeting_id: string;
  title: string;
  kind: 'names' | 'no_summary' | 'summary_failed';
  count: number;
}

interface HomeOverview {
  meetings: HomeMeeting[];
  attention: Attention[];
  week_meetings: number;
  week_seconds: number;
}

function startOfWeek(now: Date): Date {
  // Weeks start on Monday.
  const day = (now.getDay() + 6) % 7;
  return new Date(now.getFullYear(), now.getMonth(), now.getDate() - day);
}

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

function actionItemsLabel(count: number | null): string | null {
  if (count === null) return null;
  if (count === 0) return 'no action items';
  return count === 1 ? '1 action item' : `${count} action items`;
}

function attentionLabel(a: Attention): string {
  switch (a.kind) {
    case 'names':
      return a.count === 1 ? '1 suggested name to confirm' : `${a.count} suggested names to confirm`;
    case 'no_summary':
      return 'no summary yet';
    case 'summary_failed':
      return 'summary failed';
  }
}

/**
 * The home page while nothing is recording: the latest meeting with a line
 * of its summary, what still needs Dor (names to confirm, missing or failed
 * summaries), the meetings before it, and this week's totals. The sidebar
 * already lists every meeting, so this shows only what to act on next.
 */
export function RecentMeetings({ hasMicrophone, hasSystemAudio }: { hasMicrophone: boolean; hasSystemAudio: boolean }) {
  const router = useRouter();
  // The sidebar's list changes when a meeting is saved, renamed or deleted.
  const { meetings: sidebarMeetings, setCurrentMeeting } = useSidebar();
  const { selectedDevices } = useConfig();
  // No microphone check on Linux yet (see simple_level_monitor.rs).
  const isLinux = useIsLinux();
  const [overview, setOverview] = useState<HomeOverview | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [micLabel, setMicLabel] = useState<string | null>(null);
  // Today and this week move at midnight even when no meeting changes.
  const [today, setToday] = useState(() => new Date().toDateString());

  useEffect(() => {
    let current = true;
    const weekStart = startOfWeek(new Date(today)).toISOString();
    invoke<HomeOverview>('api_get_home_overview', { weekStart })
      .then(found => {
        if (!current) return;
        setOverview(found);
        setLoadFailed(false);
      })
      .catch(error => {
        console.error('Failed to load the home page:', error);
        if (current) setLoadFailed(true);
      });
    return () => { current = false; };
  }, [sidebarMeetings, today]);

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

  const open = (id: string, title: string) => {
    setCurrentMeeting({ id, title });
    router.push(`/meeting-details?id=${id}`);
  };

  const now = new Date(today);
  const when = (m: HomeMeeting) =>
    [dayLabel(new Date(m.created_at), now), lengthLabel(m.duration_seconds), m.names.join(', ') || null].filter(Boolean).join(' · ');

  const [latest, ...rest] = overview?.meetings ?? [];
  // A meeting already under Needs attention isn't listed twice.
  const flagged = new Set(overview?.attention.map(a => a.meeting_id));
  const recent = rest.filter(m => !flagged.has(m.id));
  const ready = [micLabel ?? 'Default microphone', hasSystemAudio ? 'computer audio' : 'no computer audio'].join(' · ');

  return (
    <div className="pt-6 pb-28 text-left">
      {hasMicrophone && (
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1 mb-8">
          <p className="text-sm text-gray-500">Ready to record · {ready}</p>
          {!isLinux && <MicCheck micId={selectedDevices.micDevice} />}
        </div>
      )}

      {loadFailed && overview === null && (
        <p className="text-sm text-gray-500">The home page could not load. Your meetings are still in the sidebar.</p>
      )}

      {overview !== null && !latest && (
        <div className="text-gray-500">
          <p className="text-lg font-semibold text-gray-800">No meetings yet</p>
          <p className="text-sm mt-1">Press record when a call starts. The transcript and summary land here.</p>
        </div>
      )}

      {latest && (
        <section className="mb-8">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-gray-500 mb-2">Continue where you left off</h2>
          <div className="rounded-lg border border-gray-200 bg-white p-4">
            <div className="flex items-baseline justify-between gap-4">
              <p className="text-lg font-semibold text-gray-900 truncate">{latest.title}</p>
              <Button size="sm" variant="outline" onClick={() => open(latest.id, latest.title)}>Open</Button>
            </div>
            <p className="text-sm text-gray-500 mt-0.5">{when(latest)}</p>
            <p className="text-sm text-gray-800 mt-3">
              {latest.has_summary ? latest.summary_line ?? 'Summary ready.' : 'No summary yet.'}
            </p>
            {latest.has_summary && latest.action_items !== null && (
              <p className="text-sm text-gray-500 mt-1">{actionItemsLabel(latest.action_items)}</p>
            )}
          </div>
        </section>
      )}

      {overview && overview.attention.length > 0 && (
        <section className="mb-8">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-gray-500 mb-1">Needs attention</h2>
          <ul>
            {overview.attention.map(a => (
              <li key={`${a.meeting_id}-${a.kind}`}>
                <button
                  type="button"
                  onClick={() => open(a.meeting_id, a.title)}
                  className="w-full flex items-baseline justify-between gap-4 rounded-md px-2 py-2 -mx-2 text-left hover:bg-gray-100"
                >
                  <span className="truncate text-gray-900">{a.title}</span>
                  <span className={`shrink-0 text-sm ${a.kind === 'summary_failed' ? 'text-red-600' : 'text-amber-700'}`}>{attentionLabel(a)}</span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}

      {recent.length > 0 && (
        <section className="mb-8">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-gray-500 mb-1">Recent</h2>
          <ul>
            {recent.map(m => (
              <li key={m.id}>
                <button
                  type="button"
                  onClick={() => open(m.id, m.title)}
                  className="w-full flex items-baseline justify-between gap-4 rounded-md px-2 py-2 -mx-2 text-left hover:bg-gray-100"
                >
                  <span className="truncate text-gray-900">{m.title}</span>
                  <span className="shrink-0 text-sm text-gray-500">
                    {[when(m), actionItemsLabel(m.action_items)].filter(Boolean).join(' · ')}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}

      {overview && overview.week_meetings > 0 && (
        <p className="text-sm text-gray-500">
          This week · {overview.week_meetings} {overview.week_meetings === 1 ? 'meeting' : 'meetings'} · {lengthLabel(overview.week_seconds)}
        </p>
      )}
    </div>
  );
}
