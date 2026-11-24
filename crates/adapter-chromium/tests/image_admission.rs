//! What a process image of a generation cannot carry is named before one is taken: media playing by the
//! real clock, in the page or a frame.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

const PAGE: &str = r#"<!doctype html><button id="play">Play</button><button id="stop">Stop</button>
<iframe id="child" srcdoc="<audio id=a loop></audio>"></iframe>
<audio id="tone" loop></audio>
<script>
  // A second of silence as a WAV file, so the elements have something to play.
  const wav = () => {
    const rate = 8000, n = rate, view = new DataView(new ArrayBuffer(44 + n));
    const text = (at, s) => [...s].forEach((c, i) => view.setUint8(at + i, c.charCodeAt(0)));
    text(0, 'RIFF'); view.setUint32(4, 36 + n, true); text(8, 'WAVEfmt '); view.setUint32(16, 16, true);
    view.setUint16(20, 1, true); view.setUint16(22, 1, true); view.setUint32(24, rate, true);
    view.setUint32(28, rate, true); view.setUint16(32, 1, true); view.setUint16(34, 8, true);
    text(36, 'data'); view.setUint32(40, n, true);
    for (let i = 0; i < n; i++) view.setUint8(44 + i, 128);
    return URL.createObjectURL(new Blob([view], {type: 'audio/wav'}));
  };
  const both = () => [document.querySelector('#tone'), document.querySelector('#child').contentDocument.querySelector('#a')];
  document.querySelector('#play').onclick = () => { for (const a of both()) { a.src = wav(); a.play(); } };
  document.querySelector('#stop').onclick = () => { for (const a of both()) a.pause(); };
</script>"#;

#[tokio::test]
async fn playing_media_is_named_before_an_image() {
    let rules = RuleSet::from_json(&serde_json::json!([{"id":"page","match":{"url":"https://app.test/*"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":PAGE}}]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut opts = support::options(runs.path(), "media", "https://app.test/media.html".into(), rules);
    opts.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert!(g.playing_media().await.unwrap().is_empty(), "nothing plays yet");
    g.click(RequestId::new("play").unwrap(), &Locator::Css { selector: "#play".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let playing = g.playing_media().await.unwrap();
    assert_eq!(playing, ["audio playing in page", "audio playing in page"], "the page's element and its frame's");
    g.click(RequestId::new("stop").unwrap(), &Locator::Css { selector: "#stop".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert!(g.playing_media().await.unwrap().is_empty(), "paused media is not playing");
    g.stop().await.unwrap();
}
