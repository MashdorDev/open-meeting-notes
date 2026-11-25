/// Application configuration constants
///
/// Centralized definitions for default models and settings.
/// Used across database initialization, import, and retranscription.

/// Default Whisper model for transcription when no preference is configured.
/// This is the recommended balance of accuracy and speed.
pub const DEFAULT_WHISPER_MODEL: &str = "large-v3-turbo";

/// Default Parakeet model for transcription when no preference is configured.
/// This is the quantized version optimized for speed.
pub const DEFAULT_PARAKEET_MODEL: &str = "parakeet-tdt-0.6b-v3-int8";

/// Whisper model catalog with metadata for all supported models.
/// Used by both WhisperEngine::discover_models() and discover_models_standalone().
///
/// Format: (name, filename, size_mb, accuracy, speed, description)
pub const WHISPER_MODEL_CATALOG: &[(&str, &str, u32, &str, &str, &str)] = &[
    // Standard f16 models (full precision)
    ("tiny", "ggml-tiny.bin", 74, "Decent", "Very Fast", "Fastest processing, good for real-time use"),
    ("base", "ggml-base.bin", 142, "Good", "Fast", "Good balance of speed and accuracy"),
    ("small", "ggml-small.bin", 466, "Good", "Medium", "Better accuracy, moderate speed"),
    ("medium", "ggml-medium.bin", 1463, "High", "Slow", "High accuracy for professional use"),
    ("large-v3-turbo", "ggml-large-v3-turbo.bin", 1549, "High", "Medium", "Best accuracy with improved speed"),
    ("large-v3", "ggml-large-v3.bin", 2951, "High", "Slow", "Most Accurate, latest large model"),

    // Q5_1 quantized models (balanced speed/accuracy, slightly better quality than Q5_0)
    ("tiny-q5_1", "ggml-tiny-q5_1.bin", 31, "Decent", "Very Fast", "Quantized tiny model, ~50% faster processing"),
    ("base-q5_1", "ggml-base-q5_1.bin", 57, "Good", "Fast", "Quantized base model, good speed/accuracy balance"),
    ("small-q5_1", "ggml-small-q5_1.bin", 181, "Good", "Fast", "Quantized small model, faster than f16 version"),

    // Q5_0 quantized models (balanced speed/accuracy)
    ("medium-q5_0", "ggml-medium-q5_0.bin", 514, "High", "Medium", "Quantized medium model, professional quality"),
    ("large-v3-turbo-q5_0", "ggml-large-v3-turbo-q5_0.bin", 547, "High", "Medium", "Quantized large model, best balance"),
    ("large-v3-q5_0", "ggml-large-v3-q5_0.bin", 1031, "High", "Slow", "Quantized large model, high accuracy"),

    // Language-tuned models (ivrit.ai, Apache-2.0). Each one always transcribes in
    // its own language; see whisper_model_language.
    ("he-large-v3-turbo", "ggml-he-large-v3-turbo.bin", 1545, "High", "Medium", "Hebrew. Large V3 Turbo tuned on Hebrew speech by ivrit.ai"),
    ("yi-large-v3-turbo", "ggml-yi-large-v3-turbo.bin", 1545, "High", "Medium", "Yiddish. Large V3 Turbo tuned on Yiddish speech by ivrit.ai"),
];

/// The language a language-tuned Whisper model must run in, or None for the
/// multilingual models. ivrit.ai's models lose most of their language detection
/// in tuning, so their language is set explicitly instead of auto-detected.
pub fn whisper_model_language(model_name: &str) -> Option<&'static str> {
    match model_name {
        "he-large-v3-turbo" => Some("he"),
        "yi-large-v3-turbo" => Some("yi"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tuned_models_force_their_language() {
        assert_eq!(whisper_model_language("he-large-v3-turbo"), Some("he"));
        assert_eq!(whisper_model_language("yi-large-v3-turbo"), Some("yi"));
        assert_eq!(whisper_model_language("large-v3-turbo"), None);
    }

    #[test]
    fn every_language_tuned_model_is_in_the_catalog() {
        for name in ["he-large-v3-turbo", "yi-large-v3-turbo"] {
            let entry = WHISPER_MODEL_CATALOG.iter().find(|m| m.0 == name);
            assert_eq!(entry.map(|m| m.1), Some(format!("ggml-{}.bin", name).as_str()));
        }
    }
}
