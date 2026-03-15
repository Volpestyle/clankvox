use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::ipc::{send_msg, OutMsg};

/// A flag that gates whether the IPC layer should attempt to send.
/// Set to `true` once the IPC writer thread is ready.
static IPC_LOG_READY: AtomicBool = AtomicBool::new(false);

pub fn mark_ipc_log_ready() {
    IPC_LOG_READY.store(true, Ordering::Release);
}

pub struct IpcLogLayer;

impl<S: Subscriber> Layer<S> for IpcLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !IPC_LOG_READY.load(Ordering::Acquire) {
            return;
        }

        let metadata = event.metadata();
        let level = metadata.level();

        // Only forward info, warn, error (skip debug/trace to avoid flooding IPC).
        if *level > tracing::Level::INFO {
            return;
        }

        let level_str = match *level {
            tracing::Level::ERROR => "error",
            tracing::Level::WARN => "warn",
            tracing::Level::INFO => "info",
            _ => return,
        };

        let target = metadata.target().to_string();

        // Collect fields from the event into a JSON object.
        let mut fields = serde_json::Map::new();
        let mut message = String::new();

        struct FieldVisitor<'a> {
            fields: &'a mut serde_json::Map<String, serde_json::Value>,
            message: &'a mut String,
        }

        impl<'a> tracing::field::Visit for FieldVisitor<'a> {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    *self.message = value.to_string();
                } else {
                    self.fields.insert(
                        field.name().to_string(),
                        serde_json::Value::String(value.to_string()),
                    );
                }
            }

            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let s = format!("{value:?}");
                if field.name() == "message" {
                    *self.message = s;
                } else {
                    self.fields
                        .insert(field.name().to_string(), serde_json::Value::String(s));
                }
            }

            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.fields.insert(
                    field.name().to_string(),
                    serde_json::Value::Number(value.into()),
                );
            }

            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.fields.insert(
                    field.name().to_string(),
                    serde_json::Value::Number(value.into()),
                );
            }

            fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                self.fields
                    .insert(field.name().to_string(), serde_json::Value::Bool(value));
            }

            fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
                if let Some(n) = serde_json::Number::from_f64(value) {
                    self.fields
                        .insert(field.name().to_string(), serde_json::Value::Number(n));
                }
            }
        }

        event.record(&mut FieldVisitor {
            fields: &mut fields,
            message: &mut message,
        });

        send_msg(&OutMsg::Log {
            level: level_str.to_string(),
            target,
            message,
            fields: serde_json::Value::Object(fields),
        });
    }
}
