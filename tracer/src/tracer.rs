// Copyright © 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use log::warn;
use serde::Serialize;

#[derive(Debug)]
struct Tracer {
    events: HashMap<String, Vec<TraceEvent>>,
    start: Instant,
}

impl Tracer {
    fn new() -> Self {
        Self {
            events: HashMap::default(),
            start: Instant::now(),
        }
    }

    fn end(&self) {
        let end = Instant::now();
        // SAFETY: FFI call
        let path = format!("cloud-hypervisor-{}.trace", unsafe { libc::getpid() });
        let mut file = File::create(&path).unwrap();

        #[derive(Serialize)]
        struct TraceReport<'a> {
            duration: Duration,
            events: &'a HashMap<String, Vec<TraceEvent>>,
        }

        let trace_report = TraceReport {
            duration: end.duration_since(self.start),
            events: &self.events,
        };

        serde_json::to_writer_pretty(&file, &trace_report).unwrap();

        file.flush().unwrap();

        warn!("Trace output: {path}");
    }

    fn add_event(&mut self, event: TraceEvent) {
        let current = thread::current();
        let thread_name = current.name().unwrap_or("");
        if let Some(thread_events) = self.events.get_mut(thread_name) {
            thread_events.push(event);
        } else {
            self.events.insert(thread_name.to_string(), vec![event]);
        }
    }
}

// `None` until start() is called. Tracing is a no-op in that state.
static TRACER: Mutex<Option<Tracer>> = Mutex::new(None);

thread_local! {
    // Number of open trace blocks on this thread.
    static OPEN_BLOCKS: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Debug, Serialize)]
struct TraceEvent {
    timestamp: Duration,
    event: &'static str,
    end_timestamp: Option<Duration>,
    depth: u64,
}

pub fn trace_point_log(event: &'static str) {
    if let Some(tracer) = TRACER.lock().unwrap().as_mut() {
        let trace_event = TraceEvent {
            timestamp: Instant::now().duration_since(tracer.start),
            event,
            end_timestamp: None,
            depth: OPEN_BLOCKS.get().saturating_sub(1),
        };
        tracer.add_event(trace_event);
    }
}

pub struct TraceBlock {
    start: Instant,
    event: &'static str,
}

impl TraceBlock {
    pub fn new(event: &'static str) -> Self {
        OPEN_BLOCKS.set(OPEN_BLOCKS.get() + 1);
        Self {
            start: Instant::now(),
            event,
        }
    }
}

impl Drop for TraceBlock {
    fn drop(&mut self) {
        let depth = OPEN_BLOCKS.get().saturating_sub(1);
        OPEN_BLOCKS.set(depth);
        if let Some(tracer) = TRACER.lock().unwrap().as_mut() {
            let trace_event = TraceEvent {
                timestamp: self.start.duration_since(tracer.start),
                event: self.event,
                end_timestamp: Some(Instant::now().duration_since(tracer.start)),
                depth,
            };
            tracer.add_event(trace_event);
        }
    }
}

#[macro_export]
macro_rules! trace_point {
    ($event:expr) => {
        $crate::trace_point_log($event)
    };
}

#[macro_export]
macro_rules! trace_scoped {
    ($event:expr) => {
        let _trace_scoped = $crate::TraceBlock::new($event);
    };
}

pub fn end() {
    if let Some(tracer) = TRACER.lock().unwrap().as_ref() {
        tracer.end();
    }
}

pub fn start() {
    *TRACER.lock().unwrap() = Some(Tracer::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test, as the tracer is global state.
    #[test]
    fn test_tracing_without_start_and_restart() {
        {
            crate::trace_scoped!("not-started");
            crate::trace_point!("not-started");
        }
        end();
        assert!(TRACER.lock().unwrap().is_none());

        start();
        {
            crate::trace_scoped!("outer");
            crate::trace_point!("point");
            crate::trace_scoped!("inner");
        }
        let depths: Vec<_> = TRACER
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .events
            .values()
            .flatten()
            .map(|e| (e.event, e.depth))
            .collect();
        assert_eq!(depths, [("point", 0), ("inner", 1), ("outer", 0)]);

        start();
        assert!(TRACER.lock().unwrap().as_ref().unwrap().events.is_empty());
    }
}
