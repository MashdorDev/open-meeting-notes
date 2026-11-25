import { Globe } from 'lucide-react';
import { useConfig } from '@/contexts/ConfigContext';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { LANGUAGES } from './LanguageSelection';
import { LANGUAGE_MODELS, PARAKEET_LANGUAGES } from '@/lib/whisper';
import { Button } from './ui/button';
import { Label } from './ui/label';

interface MeetingLanguagePickerProps {
  provider: string;
  onUseWhisper: () => void;
}

/**
 * The language meetings are held in. Parakeet only knows 25 European
 * languages, so for any other language this points the user to Whisper,
 * and to a language-tuned model where one exists.
 */
export function MeetingLanguagePicker({ provider, onUseWhisper }: MeetingLanguagePickerProps) {
  const { selectedLanguage, setSelectedLanguage } = useConfig();
  // The language is read per chunk, so changing it mid-recording would split a meeting.
  const { isRecording } = useRecordingState();
  const lang = selectedLanguage || 'auto';
  const name = LANGUAGES.find((l) => l.code === lang)?.name ?? lang;
  const isAuto = lang === 'auto' || lang === 'auto-translate';
  const parakeetCannot = provider === 'parakeet' && !isAuto && !PARAKEET_LANGUAGES.includes(lang);
  const tuned = LANGUAGE_MODELS[lang];

  return (
    <div>
      <Label className="flex items-center gap-2 text-sm font-medium text-gray-700 mb-1">
        <Globe className="h-4 w-4 text-gray-500" />
        Meeting language
      </Label>
      <div className="mx-1">
        <select
          value={lang}
          onChange={(e) => setSelectedLanguage(e.target.value)}
          disabled={isRecording}
          className="w-full px-3 py-2 text-sm bg-white border border-gray-300 rounded-md shadow-sm focus:outline-none focus:ring-1 focus:ring-blue-500 focus:border-blue-500 disabled:bg-gray-50 disabled:text-gray-500"
        >
          {LANGUAGES.map((l) => (
            <option key={l.code} value={l.code}>{l.name}</option>
          ))}
        </select>

        {parakeetCannot && (
          <div className="mt-2 rounded-md border border-amber-200 bg-amber-50 p-3 text-sm text-amber-900">
            <p>Parakeet does not transcribe {name}, so its text would come out wrong. Use Local Whisper for {name} meetings.</p>
            <Button size="sm" className="mt-2" onClick={onUseWhisper}>Use Local Whisper</Button>
          </div>
        )}

        {tuned && (provider === 'localWhisper' || parakeetCannot) && (
          <p className="mt-2 text-xs text-gray-500">
            For {name}, download {tuned.label} under Language-tuned Models. It is tuned on {name} speech.
          </p>
        )}

        {provider === 'parakeet' && !parakeetCannot && !isAuto && (
          <p className="mt-2 text-xs text-gray-500">Parakeet detects the language by itself.</p>
        )}
      </div>
    </div>
  );
}
