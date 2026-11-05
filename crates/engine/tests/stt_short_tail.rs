use cantrip_engine::stt::{Transcriber, Transcript};
use std::path::{Path, PathBuf};

#[test]
#[ignore = "requires real Parakeet weights; set CANTRIP_PARAKEET_MODEL_DIR and run on Boat"]
fn local_parakeet_keeps_the_final_phrase_of_a_split_recording() {
    let model = PathBuf::from(
        std::env::var_os("CANTRIP_PARAKEET_MODEL_DIR")
            .expect("set CANTRIP_PARAKEET_MODEL_DIR to installed Parakeet v3 int8 weights"),
    );
    let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/stt-short-tail.wav");
    let mut transcriber = Transcriber::load(&model).expect("loading real local Parakeet");
    let transcript = transcriber
        .transcribe_wav(&wav, None, |_| {})
        .expect("recognizing the public short-tail reproduction");
    assert!(matches!(transcript, Transcript::Complete(_)));
    let words = transcript
        .text()
        .split(|character: char| !character.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    let final_phrase = "a brighter and more welcoming place to work";
    assert!(
        words.ends_with(final_phrase),
        "local Parakeet lost the reproduction's final phrase"
    );
    assert_eq!(words.matches(final_phrase).count(), 1, "no repeated seam");
}
