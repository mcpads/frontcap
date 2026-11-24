//! Capture at level `time` from the generation's screencast (S16).
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};

async fn launched(run: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/color.html"), RuleSet::default())).await.unwrap();
    (runs, g)
}

fn patched() -> bool {
    browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some()
}

/// The colour at pixel (5, 5) of a PNG (8-bit RGB or RGBA, as the engine writes them).
fn pixel(png: &[u8]) -> [u8; 3] {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let bpp = info.color_type.samples();
    let at = (5 * info.width as usize + 5) * bpp;
    [buf[at], buf[at + 1], buf[at + 2]]
}

#[tokio::test]
async fn a_screenshot_shows_the_state_at_its_virtual_instant() {
    let (_runs, mut g) = launched("cap-1").await;
    // The start boundary: the document committed, nothing painted by its script yet.
    // While frozen the renderer may have no frame left for the first paint (S14): `no_frame` then.
    match g.screenshot().await {
        Ok(first) => assert_eq!((first.t_us, first.document), (VirtualTime::ZERO, 1)),
        Err(frontcap_chromium::ChromiumError::NoFrame) => eprintln!("the start boundary has no frame yet"),
        Err(e) => panic!("the start boundary: {e:?}"),
    }
    let colours = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
    for (step, colour) in colours.iter().enumerate() {
        // On the patched engine the first render is at the first tick (17 ms).
        let first = if patched() { 17 } else { 10 };
        g.advance(VirtualDuration::from_ms(if step == 0 { first } else { 50 })).await.unwrap();
        let shot = g.screenshot().await.unwrap();
        // A renderer slower than the frame wait (a loaded machine) leaves the frame behind: it is then
        // answered as not current, never labelled with the state it does not show.
        let shown = g.now();
        // On the patched engine a capture copies the presented surface: always current here.
        assert!(!patched() || shot.current, "step {step}: {shot:?}");
        if shot.current {
            assert_eq!((pixel(&std::fs::read(&shot.path).unwrap()), shot.t_us), (*colour, shown), "step {step}");
        } else {
            eprintln!("step {step}: the frame is behind ({:?}, exact {})", shot.t_us, shot.exact);
        }
    }
}

#[tokio::test]
async fn recording_keeps_every_frame_with_its_instant() {
    let (_runs, mut g) = launched("cap-2").await;
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    g.record_start().await.unwrap();
    for _ in 0..3 {
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    }
    let rec = g.record_stop().await.unwrap();
    let shown: Vec<(VirtualTime, [u8; 3])> = rec.frames.iter().map(|f| (f.t_us, pixel(&std::fs::read(g.trace_path().parent().unwrap().join(&f.file)).unwrap()))).collect();
    // The state at the start (10 ms, red) is kept when its frame is certain; every later instant has one.
    let later: Vec<_> = shown.iter().copied().filter(|(t, _)| *t != VirtualTime(10_000)).collect();
    if patched() {
        // Each render that drew something new, at its tick: the first paint at 17, each colour at the tick
        // after its change.
        let ticks = [17_000, 51_000, 101_000, 151_000].map(VirtualTime);
        assert_eq!(later, ticks.into_iter().zip([[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]]).collect::<Vec<_>>(), "each frame shows its tick");
        return;
    }
    assert_eq!(later, vec![(VirtualTime(60_000), [0, 255, 0]), (VirtualTime(110_000), [0, 0, 255]), (VirtualTime(160_000), [255, 255, 0])], "each frame shows its instant");
    assert!(shown.iter().all(|(t, c)| *t != VirtualTime(10_000) || *c == [255, 0, 0]), "{shown:?}");
}

/// Criterion 12 for capture: screenshots and a recording leave the behaviour log unchanged, including a
/// fetch after input (S15) — the frames come from the screencast, never from a capture.
#[tokio::test]
async fn capture_does_not_change_the_behavior_log() {
    async fn run(addr: std::net::SocketAddr, name: &str, capture: bool) -> String {
        let runs = tempfile::tempdir().unwrap();
        let rules = RuleSet::from_json(r#"[{"id":"save","match":{"url":"https://api.example.test/profile"},"action":{"type":"mock","status":200,"body":"{}"}}]"#).unwrap();
        let mut g = Generation::launch(support::options(runs.path(), name, format!("http://{addr}/profile.html"), rules)).await.unwrap();
        if capture {
            g.record_start().await.unwrap();
        }
        for _ in 0..3 {
            g.advance(VirtualDuration::from_ms(100)).await.unwrap();
            if capture {
                g.screenshot().await.unwrap();
            }
        }
        g.click(frontcap_core::ids::RequestId::new("c-1").unwrap(), &frontcap_core::predicate::Locator::Css { selector: "#save".into() }).await.unwrap();
        if capture {
            g.screenshot().await.unwrap();
        }
        g.advance(VirtualDuration::from_ms(500)).await.unwrap();
        if capture {
            g.screenshot().await.unwrap();
            assert!(!g.record_stop().await.unwrap().frames.is_empty());
        }
        let path = g.trace_path();
        g.stop().await.unwrap();
        std::fs::read_to_string(path).unwrap()
    }
    let addr = support::serve();
    let plain = run(addr, "cap-3a", false).await;
    let captured = run(addr, "cap-3b", true).await;
    assert_eq!(frontcap_core::projection::compare_behavior(&plain, &captured), Ok(()));
}

/// Without recording: after a wait made of many short segments (a held request), the screenshot shows
/// the state at the instant it says; after a click, it no longer claims to be current; after a
/// navigation, an old document's frame is never passed off as the new one.
#[tokio::test]
async fn screenshots_are_labelled_by_the_state_they_show() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(r#"[{"id":"held","match":{"url":"https://api.example.test/held"},"action":{"type":"hold"}}]"#).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "cap-4", format!("http://{addr}/color-held.html"), rules)).await.unwrap();
    let colours = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0], [0, 255, 255], [255, 0, 255], [0, 0, 0]];
    for _ in 0..4 {
        g.advance(VirtualDuration::from_ms(110)).await.unwrap();
        let shot = g.screenshot().await.unwrap();
        // The colour of the step its instant falls in (setInterval of 25 ms from ~0); on the patched engine
        // its instant is the tick that rendered it.
        let step = (shot.t_us.0 / 25_000) as usize % colours.len();
        // An exact frame shows its instant; a frame after states drawn too fast to tell apart says so.
        assert!(!patched() || shot.exact, "{shot:?}");
        if shot.exact {
            assert_eq!(pixel(&std::fs::read(&shot.path).unwrap()), colours[step], "frame at {:?}, now {:?}", shot.t_us, g.now());
        } else {
            assert!(!shot.current);
        }
    }
    g.click(frontcap_core::ids::RequestId::new("c-1").unwrap(), &frontcap_core::predicate::Locator::Css { selector: "#go".into() }).await.unwrap();
    let after = g.screenshot().await.unwrap();
    assert!(!after.current || pixel(&std::fs::read(&after.path).unwrap()) == [255, 255, 255], "a pre-click frame never claims to be current");
    // On the patched engine the click's change is drawn at the next tick, not now.
    assert!(!patched() || !after.current, "{after:?}");
}

/// After the page stops changing, a screenshot taken once virtual time moved on is current: the
/// renderer had frames to draw any change, and the screencast stayed quiet (fresh-consumer run 4).
#[tokio::test]
async fn a_page_that_stopped_changing_has_a_current_screenshot() {
    let (_runs, mut g) = launched("cap-quiet").await;
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    g.screenshot().await.unwrap();
    // Time moves on and nothing is drawn.
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let shot = g.screenshot().await.unwrap();
    assert_eq!((shot.t_us, shot.exact, shot.current), (g.now(), true, true));
    assert_eq!(pixel(&std::fs::read(&shot.path).unwrap()), [0, 0, 0], "the last colour, painted at 300 ms");
}

/// On the patched engine a recording's frames are the renders that drew something new, each at its tick:
/// the same for any slicing of time.
#[tokio::test]
async fn a_recording_is_the_same_for_any_slicing() {
    if !patched() {
        eprintln!("skipped: frames are labelled by render ticks on the patched engine only");
        return;
    }
    async fn frames(run: &str, slices: &[u64]) -> Vec<(VirtualTime, [u8; 3])> {
        let (_runs, mut g) = launched(run).await;
        g.record_start().await.unwrap();
        for ms in slices {
            g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
        }
        let rec = g.record_stop().await.unwrap();
        rec.frames.iter().map(|f| (f.t_us, pixel(&std::fs::read(g.trace_path().parent().unwrap().join(&f.file)).unwrap()))).collect()
    }
    let one = frames("cap-s1", &[200]).await;
    assert_eq!(one, frames("cap-s2", &[5; 40]).await);
    assert_eq!(one, frames("cap-s3", &[7, 43, 100, 50]).await);
    assert_eq!(one.len(), 4, "the first paint and the colours at 50, 100 and 150 ms: {one:?}");
}

/// On the patched engine a capture copies the surface the display shows: after a navigation, before the new
/// document's first render, the old document's surface is never passed off as the new one; and a change made
/// while frozen is not current until a render draws it.
#[tokio::test]
async fn a_capture_never_passes_off_an_undrawn_state() {
    if !patched() {
        eprintln!("skipped: surface copies are the patched engine's capture");
        return;
    }
    let (_runs, mut g) = launched("cap-undrawn").await;
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    let shot = g.screenshot().await.unwrap();
    assert_eq!((pixel(&std::fs::read(&shot.path).unwrap()), shot.current), ([255, 0, 0], true));
    // A change while frozen: drawn at the next tick, not now.
    g.eval_main("document.documentElement.style.background = 'rgb(0,0,0)'").await.unwrap();
    let shot = g.screenshot().await.unwrap();
    assert_eq!((pixel(&std::fs::read(&shot.path).unwrap()), shot.current), ([255, 0, 0], false), "{shot:?}");
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    let shot = g.screenshot().await.unwrap();
    assert_eq!((pixel(&std::fs::read(&shot.path).unwrap()), shot.current, shot.t_us), ([0, 0, 0], true, g.now()));
    // Another document: nothing of it is drawn before its first render.
    let url = g.eval_main("location.href.replace('color.html', 'frames_idle.html')").await.unwrap().as_str().unwrap().to_string();
    g.navigate_action(&frontcap_core::ids::RequestId::new("n-1").unwrap(), &frontcap_core::action::Action::Navigate { url }).await.unwrap();
    assert!(matches!(g.screenshot().await, Err(frontcap_chromium::ChromiumError::StaleFrame | frontcap_chromium::ChromiumError::NoFrame)));
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let shot = g.screenshot().await.unwrap();
    assert_eq!((shot.document, pixel(&std::fs::read(&shot.path).unwrap()), shot.current, shot.t_us), (2, [255, 255, 255], true, g.now()));
    // A recorded change that draws nothing (a same-document navigation): once nothing is left to draw, the
    // surface is the page as it is.
    g.eval_main("history.pushState({}, '', '#x')").await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    let shot = g.screenshot().await.unwrap();
    assert_eq!((shot.exact, shot.current, shot.t_us), (true, true, g.now()));
}

/// A capture changes nothing (engine patch 12 notwithstanding): a task the page posted at a stop between
/// frame ticks has not run after a screenshot; it runs when time moves on.
#[tokio::test]
async fn a_screenshot_runs_no_task_of_the_page() {
    if !patched() {
        return;
    }
    let (_runs, mut g) = launched("cap-still").await;
    g.advance(VirtualDuration::from_ms(25)).await.unwrap();
    g.eval_main("window.ran = false; setTimeout(() => { window.ran = true; }, 0); 0").await.unwrap();
    g.screenshot().await.unwrap();
    assert_eq!(g.eval_main("window.ran").await.unwrap(), false, "the screenshot ran the page's timer");
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("window.ran").await.unwrap(), true);
}
