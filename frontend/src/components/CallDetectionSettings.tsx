'use client';

import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { Switch } from '@/components/ui/switch';
import { useIsLinux } from '@/hooks/usePlatform';

interface Settings {
  enabled: boolean;
  auto_start: boolean;
  ignored_apps: string[];
}

interface CallApp {
  key: string;
  name: string;
}

/**
 * Settings > Recordings: notice when a call app starts using the microphone
 * and offer to record (or record right away), with a list of apps to watch.
 */
export function CallDetectionSettings() {
  const isLinux = useIsLinux();
  const [settings, setSettings] = useState<Settings | null>(null);
  const [apps, setApps] = useState<CallApp[]>([]);

  useEffect(() => {
    Promise.all([
      invoke<Settings>('get_call_detection_settings'),
      invoke<CallApp[]>('list_call_apps'),
    ])
      .then(([loaded, known]) => {
        setSettings(loaded);
        setApps(known);
      })
      .catch(error => console.error('Failed to load call detection settings:', error));
  }, []);

  const save = async (next: Settings) => {
    const previous = settings;
    setSettings(next);
    try {
      await invoke('set_call_detection_settings', { settings: next });
    } catch (error) {
      setSettings(previous);
      toast.error(`Could not save: ${String(error)}`);
    }
  };

  if (isLinux) return null;
  if (!settings) return null;

  const toggleApp = (key: string, watched: boolean) =>
    void save({
      ...settings,
      ignored_apps: watched ? settings.ignored_apps.filter(k => k !== key) : [...settings.ignored_apps, key],
    });

  return (
    <div className="p-4 border rounded-lg space-y-4">
      <div className="flex items-center justify-between">
        <div className="flex-1">
          <div className="font-medium">Notice calls</div>
          <div className="text-sm text-gray-600">
            When a call app starts using the microphone, offer to record the call. Only which app holds the
            microphone is checked, never what it hears.
          </div>
        </div>
        <Switch checked={settings.enabled} onCheckedChange={enabled => void save({ ...settings, enabled })} />
      </div>

      {settings.enabled && (
        <>
          <div className="flex items-center justify-between">
            <div className="flex-1">
              <div className="font-medium">Start recording without asking</div>
              <div className="text-sm text-gray-600">
                Records as soon as a call is noticed, and shows a notification that it did.
              </div>
            </div>
            <Switch checked={settings.auto_start} onCheckedChange={auto_start => void save({ ...settings, auto_start })} />
          </div>

          <div>
            <div className="font-medium mb-1">Apps to watch</div>
            <ul className="grid grid-cols-1 sm:grid-cols-2 gap-x-6">
              {apps.map(app => {
                const watched = !settings.ignored_apps.includes(app.key);
                return (
                  <li key={app.key}>
                    <label className="flex items-center gap-2 py-1 text-sm text-gray-800">
                      <input type="checkbox" checked={watched} onChange={e => toggleApp(app.key, e.target.checked)} />
                      {app.name}
                    </label>
                  </li>
                );
              })}
            </ul>
          </div>
        </>
      )}
    </div>
  );
}
