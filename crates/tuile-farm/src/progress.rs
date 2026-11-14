// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A long piece of work's progress, said while it goes on: what a render
//! service tells whoever asked for a segment or a film's final stage, so
//! that a piece of work that has stopped moving is known for stuck within
//! minutes, not when the asker's deadline comes.
//!
//! ```text
//! POST /segments | /compress    Accept: application/x-ndjson
//! ← 200, application/x-ndjson   one Line a line, until the last:
//!   {"line":"progress","step":"render","waiting":false,"frames":120,"encoded":117,"of":450,"bytes":0,"pass":0}
//!   {"line":"done","output":…}                    what a plain request is answered, as JSON
//!   {"line":"failed","failure":{"message":"…"}}   why not
//! ```
//!
//! A request that does not ask for lines is answered as it always was, with
//! the whole answer at the end: a service that says its progress and an
//! asker that does not read it, or the other way round, still work together.
//!
//! **A progress line says the work moved on, and nothing else does.** One is
//! written when something has changed since the last — a frame more
//! rendered, decoded or encoded, bytes more fetched, a step begun — at most
//! once every [`Cadence::line_every`]: a step that stops moving (a card or an
//! encoder that hangs, a read that never returns) goes silent, and the
//! asker's silence bound catches it.
//!
//! **A wait is not a silence.** A step that waits for something outside the
//! work — the pack it renders ([`Step::PackWait`]), its turn for the card
//! ([`Step::CardWait`]) — is said `waiting`, and said again every
//! [`Cadence::waiting_every`] while it waits: the asker heartbeats on it,
//! and bounds the time spent waiting apart from its silence bound.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

/// The answer's content type, and what a request asks for in `Accept` to be
/// answered in lines: a JSON document a line.
pub const NDJSON: &str = "application/x-ndjson";

/// Whether a request's `Accept` asks for lines.
pub fn asks_for_lines(accept: Option<&str>) -> bool {
    accept.is_some_and(|accept| {
        accept.split(',').any(|kind| {
            kind.split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case(NDJSON))
        })
    })
}

/// Where a piece of work is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Taken, nothing begun yet.
    #[default]
    Start,
    /// Waiting for a segment's pack to be in the store.
    PackWait,
    /// Reading what the work is made of: the film's packs, its sun.
    Plan,
    /// The tiles a segment reads, fetched before its first frame.
    FetchTiles,
    /// Waiting for a turn on the card.
    CardWait,
    /// Frames drawn, each handed to the encoder.
    Render,
    /// Encoding: a segment's encoder writing its last frames out, or a
    /// film's final stage decoding and encoding it whole.
    Encode,
    /// The segments of a film fetched and joined.
    Gather,
    /// What was made, put in the store.
    Upload,
    /// A step this side does not know, said by a newer one.
    #[serde(other)]
    Other,
}

impl Step {
    /// Whether the step waits for something outside the work, rather than
    /// working.
    pub fn is_wait(self) -> bool {
        matches!(self, Step::PackWait | Step::CardWait)
    }

    /// Its name, as a line says it.
    pub fn name(self) -> &'static str {
        match self {
            Step::Start => "start",
            Step::PackWait => "pack_wait",
            Step::Plan => "plan",
            Step::FetchTiles => "fetch_tiles",
            Step::CardWait => "card_wait",
            Step::Render => "render",
            Step::Encode => "encode",
            Step::Gather => "gather",
            Step::Upload => "upload",
            Step::Other => "other",
        }
    }
}

/// How far a piece of work has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    pub step: Step,
    /// The step waits rather than works ([`Step::is_wait`]).
    pub waiting: bool,
    /// Frames drawn (a segment) or decoded (a final stage), in this pass.
    pub frames: u64,
    /// Frames the encoder has taken, in this pass: a few behind `frames`,
    /// as many as the encoder holds before the render waits for it.
    pub encoded: u64,
    /// The frames the work has in all; 0 while unknown.
    pub of: u64,
    /// Bytes moved: the segments fetched, what was put in the store.
    pub bytes: u64,
    /// The encode of a final stage, from 1: a film over its size cap is
    /// encoded again. 0 where there is none.
    pub pass: u32,
}

/// Why a piece of work did not end well.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Failure {
    pub message: String,
}

/// One line of an answer: its progress, then its output or its failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "line", rename_all = "snake_case")]
pub enum Line<T = serde_json::Value> {
    /// The work has moved on — or waits, and says so.
    Progress(Report),
    /// The work's answer: the last line.
    Done { output: T },
    /// Why there is none: the last line.
    Failed { failure: Failure },
}

impl<T: Serialize> Line<T> {
    /// The line as it is written: JSON and a newline.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(self).unwrap_or_else(|e| {
            serde_json::to_vec(&Line::<()>::Failed {
                failure: Failure {
                    message: format!("an answer that is not JSON: {e}"),
                },
            })
            .unwrap_or_default()
        });
        bytes.push(b'\n');
        bytes
    }
}

impl<T> Line<T> {
    /// Whether nothing comes after this line.
    pub fn is_last(&self) -> bool {
        matches!(self, Line::Done { .. } | Line::Failed { .. })
    }
}

/// A piece of work's progress, told as it goes: cloned into whatever does
/// the work — a render's thread, an encoder's, a fetch — and read by
/// whoever says it ([`said`]). Cheap: a frame told is a lock and a wake.
#[derive(Debug, Clone)]
pub struct Progress(Arc<watch::Sender<Report>>);

impl Default for Progress {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(Report::default())))
    }
}

impl Progress {
    /// A piece of work that has not begun.
    pub fn new() -> Self {
        Self::default()
    }

    /// Where the work is now.
    pub fn report(&self) -> Report {
        *self.0.borrow()
    }

    /// The work's reports, as they change.
    pub fn watch(&self) -> watch::Receiver<Report> {
        self.0.subscribe()
    }

    /// `step` begun: what is counted goes on being counted.
    pub fn step(&self, step: Step) {
        self.0.send_if_modified(|r| {
            let was = *r;
            r.step = step;
            r.waiting = step.is_wait();
            *r != was
        });
    }

    /// The frames the work has in all.
    pub fn of(&self, frames: u64) {
        self.0
            .send_if_modified(|r| std::mem::replace(&mut r.of, frames) != frames);
    }

    /// One frame more drawn, or decoded.
    pub fn frame(&self) {
        self.0.send_modify(|r| r.frames += 1);
    }

    /// `n` frames more taken by the encoder.
    pub fn encoded(&self, n: u64) {
        if n > 0 {
            self.0.send_modify(|r| r.encoded += n);
        }
    }

    /// `n` bytes more moved.
    pub fn bytes(&self, n: u64) {
        if n > 0 {
            self.0.send_modify(|r| r.bytes += n);
        }
    }

    /// The next encode of the whole work begun: its frames counted anew.
    pub fn next_pass(&self) {
        self.0.send_modify(|r| {
            r.pass += 1;
            r.frames = 0;
            r.encoded = 0;
        });
    }
}

/// How often lines are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// The most often a progress line is written: what changed meanwhile
    /// is said in one.
    pub line_every: Duration,
    /// How often a step that waits is said to wait, while nothing changes.
    pub waiting_every: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            line_every: Duration::from_secs(1),
            waiting_every: Duration::from_secs(10),
        }
    }
}

/// `work` run to its end, its progress written on `lines` meanwhile, as
/// `cadence` says: a line at once, then one whenever the report has changed
/// since the last, then the work's output or its failure. Once `lines` is
/// closed — whoever asked has gone — `work` is dropped and nothing more is
/// written: nothing is done for an asker that is gone.
pub async fn said<T, E: std::fmt::Display>(
    work: impl Future<Output = Result<T, E>>,
    progress: &Progress,
    cadence: Cadence,
    lines: mpsc::Sender<Line<T>>,
) {
    tokio::pin!(work);
    let mut tick = tokio::time::interval(cadence.line_every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last: Option<(Report, tokio::time::Instant)> = None;
    let ended = loop {
        tokio::select! {
            biased;
            ended = &mut work => break ended,
            _ = lines.closed() => return,
            _ = tick.tick() => {
                let now = progress.report();
                let due = match last {
                    None => true,
                    Some((said, at)) => {
                        said != now || (now.waiting && at.elapsed() >= cadence.waiting_every)
                    }
                };
                if due {
                    if lines.send(Line::Progress(now)).await.is_err() {
                        return;
                    }
                    last = Some((now, tokio::time::Instant::now()));
                }
            }
        }
    };
    let last = match ended {
        Ok(output) => Line::Done { output },
        Err(e) => Line::Failed {
            failure: Failure {
                message: e.to_string(),
            },
        },
    };
    let _ = lines.send(last).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const QUICK: Cadence = Cadence {
        line_every: Duration::from_millis(20),
        waiting_every: Duration::from_millis(200),
    };

    #[test]
    fn a_request_asks_for_lines_by_its_accept() {
        assert!(asks_for_lines(Some("application/x-ndjson")));
        assert!(asks_for_lines(Some(
            "application/json;q=0.5, Application/X-NDJSON; q=1"
        )));
        assert!(!asks_for_lines(Some("application/json")));
        assert!(!asks_for_lines(Some("*/*")));
        assert!(!asks_for_lines(None));
    }

    /// Each line is one JSON document on one line, read back as written;
    /// a line from a newer service, with a step and fields this side does
    /// not know, still reads.
    #[test]
    fn lines_are_one_json_document_a_line() {
        let lines: [Line; 3] = [
            Line::Progress(Report {
                step: Step::Render,
                frames: 120,
                encoded: 117,
                of: 450,
                ..Report::default()
            }),
            Line::Done {
                output: serde_json::json!({"frames": 450}),
            },
            Line::Failed {
                failure: Failure {
                    message: "no\ncard".into(),
                },
            },
        ];
        for line in &lines {
            let bytes = line.to_bytes();
            assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1, "{line:?}");
            assert_eq!(
                &serde_json::from_slice::<Line>(&bytes).expect("a line"),
                line
            );
        }
        assert_eq!(
            String::from_utf8(lines[0].to_bytes()).expect("utf-8"),
            "{\"line\":\"progress\",\"step\":\"render\",\"waiting\":false,\"frames\":120,\
             \"encoded\":117,\"of\":450,\"bytes\":0,\"pass\":0}\n"
        );
        let newer: Line = serde_json::from_str(
            r#"{"line":"progress","step":"denoise","frames":3,"temperature":71}"#,
        )
        .expect("a newer line");
        assert_eq!(
            newer,
            Line::Progress(Report {
                step: Step::Other,
                frames: 3,
                ..Report::default()
            })
        );
        assert!(!lines[0].is_last() && lines[1].is_last() && lines[2].is_last());
    }

    #[test]
    fn a_step_that_waits_is_said_waiting() {
        let progress = Progress::new();
        for (step, waiting) in [
            (Step::PackWait, true),
            (Step::Plan, false),
            (Step::CardWait, true),
            (Step::Render, false),
        ] {
            progress.step(step);
            assert_eq!(progress.report().waiting, waiting, "{step:?}");
        }
        progress.frame();
        progress.encoded(2);
        progress.next_pass();
        let r = progress.report();
        assert_eq!(
            (r.frames, r.encoded, r.pass),
            (0, 0, 1),
            "a pass counts anew"
        );
    }

    async fn run<T: Send + 'static>(
        work: impl Future<Output = Result<T, String>> + Send + 'static,
        progress: Progress,
    ) -> Vec<(Duration, Line<T>)> {
        let (tx, mut rx) = mpsc::channel(64);
        let began = Instant::now();
        tokio::spawn(async move { said(work, &progress, QUICK, tx).await });
        let mut got = Vec::new();
        while let Some(line) = rx.recv().await {
            got.push((began.elapsed(), line));
        }
        got
    }

    /// A line as the work moves on — and none while it does not.
    #[tokio::test]
    async fn the_work_is_said_as_it_moves_and_not_while_it_hangs() {
        let progress = Progress::new();
        let worker = progress.clone();
        let lines = run(
            async move {
                worker.step(Step::Render);
                worker.of(5);
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    worker.frame();
                }
                // Wedged: nothing moves for 600 ms.
                tokio::time::sleep(Duration::from_millis(600)).await;
                Ok::<_, String>(5u64)
            },
            progress,
        )
        .await;
        let frames: Vec<u64> = lines
            .iter()
            .filter_map(|(_, l)| match l {
                Line::Progress(r) => Some(r.frames),
                _ => None,
            })
            .collect();
        assert!(
            frames.windows(2).all(|w| w[0] < w[1]),
            "a line only when it moved: {frames:?}"
        );
        assert_eq!(frames.last(), Some(&5));
        assert!(frames.len() >= 4, "a line for most frames: {frames:?}");
        // Nothing between the last frame and the end: the hang is silent.
        let progress_at: Vec<Duration> = lines
            .iter()
            .filter(|(_, l)| matches!(l, Line::Progress(_)))
            .map(|(at, _)| *at)
            .collect();
        let (last_said, ended) = (
            progress_at[progress_at.len() - 1],
            lines.last().expect("an end").0,
        );
        assert!(
            ended - last_said >= Duration::from_millis(500),
            "silent while wedged: last line at {last_said:?}, end at {ended:?}"
        );
        assert_eq!(lines.last().map(|l| &l.1), Some(&Line::Done { output: 5 }));
    }

    /// A wait is said, again and again while it lasts; it is not taken for
    /// a silence.
    #[tokio::test]
    async fn a_wait_is_said_while_it_lasts() {
        let progress = Progress::new();
        let worker = progress.clone();
        let lines = run(
            async move {
                worker.step(Step::CardWait);
                tokio::time::sleep(Duration::from_millis(1000)).await;
                worker.step(Step::Render);
                Ok::<_, String>(())
            },
            progress,
        )
        .await;
        let waiting = lines
            .iter()
            .filter(
                |(_, l)| matches!(l, Line::Progress(r) if r.waiting && r.step == Step::CardWait),
            )
            .count();
        assert!(
            waiting >= 4,
            "a 1 s wait said every 200 ms: {waiting} lines"
        );
    }

    #[tokio::test]
    async fn a_failure_is_the_last_line() {
        let lines = run(
            async { Err::<(), _>("the card is gone".to_string()) },
            Progress::new(),
        )
        .await;
        assert_eq!(
            lines.last().map(|l| &l.1),
            Some(&Line::Failed {
                failure: Failure {
                    message: "the card is gone".into()
                }
            })
        );
    }

    /// Whoever asked has gone: the work is dropped, not run to its end.
    #[tokio::test]
    async fn an_asker_gone_drops_the_work() {
        struct Dropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = Dropped(dropped.clone());
        let (tx, mut rx) = mpsc::channel::<Line<()>>(4);
        let progress = Progress::new();
        let task = tokio::spawn(async move {
            said(
                async move {
                    let _held = held;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok::<_, String>(())
                },
                &progress,
                QUICK,
                tx,
            )
            .await
        });
        assert!(matches!(rx.recv().await, Some(Line::Progress(_))));
        drop(rx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("ended once the asker went")
            .expect("no panic");
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}
