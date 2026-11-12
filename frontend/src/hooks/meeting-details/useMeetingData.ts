import { useState, useCallback, useRef, useEffect } from 'react';
import { MeetingSummary, Summary, Transcript } from '@/types';
import { BlockNoteSummaryViewRef } from '@/components/AISummary/BlockNoteSummaryView';
import { CurrentMeeting, useSidebar } from '@/components/Sidebar/SidebarProvider';
import { invoke as invokeTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { hasVisibleSummaryContent } from '@/lib/summary-content';

interface UseMeetingDataProps {
  meeting: any;
  summaryData: MeetingSummary | null;
  onMeetingUpdated?: () => Promise<void>;
}

export function useMeetingData({ meeting, summaryData, onMeetingUpdated }: UseMeetingDataProps) {
  // State
  // Use prop directly since summary generation fetches transcripts independently
  const transcripts = meeting.transcripts;
  const [meetingTitle, setMeetingTitle] = useState(meeting.title || '+ New Call');
  const [aiSummary, setAiSummary] = useState<MeetingSummary | null>(summaryData);
  const [isSaving, setIsSaving] = useState(false);
  const [isSummaryDirty, setIsSummaryDirty] = useState(false);

  // Ref for BlockNoteSummaryView
  const blockNoteSummaryRef = useRef<BlockNoteSummaryViewRef>(null);

  // Sidebar context
  const { setCurrentMeeting, setMeetings, meetings: sidebarMeetings } = useSidebar();

  // Sync aiSummary state when summaryData prop changes (fixes display of fetched summaries)
  useEffect(() => {
    console.log('[useMeetingData] Syncing summary data from prop:', summaryData ? 'present' : 'null');
    setAiSummary(summaryData);
  }, [summaryData]); // Only trigger when parent prop changes, not when aiSummary changes

  // Names of the people on the other side, found in the transcript after saving
  const [otherSpeakerNames, setOtherSpeakerNames] = useState<string[]>([]);
  useEffect(() => {
    let cancelled = false;
    setOtherSpeakerNames([]);
    invokeTauri<string[]>('api_get_meeting_speakers', { meetingId: meeting.id })
      .then(names => { if (!cancelled) setOtherSpeakerNames(names); })
      .catch(error => console.error('Failed to load speaker names:', error));
    const unlisten = listen<{ meeting_id: string; names: string[] }>('meeting-speakers-identified', (event) => {
      if (event.payload.meeting_id === meeting.id) {
        setOtherSpeakerNames(event.payload.names);
      }
    });
    return () => {
      cancelled = true;
      unlisten.then(fn => fn());
    };
  }, [meeting.id]);

  useEffect(() => {
    const unlisten = listen<{ meeting_id: string; title: string }>('meeting-title-generated', (event) => {
      if (event.payload.meeting_id === meeting.id) {
        setMeetingTitle(event.payload.title);
      }
    });
    return () => {
      unlisten.then(fn => fn());
    };
  }, [meeting.id]);

  const handleSummaryChange = useCallback((newSummary: Summary) => {
    setAiSummary(newSummary);
  }, []);



  const handleSaveSummary = useCallback(async (summary: MeetingSummary) => {
    if (!hasVisibleSummaryContent(summary)) {
      throw new Error('Summary contains no visible content to save.');
    }

    const formattedSummary = 'markdown' in summary || 'summary_json' in summary
      ? summary
      : { MeetingName: meetingTitle, ...summary };
    await invokeTauri('api_save_meeting_summary', {
      meetingId: meeting.id,
      summary: formattedSummary,
    });
  }, [meeting.id, meetingTitle]);

  const saveAllChanges = useCallback(async () => {
    setIsSaving(true);
    try {
      // Save BlockNote editor changes if dirty
      if (blockNoteSummaryRef.current?.isDirty) {
        console.log('💾 Saving BlockNote editor changes...');
        await blockNoteSummaryRef.current.saveSummary();
      } else if (aiSummary) {
        await handleSaveSummary(aiSummary);
      }

      toast.success("Changes saved successfully");
    } catch (error) {
      console.error('Failed to save changes:', error);
      toast.error("Failed to save changes", { description: String(error) });
    } finally {
      setIsSaving(false);
    }
  }, [aiSummary, handleSaveSummary]);

  // Update meeting title from external source (e.g., AI summary)
  const updateMeetingTitle = useCallback((newTitle: string) => {
    console.log('📝 Updating meeting title to:', newTitle);
    setMeetingTitle(newTitle);
    const updatedMeetings = sidebarMeetings.map((m: CurrentMeeting) =>
      m.id === meeting.id ? { id: m.id, title: newTitle } : m
    );
    setMeetings(updatedMeetings);
    setCurrentMeeting({ id: meeting.id, title: newTitle });
  }, [meeting.id, sidebarMeetings, setMeetings, setCurrentMeeting]);

  const saveOtherSpeakerNames = useCallback(async (names: string[]) => {
    try {
      const saved = await invokeTauri<string[]>('api_set_meeting_speakers', { meetingId: meeting.id, names });
      setOtherSpeakerNames(saved);
    } catch (error) {
      console.error('Failed to save speaker names:', error);
      toast.error('Could not save the name', { description: String(error) });
    }
  }, [meeting.id]);

  return {
    // State
    transcripts,
    otherSpeakerNames,
    saveOtherSpeakerNames,
    meetingTitle,
    aiSummary,
    isSaving,
    isSummaryDirty,
    blockNoteSummaryRef,

    // Setters
    setMeetingTitle,
    setAiSummary,
    setIsSummaryDirty,

    // Handlers
    handleSummaryChange,
    handleSaveSummary,
    saveAllChanges,
    updateMeetingTitle,
  };
}
