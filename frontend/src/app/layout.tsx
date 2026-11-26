'use client'

import './globals.css'
import localFont from 'next/font/local'
import Sidebar from '@/components/Sidebar'
import { SidebarProvider } from '@/components/Sidebar/SidebarProvider'
import MainContent from '@/components/MainContent'
import AnalyticsProvider from '@/components/AnalyticsProvider'
import { Toaster, toast } from 'sonner'
import "sonner/dist/styles.css"
import { useState, useEffect, useCallback } from 'react'
import { listen, UnlistenFn } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'
import { useRouter } from 'next/navigation'
import { TooltipProvider } from '@/components/ui/tooltip'
import { RecordingStateProvider } from '@/contexts/RecordingStateContext'
import { OllamaDownloadProvider } from '@/contexts/OllamaDownloadContext'
import { TranscriptProvider } from '@/contexts/TranscriptContext'
import { ConfigProvider, useConfig } from '@/contexts/ConfigContext'
import { OnboardingProvider } from '@/contexts/OnboardingContext'
import { OnboardingFlow } from '@/components/onboarding'
import { loadBetaFeatures } from '@/types/betaFeatures'
import { DownloadProgressToastProvider } from '@/components/shared/DownloadProgressToast'
import { UpdateCheckProvider } from '@/components/UpdateCheckProvider'
import { RecordingPostProcessingProvider } from '@/contexts/RecordingPostProcessingProvider'
import { ImportAudioDialog, ImportDropOverlay } from '@/components/ImportAudio'
import { ImportDialogProvider } from '@/contexts/ImportDialogContext'
import { isAudioExtension, getAudioFormatsDisplayList } from '@/constants/audioFormats'
import { startTheme, THEME_BOOT_SCRIPT } from '@/lib/theme'
import { PLATFORM_BOOT_SCRIPT } from '@/lib/platform'


// Both SIL Open Font License, from versioned @fontsource packages. Loading
// the files locally keeps the build off Google Fonts: a CI build failed
// there when Google's CSS came back in a shape next/font could not parse.
const figtree = localFont({
  src: '../../node_modules/@fontsource-variable/figtree/files/figtree-latin-wght-normal.woff2',
  weight: '300 900',
  variable: '--font-figtree',
})
const bricolage = localFont({
  src: '../../node_modules/@fontsource-variable/bricolage-grotesque/files/bricolage-grotesque-latin-wght-normal.woff2',
  weight: '200 800',
  variable: '--font-bricolage',
})

// Module-level component — stable reference across RootLayout re-renders.
// Defined here (not inside RootLayout) so React never sees a new function type
// on re-render, which would cause unmount/remount and break initialization logic.
function ConditionalImportDialog({
  showImportDialog,
  handleImportDialogClose,
  importFilePath,
}: {
  showImportDialog: boolean;
  handleImportDialogClose: (open: boolean) => void;
  importFilePath: string | null;
}) {
  const { betaFeatures } = useConfig();

  // Only mount ImportAudioDialog (and its hooks/listeners) when feature is enabled
  if (!betaFeatures.importAndRetranscribe) {
    return null;
  }

  return (
    <ImportAudioDialog
      open={showImportDialog}
      onOpenChange={handleImportDialogClose}
      preselectedFile={importFilePath}
    />
  );
}

// export { metadata } from './metadata'

export default function RootLayout({
  children,
}: {
  children: React.ReactNode
}) {
  const [showOnboarding, setShowOnboarding] = useState(false)
  const router = useRouter()
  const [onboardingCompleted, setOnboardingCompleted] = useState(false)

  // Import audio state
  const [showDropOverlay, setShowDropOverlay] = useState(false)
  const [showImportDialog, setShowImportDialog] = useState(false)
  const [importFilePath, setImportFilePath] = useState<string | null>(null)

  useEffect(() => startTheme(), [])

  useEffect(() => {
    // Check onboarding status first
    invoke<{ completed: boolean } | null>('get_onboarding_status')
      .then((status) => {
        const isComplete = status?.completed ?? false
        setOnboardingCompleted(isComplete)

        if (!isComplete) {
          console.log('[Layout] Onboarding not completed, showing onboarding flow')
          setShowOnboarding(true)
        } else {
          console.log('[Layout] Onboarding completed, showing main app')
        }
      })
      .catch((error) => {
        console.error('[Layout] Failed to check onboarding status:', error)
        // Default to showing onboarding if we can't check
        setShowOnboarding(true)
        setOnboardingCompleted(false)
      })
  }, [])

  // In production the webview's own menu (Reload, Back) makes no sense on the
  // app's chrome, but on selected text or in a text field the system menu is
  // the native one (Copy, Look Up, Spelling), so it stays there.
  useEffect(() => {
    if (process.env.NODE_ENV === 'production') {
      const handleContextMenu = (e: MouseEvent) => {
        const target = e.target as HTMLElement | null;
        const editable = target?.closest('input, textarea, [contenteditable="true"]');
        // Only a right-click on the selected text itself: a selection elsewhere
        // must not open the webview menu (Refresh) over the app's controls.
        const selection = window.getSelection();
        const onSelection = !!target && !!selection && selection.toString().trim() !== ''
          && selection.containsNode(target, true);
        if (!editable && !onSelection) e.preventDefault();
      };
      document.addEventListener('contextmenu', handleContextMenu);
      return () => document.removeEventListener('contextmenu', handleContextMenu);
    }
  }, []);
  useEffect(() => {
    // Listen for tray recording toggle request
    const unlisten = listen('request-recording-toggle', () => {
      console.log('[Layout] Received request-recording-toggle from tray');

      if (showOnboarding) {
        toast.error("Please complete setup first", {
          description: "You need to finish onboarding before you can start recording."
        });
      } else {
        // If in main app, forward to useRecordingStart via window event
        console.log('[Layout] Forwarding to start-recording-from-sidebar');
        window.dispatchEvent(new CustomEvent('start-recording-from-sidebar'));
      }
    });

    return () => {
      unlisten.then(fn => fn());
    };
  }, [showOnboarding]);

  // Call detection (call_detect.rs): a call app started or stopped using the
  // microphone. The notification reaches the user outside the app; these
  // toasts carry the actions inside it.
  useEffect(() => {
    type CallEvent = { key: string; name: string; auto_started: boolean };
    const started = listen<CallEvent>('call-detected', ({ payload }) => {
      // Not during setup: there may be no model yet, and Record would leave
      // a start flag behind for later.
      if (showOnboarding) return;
      if (payload.auto_started) {
        toast.success(`Recording your ${payload.name} call`);
        return;
      }
      toast(`${payload.name} is using your microphone`, {
        description: 'Record this call?',
        duration: 30_000,
        action: {
          label: 'Record',
          onClick: async () => {
            // The toast lives 30 s; a recording may have started meanwhile.
            if (await invoke<boolean>('is_recording')) {
              toast('Already recording');
              return;
            }
            await invoke('mark_recording_for_call', { key: payload.key });
            sessionStorage.setItem('autoStartRecording', 'true');
            window.location.assign('/');
          },
        },
      });
    });
    const ended = listen<CallEvent>('call-ended', ({ payload }) => {
      toast(`${payload.name} stopped using your microphone`, {
        description: 'Stop recording?',
        duration: 60_000,
        action: {
          label: 'Stop',
          onClick: () => {
            invoke<boolean>('stop_recording_for_call', { key: payload.key })
              .then(stopped => { if (!stopped) toast('No recording of this call to stop'); })
              .catch(error => console.error('Stop failed:', error));
          },
        },
      });
    });
    return () => {
      started.then(fn => fn());
      ended.then(fn => fn());
    };
  }, [showOnboarding]);

  // The tray's Settings item. A client-side push keeps the page's state and
  // history; a full reload from the tray could land on the old page or leave
  // Back on a blank one.
  useEffect(() => {
    const unlisten = listen('open-settings', () => {
      router.push('/settings');
    });
    return () => {
      unlisten.then(fn => fn());
    };
  }, [router]);

  // Handle file drop for audio import
  const handleFileDrop = useCallback((paths: string[]) => {
    // Check if beta features are enabled (read from localStorage directly since we're outside ConfigProvider)
    const betaFeatures = loadBetaFeatures();

    if (!betaFeatures.importAndRetranscribe) {
      toast.error('Beta feature disabled', {
        description: 'Enable "Import Audio & Retranscribe" in Settings > Beta to use this feature.'
      });
      return;
    }

    // Find the first audio file
    const audioFile = paths.find(p => {
      const ext = p.split('.').pop()?.toLowerCase();
      return !!ext && isAudioExtension(ext);
    });

    if (audioFile) {
      console.log('[Layout] Audio file dropped:', audioFile);
      setImportFilePath(audioFile);
      setShowImportDialog(true);
    } else if (paths.length > 0) {
      toast.error('Please drop an audio file', {
        description: `Supported formats: ${getAudioFormatsDisplayList()}`
      });
    }
  }, []);

  // Listen for drag-drop events
  useEffect(() => {
    if (showOnboarding) return; // Don't handle drops during onboarding

    const unlisteners: UnlistenFn[] = [];
    const cleanedUpRef = { current: false };

    const setupListeners = async () => {
      // Drag enter/over - show overlay only if beta feature is enabled
      const unlistenDragEnter = await listen('tauri://drag-enter', () => {
        if (loadBetaFeatures().importAndRetranscribe) {
          setShowDropOverlay(true);
        }
      });
      if (cleanedUpRef.current) {
        unlistenDragEnter();
        return;
      }
      unlisteners.push(unlistenDragEnter);

      // Drag leave - hide overlay
      const unlistenDragLeave = await listen('tauri://drag-leave', () => {
        setShowDropOverlay(false);
      });
      // The cleanup already removed the listeners registered before it ran;
      // removing one twice makes Tauri throw (listeners[eventId] is gone).
      if (cleanedUpRef.current) {
        unlistenDragLeave();
        return;
      }
      unlisteners.push(unlistenDragLeave);

      // Drop - process files
      const unlistenDrop = await listen<{ paths: string[] }>('tauri://drag-drop', (event) => {
        setShowDropOverlay(false);
        handleFileDrop(event.payload.paths);
      });
      if (cleanedUpRef.current) {
        unlistenDrop();
        return;
      }
      unlisteners.push(unlistenDrop);
    };

    setupListeners();

    return () => {
      cleanedUpRef.current = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [showOnboarding, handleFileDrop]);

  // Handle import dialog close
  const handleImportDialogClose = useCallback((open: boolean) => {
    setShowImportDialog(open);
    if (!open) {
      setImportFilePath(null);
    }
  }, []);

  // Handler for ImportDialogProvider - opens import dialog from any child component
  const handleOpenImportDialog = useCallback((filePath?: string | null) => {
    setImportFilePath(filePath ?? null);
    setShowImportDialog(true);
  }, []);

  // The menu bar's Import Audio… (macOS), and the same commands as keyboard
  // shortcuts on Windows, where apps usually have no menu bar.
  // Same gates as the other ways in: not during onboarding (no model yet),
  // and only with the Import beta on, saying so when it is off.
  const openImportFromShortcut = useCallback(() => {
    if (showOnboarding) return;
    if (!loadBetaFeatures().importAndRetranscribe) {
      toast.error('Beta feature disabled', {
        description: 'Enable "Import Audio & Retranscribe" in Settings > Beta to use this feature.'
      });
      return;
    }
    handleOpenImportDialog();
  }, [showOnboarding, handleOpenImportDialog]);
  useEffect(() => {
    const unlisten = listen('open-import-dialog', openImportFromShortcut);
    return () => {
      unlisten.then(fn => fn());
    };
  }, [openImportFromShortcut]);
  useEffect(() => {
    if (document.documentElement.dataset.platform !== 'windows') return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (!e.ctrlKey || e.altKey || e.metaKey || e.repeat) return;
      const key = e.key.toLowerCase();
      if (key === ',' && !e.shiftKey) {
        e.preventDefault();
        router.push('/settings');
      } else if (key === 'o' && !e.shiftKey) {
        e.preventDefault();
        openImportFromShortcut();
      } else if (key === 'r' && e.shiftKey) {
        e.preventDefault();
        invoke('toggle_recording_from_shortcut').catch(error => console.error('Shortcut failed:', error));
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [router, openImportFromShortcut]);

  const handleOnboardingComplete = () => {
    console.log('[Layout] Onboarding completed, reloading app')
    setShowOnboarding(false)
    setOnboardingCompleted(true)
    // Optionally reload the window to ensure all state is fresh
    window.location.reload()
  }

  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        <script dangerouslySetInnerHTML={{ __html: THEME_BOOT_SCRIPT }} />
        <script dangerouslySetInnerHTML={{ __html: PLATFORM_BOOT_SCRIPT }} />
      </head>
      <body className={`${figtree.variable} ${bricolage.variable} font-sans antialiased`}>
        <AnalyticsProvider>
          <RecordingStateProvider>
            <TranscriptProvider>
              <ConfigProvider>
                <OllamaDownloadProvider>
                  <OnboardingProvider>
                    <UpdateCheckProvider>
                      <SidebarProvider>
                        <TooltipProvider>
                          <RecordingPostProcessingProvider>
                            <ImportDialogProvider onOpen={handleOpenImportDialog}>
                              {/* Download progress toast provider - listens for background downloads */}
                              <DownloadProgressToastProvider />

                              {/* Show onboarding or main app */}
                              {showOnboarding ? (
                                <OnboardingFlow onComplete={handleOnboardingComplete} />
                              ) : (
                                <div className="flex">
                                  <Sidebar />
                                  <MainContent>{children}</MainContent>
                                </div>
                              )}
                              {/* Import audio overlay and dialog */}
                              <ImportDropOverlay visible={showDropOverlay} />
                              <ConditionalImportDialog
                                showImportDialog={showImportDialog}
                                handleImportDialogClose={handleImportDialogClose}
                                importFilePath={importFilePath}
                              />
                            </ImportDialogProvider>
                          </RecordingPostProcessingProvider>
                        </TooltipProvider>
                      </SidebarProvider>
                    </UpdateCheckProvider>
                  </OnboardingProvider>

                </OllamaDownloadProvider>
              </ConfigProvider>
            </TranscriptProvider>
          </RecordingStateProvider>
        </AnalyticsProvider>

        <Toaster position="bottom-center" richColors closeButton />
      </body>
    </html>
  )
}
