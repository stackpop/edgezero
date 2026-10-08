//! Workers-only KV acquisition and caller-sized byte reads.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use edgezero_core::config_store::ConfigStoreError;
use edgezero_core::time::{Deadline, MonotonicClock};
use wasm_streams::readable::ReadableStreamBYOBReader;
use worker::Delay;
use worker::js_sys::{Function, Object, Promise, Reflect, Uint8Array, global};
use worker::kv::KvStore as WorkerKvStore;
use worker::wasm_bindgen::closure::Closure;
use worker::wasm_bindgen::{JsCast as _, JsValue};
use worker::wasm_bindgen_futures::JsFuture;
use worker::web_sys::ReadableStream;

const CONFIG_READ_CHUNK_BYTES: usize = 0x4000;

struct Acquisition(Rc<RefCell<AcquisitionState>>);

#[derive(Default)]
struct AcquisitionState {
    abandoned: bool,
    missing: bool,
    stream: Option<ReadableStream>,
}

struct ConfigReader<'stream> {
    completed: bool,
    reader: ReadableStreamBYOBReader<'stream>,
}

pub(super) struct KvBinding {
    get: Function,
    namespace: JsValue,
    store: WorkerKvStore,
}

impl Drop for Acquisition {
    fn drop(&mut self) {
        let acquired = {
            let mut state = self.0.borrow_mut();
            state.abandoned = true;
            state.stream.take()
        };
        if let Some(stream) = acquired {
            consume_cancellation(&stream.cancel());
        }
    }
}

impl Drop for ConfigReader<'_> {
    fn drop(&mut self) {
        if !self.completed {
            consume_cancellation(&self.reader.as_raw().cancel());
        }
    }
}

impl KvBinding {
    pub(super) fn from_this(env: &JsValue, name: &str) -> Result<Self, ConfigStoreError> {
        let store = WorkerKvStore::from_this(env, name).map_err(|_error| unavailable())?;
        let namespace = Reflect::get(env, &JsValue::from(name)).map_err(|_error| unavailable())?;
        let get = Reflect::get(&namespace, &JsValue::from("get"))
            .map_err(|_error| unavailable())?
            .dyn_into::<Function>()
            .map_err(|_error| unavailable())?;
        Ok(Self {
            get,
            namespace,
            store,
        })
    }

    pub(super) async fn materialized_get(
        &self,
        key: &str,
    ) -> Result<Option<String>, ConfigStoreError> {
        self.store
            .get(key)
            .text()
            .await
            .map_err(|_error| unavailable())
    }

    pub(super) async fn streaming_get(
        &self,
        key: &str,
        max_bytes: u64,
        clock: &MonotonicClock,
        deadline: Deadline,
    ) -> Result<Option<String>, ConfigStoreError> {
        // Transferred callbacks require wasm-bindgen's JS-GC cleanup support.
        // Older/explicitly disabled Workers profiles must fail before dispatch.
        if Reflect::get(&global(), &JsValue::from("FinalizationRegistry"))
            .map_err(|_error| unavailable())?
            .dyn_into::<Function>()
            .is_err()
        {
            return Err(unavailable());
        }
        let options = Object::new();
        Reflect::set(&options, &JsValue::from("type"), &JsValue::from("stream"))
            .map_err(|_error| unavailable())?;
        let promise = self
            .get
            .call2(&self.namespace, &JsValue::from(key), &options)
            .map_err(|_error| unavailable())?
            .dyn_into::<Promise>()
            .map_err(|_error| unavailable())?;
        let acquisition = Acquisition(Rc::new(RefCell::new(AcquisitionState::default())));
        let state = Rc::clone(&acquisition.0);
        // The JS promise retains the callback even if the Rust waiter is dropped.
        // Transfer the closure to JS GC ownership; no detached Rust cleanup task.
        let resolved = Closure::new(move |value: JsValue| {
            if value.is_null() {
                state.borrow_mut().missing = true;
                return;
            }
            if let Ok(stream) = value.dyn_into::<ReadableStream>() {
                let mut acquired = state.borrow_mut();
                if acquired.abandoned {
                    drop(acquired);
                    consume_cancellation(&stream.cancel());
                } else {
                    acquired.stream = Some(stream);
                }
            }
        });
        let observed = promise.then(&resolved);
        drop(resolved.into_js_value());
        JsFuture::from(observed)
            .await
            .map_err(|_error| unavailable())?;
        if acquisition.0.borrow().missing {
            return Ok(None);
        }
        let raw = acquisition
            .0
            .borrow_mut()
            .stream
            .take()
            .ok_or_else(unavailable)?;
        drain(raw, max_bytes, clock, deadline).await.map(Some)
    }
}

fn consume_cancellation(promise: &Promise) {
    // Both branches must be handled: an ignored rejected cancel promise can
    // otherwise publish secret-bearing provider diagnostics as an unhandled rejection.
    let ignore = Closure::new(|_value: JsValue| {});
    drop(promise.then2(&ignore, &ignore));
    drop(ignore.into_js_value());
}

async fn drain(
    raw: ReadableStream,
    max_bytes: u64,
    clock: &MonotonicClock,
    deadline: Deadline,
) -> Result<String, ConfigStoreError> {
    let mut stream = wasm_streams::ReadableStream::from_raw(raw.clone().unchecked_into());
    let reader = match stream.try_get_byob_reader() {
        Ok(reader) => reader,
        Err(_error) => {
            consume_cancellation(&raw.cancel());
            return Err(unavailable());
        }
    };
    let mut guard = ConfigReader {
        completed: false,
        reader,
    };
    let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let mut bytes = Vec::new();
    let mut scratch = vec![0_u8; CONFIG_READ_CHUNK_BYTES];
    let mut buffer = Uint8Array::new_with_length(0x4000);
    let mut pulls = 0_u32;
    loop {
        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }
        let remaining = limit.saturating_sub(bytes.len());
        let wanted = remaining.saturating_add(1).min(CONFIG_READ_CHUNK_BYTES);
        let (count, returned_buffer) = guard
            .reader
            .read_with_buffer(scratch.get_mut(..wanted).ok_or_else(unavailable)?, buffer)
            .await
            .map_err(|_error| unavailable())?;
        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }
        buffer = returned_buffer.ok_or_else(unavailable)?;
        if count == 0 {
            guard.completed = true;
            break;
        }
        if count > remaining {
            return Err(ConfigStoreError::ValueTooLarge);
        }
        let next_len = bytes
            .len()
            .checked_add(count)
            .ok_or(ConfigStoreError::ValueTooLarge)?;
        if next_len > bytes.capacity() {
            let capacity = next_len.max(bytes.capacity().saturating_mul(2)).min(limit);
            bytes
                .try_reserve_exact(capacity.saturating_sub(bytes.len()))
                .map_err(|_error| ConfigStoreError::ValueTooLarge)?;
        }
        bytes.extend_from_slice(scratch.get(..count).ok_or_else(unavailable)?);
        pulls = pulls.saturating_add(1);
        if pulls == 16 {
            // Workers may freeze its clock within a task. Yield to the task queue,
            // not just the promise microtask queue, so timers can make progress.
            Delay::from(Duration::ZERO).await;
            pulls = 0;
        }
    }
    String::from_utf8(bytes).map_err(|_error| unavailable())
}

fn unavailable() -> ConfigStoreError {
    ConfigStoreError::unavailable("config KV byte stream is unavailable")
}
