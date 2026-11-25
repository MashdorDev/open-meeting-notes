"use client";

import { useState, useCallback, useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { ButtonGroup } from '@/components/ui/button-group';
import { Copy, FolderOpen, RefreshCw, RotateCcw } from 'lucide-react';
import Analytics from '@/lib/analytics';
import { RetranscribeDialog } from './RetranscribeDialog';
import { ConfirmationModal } from '@/components/ConfirmationModel/confirmation-modal';
import { useConfig } from '@/contexts/ConfigContext';


interface TranscriptButtonGroupProps {
  transcriptCount: number;
  onCopyTranscript: () => void;
  onOpenMeetingFolder: () => Promise<void>;
  meetingId?: string;
  meetingFolderPath?: string | null;
  onRefetchTranscripts?: () => Promise<void>;
}


export function TranscriptButtonGroup({
  transcriptCount,
  onCopyTranscript,
  onOpenMeetingFolder,
  meetingId,
  meetingFolderPath,
  onRefetchTranscripts,
}: TranscriptButtonGroupProps) {
  const { betaFeatures } = useConfig();
  const [showRetranscribeDialog, setShowRetranscribeDialog] = useState(false);
  // Whether the stop pass rewrote lines whose live text can be restored.
  const [hasLiveText, setHasLiveText] = useState(false);
  const [showRestoreConfirm, setShowRestoreConfirm] = useState(false);
  // The meeting shown now, so a reply for a meeting left behind is ignored.
  const meetingIdRef = useRef(meetingId);
  meetingIdRef.current = meetingId;

  const checkLiveText = useCallback(async () => {
    if (!meetingId) return;
    try {
      const hasLive = await invoke<boolean>('api_meeting_has_live_text', { meetingId });
      if (meetingIdRef.current === meetingId) setHasLiveText(hasLive);
    } catch (error) {
      console.error('Failed to check for live text:', error);
    }
  }, [meetingId]);

  // Checked on load, after the stop pass rewrites lines and after a restore.
  useEffect(() => {
    setHasLiveText(false);
    void checkLiveText();
    const unlisten = listen<{ meeting_id: string }>('meeting-transcript-refined', (event) => {
      if (event.payload.meeting_id === meetingId) void checkLiveText();
    });
    return () => {
      unlisten.then(fn => fn());
    };
  }, [meetingId, checkLiveText]);

  const handleRestoreLiveText = useCallback(async () => {
    setShowRestoreConfirm(false);
    if (!meetingId) return;
    try {
      const lines = await invoke<number>('api_restore_live_text', { meetingId });
      if (lines === 0) {
        toast.info('Nothing to restore');
      } else {
        toast.success(lines === 1 ? 'Restored the live text of 1 line' : `Restored the live text of ${lines} lines`);
      }
    } catch (error) {
      toast.error(String(error));
    }
    await checkLiveText();
  }, [meetingId, checkLiveText]);

  const handleRetranscribeComplete = useCallback(async () => {
    // Refetch transcripts to show the updated data
    if (onRefetchTranscripts) {
      await onRefetchTranscripts();
    }
    // Retranscribing replaces every line, so no live text is left.
    await checkLiveText();
  }, [onRefetchTranscripts, checkLiveText]);

  return (
    <div className="flex items-center justify-center w-full gap-2">
      <ButtonGroup>
        <Button
          variant="outline"
          size="sm"
          className="px-2 @[22rem]:px-3"
          onClick={() => {
            Analytics.trackButtonClick('copy_transcript', 'meeting_details');
            onCopyTranscript();
          }}
          disabled={transcriptCount === 0}
          title={transcriptCount === 0 ? 'No transcript available' : 'Copy Transcript'}
        >
          <Copy />
          <span className="hidden @[22rem]:inline">Copy</span>
        </Button>

        <Button
          size="sm"
          variant="outline"
          className="px-2 @[22rem]:px-4"
          onClick={() => {
            Analytics.trackButtonClick('open_recording_folder', 'meeting_details');
            onOpenMeetingFolder();
          }}
          title="Open Recording Folder"
        >
          <FolderOpen className="@[22rem]:mr-2" size={18} />
          <span className="hidden @[22rem]:inline">Recording</span>
        </Button>

        {betaFeatures.importAndRetranscribe && meetingId && meetingFolderPath && (
          <Button
            size="sm"
            variant="outline"
            className="bg-gradient-to-r from-blue-50 to-purple-50 hover:from-blue-100 hover:to-purple-100 border-blue-200 px-2 @[22rem]:px-4"
            onClick={() => {
              Analytics.trackButtonClick('enhance_transcript', 'meeting_details');
              setShowRetranscribeDialog(true);
            }}
            title="Retranscribe to enhance your recorded audio"
          >
            <RefreshCw className="@[22rem]:mr-2" size={18} />
            <span className="hidden @[22rem]:inline">Enhance</span>
          </Button>
        )}

        {meetingId && hasLiveText && (
          <Button
            size="sm"
            variant="outline"
            className="px-2 @[22rem]:px-4"
            onClick={() => {
              Analytics.trackButtonClick('restore_live_text', 'meeting_details');
              setShowRestoreConfirm(true);
            }}
            title="Put back the text written live, before the transcript was improved"
          >
            <RotateCcw className="@[22rem]:mr-2" size={18} />
            <span className="hidden @[22rem]:inline">Restore live text</span>
          </Button>
        )}
      </ButtonGroup>

      <ConfirmationModal
        isOpen={showRestoreConfirm}
        title="Restore live text"
        confirmLabel="Restore"
        text="Replace the improved transcript with the text written live during the meeting? The improved text is discarded."
        onConfirm={handleRestoreLiveText}
        onCancel={() => setShowRestoreConfirm(false)}
      />

      {betaFeatures.importAndRetranscribe && meetingId && meetingFolderPath && (
        <RetranscribeDialog
          open={showRetranscribeDialog}
          onOpenChange={setShowRetranscribeDialog}
          meetingId={meetingId}
          meetingFolderPath={meetingFolderPath}
          onComplete={handleRetranscribeComplete}
        />
      )}
    </div>
  );
}
