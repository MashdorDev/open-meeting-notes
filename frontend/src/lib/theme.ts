import { useSyncExternalStore } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';

export type ThemeChoice = 'light' | 'dark' | 'system';

const STORAGE_KEY = 'theme';
const darkQuery = () => window.matchMedia('(prefers-color-scheme: dark)');

export function loadThemeChoice(): ThemeChoice {
  if (typeof window === 'undefined') return 'system'; // static export prerender
  const saved = localStorage.getItem(STORAGE_KEY);
  return saved === 'light' || saved === 'dark' ? saved : 'system';
}

function applyTheme(choice: ThemeChoice) {
  const dark = choice === 'dark' || (choice === 'system' && darkQuery().matches);
  document.documentElement.classList.toggle('dark', dark);
  // Title bar follows the app; null hands it back to the system setting.
  getCurrentWindow()
    .setTheme(choice === 'system' ? null : choice)
    .catch((error) => console.error('Failed to set window theme:', error));
}

export function saveThemeChoice(choice: ThemeChoice) {
  localStorage.setItem(STORAGE_KEY, choice);
  applyTheme(choice);
}

/** Applies the saved choice and follows the system while it is "system". */
export function startTheme(): () => void {
  applyTheme(loadThemeChoice());
  const query = darkQuery();
  const onSystemChange = () => {
    if (loadThemeChoice() === 'system') applyTheme('system');
  };
  query.addEventListener('change', onSystemChange);
  return () => query.removeEventListener('change', onSystemChange);
}

/**
 * Runs before React, inline in <head>, so a dark start never flashes white.
 * Keep the logic in step with applyTheme above.
 */
export const THEME_BOOT_SCRIPT = `try{var t=localStorage.getItem('${STORAGE_KEY}');if(t==='dark'||(t!=='light'&&matchMedia('(prefers-color-scheme: dark)').matches))document.documentElement.classList.add('dark')}catch(e){}`;

function subscribeToDarkClass(onChange: () => void): () => void {
  const observer = new MutationObserver(onChange);
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
  return () => observer.disconnect();
}

/**
 * The theme actually on screen, for components that take a theme prop and
 * set their own colors (BlockNote paints light variables on its own root).
 */
export function useResolvedTheme(): 'light' | 'dark' {
  return useSyncExternalStore(
    subscribeToDarkClass,
    () => (document.documentElement.classList.contains('dark') ? 'dark' : 'light'),
    () => 'light',
  );
}
